#[allow(unused_imports)]
use super::*;

/// HotSpot's `ObjectSynchronizer::handle_sync_on_value_based_class` for a
/// `monitorenter` (interpreted, compiled through the helper, or JNI
/// `MonitorEnter`: every route through [`monitor_enter_blocking_or_unmount`])
/// on `obj` under `-XX:DiagnoseSyncOnValueBasedClasses`
/// (`VmConfig::diagnose_sync_on_value_based_classes`; interpreter round i5
/// wave 102, lane L6). Called only with the flag non-zero; the caller pays
/// one per-VM field load when it is 0 (HotSpot's default). Answers `obj`,
/// refreshed: it is pinned across work that may allocate (the JFR event's
/// class mirror). The value-based check and its two answers:
///
/// * `1` (`FATAL_EXIT`): HotSpot's `fatal("Synchronizing on object
///   0x%016x of klass %s %s")`, the last part the locking thread's top
///   stack line from `at` on. The report header goes to stdout (HotSpot's
///   `VMError` writes it to `defaultStream::output_fd()`), then the process
///   aborts, as `-XX:+CrashOnOutOfMemoryError` does here.
/// * `2` (`LOG_WARNING`): `[valuebasedclasses]` info `Synchronizing on
///   object 0x%016x of klass %s`, then the locking thread's stack
///   (`print_active_stack_on`: `\tat ...` lines, the top frame's `\t- locked
///   <0x...> (a ...)` for the object being entered), each line a log line;
///   when `-Xlog` did not select `valuebasedclasses` they go to stdout with
///   the default decorations, as HotSpot's `Arguments` configures stdout for
///   the tag at info under this mode. Then `jdk.SyncOnValueBasedClass`.
///
/// Residue: lock lines of the frames below the top are not printed (HotSpot
/// prints each frame's held monitors); the line number is the frame's
/// current one as a stack capture reports it.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn diagnose_sync_on_value_based_class_cold(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> ObjectRef {
    use crate::runtime::serviceability::{external_class_name, FrameInfo, JstackProcessor};
    use crate::runtime::unified_logging::{
        is_unified_logging_enabled, log_unified, LogLevel, LogTag,
    };
    use std::io::Write as _;

    let class_id = shared.mem.heap.class_id_of(obj);
    let klass = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(class_id) {
            Some(class) if class_is_value_based(class) => external_class_name(&class.name),
            _ => return obj,
        }
    };
    let mode = shared.config.diagnose_sync_on_value_based_classes;
    let addr = obj.as_ptr() as usize;

    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj);

    // Innermost first, each as `print_stack_element` prints it.
    let trace = {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.capture_stack_trace()
    };
    let (lines, top_is_native) = {
        let cm = shared.classes.class_manager.read();
        let lines: Vec<String> = trace
            .iter()
            .rev()
            .map(|e| {
                let frame = FrameInfo {
                    class_name: external_class_name(&e.class_name),
                    method_name: e.method_name.to_string(),
                    file_name: e.source_file.as_deref().map(str::to_owned),
                    line_number: e.line_number,
                    native_method: e.line_number
                        == crate::runtime::stackwalker::LINE_NUMBER_NATIVE,
                };
                let module = thread_dump_frame_module(&cm, e.class_id);
                JstackProcessor::frame_line(&frame, module.as_deref())
            })
            .collect();
        let top_is_native = trace
            .last()
            .is_some_and(|e| e.line_number == crate::runtime::stackwalker::LINE_NUMBER_NATIVE);
        (lines, top_is_native)
    };

    if mode == 1 {
        // Java's buffered console bytes first: HotSpot's `System.out` writes
        // reach the descriptor at once, so they precede the report there.
        let _ = shared.natives.fd_table.flush(1);
        let _ = shared.natives.fd_table.flush(2);
        let at = lines
            .first()
            .map(|l| l.trim_start_matches('\t').to_string())
            .unwrap_or_default();
        {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "#");
            let _ = writeln!(
                out,
                "# A fatal error has been detected by the CratonVM Runtime Environment:"
            );
            let _ = writeln!(out, "#");
            let _ = writeln!(
                out,
                "#  Internal Error (synchronizer.cpp), pid={}, tid={}",
                std::process::id(),
                thread.thread_id.0
            );
            let _ = writeln!(
                out,
                "#  fatal error: Synchronizing on object 0x{addr:016x} of klass {klass} {at}"
            );
            let _ = writeln!(out, "#");
            // Interpreter round i5 wave 105, lane L6 (page
            // `i101-L6-diagnose-sync-on-value-based-classes-ignored`, residue):
            // HotSpot's `VMError::report_and_die` goes on with the version
            // lines and an error report file in the working directory (the
            // temporary directory when that cannot be written), whose path it
            // prints. The file holds the header and the locking thread's
            // Java frames.
            let _ = writeln!(out, "# JRE version: CratonVM 25.0");
            let _ = writeln!(
                out,
                "# Java VM: CratonVM ({})",
                crate::runtime::crash_handler::jdk_mode_line()
            );
            let _ = writeln!(out, "#");
            let report = {
                let mut r = String::new();
                r.push_str("#\n# A fatal error has been detected by the CratonVM Runtime Environment:\n#\n");
                r.push_str(&format!(
                    "#  Internal Error (synchronizer.cpp), pid={}, tid={}\n",
                    std::process::id(),
                    thread.thread_id.0
                ));
                r.push_str(&format!(
                    "#  fatal error: Synchronizing on object 0x{addr:016x} of klass {klass} {at}\n#\n"
                ));
                r.push_str("\n---------------  T H R E A D  ---------------\n\n");
                r.push_str(&format!("Current thread: \"{}\"\n\n", thread.name));
                r.push_str("Java frames:\n");
                for line in &lines {
                    r.push_str(line);
                    r.push('\n');
                }
                r
            };
            let file_name = format!("hs_err_pid{}.log", std::process::id());
            let written = [
                std::env::current_dir().ok(),
                Some(std::env::temp_dir()),
            ]
            .into_iter()
            .flatten()
            .map(|dir| dir.join(&file_name))
            .find(|path| std::fs::write(path, report.as_bytes()).is_ok());
            match written {
                Some(path) => {
                    let _ = writeln!(
                        out,
                        "# An error report file with more information is saved as:"
                    );
                    let _ = writeln!(out, "# {}", path.display());
                }
                None => {
                    let _ = writeln!(out, "# Can not save log file, dump to screen..");
                    let _ = out.write_all(report.as_bytes());
                }
            }
            let _ = writeln!(out, "#");
            let _ = out.flush();
        }
        let _ = std::io::stderr().flush();
        // Interpreter round i5 wave 104 hand-back, lane L6: HotSpot's
        // `fatal()` reports once and then `os::abort`s with no second report;
        // a plain `abort()` here ran the VM's own SIGABRT crash handler, which
        // wrote a second `# A fatal error ...` report on stderr (Linux
        // `L6W103DiagnoseSyncArms` arm 1). Unix: the handler is taken off
        // SIGABRT first (exit status 134 kept); Windows has no handler on it.
        #[cfg(unix)]
        crate::abort_without_crash_report();
        #[cfg(not(unix))]
        std::process::abort();
    }

    // Mode 2.
    let mut text = Vec::with_capacity(lines.len() + 2);
    text.push(format!(
        "Synchronizing on object 0x{addr:016x} of klass {klass}"
    ));
    if lines.is_empty() {
        text.push("Cannot find the last Java frame".to_string());
    }
    // Interpreter round i5 wave 104, lane L6 (`L6W104DiagnoseSyncLockLines`):
    // HotSpot's `print_stack_on` prints every frame's lock lines
    // (`print_lock_info_on`, `JavaMonitorsInStackTrace`), not only the top
    // frame's: the monitors each frame already holds, newest first, after
    // the top frame's value-based object. Read off the interpreted frames
    // when the trace is theirs one to one (no compiled or native frame
    // interleaved); otherwise the top frame's line alone, as before.
    let held_lines: Vec<(usize, String)> = if trace.len() == thread.frames.len() {
        let top = thread.frames.len().saturating_sub(1);
        crate::runtime::interpreter::held_monitors::frame_locked_monitors(&thread.frames)
            .into_iter()
            .filter_map(|(index, held)| {
                let monitor = thread_dump_monitor(shared, held)?;
                Some((top.saturating_sub(index), monitor.line("locked")))
            })
            .collect()
    } else {
        Vec::new()
    };
    for (i, line) in lines.iter().enumerate() {
        text.push(line.clone());
        if i == 0 && !top_is_native {
            text.push(format!("\t- locked <0x{addr:016x}> (a {klass})"));
        }
        for (_, held) in held_lines.iter().filter(|(at, _)| *at == i) {
            text.push(format!("\t- {held}"));
        }
    }
    let _ = shared.natives.fd_table.flush(1);
    let tags = [LogTag::ValueBasedClasses];
    if is_unified_logging_enabled(&tags, LogLevel::Info) {
        for line in &text {
            log_unified(&tags, LogLevel::Info, line);
        }
    } else {
        let uptime = shared
            .debug
            .diagnostic_counters
            .start_instant()
            .elapsed()
            .as_secs_f64();
        let mut out = std::io::stdout().lock();
        for line in &text {
            let _ = writeln!(out, "[{uptime:.3}s][info][valuebasedclasses] {line}");
        }
        let _ = out.flush();
    }

    if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        cratonvm_native_builtins_jfr::jfr_vm_events::sync_on_value_based_class(&mut ctx, class_id);
    }

    let fixed = thread
        .native_pin_roots
        .get(pin_base)
        .copied()
        .unwrap_or(obj);
    thread.native_pin_roots.truncate(pin_base);
    fixed
}

/// How [`monitor_enter_or_unmount`] ended (interpreter round i4 wave 87,
/// lane L6, `CRATONVM_VTHREAD_MONITOR_UNMOUNT`).
pub(crate) enum VthreadMonitorEntry {
    /// The monitor is held; the (possibly moved) object.
    Entered(ObjectRef),
    /// Contended: the virtual thread is queued on the monitor and its
    /// `VirtualThread.state` is BLOCKING. The caller must leave its frame at
    /// the `monitorenter` (object back on the operand stack) and yield the
    /// continuation. Nothing collected meanwhile, so the object it popped is
    /// still current.
    Unmount,
}

/// JDK 25 `java.lang.VirtualThread` states the unmount reads and writes
/// (`private static final int RUNNING = 2`, `BLOCKING = 12`; JDK 25 source).
pub(in crate::vm::vm_exec) const VTHREAD_STATE_RUNNING: i32 = 2;
pub(in crate::vm::vm_exec) const VTHREAD_STATE_BLOCKING: i32 = 12;

/// May a contended interpreted `monitorenter` of `thread` UNMOUNT it instead
/// of blocking its carrier (`CRATONVM_VTHREAD_MONITOR_UNMOUNT`, interpreter
/// round i4 wave 87, lane L6)? Only with the per-VM switch on, for a MOUNTED
/// JDK-driven continuation with nothing on its stack a yield would lose:
/// no compiled frame (this OS thread's JIT entry chain is empty -- the
/// runner's own base is Rust, so any entry is the continuation's), no native
/// (`continuation_has_native_frame`), no class initializer, and no pin at all
/// (`Continuation.pin`, or a monitor a native entered through JNI, which
/// HotSpot also pins on). Everything else keeps the blocking entry with the
/// wave-84 compensation. Off: one field load.
#[inline]
pub(crate) fn vthread_monitor_unmount_possible(shared: &SharedVm, thread: &JvmThread) -> bool {
    // Wave 102 (lane L6): not under `-XX:DiagnoseSyncOnValueBasedClasses`,
    // whose check may move the object while the unmount's caller pushes back
    // the reference it popped before it (and would log twice on the
    // re-executed `monitorenter`).
    shared.threads.monitors.vthread_monitor_unmount()
        && shared.config.diagnose_sync_on_value_based_classes == 0
        && vthread_monitor_unmount_allowed(thread)
}

#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn vthread_monitor_unmount_allowed(thread: &JvmThread) -> bool {
    thread.jdk_continuation.jdk()
        && thread.jdk_continuation.carrier() != 0
        && thread.java_thread_obj.is_some()
        && thread.pin_count == 0
        && crate::jit::conservative_roots::current_thread_jit_depth() == 0
        && !crate::runtime::native_callee_memo::continuation_has_native_frame()
        && !super::vm_util::in_clinit_shared()
}

/// The `VirtualThread.state` slot of `vthread` and its value.
pub(in crate::vm::vm_exec) fn virtual_thread_state(shared: &SharedVm, vthread: ObjectRef) -> Option<(usize, i32)> {
    let class_id = shared.mem.heap.class_id_of(vthread);
    let slot = {
        let cm = shared.classes.class_manager.read();
        resolve_field_index_in_hierarchy(class_id, "state", &cm.class_store)
    }?;
    match shared.mem.heap.get_field(vthread, slot) {
        Value::Int(s) => Some((slot, s)),
        _ => None,
    }
}

/// JDK 25 `java.lang.VirtualThread` states of `Object.wait` (interpreter round
/// i4 wave 89, lane L6; `BLOCKED = 13`, `WAITING = 15`, `WAIT = 16`,
/// `TIMED_WAITING = 17`, `TIMED_WAIT = 18`; JDK 25 source).
pub(in crate::vm::vm_exec) const VTHREAD_STATE_BLOCKED: i32 = 13;
pub(in crate::vm::vm_exec) const VTHREAD_STATE_WAITING: i32 = 15;
pub(in crate::vm::vm_exec) const VTHREAD_STATE_WAIT: i32 = 16;
pub(in crate::vm::vm_exec) const VTHREAD_STATE_TIMED_WAITING: i32 = 17;
pub(in crate::vm::vm_exec) const VTHREAD_STATE_TIMED_WAIT: i32 = 18;

/// The slot of `VirtualThread`'s own field `name` on `vthread`'s class.
pub(in crate::vm::vm_exec) fn virtual_thread_field_slot(shared: &SharedVm, vthread: ObjectRef, name: &str) -> Option<usize> {
    let class_id = shared.mem.heap.class_id_of(vthread);
    let cm = shared.classes.class_manager.read();
    resolve_field_index_in_hierarchy(class_id, name, &cm.class_store)
}

/// HotSpot's `ObjectMonitor::INotify` for a virtual-thread waiter: `notified`
/// set, and WAIT / TIMED_WAIT moved to BLOCKED (a waiter still WAITING /
/// TIMED_WAITING, whose `afterYield` has not run, keeps its state: that
/// `afterYield` reads `notified`). A state an expired timeout or an interrupt
/// already changed fails the CAS and stays. Field writes only: nothing
/// allocates (interpreter round i4 waves 89 and 98, lane L6).
pub(in crate::vm::vm_exec) fn mark_virtual_waiter_notified(shared: &SharedVm, vthread: ObjectRef) {
    if let Some(slot) = virtual_thread_field_slot(shared, vthread, "notified") {
        shared.mem.heap.set_field(vthread, slot, Value::Int(1));
    }
    if let Some(slot) = virtual_thread_field_slot(shared, vthread, "state") {
        for from in [VTHREAD_STATE_WAIT, VTHREAD_STATE_TIMED_WAIT] {
            if compare_and_swap_field_shared(
                shared,
                vthread,
                slot,
                Value::Int(from),
                Value::Int(VTHREAD_STATE_BLOCKED),
            ) {
                break;
            }
        }
    }
}

/// Wave 98 (lane L6): mark the unmounted virtual-thread waiters a `notify` /
/// `notifyAll` just chose ([`mark_virtual_waiter_notified`]) while the
/// notifier still owns the monitor, so `Thread.getState()` reads BLOCKED from
/// the notify on, as on HotSpot; it read WAITING until the release handed
/// them to the unblocker.
pub(crate) fn mark_notified_virtual_waiters(shared: &SharedVm, tids: &[ThreadId]) {
    for tid in tids {
        if let Some(vthread) = shared.threads.thread_registry.java_thread_obj(*tid) {
            mark_virtual_waiter_notified(shared, vthread);
        }
    }
}

/// [`vthread_monitor_unmount_allowed`] for `Object.wait`: the yield comes out
/// of the `wait0` native itself, which is one counted native
/// (`continuation_pinned_by_native` allows exactly that one).
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn vthread_wait_unmount_allowed(thread: &JvmThread) -> bool {
    thread.jdk_continuation.jdk()
        && thread.jdk_continuation.carrier() != 0
        && thread.java_thread_obj.is_some()
        && thread.pin_count == 0
        && crate::jit::conservative_roots::current_thread_jit_depth() == 0
        && !crate::runtime::native_callee_memo::continuation_pinned_by_native()
        && !super::vm_util::in_clinit_shared()
}

/// What the remount of an unmounted `Object.wait` does
/// ([`vthread_wait_remount`]; interpreter round i4 wave 89, lane L6).
pub(crate) enum VthreadWaitRemount {
    /// Return from `wait0` (notified, timed out, or no wait was recorded).
    Run,
    /// The re-acquisition is contended: queued as an entrant and BLOCKING;
    /// unmount again.
    Yield,
    /// Woken by an interrupt, not notified: `InterruptedException` (no
    /// message), the interrupt consumed.
    Interrupted,
}

/// The remount of virtual thread `thread` after an unmounted `Object.wait`
/// (`VtYield::MonitorWait`): re-acquire the monitor with its saved count, or
/// queue as an entrant and unmount again (BLOCKING), then answer as HotSpot's
/// `ObjectMonitor::wait` does -- a notified waiter returns normally even with
/// an interrupt pending; otherwise a pending interrupt throws.
pub(crate) fn vthread_wait_remount(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> VthreadWaitRemount {
    use crate::threading::monitor::VthreadWaitResume;
    let tid = thread.thread_id;
    match shared.threads.monitors.vthread_wait_resume(tid) {
        VthreadWaitResume::NotWaiting => VthreadWaitRemount::Run,
        VthreadWaitResume::Acquired { notified } => {
            let _ = shared.threads.thread_registry.take_jmx_waiting_monitor(tid);
            if !notified
                && thread
                    .interrupted
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                VthreadWaitRemount::Interrupted
            } else {
                VthreadWaitRemount::Run
            }
        }
        VthreadWaitResume::Queued => {
            if let Some(vthread) = thread.java_thread_obj {
                if let Some((slot, VTHREAD_STATE_RUNNING)) = virtual_thread_state(shared, vthread) {
                    shared
                        .mem
                        .heap
                        .set_field(vthread, slot, Value::Int(VTHREAD_STATE_BLOCKING));
                }
            }
            thread.vt_yield = crate::threading::jvm_thread::VtYield::MonitorWait;
            VthreadWaitRemount::Yield
        }
    }
}

/// [`monitor_enter_blocking`] that may unmount a virtual thread instead of
/// blocking (see [`VthreadMonitorEntry`]); the interpreter's `monitorenter`
/// calls it when [`vthread_monitor_unmount_possible`] answers yes.
pub(crate) fn monitor_enter_or_unmount(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> VthreadMonitorEntry {
    monitor_enter_blocking_or_unmount(shared, thread, obj, true)
}

pub(in crate::vm::vm_exec) fn monitor_enter_blocking_or_unmount(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    may_unmount: bool,
) -> VthreadMonitorEntry {
    // Interpreter round i5 wave 102, lane L6: `-XX:DiagnoseSyncOnValueBasedClasses`
    // (one per-VM field load when 0, HotSpot's default). Non-zero also leaves
    // compiled code unarmed below, so its every `monitorenter` takes the
    // helper and comes through here (HotSpot's compiled fast paths test the
    // value-based bit themselves).
    let diagnose_value_based = shared.config.diagnose_sync_on_value_based_classes != 0;
    let obj = if diagnose_value_based {
        diagnose_sync_on_value_based_class_cold(shared, thread, obj)
    } else {
        obj
    };
    // Give compiled code on this thread its inline thin-lock path (one
    // compare when already armed). Every `monitorenter` route that is not
    // already inline passes through here on its own thread, so the first
    // interpreted or helper-path acquisition arms it; see
    // `ThreadRegistry::arm_jit_monitor_block`.
    let tid = thread.thread_id;
    if !diagnose_value_based {
        shared.threads.thread_registry.arm_jit_monitor_block(
            &mut thread.jit_monitor_block,
            tid,
            || shared.threads.monitors.jit_thin_lease(tid),
        );
    }
    if thread.jit_monitor_block.census_on() {
        report_monitor_inline_census(shared, thread, obj);
    }
    let Some(m) = shared
        .threads
        .monitors
        .enter_or_contend(obj, thread.thread_id)
    else {
        // The uncontended and re-entrant fast paths already acquired the
        // monitor. JMX must publish them too; restricting ownership to the
        // blocking branch makes getLockedMonitors() silently partial.
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return VthreadMonitorEntry::Entered(obj);
    };
    release_dead_thin_owner(shared, &m, thread.thread_id);
    // gen r4w5/thrash5 (2026-09-24): spin briefly before parking. Parking
    // retires this thread's TLAB (below), so a hot lock handed back and forth
    // between two allocating threads used to bury most of a 256 KiB TLAB under
    // a filler at every hand-off, and the young trigger collected on that
    // filler: `GenR4W4HeapFullThrashProbe` step `threads` never finished.
    // See `Monitor::spin_try_enter`. The spin leaves at once when a pause is
    // requested, and nothing in it can move `obj`: this thread is running and
    // not GC-blocked, so no collection completes until it arrives.
    if crate::threading::monitor::monitor_enter_spin_enabled()
        && m.spin_try_enter(thread.thread_id, || {
            !shared
                .mem
                .gc_barrier
                .stw_requested
                .load(std::sync::atomic::Ordering::Acquire)
        })
    {
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return VthreadMonitorEntry::Entered(obj);
    }
    // Interpreter round i4 wave 87, lane L6 (`CRATONVM_VTHREAD_MONITOR_UNMOUNT`):
    // JEP 491 -- the virtual thread queues on the monitor and unmounts; the
    // release hands it to the JDK's unblocker, and its remount re-executes the
    // `monitorenter`. Only from a RUNNING virtual thread (the state the JDK's
    // `afterYield` expects to find BLOCKING after the yield).
    if may_unmount {
        let tid = thread.thread_id;
        let state = thread
            .java_thread_obj
            .and_then(|vt| virtual_thread_state(shared, vt).map(|s| (vt, s)));
        if let Some((vthread, (slot, VTHREAD_STATE_RUNNING))) = state {
            if shared.threads.monitors.vthread_enqueue_or_enter(&m, tid) {
                shared
                    .threads
                    .thread_registry
                    .complete_jmx_monitor_enter(tid, obj);
                return VthreadMonitorEntry::Entered(obj);
            }
            // Queued. A hand-off that already ran found the state RUNNING
            // and only set `blockPermit`; `afterYield` sees it after
            // BLOCKING -> BLOCKED and resubmits at once.
            shared
                .mem
                .heap
                .set_field(vthread, slot, Value::Int(VTHREAD_STATE_BLOCKING));
            // `ThreadInfo.getLockName()` / `getLockOwnerName()` while it is
            // unmounted; the acquisition on a later remount completes it.
            shared
                .threads
                .thread_registry
                .set_jmx_contended_monitor(tid, obj);
            if crate::runtime::env_cache::frame_trace() {
                eprintln!("[VTHREAD_MONITOR_UNMOUNT] tid={} queued, unmounting", tid.0);
            }
            return VthreadMonitorEntry::Unmount;
        }
    }
    // Interpreter round i4 wave 84, lane L6: a mounted JDK virtual thread is
    // about to hold its carrier for the block; the scheduler compensates
    // (`monitor_entry_compensates`). Before the JMX publish below, so the
    // Java the compensation runs cannot overwrite it.
    if monitor_entry_compensates(shared, thread) {
        let (obj, attempted) = monitor_entry_compensation(shared, thread, obj, &mut [], true);
        let held = monitor_enter_contended(shared, thread, obj, m);
        if !attempted {
            return VthreadMonitorEntry::Entered(held);
        }
        return VthreadMonitorEntry::Entered(
            monitor_entry_compensation(shared, thread, held, &mut [], false).0,
        );
    }
    VthreadMonitorEntry::Entered(monitor_enter_contended(shared, thread, obj, m))
}

/// The contended half of [`monitor_enter_blocking`], once the spin lost:
/// the JMX publish, the JVMTI events, the opt-in lazy park and the GC-blocked
/// park. Answers the acquired (possibly moved) object.
pub(in crate::vm::vm_exec) fn monitor_enter_contended(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    m: std::sync::Arc<crate::threading::monitor::Monitor>,
) -> ObjectRef {
    let tid = thread.thread_id;
    // JMX contention tracking. `enter_or_contend` returning `Some` is exactly
    // the contended case, so this is the point at which the thread becomes
    // BLOCKED_ON_MONITOR_ENTER. Publishing it here is what makes
    // `ThreadInfo.getLockName()` / `getLockOwnerId()` and
    // `findDeadlockedThreads()` work at all: the registry fields were being
    // READ by `thread_jmx_snapshot` but never written by anyone, so every
    // snapshot came back empty and the deadlock detector could not observe a
    // single wait-for edge.
    shared
        .threads
        .thread_registry
        .set_jmx_contended_monitor(tid, obj);
    // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
    // `MonitorContendedEnter`, before the thread blocks (HotSpot posts it
    // after its spin, as here). The callback may collect: the object is
    // where it answers.
    #[cfg(feature = "experimental-debug")]
    let obj = if shared
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::Monitor)
    {
        // gci i11/ja: delivered under this thread's own handle.
        crate::runtime::interpreter::post_on_own_thread(thread, || {
            crate::jvmti::native_env::post_monitor_event(
                shared,
                tid.0,
                crate::jvmti::native_env::MonitorEvent::ContendedEnter,
                obj,
            )
        })
    } else {
        obj
    };
    // Round 11 wave 19 (opt-in, `CRATONVM_MONITOR_LAZY_PARK_US`): park a
    // bounded while as a running mutator before paying the GC-blocked park
    // below. No collection completes meanwhile, so `obj` is still the
    // object's address and needs no pin.
    if lazy_monitor_park(shared, &m, tid) && !lazy_win_given_back(shared, &m, tid) {
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(tid, obj);
        // Wave 63: `MonitorContendedEntered` (the lazy park won it).
        #[cfg(feature = "experimental-debug")]
        if shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::Monitor)
        {
            // gci i11/ja: delivered under this thread's own handle.
            return crate::runtime::interpreter::post_on_own_thread(thread, || {
                crate::jvmti::native_env::post_monitor_event(
                    shared,
                    tid.0,
                    crate::jvmti::native_env::MonitorEvent::ContendedEntered,
                    obj,
                )
            });
        }
        return obj;
    }
    crate::threading::monitor::note_contention(
        crate::threading::monitor::ContentionEvent::BlockedPark,
    );
    let mut ctx = NativeContextImpl { shared, thread };
    ctx.thread
        .gc_block_state
        .java_state
        .store(2, std::sync::atomic::Ordering::Release);
    let pin_base = ctx.thread.native_pin_roots.len();
    ctx.thread.native_pin_roots.push(obj);
    // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE deposit —
    // `deposit_root_snapshot` publishes `in_blocked_region`, the signal a
    // concurrent STW census uses to decide THIS thread needs no further
    // waiting. Once that flag is visible, nothing on this thread may still
    // write to the heap — retiring the TLAB writes a filler object into the
    // young arena, which would otherwise race a collector that (correctly,
    // per the now-true flag) proceeded without waiting for us.
    ctx.thread.tlab.retire();
    // JDWP (interpreter round i1 wave 25, lane L1): what
    // `begin_blocking_region` does for a blocking native — a thread the
    // debugger holds suspended publishes its frames now, and the others open
    // the inspection window once blocked (closed below, still blocked) — so
    // `ThreadReference.Frames` answers for a suspended thread blocked
    // entering a monitor. It listed no frame (`L1W24JdiThreads`, `blocked`).
    #[cfg(feature = "experimental-debug")]
    let debugger_published =
        crate::runtime::interpreter::publish_blocked_frames(shared, ctx.thread);
    // Interpreter round i4 wave 99, lane L1: the quiet leave's key (terms
    // before the deposit, the rest after it).
    let leave_terms = ctx.blocking_leave_terms(2);
    ctx.deposit_root_snapshot();
    ctx.record_blocking_leave_key(leave_terms);
    // Gated diagnostic only (CRATONVM_DBG_MONENTER, default OFF): deposit this
    // thread's frame snapshot so the contended-`enter` poll loop can emit it
    // if a watchdog stack-dump fires while we are blocked acquiring this
    // `synchronized` monitor — the one blocking site otherwise invisible to
    // the watchdog. No effect in normal runs (flag read once + cached).
    let dbg_mon_dump = crate::threading::monitor::mon_enter_dump_enabled();
    if dbg_mon_dump {
        crate::vm::vm_init::set_wait_site_snapshot(ctx.thread);
    }
    {
        let blk = shared.mem.gc_barrier.enter_blocked();
        if blk.pre_stw {
            // A STW was already in progress when we became blocked —
            // arrive at the barrier so its `wait_for_all` completes.
            // GCAUDIT-0711-FIX (finding 1a): `_auto`, not plain
            // `arrive_and_wait` — `deposit_root_snapshot` above already
            // raised `in_blocked_region`, so this pause's own census may
            // have already excluded us; only the exclusion snapshot the
            // census recorded (not this thread's guess) can say which.
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::open_blocked_inspection(
            shared,
            ctx.thread,
            debugger_published,
        );
        let cls = if dbg_mon_dump {
            let cid = shared.mem.heap.class_id_of(obj);
            let cm = shared.classes.class_manager.read();
            cm.get_class(cid).map(|c| c.name.to_string())
        } else {
            None
        };
        // Interpreter round i2 wave 54 (lane L6): a thread the debugger holds
        // suspended gives the monitor it just won back until its resume, as
        // HotSpot's `ExitOnSuspend` does. One load without a suspension.
        block_enter_suspension_aware(shared, tid, &m, cls.as_deref());
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::close_blocked_inspection(shared, ctx.thread);
        drop(blk);
    }
    if dbg_mon_dump {
        crate::vm::vm_init::clear_wait_site_snapshot();
    }
    // Wave 99 (lane L1): the quiet leave.
    let _kept = ctx.check_post_block_gc_quiet_refs(&mut []);
    let fixed = ctx
        .thread
        .native_pin_roots
        .get(pin_base)
        .copied()
        .unwrap_or(obj);
    ctx.thread.native_pin_roots.truncate(pin_base);
    // Acquisition succeeded: the monitor moves from "contended for" to
    // "owned". Report the REMAPPED ref — a GC during the blocked region can
    // move the object, and recording the pre-move address would leave a stale
    // pointer in the owned-monitor root set.
    shared
        .threads
        .thread_registry
        .complete_jmx_monitor_enter(tid, fixed);
    // Wave 63: `MonitorContendedEntered`, the monitor won.
    #[cfg(feature = "experimental-debug")]
    if shared
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::Monitor)
    {
        // gci i11/ja: delivered under this thread's own handle.
        return crate::runtime::interpreter::post_on_own_thread(ctx.thread, || {
            crate::jvmti::native_env::post_monitor_event(
                shared,
                tid.0,
                crate::jvmti::native_env::MonitorEvent::ContendedEntered,
                fixed,
            )
        });
    }
    fixed
}

/// [`monitor_enter_blocking`] for the COMPILED `monitorenter` helper
/// (`jit::helpers::jit_monitor_enter_body`), which may let a contended block
/// answer for the thread's compiled frames with the per-thread coverage proof
/// instead of as a helper window (gcd d2/i, opt-in
/// `CRATONVM_XT_BLOCKED_MONITOR_PROOF`; see
/// `cratonvm_gc::gc_quiescence::blocked_monitor_proof_enabled`).
///
/// With the flag off this IS `monitor_enter_blocking`. With it on, it arms
/// `GcBlockState::monitor_block_proven_jit_depth` for the one flag-raising
/// deposit the contended path takes (`deposit_root_snapshot_inner` settles it
/// to the proven depth or 0 before the thread becomes blocked) and clears it
/// when the call returns -- also on unwind, so an arm can never outlive this
/// call and be settled by a blocking NATIVE's deposit, whose Rust frames were
/// never read for this purpose.
///
/// Not armed while a JNI local frame is open: a JNI native on this stack holds
/// its `jobject`s in C frames nothing rewrites (`common-w2c-jni-local-refs-
/// are-raw-addresses`), so the proof would not describe every live reference.
///
/// Its one caller is the compiled helper, a one-line change in
/// `jit/helpers.rs` that belongs to that file's owner (gcd d2/i cross-lane
/// request, applied by the orchestrator in `jit_monitor_enter_body`).
pub(crate) fn monitor_enter_blocking_from_compiled(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> ObjectRef {
    if !cratonvm_gc::gc_quiescence::blocked_monitor_proof_enabled()
        || crate::native::jni::local_frame_depth() != 0
    {
        return monitor_enter_blocking(shared, thread, obj);
    }
    /// Clears the arm however the call ends.
    struct Disarm(std::sync::Arc<crate::threading::jvm_thread::GcBlockState>);
    impl Drop for Disarm {
        fn drop(&mut self) {
            self.0
                .monitor_block_proven_jit_depth
                .store(0, std::sync::atomic::Ordering::Release);
        }
    }
    thread.gc_block_state.monitor_block_proven_jit_depth.store(
        crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED,
        std::sync::atomic::Ordering::Relaxed,
    );
    let _disarm = Disarm(std::sync::Arc::clone(&thread.gc_block_state));
    monitor_enter_blocking(shared, thread, obj)
}

/// Count a monitor `thread_id` entered outside structured locking (JNI
/// `MonitorEnter`, a VM native's `NativeContext::monitor_enter`), for
/// `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT` (interpreter round i4 wave 95,
/// lane L6; `ThreadRegistry::note_unstructured_monitor_enter`). Nothing with
/// the switch off.
#[inline]
pub(crate) fn note_unstructured_monitor_enter(shared: &SharedVm, thread_id: ThreadId) {
    if shared.config.vthread_death_sweep_by_count {
        shared
            .threads
            .thread_registry
            .note_unstructured_monitor_enter(thread_id);
    }
}

/// The successful exit matching [`note_unstructured_monitor_enter`].
#[inline]
pub(crate) fn note_unstructured_monitor_exit(shared: &SharedVm, thread_id: ThreadId) {
    if shared.config.vthread_death_sweep_by_count {
        shared
            .threads
            .thread_registry
            .note_unstructured_monitor_exit(thread_id);
    }
}

/// Release a monitor AND retract the JMX ownership publish its acquisition
/// made, in the one order that is correct.
///
/// Every `monitorenter` route in the VM ends in
/// [`crate::threading::thread_registry::ThreadRegistry::complete_jmx_monitor_enter`],
/// which appends the object to the thread's `jmx_locked_monitors`. That list is
/// two things at once: what `ThreadMXBean.getLockedMonitors()` reports, and a GC
/// root set. It is also **membership-scanned linearly on every acquisition**, so
/// a publish with no matching retract does not merely misreport — it makes the
/// list grow to "every distinct object this thread has ever locked" and turns
/// `monitorenter` into an O(objects-ever-locked) scan. (Since round 11 wave 5
/// the list is an owner-written lock stack whose first eight entries cost no
/// RMW — `thread_registry::JmxLockStack` — but leaked entries fill those slots
/// and then send every later publish to its mutexed, scanned overflow.)
///
/// That has now happened twice. The first time it was one synchronized-method
/// path and cost 14.9% of a Tomcat webapp deploy (see
/// [`SynchronizedMethodGuard`]). The second time it was the JIT's `monitorexit`
/// helper (`jit::helpers::jit_monitor_exit`), which released the monitor and
/// stopped there: on `ZipContentTests` the two scan functions were **54% of the
/// whole JIT run** and were the bulk of the JIT's +52% CPU against `--nojit`.
/// Both were a missing three-line tail on one of five otherwise identical exit
/// sites, which is the shape of defect that recurs until the idiom is a
/// function. This is that function — release through it, never through a bare
/// `monitors.exit`.
///
/// The "still held?" test is load-bearing and must stay INSIDE: a re-entrant
/// acquisition is still held after this release, and retracting there would
/// under-report a monitor the thread really does own. On a successful exit the
/// release itself answers it (`MonitorTable::exit_reporting_release`, the same
/// answer as a `holds` re-check without the second mark-word load and, for an
/// inflated monitor, the second state-lock round trip); only a FAILED exit asks
/// `holds`.
pub(crate) fn monitor_exit_and_retract_jmx(
    shared: &SharedVm,
    obj: ObjectRef,
    thread_id: ThreadId,
) -> Result<(), crate::error::MethodCallFailed> {
    let (result, no_longer_held) = match shared
        .threads
        .monitors
        .exit_reporting_release(obj, thread_id)
    {
        Ok(released) => (Ok(()), released),
        // Retract even when `exit` failed: an exit that reports "not held"
        // leaves no ownership for the publish to describe, and keeping the
        // entry is the leak.
        Err(e) => (Err(e), !shared.threads.monitors.holds(obj, thread_id)),
    };
    if no_longer_held {
        shared
            .threads
            .thread_registry
            .remove_jmx_locked_monitor(thread_id, obj);
    }
    result
}

/// GC-safe acquire for an `ACC_SYNCHRONIZED` method monitor.
///
/// Synchronized invoke paths pop arguments into Rust locals before pushing the
/// callee frame. If the method monitor is contended, a stop-the-world GC can
/// run while those arguments are no longer visible in any Java frame. Pin and
/// remap them across the contended wait, and mark the thread GC-blocked so the
/// collector does not wait for a mutator parked on a Java monitor.
pub(crate) fn monitor_enter_synchronized_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    args: &mut [Value],
) -> ObjectRef {
    let Some(monitor) = shared
        .threads
        .monitors
        .enter_or_contend(obj, thread.thread_id)
    else {
        // See the uncontended branch in `monitor_enter_blocking`: synchronized
        // methods own their monitor even when no contender forced inflation.
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return obj;
    };
    release_dead_thin_owner(shared, &monitor, thread.thread_id);
    // gen r4w5/thrash5: the same bounded spin as `monitor_enter_blocking`,
    // for the same reason (a park retires the TLAB). Nothing moves during
    // it, so `args` need no pinning yet.
    if crate::threading::monitor::monitor_enter_spin_enabled()
        && monitor.spin_try_enter(thread.thread_id, || {
            !shared
                .mem
                .gc_barrier
                .stw_requested
                .load(std::sync::atomic::Ordering::Acquire)
        })
    {
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return obj;
    }
    // Interpreter round i4 wave 84, lane L6: the carrier compensation of
    // `monitor_enter_blocking`, with the popped arguments pinned across its
    // Java as across the park.
    if monitor_entry_compensates(shared, thread) {
        let (obj, attempted) = monitor_entry_compensation(shared, thread, obj, args, true);
        let held = monitor_enter_synchronized_method_contended(shared, thread, obj, args, monitor);
        if !attempted {
            return held;
        }
        return monitor_entry_compensation(shared, thread, held, args, false).0;
    }
    monitor_enter_synchronized_method_contended(shared, thread, obj, args, monitor)
}

/// The contended half of [`monitor_enter_synchronized_method`], once the spin
/// lost: the JMX publish, the opt-in lazy park and the GC-blocked park with
/// `args` pinned and remapped. Answers the acquired (possibly moved) object.
pub(in crate::vm::vm_exec) fn monitor_enter_synchronized_method_contended(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    args: &mut [Value],
    monitor: std::sync::Arc<crate::threading::monitor::Monitor>,
) -> ObjectRef {
    // Same JMX contention publish as `monitor_enter_blocking` — an
    // `ACC_SYNCHRONIZED` method's monitor contends exactly as a `synchronized`
    // block's does, and a thread blocked here is just as deadlocked. Tracking
    // only one of the two entry points would make deadlock detection depend on
    // which syntax the Java code happened to use.
    shared
        .threads
        .thread_registry
        .set_jmx_contended_monitor(thread.thread_id, obj);
    // The bounded running park of `monitor_enter_blocking` (opt-in). Nothing
    // moves during it, so neither `obj` nor `args` needs a pin yet.
    if lazy_monitor_park(shared, &monitor, thread.thread_id)
        && !lazy_win_given_back(shared, &monitor, thread.thread_id)
    {
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return obj;
    }
    crate::threading::monitor::note_contention(
        crate::threading::monitor::ContentionEvent::BlockedPark,
    );

    let pin_base = thread.native_pin_roots.len();
    let monitor_pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj);

    let mut arg_root_indices = Vec::with_capacity(args.len());
    for arg in args.iter() {
        let before = thread.native_pin_roots.len();
        pin_value_for_native_call(shared, &mut thread.native_pin_roots, arg);
        arg_root_indices.push((thread.native_pin_roots.len() > before).then_some(before));
    }

    let tid = thread.thread_id;
    let mut ctx = NativeContextImpl { shared, thread };
    // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE deposit — see
    // `monitor_enter_blocking`. A parked contender must not retain a TLAB
    // into a young arena that a concurrent STW can grow/reallocate while
    // this thread is blocked, AND must not still be writing to the heap
    // after `in_blocked_region` tells a census it is done doing so.
    ctx.thread.tlab.retire();
    // Interpreter round i2 wave 54 (lane L6): BLOCKED, as
    // `monitor_enter_blocking` stores it. The deposit below writes this state
    // into `holder.threadStatus` (`Thread.getState()`), and JDWP's
    // `ThreadReference.Status` reads it while the thread is blocked. Nothing
    // stored it here, so a thread blocked entering a synchronized METHOD
    // reported whatever its last blocking call had left (RUNNABLE for a fresh
    // thread, TIMED_WAITING after a sleep;
    // `L6W54SynchronizedMethodEntrantState`).
    ctx.thread
        .gc_block_state
        .java_state
        .store(2, std::sync::atomic::Ordering::Release);
    // JDWP (wave 54, lane L6): as `monitor_enter_blocking` does -- a thread
    // the debugger holds suspended publishes its frames now, the others open
    // the inspection window once blocked -- so `ThreadReference.Frames`
    // answers for a suspended thread blocked entering a synchronized method.
    // It listed no frame at all (`L6W54JdiFramesOfASynchronizedMethodEntrant`).
    #[cfg(feature = "experimental-debug")]
    let debugger_published =
        crate::runtime::interpreter::publish_blocked_frames(ctx.shared, ctx.thread);
    // Interpreter round i4 wave 99, lane L1: the quiet leave's key (terms
    // before the deposit, the rest after it).
    let leave_terms = ctx.blocking_leave_terms(2);
    ctx.deposit_root_snapshot();
    ctx.record_blocking_leave_key(leave_terms);
    {
        let blk = ctx.shared.mem.gc_barrier.enter_blocked();
        if blk.pre_stw {
            let _ = ctx.shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::open_blocked_inspection(
            ctx.shared,
            ctx.thread,
            debugger_published,
        );
        // Wave 54 (lane L6): as in `monitor_enter_blocking`.
        block_enter_suspension_aware(ctx.shared, tid, &monitor, None);
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::close_blocked_inspection(ctx.shared, ctx.thread);
        drop(blk);
    }
    // Wave 99 (lane L1): the quiet leave.
    let _kept = ctx.check_post_block_gc_quiet_refs(&mut []);

    let fixed_monitor = ctx
        .thread
        .native_pin_roots
        .get(monitor_pin)
        .copied()
        .unwrap_or(obj);
    remap_args_from_pins(shared, &*ctx.thread, args, arg_root_indices);
    ctx.thread.native_pin_roots.truncate(pin_base);
    // Acquired: contended -> owned, reporting the remapped ref (see the twin
    // in `monitor_enter_blocking`).
    shared
        .threads
        .thread_registry
        .complete_jmx_monitor_enter(tid, fixed_monitor);
    fixed_monitor
}

/// How an interpreter door entered an `ACC_SYNCHRONIZED` callee's monitor
/// (interpreter round i2 wave 55, lane L6, `CRATONVM_SYNC_METHOD_FRAME_FIRST`;
/// `docs/feature-designs/real-jdk-mode.md`).
#[derive(Clone, Copy)]
pub(crate) enum SyncEntry {
    /// Held: the frame's `monitor_on_exit`, exactly as before.
    Held(ObjectRef),
    /// Contended, and not taken yet: the door pushes the callee's frame with
    /// no monitor and then calls [`enter_synchronized_method_in_pushed_frame`],
    /// which blocks with that frame on top, as HotSpot builds a synchronized
    /// method's frame before `lock_method`.
    Deferred(ObjectRef),
}

impl SyncEntry {
    /// `(the monitor to install with the frame, the one to enter after the
    /// push)`.
    #[inline]
    pub(crate) fn split(entry: Option<SyncEntry>) -> (Option<ObjectRef>, Option<ObjectRef>) {
        match entry {
            None => (None, None),
            Some(SyncEntry::Held(obj)) => (Some(obj), None),
            Some(SyncEntry::Deferred(obj)) => (None, Some(obj)),
        }
    }
}

/// [`monitor_enter_synchronized_method`] for a door that pushes the callee's
/// frame next (interpreter round i2 wave 55, lane L6). With
/// `CRATONVM_SYNC_METHOD_FRAME_FIRST` off: that function, `Held`. On: the
/// uncontended acquire as before (`Held`), and a contended one is left to
/// the pushed frame (`Deferred`). A C JVMTI agent listening for `MethodEntry`
/// gets it after the acquisition, as HotSpot posts it after `lock_method`
/// (`JvmThread::deferred_method_entry_depth`; until the i54-L6 retirement
/// that thread kept the old order). One load of the per-VM switch on a
/// synchronized call, nothing on any other.
#[inline]
pub(crate) fn monitor_enter_synchronized_method_or_defer(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    args: &mut [Value],
) -> SyncEntry {
    if shared.threads.monitors.sync_method_frame_first() {
        return monitor_enter_or_defer_frame_first(shared, thread, obj);
    }
    SyncEntry::Held(monitor_enter_synchronized_method(shared, thread, obj, args))
}

/// [`monitor_enter_synchronized_method_or_defer`] with the switch on.
#[inline(never)]
pub(in crate::vm::vm_exec) fn monitor_enter_or_defer_frame_first(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> SyncEntry {
    if shared
        .threads
        .monitors
        .enter_or_contend(obj, thread.thread_id)
        .is_none()
    {
        // Uncontended: acquired, published as the old path publishes it.
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return SyncEntry::Held(obj);
    }
    // The i54-L6 retirement: a C agent listening for `MethodEntry` gets it
    // once the monitor is held (`interpreter::fire_deferred_method_entry`),
    // not at the push, as HotSpot posts it after `lock_method`.
    if crate::runtime::jvmti::any_method_entry_listener_active() {
        thread.deferred_method_entry_depth = thread.frames.len() + 1;
    }
    SyncEntry::Deferred(obj)
}

/// What a COMPILED-body door (`jit_bridge::execute_jit_call` /
/// `execute_jit_call_decoded`) does with an `ACC_SYNCHRONIZED` callee's
/// monitor before it pops anything (interpreter round i2 wave 56, lane L6,
/// stage 2 of `CRATONVM_SYNC_METHOD_FRAME_FIRST`). A compiled body has no
/// frame to block in, so a contended entry declines the door and the
/// interpreter's general path pushes the method's frame first and blocks
/// with it on top ([`SyncEntry::Deferred`]), as HotSpot's `lock_method`
/// does.
#[derive(Clone, Copy)]
pub(crate) enum DoorSyncTry {
    /// The switch is off: the door acquires as before.
    Unchanged,
    /// Acquired uncontended (the JMX publish made): the door adopts it.
    Held(ObjectRef),
    /// Contended and not taken: the door declines before popping.
    Decline,
}

/// [`DoorSyncTry`] for `obj`, the callee's monitor object. One load of the
/// per-VM switch; the rest is out of line.
#[inline]
pub(crate) fn frame_first_door_try(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> DoorSyncTry {
    if !shared.threads.monitors.sync_method_frame_first() {
        return DoorSyncTry::Unchanged;
    }
    frame_first_door_try_on(shared, thread, obj)
}

#[inline(never)]
pub(in crate::vm::vm_exec) fn frame_first_door_try_on(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> DoorSyncTry {
    if shared
        .threads
        .monitors
        .enter_or_contend(obj, thread.thread_id)
        .is_none()
    {
        shared
            .threads
            .thread_registry
            .complete_jmx_monitor_enter(thread.thread_id, obj);
        return DoorSyncTry::Held(obj);
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SYNC_FRAME_FIRST] tid={} compiled door declined",
            thread.thread_id.0
        );
    }
    DoorSyncTry::Decline
}

/// The contended entry of a synchronized method whose frame the door has
/// just pushed with no monitor ([`SyncEntry::Deferred`]; interpreter round i2
/// wave 55, lane L6): [`monitor_enter_synchronized_method`]'s whole protocol
/// -- spin, lazy park, the GC-blocked park with its deposit, the debugger's
/// window and give-back -- with the callee's frame on top, so the frames the
/// blocked thread deposits (`Thread.getStackTrace()`, a thread dump, JMX)
/// and the debugger's listing show the method at bytecode index 0 with its
/// arguments as locals. The arguments are the frame's locals now, scanned
/// and remapped as every frame's are: only the monitor object is pinned.
/// The acquired (possibly moved) object becomes the frame's
/// `monitor_on_exit`, as the door would have installed it.
/// `CRATONVM_FRAME_TRACE=1` prints `[SYNC_FRAME_FIRST] tid=<n> <method>`.
#[cold]
#[inline(never)]
pub(crate) fn enter_synchronized_method_in_pushed_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) {
    enter_synchronized_method_in_pushed_frame_with(shared, thread, obj, true);
}

/// [`enter_synchronized_method_in_pushed_frame`] for the ENTRY frame of a
/// nested dispatch loop (`interpreter::execute`'s deferred entry): its
/// deferred `MethodEntry` is posted at the entry frame's own callback point
/// since interpreter round i7 wave 135, lane L1
/// (`interpreter::fire_deferred_method_entry_as_loop_entry`,
/// `CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL`, default ON; `=0` posts it
/// outside any callback point, as before; shape 2 of
/// `docs/feature-designs/jvmti-delivery-threading.md`).
#[cold]
#[inline(never)]
pub(crate) fn enter_synchronized_method_in_pushed_entry_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) {
    enter_synchronized_method_in_pushed_frame_with(shared, thread, obj, false);
}

/// The body of the two entries above; `in_loop` is true for a frame an
/// interpreter invoke pushed for the loop running its caller (interpreter
/// round i6 wave 134, lane L1: its deferred `MethodEntry` runs at a callback
/// point, `interpreter::fire_deferred_method_entry_in_loop`).
pub(in crate::vm::vm_exec) fn enter_synchronized_method_in_pushed_frame_with(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    in_loop: bool,
) {
    let held = monitor_enter_synchronized_method(shared, thread, obj, &mut []);
    if let Some(top) = thread.frames.last_mut() {
        debug_assert!(
            top.monitor_on_exit.is_none(),
            "a deferred entry's frame owns a monitor"
        );
        top.monitor_on_exit = Some(held);
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[SYNC_FRAME_FIRST] tid={} {}.{}{}",
                thread.thread_id.0,
                top.class_name(),
                top.method_name(),
                top.method_descriptor()
            );
        }
    }
    if thread.deferred_method_entry_depth != 0 {
        // Interpreter round i6 wave 134, lane L1 (i133-L1): a `PopFrame` or
        // `ForceEarlyReturn*` a C agent made while the thread was suspended
        // blocked on this entry was adopted at the wake, for this frame. On
        // HotSpot the pending pop or early return is taken in
        // `lock_method`'s call epilogue, before `notify_method_entry`: no
        // `MethodEntry` for the frame (by reading the template interpreter).
        #[cfg(feature = "experimental-debug")]
        if shared.config.jvmti_blocked_entry_frame_control
            && crate::debug::early_return::jvmti_frame_control_at(
                shared,
                thread.thread_id.0,
                thread.frames.len(),
            )
        {
            thread.deferred_method_entry_depth = 0;
            return;
        }
        if in_loop {
            crate::runtime::interpreter::fire_deferred_method_entry_in_loop(shared, thread);
        } else {
            // Interpreter round i7 wave 135, lane L1 (i133-L8 shape 2,
            // `CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL`): the nested loop's
            // entry frame posts at its own callback point; with the switch
            // off this is `fire_deferred_method_entry` exactly.
            crate::runtime::interpreter::fire_deferred_method_entry_as_loop_entry(shared, thread);
        }
    }
}

/// The implicit monitor of an `ACC_SYNCHRONIZED` method (JVMS §2.11.10) while
/// Rust code, not an interpreter frame's `monitor_on_exit`, owns its release.
///
/// The ONE guard for that job. Its users: `interpreter::execute` (until it
/// hands the monitor to the frame it pushes), `invoke_on_class_shared_inner`'s
/// native arms, the JIT entry (`jit_bridge::JitSynchronizedMonitorGuard` wraps
/// it) and the opt-in synchronized-native call in `interpreter::invoke`. There
/// used to be three copies with their own pin handling and release paths; see
/// `docs/feature-designs/real-jdk-mode.md`.
///
/// * RAII, so the release runs on a normal return, an early `return Err`, and a
///   panic unwind (B9: the old manual `monitors.exit` after the call leaked the
///   monitor when the call panicked).
/// * The monitor object is pinned in `thread.native_pin_roots` at `pin` for the
///   guard's lifetime, and the release reads the pin, never the entry-time
///   address: a guard outlives the whole method body, arbitrarily many moving
///   GCs, and releasing the stale address raised "does not own the monitor"
///   while the real monitor stayed locked forever (the WildFly domain-server
///   boot stall, 2026-07-16). The pin must be the top of the pin stack when the
///   guard releases or transfers (it truncates to `pin`): take the guard after
///   any argument pins that outlive it, and drop it before them.
/// * The release goes through [`monitor_exit_and_retract_jmx`], which retracts
///   the JMX ownership publish on the outermost exit (the missing retract cost
///   14.9% of a Tomcat webapp deploy).
pub(crate) struct SynchronizedMethodGuard {
    pub(in crate::vm::vm_exec) shared: *const SharedVm,
    /// Same lifetime-erasure contract as `ChmMonitorGuard`: the guard is a
    /// stack local of a function that exclusively borrows the thread; it never
    /// escapes, and it drops (normal path or unwind) inside that borrow.
    pub(in crate::vm::vm_exec) thread: *mut JvmThread,
    /// Index of the monitor object in `thread.native_pin_roots`.
    pub(in crate::vm::vm_exec) pin: usize,
    /// Address at entry: only the fallback when the pin was truncated away
    /// underneath the guard, which would be a pin-discipline bug elsewhere.
    pub(in crate::vm::vm_exec) entry: ObjectRef,
    pub(in crate::vm::vm_exec) armed: bool,
}

impl SynchronizedMethodGuard {
    /// Enter `monitor` — blocking on contention, with `args` pinned and
    /// remapped in place across the wait — and pin it for the guard's life.
    pub(crate) fn enter(
        shared: &SharedVm,
        thread: &mut JvmThread,
        monitor: ObjectRef,
        args: &mut [Value],
    ) -> Self {
        let monitor = monitor_enter_synchronized_method(shared, thread, monitor, args);
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(monitor);
        Self {
            shared: shared as *const SharedVm,
            thread: thread as *mut JvmThread,
            pin,
            entry: monitor,
            armed: true,
        }
    }

    /// Take ownership of a monitor the caller ALREADY holds (a door's
    /// uncontended acquire), pinned for the guard's life and released like one
    /// [`Self::enter`] took. Nothing is acquired and nothing can collect here,
    /// so `monitor` is current. Round 11 wave 6 (the JIT's synchronized door).
    pub(crate) fn adopt(shared: &SharedVm, thread: &mut JvmThread, monitor: ObjectRef) -> Self {
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(monitor);
        Self {
            shared: shared as *const SharedVm,
            thread: thread as *mut JvmThread,
            pin,
            entry: monitor,
            armed: true,
        }
    }

    /// The (possibly moved) monitor object, for an interpreter frame's
    /// `monitor_on_exit`: the frame owns the release from here on, and the
    /// guard's `Drop` does nothing. `None` once transferred or released.
    pub(crate) fn transfer_to_frame(&mut self) -> Option<ObjectRef> {
        if !self.armed {
            return None;
        }
        self.armed = false;
        // SAFETY: see the `thread` field's contract.
        let thread = unsafe { &mut *self.thread };
        let obj = thread
            .native_pin_roots
            .get(self.pin)
            .copied()
            .unwrap_or(self.entry);
        thread.native_pin_roots.truncate(self.pin);
        Some(obj)
    }
}

impl Drop for SynchronizedMethodGuard {
    fn drop(&mut self) {
        let Some(obj) = self.transfer_to_frame() else {
            return;
        };
        // SAFETY: `shared` outlives this stack-local guard; `thread` as in
        // `transfer_to_frame`.
        let (shared, thread_id) = unsafe { (&*self.shared, (*self.thread).thread_id) };
        if let Err(e) = monitor_exit_and_retract_jmx(shared, obj, thread_id) {
            tracing::warn!(
                ?thread_id,
                error = ?e,
                "implicit monitorexit on synchronized-method exit failed"
            );
        }
    }
}

/// One entry of [`FIELD_NAME_RING`]. The name is stored inline, so a hit
/// compares CONTENT, never an address: a caller that builds its field name at
/// run time can reuse a buffer address for a different name, and an
/// address-keyed entry would then return the wrong field's slot.
#[derive(Clone, Copy)]
pub(in crate::vm::vm_exec) struct FieldNameSlot {
    /// `SharedVm::vm_identity`; vacant entries carry 0, never a real identity.
    pub(in crate::vm::vm_exec) vm_key: usize,
    pub(in crate::vm::vm_exec) class_id: u32,
    pub(in crate::vm::vm_exec) name_len: u8,
    pub(in crate::vm::vm_exec) name: [u8; FIELD_NAME_INLINE],
    pub(in crate::vm::vm_exec) slot: usize,
    /// [`cratonvm_classloading::class_origin_epoch`] the slot was resolved
    /// under. Honoured only while it is unchanged: a synthetic stub's promotion
    /// or a class (re)definition moves it, and a class-id reuse after an unload
    /// needs a definition first. [`FIELD_NAME_DURABLE`] (only with
    /// `CRATONVM_FIELD_NAME_RING_DURABLE`) never expires.
    pub(in crate::vm::vm_exec) epoch: u64,
}

/// Longest field name held inline by [`FIELD_NAME_RING`]; longer names take the
/// uncached path.
pub(in crate::vm::vm_exec) const FIELD_NAME_INLINE: usize = 31;

pub(in crate::vm::vm_exec) const FIELD_NAME_SLOT_VACANT: FieldNameSlot = FieldNameSlot {
    vm_key: 0,
    class_id: 0,
    name_len: 0,
    name: [0; FIELD_NAME_INLINE],
    slot: 0,
    epoch: 0,
};

pub(in crate::vm::vm_exec) const FIELD_NAME_RING_LEN: usize = 256;

thread_local! {
    /// Per-mutator, direct-mapped `(class id, field name) -> slot` memo for the
    /// by-name field accessors.
    ///
    /// Every `get_field_by_name` / `set_field_by_name` used to take the class
    /// manager's read lock and walk the hierarchy comparing the name against
    /// each declared field. A native that touches two dozen fields of one
    /// `MessageBytes` / `MappingData` per call paid that two dozen times: the
    /// walk (`memcmp`, `ClassFileField::is_static`) plus the lock was 12-15% of
    /// a Tomcat `Mapper.map` memo hit.
    ///
    /// Only POSITIVE results are memoized (an unresolved name may resolve once
    /// the class finishes loading), and an eviction only costs a re-walk of
    /// immutable class metadata.
    pub(in crate::vm::vm_exec) static FIELD_NAME_RING: std::cell::RefCell<[FieldNameSlot; FIELD_NAME_RING_LEN]> =
        const { std::cell::RefCell::new([FIELD_NAME_SLOT_VACANT; FIELD_NAME_RING_LEN]) };
}

#[inline]
pub(in crate::vm::vm_exec) fn field_name_bucket(class_id: u32, name: &[u8]) -> usize {
    let len = name.len();
    let mid = name[len / 2] as usize;
    ((class_id as usize).wrapping_mul(0x9E37_79B1))
        .wrapping_add(len.wrapping_mul(131))
        .wrapping_add(name[0] as usize * 7)
        .wrapping_add(name[len - 1] as usize * 31)
        .wrapping_add(mid * 17)
        .wrapping_add((class_id as usize) >> 5)
        % FIELD_NAME_RING_LEN
}

/// One entry of [`STATIC_NAME_RING`]: a `(class, name)` pair keyed on the name's
/// ADDRESS and length. Only ever filled from a `&'static str`, whose address
/// cannot be reused for different content.
#[derive(Clone, Copy)]
pub(in crate::vm::vm_exec) struct StaticNameSlot {
    pub(in crate::vm::vm_exec) vm_key: usize,
    pub(in crate::vm::vm_exec) name_ptr: usize,
    pub(in crate::vm::vm_exec) slot: usize,
    pub(in crate::vm::vm_exec) epoch: u64,
    pub(in crate::vm::vm_exec) class_id: u32,
    pub(in crate::vm::vm_exec) name_len: u32,
}

pub(in crate::vm::vm_exec) const STATIC_NAME_SLOT_VACANT: StaticNameSlot = StaticNameSlot {
    vm_key: 0,
    name_ptr: 0,
    slot: 0,
    epoch: 0,
    class_id: 0,
    name_len: 0,
};

pub(in crate::vm::vm_exec) const STATIC_NAME_RING_LEN: usize = 512;

/// Most names one batch resolves per pass; larger batches run in chunks.
pub(in crate::vm::vm_exec) const FIELD_BATCH_CHUNK: usize = 16;

thread_local! {
    /// The batch accessors' `(class id, &'static str) -> slot` memo. Same
    /// contract as [`FIELD_NAME_RING`] (positive results only, stamped with the
    /// class-provenance epoch, eviction costs a re-walk) but a hit compares
    /// integers instead of characters, and a whole batch looks up under ONE
    /// borrow of the table.
    pub(in crate::vm::vm_exec) static STATIC_NAME_RING: std::cell::RefCell<[StaticNameSlot; STATIC_NAME_RING_LEN]> =
        const { std::cell::RefCell::new([STATIC_NAME_SLOT_VACANT; STATIC_NAME_RING_LEN]) };
}

#[inline]
pub(in crate::vm::vm_exec) fn static_name_bucket(class_id: u32, name_ptr: usize) -> usize {
    ((class_id as usize).wrapping_mul(0x9E37_79B1) ^ (name_ptr >> 3).wrapping_mul(0x85EB_CA6B))
        % STATIC_NAME_RING_LEN
}

/// Resolve each of `names` (at most [`FIELD_BATCH_CHUNK`]) against `class_id`,
/// writing the slot into `slots[i]` or `usize::MAX` when the name does not
/// resolve. Same answers as one uncached hierarchy walk per name.
pub(in crate::vm::vm_exec) fn resolve_static_field_slots(
    shared: &SharedVm,
    class_id: ClassId,
    names: &[&'static str],
    slots: &mut [usize; FIELD_BATCH_CHUNK],
) {
    debug_assert!(names.len() <= FIELD_BATCH_CHUNK);
    let cid = class_id.as_u32();
    let vm_key = shared.vm_identity;
    let epoch = class_id_memo_epoch(shared);
    let mut missing = 0u32;
    STATIC_NAME_RING.with(|cell| {
        let ring = cell.borrow();
        for (i, name) in names.iter().enumerate() {
            let ptr = name.as_ptr() as usize;
            let e = &ring[static_name_bucket(cid, ptr)];
            if e.vm_key == vm_key
                && e.class_id == cid
                && e.name_ptr == ptr
                && e.name_len as usize == name.len()
                && (e.epoch == epoch || e.epoch == FIELD_NAME_DURABLE)
            {
                slots[i] = e.slot;
            } else {
                slots[i] = usize::MAX;
                missing |= 1 << i;
            }
        }
    });
    if missing == 0 {
        return;
    }
    let cm = shared.classes.class_manager.read();
    // `CRATONVM_FIELD_NAME_RING_DURABLE` (see [`FIELD_NAME_DURABLE`]): asked
    // at most once per batch, and only when a name resolved.
    let mut stamp: Option<u64> = None;
    for (i, name) in names.iter().enumerate() {
        if missing & (1 << i) == 0 {
            continue;
        }
        let Some(slot) = resolve_field_index_in_hierarchy(class_id, name, &cm.class_store) else {
            continue;
        };
        slots[i] = slot;
        if cid != 0 {
            let ptr = name.as_ptr() as usize;
            let epoch = *stamp.get_or_insert_with(|| {
                field_name_ring_stamp(shared, class_id, &cm.class_store, epoch)
            });
            STATIC_NAME_RING.with(|cell| {
                cell.borrow_mut()[static_name_bucket(cid, ptr)] = StaticNameSlot {
                    vm_key,
                    name_ptr: ptr,
                    slot,
                    epoch,
                    class_id: cid,
                    name_len: name.len() as u32,
                };
            });
        }
    }
}

/// `resolve_field_index_in_hierarchy` behind [`FIELD_NAME_RING`], for the
/// by-name accessors. Same answer as the uncached walk under the class manager
/// lock; only the cost differs.
///
/// `pub(crate)` since 2026-09-21: `jit::helpers::jit_buffer_session_direct`
/// needs the identical cached resolution `get_field_by_name` uses, so that its
/// fast path can never disagree with a real `getfield` on the same class+name.
pub(crate) fn resolve_field_slot_by_name_cached(
    shared: &SharedVm,
    class_id: ClassId,
    name: &str,
) -> Option<usize> {
    let bytes = name.as_bytes();
    let cid = class_id.as_u32();
    let cacheable = cid != 0 && !bytes.is_empty() && bytes.len() <= FIELD_NAME_INLINE;
    let vm_key = shared.vm_identity;
    let epoch = class_id_memo_epoch(shared);
    if cacheable {
        let bucket = field_name_bucket(cid, bytes);
        let hit = FIELD_NAME_RING.with(|cell| {
            let ring = cell.borrow();
            let e = &ring[bucket];
            (e.vm_key == vm_key
                && e.class_id == cid
                && (e.epoch == epoch || e.epoch == FIELD_NAME_DURABLE)
                && e.name_len as usize == bytes.len()
                && e.name[..bytes.len()] == *bytes)
                .then_some(e.slot)
        });
        if hit.is_some() {
            return hit;
        }
    }
    let (slot, epoch) = {
        let cm = shared.classes.class_manager.read();
        let slot = resolve_field_index_in_hierarchy(class_id, name, &cm.class_store);
        let stamp = if cacheable && slot.is_some() {
            field_name_ring_stamp(shared, class_id, &cm.class_store, epoch)
        } else {
            epoch
        };
        (slot, stamp)
    };
    if let (true, Some(slot)) = (cacheable, slot) {
        let mut inline = [0u8; FIELD_NAME_INLINE];
        inline[..bytes.len()].copy_from_slice(bytes);
        let entry = FieldNameSlot {
            vm_key,
            class_id: cid,
            name_len: bytes.len() as u8,
            name: inline,
            slot,
            epoch,
        };
        FIELD_NAME_RING.with(|cell| {
            cell.borrow_mut()[field_name_bucket(cid, bytes)] = entry;
        });
    }
    slot
}

/// Stamp of a by-name ring entry ([`FIELD_NAME_RING`], [`STATIC_NAME_RING`])
/// whose answer never expires: `class_origin_epoch` starts at 0 and counts up,
/// so it can never return this.
///
/// Interpreter round i4 wave 91, lane L3 (`CRATONVM_FIELD_NAME_RING_DURABLE`,
/// `VmConfig::field_name_ring_durable`, default OFF;
/// `i88-L3-proposal-first-use-reflective-constructor-cost`). Every class
/// definition moves the provenance epoch (`ClassStore::add`, `set_origin`), so
/// with the epoch as the only stamp one define retired every thread's
/// positive by-name answers: a reflective mint right after a define (a fresh
/// class's `getConstructor`, a framework's proxy then its first
/// `getDeclaredField`) re-walked the hierarchy under the class-manager lock
/// for every field it wrote -- the `Constructor` mirror's own fields included,
/// whose answers no define can change. The field descriptor memo had the same
/// problem and keeps definitive answers durable
/// ([`FIELD_DESCRIPTOR_DURABLE`]); this is the same rule for the name memos.
pub(in crate::vm::vm_exec) const FIELD_NAME_DURABLE: u64 = u64::MAX;

/// The provenance stamp of a per-thread memo keyed by class id (and the VM
/// identity) whose answer reads only classes stored in this VM's class store,
/// by id: their fields, `first_field_index`, superclass chain and provenance
/// -- never a name -> class answer. [`FIELD_NAME_RING`], [`STATIC_NAME_RING`],
/// the transient entries of [`FIELD_DESCRIPTOR_LAST`] / [`FIELD_DESCRIPTOR_RING`]
/// and [`SOFT_TOUCH_MEMO`] (with a layout). Not for a memo whose answer reads
/// a name -> class answer: the JIT's buffer-session proof
/// (`jit::helpers::buffer_stored_session`; `panama::jit_carrier_stored_session`
/// compares the segment's class with `class_id_by_name`) and the overlay
/// hunter's [`SHADOW_LAYOUT_CACHE`] (it memoises `None` for an id not stored
/// yet) keep `class_origin_epoch`.
///
/// `class_origin_epoch` by default, which moves on every class DEFINITION in
/// the process as well as on every in-place provenance write, so one define
/// anywhere retires every such entry on every thread. Under
/// `CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH` (`VmConfig::class_memo_narrow_epoch`;
/// interpreter round i4 wave 95, lane L7,
/// `i94-L7-proposal-memos-stamped-with-the-definition-or-origin-epoch-retire-on-every-define`)
/// it is this store's hierarchy-edge epoch plus `class_origin_in_place_epoch`.
/// Both only count up, so the sum is unchanged exactly when both are, and it
/// never reaches the durable sentinels ([`FIELD_NAME_DURABLE`],
/// [`FIELD_DESCRIPTOR_DURABLE`]). Why the pair is enough for these answers:
///
/// * an in-place stub upgrade rewrites a stored class's fields, layout and
///   provenance (`ClassManager::upgrade_synthetic_class`, which also
///   recomputes the descendants' `first_field_index`): its `set_origin` moves
///   `class_origin_in_place_epoch`, under the class-manager write lock the
///   memos' slow paths read under;
/// * a superclass or interface edge of a stored class changes, a forward
///   edge comes into existence (`ClassStore::add` of an id a stored class
///   already names: the "ancestor not loaded yet" transient answers), a
///   class is removed (its id's entries): each moves the edge epoch;
/// * a brand-new class has a fresh id with no entry (positive and durable
///   answers need the class stored), and a JVMTI redefinition keeps the
///   field layout.
///
/// The edge epoch is per store slot and the in-place epoch moves every slot,
/// so another VM's definitions retire nothing here either.
#[inline]
pub(crate) fn class_id_memo_epoch(shared: &SharedVm) -> u64 {
    if shared.config.class_memo_narrow_epoch {
        let epochs = cratonvm_classloading::store_epochs(shared.jit.class_layout_domain);
        epochs
            .hierarchy_edge_epoch()
            .wrapping_add(epochs.class_origin_in_place_epoch())
    } else {
        cratonvm_classloading::class_origin_epoch()
    }
}

/// The stamp for a positive by-name answer of `class_id` resolved under
/// `epoch` while `store` was read: [`FIELD_NAME_DURABLE`] when the switch is
/// on and the answer cannot change, else `epoch`.
///
/// It cannot change when the class and every superclass are installed and
/// none is a compatibility stub: the in-place stub upgrade
/// (`ClassManager::upgrade_synthetic_class`, and the descendant recompute it
/// drives) is the only writer of an installed class's fields and
/// `first_field_index`; a redefinition keeps the layout; a `ClassId` is never
/// reused after an unload (`ClassStore::remove`), and the entry is keyed by
/// the VM's identity.
pub(in crate::vm::vm_exec) fn field_name_ring_stamp(
    shared: &SharedVm,
    class_id: ClassId,
    store: &ClassStore,
    epoch: u64,
) -> u64 {
    if !shared.config.field_name_ring_durable {
        return epoch;
    }
    let mut current = Some(class_id);
    // A chain longer than this is not a real hierarchy; stay perishable.
    let mut budget = 4096u32;
    while let Some(cid) = current {
        let Some(class) = store.get(cid) else {
            return epoch;
        };
        if class.origin.is_compatibility_stub() || budget == 0 {
            return epoch;
        }
        budget -= 1;
        current = class.superclass;
    }
    FIELD_NAME_DURABLE
}

pub(crate) fn resolve_field_index_in_hierarchy(
    class_id: ClassId,
    field_name: &str,
    store: &ClassStore,
) -> Option<usize> {
    resolve_field_index_in_hierarchy_desc(class_id, field_name, None, store)
}

/// Descriptor-aware variant of [`resolve_field_index_in_hierarchy`].
///
/// Resolves an instance field by name, optionally disambiguated by its JVM
/// type `descriptor` (`"I"`, `"Ljava/lang/String;"`, `"[J"`, …). When
/// `descriptor` is `Some`, only a field whose declared descriptor matches is
/// considered — this is what lets a caller address a **shadowed** super-class
/// field that a subclass re-declares with the same name: a name-only resolve
/// always returns the most-derived declaration, but passing the super-class
/// field's descriptor walks past the subclass shadow to the intended slot.
///
/// `descriptor == None` is exactly the name-only behaviour (most-derived
/// declaration wins). The hierarchy is walked subclass → super, so among
/// several descriptor-matching fields the most-derived still wins (two fields
/// with the *same* name and *same* descriptor in the chain are inherently
/// indistinguishable by descriptor — full disambiguation there would need the
/// declaring class, which is out of scope).
pub(crate) fn resolve_field_index_in_hierarchy_desc(
    class_id: ClassId,
    field_name: &str,
    descriptor: Option<&str>,
    store: &ClassStore,
) -> Option<usize> {
    let mut current_id = Some(class_id);
    while let Some(cid) = current_id {
        let class = store.get(cid)?;
        // Search non-static fields declared in this class. `instance_offset`
        // counts every non-static field (matching or not) so the layout slot
        // stays correct regardless of the descriptor filter.
        let mut instance_offset = 0;
        for field in &class.fields {
            if field.is_static() {
                continue;
            }
            if &*field.name == field_name && descriptor.is_none_or(|d| &*field.descriptor == d) {
                return Some(class.first_field_index + instance_offset);
            }
            instance_offset += 1;
        }
        current_id = class.superclass;
    }
    None
}

// ---------------------------------------------------------------------------
// T10.9.E вЂ” Descriptor-aware field-access helpers for NativeContextImpl
// ---------------------------------------------------------------------------

/// Resolve the declared JVM-descriptor first byte (e.g. `b'J'` for a long,
/// `b'L'` for a reference) of the field at `(class_id, slot_index)` using
/// the [`SharedVm::field_descriptor_cache`] as a memoizer.
///
/// The cache is populated on the first miss by walking the class hierarchy
/// starting at `class_id`: we traverse the superclass chain (since
/// `slot_index` can belong to any ancestor layout), and when the owning
/// class is found we extract the field's descriptor's first byte.
///
/// Returns `None` when:
/// - the class is not loaded,
/// - the class is a **synthetic stub** (the stub's field descriptors are
///   generic `Ljava/lang/Object;` placeholders that do NOT reflect the
///   real field type; native code stores primitives into these slots
///   and expects them to round-trip as primitives, so descriptor-aware
///   normalization would be a regression),
/// - the slot index does not match any field in the class or its ancestors,
/// - the descriptor string is empty (should never happen for valid
///   class files, but defensive).
///
/// The `None` return is the signal for the caller to fall back to the
/// legacy descriptor-unaware heap access вЂ” see `NativeContextImpl::get_field`.
///
/// Memoization: positive results cache the descriptor first byte. CONFIRMED
/// misses (a fully-walked real, non-stub hierarchy whose slot has no
/// descriptor) cache the reserved `0u8` (NUL) sentinel — NUL is never a valid
/// descriptor first byte — so repeated lookups short-circuit instead of
/// re-walking the hierarchy on every miss. TRANSIENT misses (unloaded
/// class/ancestor, or a synthetic stub that may be promoted) are NOT cached.
/// `NativeContextImpl::redefine_class` clears the cache as a conservative
/// guard, though JVMTI redefinition is required to preserve field layout.
///
/// Concurrency: the cache write is guarded by `field_descriptor_cache`'s
/// own `RwLock`; we take a read-first fast path so the hot case (cache
/// hit) is lock-free beyond the shared read lock.
thread_local! {
    /// Per-mutator last descriptor lookup. Native wrapper allocation performs
    /// millions of consecutive writes to the same `(class, slot)`; once the
    /// shared cache has a definitive result, repeating its RwLock + hash probe
    /// adds no correctness value. The VM identity prevents cross-VM reuse.
    ///
    /// Entries carry the [`class_origin_epoch`] they were resolved under, so
    /// a TRANSIENT miss can live here too — see
    /// [`resolve_field_descriptor_byte_cached`]'s stub note.
    pub(in crate::vm::vm_exec) static FIELD_DESCRIPTOR_LAST:
        std::cell::Cell<Option<(usize, u32, usize, u8, u64)>> =
        const { std::cell::Cell::new(None) };

    /// PERF (perf/halfgap-20260717): the single-entry cache above thrashes
    /// on the alternating-(class,slot) pattern of hot native fast paths
    /// (real-JDK Matcher: String.value/coder + Matcher scalars +
    /// StringBuilder value/count in rotation — measured 8.7% of a
    /// StringRegexOnly run falling through to the RwLock probe). Small
    /// round-robin working set behind it, same shape as
    /// `gen_heap::compact_field_slot`'s cache. Entries: (vm_key, class_id,
    /// slot, byte); vacant slots have vm_key == 0 (never a real address).
    /// Direct-mapped, not round-robin, and 64 entries rather than 8.
    ///
    /// The 8-entry linear ring was sized for the `String.value`/`coder` +
    /// `Matcher` scalars rotation. A native that reads a dozen fields across
    /// four classes per call — `date_format_fast` touches ~14 distinct
    /// (class, slot) pairs per `format` — overflows it on every call, so every
    /// access fell through to the `RwLock` + hash tier.
    /// `resolve_field_descriptor_byte_cached` was then **15.3%** of that
    /// native in a perf profile, with `coerce_field_value_by_descriptor`
    /// another 3.6%. Indexing instead of scanning also drops the 8-way compare
    /// on the common hit.
    ///
    /// Eviction stays correctness-neutral: a colliding write simply replaces
    /// the entry and the next miss re-walks immutable class metadata. The
    /// stored `(vm_key, class_id, slot)` triple is still verified on every hit,
    /// so a collision can only cost a re-walk, never return another field's
    /// descriptor.
    pub(in crate::vm::vm_exec) static FIELD_DESCRIPTOR_RING:
        std::cell::RefCell<[(usize, u32, usize, u8, u64); 64]> =
        const { std::cell::RefCell::new([(0, 0, 0, 0, 0); 64]) };
}

/// Bucket for [`FIELD_DESCRIPTOR_RING`]. Slot indices are small and dense, so
/// mixing the class id in keeps two classes' slot 0 from colliding.
#[inline]
pub(in crate::vm::vm_exec) fn field_descriptor_bucket(class_id: u32, slot_index: usize) -> usize {
    // Widening: u32 -> usize (value preserved).
    ((class_id as usize)
        .wrapping_mul(31)
        .wrapping_add(slot_index))
        & 63
}

/// Stamp for a DEFINITIVE thread-local entry: one resolved against a real,
/// fully-loaded hierarchy, whose layout is immutable. Such an entry must not
/// expire — it was durable before the epoch existed, and making it perishable
/// would push every lookup back onto the shared `RwLock` for the duration of
/// any class-loading burst, which is a net loss on boot-dominated work.
/// [`class_origin_epoch`] can never return this: it starts at 0 and counts up.
pub(in crate::vm::vm_exec) const FIELD_DESCRIPTOR_DURABLE: u64 = u64::MAX;

/// Record a descriptor result (byte, or 0 = negative) in both thread-local
/// tiers.
///
/// `epoch` is [`FIELD_DESCRIPTOR_DURABLE`] for a definitive result, or the
/// observed [`class_origin_epoch`] for a TRANSIENT one (stub, or an ancestor
/// not loaded yet). Stamping the transient case is what lets it be memoized at
/// all: the entry is honoured only while provenance has not moved, so a stub's
/// promotion retires it.
///
/// The epoch is held PER ENTRY, never as one stamp over the whole table. A
/// table-wide stamp would turn every bump into an O(table) wipe, and
/// provenance moves on every class definition — that shape is a memset with a
/// lookup attached. Per entry, a bump costs nothing: stale entries miss one at
/// a time and are overwritten in place.
pub(in crate::vm::vm_exec) fn field_descriptor_remember(
    vm_key: usize,
    class_id: u32,
    slot_index: usize,
    byte: u8,
    epoch: u64,
) {
    FIELD_DESCRIPTOR_LAST.with(|last| last.set(Some((vm_key, class_id, slot_index, byte, epoch))));
    FIELD_DESCRIPTOR_RING.with(|cell| {
        let mut ring = cell.borrow_mut();
        let idx = field_descriptor_bucket(class_id, slot_index);
        ring[idx] = (vm_key, class_id, slot_index, byte, epoch);
    });
}

/// Hard bound for descriptor memoization. Eviction is correctness-neutral:
/// a miss simply re-walks immutable class metadata and repopulates the entry.
pub(in crate::vm::vm_exec) const FIELD_DESCRIPTOR_CACHE_CAP: usize = 1 << 16;

pub(in crate::vm::vm_exec) fn cache_field_descriptor(shared: &SharedVm, key: (ClassId, usize), byte: u8) {
    let mut cache = shared.classes.field_descriptor_cache.write();
    if !cache.contains_key(&key) && cache.len() >= FIELD_DESCRIPTOR_CACHE_CAP {
        if let Some(victim) = cache.keys().next().copied() {
            cache.remove(&victim);
        }
    }
    cache.insert(key, byte);
}

/// `CRATONVM_DBG_HW_ATOMIC=1` — announce the first hardware field atomic of
/// each kind. A perf or soundness claim about this path is not checkable
/// without it: every guard in `hw_atomic_addr` fails CLOSED to the lock path,
/// so a silently-never-taken fast path looks exactly like a working one.
pub(in crate::vm::vm_exec) fn hw_atomic_dbg() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

pub(in crate::vm::vm_exec) static HW_ATOMIC_SEEN: [std::sync::atomic::AtomicBool; 3] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];

/// Which payload a caller wants to touch with a hardware atomic, and how to
/// prove the field really holds one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::vm::vm_exec) enum HwAtomicKind {
    Int,
    Long,
    Reference,
    /// JIT round 22 wave 4b, lane sync (`CRATONVM_HW_FLOAT_CAS`): a `float`
    /// field, CASed on its RAW bits (`F`; legacy tag 2, 4-byte payload).
    Float,
    /// A `double` field, CASed on its raw bits (`D`; legacy tag 3, 8-byte
    /// payload).
    Double,
}

impl HwAtomicKind {
    pub(in crate::vm::vm_exec) fn matches_descriptor(self, d: u8) -> bool {
        match self {
            HwAtomicKind::Int => d == b'I',
            HwAtomicKind::Long => d == b'J',
            HwAtomicKind::Reference => d == b'L' || d == b'[',
            HwAtomicKind::Float => d == b'F',
            HwAtomicKind::Double => d == b'D',
        }
    }

    /// May a COMPACT cell of this kind be published into
    /// `JitRealm::unsafe_inline` for the compiled `Unsafe` read-modify-write
    /// prefixes (`note_compact_field` / `note_compact_ref_field`)? Only the
    /// kinds those prefixes are written for: an `int` / `long` cell (its
    /// `getAndAdd` / `getAndSet` / CAS prefixes) and a reference cell. A
    /// `float` / `double` cell is CASed here on its raw bits, but a compiled
    /// `LOCK XADD` on it would add integers to an IEEE pattern, so it is never
    /// published (JIT round 22 wave 4b, lane sync; pinned by
    /// `r22_sync_hw_float_cas_tests`).
    pub(in crate::vm::vm_exec) fn publishes_unsafe_rmw_cell(self) -> bool {
        matches!(
            self,
            HwAtomicKind::Int | HwAtomicKind::Long | HwAtomicKind::Reference
        )
    }

    /// The `Value` discriminant a LEGACY cell must ALREADY carry.
    ///
    /// Pinned by `field_cell_layout_matches_value_enum`: Int 0, Long 1,
    /// Object 4. Null and non-null references share tag 4 — a JVM null is a
    /// zero payload word, not a different tag — so a reference CAS across
    /// null never has to rewrite the tag.
    pub(in crate::vm::vm_exec) fn legacy_tag(self) -> u32 {
        match self {
            HwAtomicKind::Int => 0,
            HwAtomicKind::Long => 1,
            HwAtomicKind::Float => 2,
            HwAtomicKind::Double => 3,
            HwAtomicKind::Reference => 4,
        }
    }

    // `Value` is `#[repr(u32)]`: a 4-byte payload (`Int`, `Float`) sits at
    // `FIELD_CELL_PAYLOAD32_OFFSET`, an 8-byte one at `..PAYLOAD64_OFFSET`.
    pub(in crate::vm::vm_exec) fn payload_offset(self) -> usize {
        match self {
            HwAtomicKind::Int | HwAtomicKind::Float => cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET,
            _ => cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET,
        }
    }

    pub(in crate::vm::vm_exec) fn width(self) -> usize {
        match self {
            HwAtomicKind::Int | HwAtomicKind::Float => 4,
            _ => 8,
        }
    }
}

/// Address of a field payload that may be updated with a HARDWARE atomic — the
/// one location the interpreter, the natives and the JIT must all agree on, and
/// all reach with the same primitive.
///
/// Why this exists: `compare_and_swap_field` used to get its atomicity from
/// `monitors.with_cas_lock` plus the collector's `volatile_stripe_lock`. Those
/// make the native path atomic against ITSELF, not against a hardware atomic
/// issued from compiled code, so a compiled `LOCK XADD` racing an interpreted
/// read-compare-write lost updates (24,908 of 600,000, measured). Routing both
/// through one aligned hardware atomic here is what makes the two modes atomic
/// against EACH OTHER.
///
/// Offsets come from [`cratonvm_jit::AtomicIntFieldLayout`], the same type the
/// JIT bakes its displacement from, and the COMPACT/LEGACY arm is chosen per
/// OBJECT on the `GC_FLAG_COMPACT` header bit exactly as the codegen does — a
/// class with a registered compact layout may still have legacy-laid-out
/// instances. If that layout moves, both sides move together.
///
/// `None` means a precondition did not hold and the caller MUST fall back to
/// the lock-based `Value`-cell path. Callers pass an already-forwarded `obj`.
#[inline]
pub(in crate::vm::vm_exec) fn hw_atomic_addr(
    shared: &SharedVm,
    obj: ObjectRef,
    index: usize,
    kind: HwAtomicKind,
) -> Option<*mut u8> {
    hw_atomic_addr_checked(shared, obj, index, kind).ok()
}

/// Why [`hw_atomic_addr_checked`] refused (JIT round 21 wave 6, lane
/// updaters, review item 6). Only the field-cell publication path
/// (`note_unsafe_volatile_field_cell`) asks: a refusal settles a cell key
/// only when it is fixed for the class's lifetime or for its layout epoch
/// ([`hw_atomic_refusal_settles`]); a per-object one must not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::vm::vm_exec) enum HwAtomicRefusal {
    /// An array: its class has no field layout (the class's lifetime).
    Array,
    /// The slot's descriptor did not resolve: a confirmed miss (memoised,
    /// the class's lifetime) or a transient one (the class or an ancestor
    /// unstored, or a synthetic stub that may be promoted).
    DescriptorUnresolved,
    /// The slot is declared, of another type (the class's lifetime).
    DescriptorMismatch,
    /// Compact: the current layout has no displacement / storage for the
    /// slot (the layout epoch).
    CompactNoSlot,
    /// Compact: the slot's storage is not this atomic's width (the layout
    /// epoch).
    CompactWidth,
    /// Compact: the displacement is not naturally aligned (the layout epoch).
    CompactDispMisaligned,
    /// Legacy: the slot is past the object's slot count (the object).
    LegacyIndex,
    /// Legacy: the cell's tag is not already this type (the object's
    /// current contents).
    LegacyTag,
    /// The payload address is not naturally aligned, or overflows (the
    /// object's address).
    Address,
}

/// Address of a field payload that may be updated with a hardware atomic, or
/// why not: [`hw_atomic_addr`] (whose doc states the rules) with its refusal
/// named. The two answer `Some` / `Ok` for exactly the same inputs and
/// publish exactly the same cells.
pub(in crate::vm::vm_exec) fn hw_atomic_addr_checked(
    shared: &SharedVm,
    obj: ObjectRef,
    index: usize,
    kind: HwAtomicKind,
) -> Result<*mut u8, HwAtomicRefusal> {
    // Arrays have no field layout; their elements keep the existing path.
    if shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return Err(HwAtomicRefusal::Array);
    }
    let class_id = shared.mem.heap.class_id_of(obj);
    // The slot must really be of the requested type. This doubles as the bounds
    // proof: the descriptor only resolves for a declared field of this class.
    if !kind.matches_descriptor(
        resolve_field_descriptor_byte_cached(shared, class_id, index)
            .ok_or(HwAtomicRefusal::DescriptorUnresolved)?,
    ) {
        return Err(HwAtomicRefusal::DescriptorMismatch);
    }
    let header = shared.mem.heap.get_header(obj);
    let compact = header.gc_flags() & cratonvm_types::GC_FLAG_COMPACT != 0;
    let base = obj.as_ptr() as usize;
    // nio21 wave 7 L3 (W6-L3-1): the compact displacement to publish for the
    // compiled `Unsafe` RMW prefixes' field arm, with the layout epoch it was
    // resolved under (read BEFORE the resolution; `None` until a compile
    // baked this VM's cell table, and for a reference).
    let mut publish: Option<(usize, u32)> = None;

    let addr = if compact {
        // JIT round 20 wave 12, lane qUnsafe (AT-4): a reference field is
        // published into the table's reference half, armed only by a compile
        // with `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD` on, and never on ZGC
        // (an armed colored-pointer barrier needs heal-then-CAS, AT-7).
        let epoch = if kind == HwAtomicKind::Reference {
            if shared.mem.heap.is_generational() || shared.mem.heap.is_g1() {
                shared.jit.unsafe_inline.ref_epoch_if_armed()
            } else {
                None
            }
        } else {
            shared.jit.unsafe_inline.epoch_if_armed()
        };
        // A compact body offset IS the payload address; there is no tag word,
        // so there is nothing to guard. Refuse any width this atomic cannot
        // address (a narrow reference, say) rather than tearing it.
        let (disp, storage) = cratonvm_types::compact_field_disp_storage(class_id.as_u32(), index)
            .ok_or(HwAtomicRefusal::CompactNoSlot)?;
        if storage.size_runtime() as usize != kind.width() {
            return Err(HwAtomicRefusal::CompactWidth);
        }
        // Refused by the alignment test below anyway; named here so the
        // refusal reads as the layout's, not the object's.
        if disp % kind.width() != 0 {
            return Err(HwAtomicRefusal::CompactDispMisaligned);
        }
        publish = epoch.map(|e| (disp, e));
        base.checked_add(disp).ok_or(HwAtomicRefusal::Address)?
    } else {
        if index >= header.num_slots() as usize {
            return Err(HwAtomicRefusal::LegacyIndex);
        }
        let cell = index
            .checked_mul(cratonvm_types::SLOT_SIZE)
            .and_then(|off| base.checked_add(cratonvm_types::HEADER_SIZE)?.checked_add(off))
            .ok_or(HwAtomicRefusal::Address)?;
        // TAG GUARD. A LEGACY cell carries its type in the tag word, and
        // `values_equal_for_cas` deliberately treats an uninitialized slot
        // (which reads as `Object(None)`) as equal to a typed zero. Writing
        // only the payload of such a cell would leave the tag saying `Object`,
        // and the next descriptor-coercing read would hand back the coerced
        // zero — losing the write. Take the hardware path only when the cell is
        // already a properly tagged cell of this type; everything else falls
        // back to the lock path, which keeps that coercion behaviour.
        if cell % std::mem::align_of::<u32>() != 0 {
            return Err(HwAtomicRefusal::Address);
        }
        // SAFETY: `cell` is the start of a live 16-byte `Value` slot.
        let tag = unsafe {
            (*(cell as *const std::sync::atomic::AtomicU32))
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        if tag != kind.legacy_tag() {
            return Err(HwAtomicRefusal::LegacyTag);
        }
        cell.checked_add(kind.payload_offset())
            .ok_or(HwAtomicRefusal::Address)?
    };

    // A hardware atomic requires natural alignment; anything else would be a
    // torn access rather than a slow one, so refuse instead of degrading.
    if addr % kind.width() != 0 {
        return Err(HwAtomicRefusal::Address);
    }
    // Every guard passed: the caller does ONE hardware atomic at `base + disp`
    // for this `(class, slot, width)` on every compact instance, so a
    // compiled locked instruction there is in the same domain
    // (`JitRealm::unsafe_inline`, `UnsafeInlineCells`).
    // JIT round 22 wave 4b, lane sync: a `float` / `double` cell is never
    // published (`HwAtomicKind::publishes_unsafe_rmw_cell`).
    let publish = publish.filter(|_| kind.publishes_unsafe_rmw_cell());
    if let Some((disp, epoch)) = publish.filter(|_| kind == HwAtomicKind::Reference) {
        // JIT round 21 wave 1, lane updaters (review UPD21-R1): never a cell
        // for `java.lang.ref.Reference`'s own fields (slots 0..4 of every
        // subclass), whose `referent` a compiled plain load must not read;
        // `note_unsafe_volatile_field_cell` refuses them for the get / put
        // publisher, and this is the CAS publisher's half. Asked only when a
        // cell would be written.
        shared.jit.unsafe_inline.note_compact_ref_field(
            class_id.as_u32(),
            index,
            disp,
            epoch,
            || index < 4 && class_is_java_lang_ref_reference(shared, class_id),
        );
    } else if let Some((disp, epoch)) = publish {
        shared.jit.unsafe_inline.note_compact_field(
            class_id.as_u32(),
            index,
            kind == HwAtomicKind::Long,
            disp,
            epoch,
        );
    }
    if hw_atomic_dbg() {
        let slot = match kind {
            HwAtomicKind::Int | HwAtomicKind::Float => 0usize,
            HwAtomicKind::Long | HwAtomicKind::Double => 1,
            HwAtomicKind::Reference => 2,
        };
        if !HW_ATOMIC_SEEN[slot].swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[hw-atomic] first {:?} field atomic: class_id={} index={} layout={}",
                kind,
                class_id.as_u32(),
                index,
                if compact { "compact" } else { "legacy" },
            );
        }
    }
    Ok(addr as *mut u8)
}

/// JIT round 21 wave 6, lane updaters (review item 6): may `why`, refused
/// for `class_id`'s slot `index` on a COMPACT instance, settle that field's
/// cell key (`UnsafeInlineCells::note_field_refusal`)? `true` only for a
/// refusal fixed for the class's lifetime or for its layout epoch (the
/// refusal is recorded under the epoch the attempt began in, and a layout
/// replacement resets the table). A descriptor miss settles only when it is
/// the CONFIRMED one `resolve_field_descriptor_byte_cached` memoises (`0`);
/// a transient miss, and every per-object refusal, does not.
pub(in crate::vm::vm_exec) fn hw_atomic_refusal_settles(
    shared: &SharedVm,
    class_id: ClassId,
    index: usize,
    why: HwAtomicRefusal,
) -> bool {
    match why {
        HwAtomicRefusal::Array
        | HwAtomicRefusal::DescriptorMismatch
        | HwAtomicRefusal::CompactNoSlot
        | HwAtomicRefusal::CompactWidth
        | HwAtomicRefusal::CompactDispMisaligned => true,
        HwAtomicRefusal::DescriptorUnresolved => shared
            .classes
            .field_descriptor_cache
            .read()
            .get(&(class_id, index))
            .is_some_and(|&b| b == 0),
        HwAtomicRefusal::LegacyIndex | HwAtomicRefusal::LegacyTag | HwAtomicRefusal::Address => {
            false
        }
    }
}

/// # Safety
/// `addr` must come from [`hw_atomic_addr`] with the matching [`HwAtomicKind`]
/// for a live, already-forwarded object. Natives run without reaching a
/// safepoint, so a relocating collector cannot move the object underneath a
/// single atomic op.
pub(in crate::vm::vm_exec) unsafe fn hw_atomic_i32<'a>(addr: *mut u8) -> &'a std::sync::atomic::AtomicI32 {
    &*(addr as *const std::sync::atomic::AtomicI32)
}

/// # Safety
/// See [`hw_atomic_i32`].
pub(in crate::vm::vm_exec) unsafe fn hw_atomic_i64<'a>(addr: *mut u8) -> &'a std::sync::atomic::AtomicI64 {
    &*(addr as *const std::sync::atomic::AtomicI64)
}

/// # Safety
/// See [`hw_atomic_i32`]. The payload word of a reference cell is the raw
/// pointer (or zero for a JVM null), which is what `values_equal_for_cas`
/// compares references by, so a bit-level CAS here matches the lock path's
/// semantics exactly.
pub(in crate::vm::vm_exec) unsafe fn hw_atomic_usize<'a>(addr: *mut u8) -> &'a std::sync::atomic::AtomicUsize {
    &*(addr as *const std::sync::atomic::AtomicUsize)
}

pub(in crate::vm::vm_exec) fn resolve_field_descriptor_byte_cached(
    shared: &SharedVm,
    class_id: ClassId,
    slot_index: usize,
) -> Option<u8> {
    // A SharedVm address can be recycled after a short-lived VM is dropped,
    // especially by unit tests. Use the process-unique lifetime identity so
    // thread-local descriptor entries can never bleed into a later VM.
    let vm_key = shared.vm_identity;
    // Provenance generation. Entries in the two thread-local tiers are only
    // honoured while this is unchanged, which is what lets a synthetic-stub
    // answer be memoized at all (see the stub note on the slow path).
    // Under `CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH` the narrower stamp of
    // [`class_id_memo_epoch`] (interpreter round i4 wave 95, lane L7).
    let epoch = class_id_memo_epoch(shared);
    // An entry is live if it is DURABLE (definitive, immutable layout) or was
    // stamped under the provenance generation still in force.
    let live = |stamp: u64| stamp == FIELD_DESCRIPTOR_DURABLE || stamp == epoch;
    if let Some((cached, stamp)) = FIELD_DESCRIPTOR_LAST.with(|cache| {
        cache
            .get()
            .filter(|(vm, cid, slot, _, _)| {
                *vm == vm_key && *cid == class_id.as_u32() && *slot == slot_index
            })
            .map(|(_, _, _, byte, ep)| (byte, ep))
    }) {
        if live(stamp) {
            return if cached == 0 { None } else { Some(cached) };
        }
    }
    // Second tier: the direct-mapped table (see FIELD_DESCRIPTOR_RING's doc).
    let ring_hit = FIELD_DESCRIPTOR_RING.with(|cell| {
        let ring = cell.borrow();
        let entry = ring[field_descriptor_bucket(class_id.as_u32(), slot_index)];
        (entry.0 == vm_key
            && entry.1 == class_id.as_u32()
            && entry.2 == slot_index
            && live(entry.4))
        .then_some((entry.3, entry.4))
    });
    if let Some((byte, stamp)) = ring_hit {
        FIELD_DESCRIPTOR_LAST
            .with(|last| last.set(Some((vm_key, class_id.as_u32(), slot_index, byte, stamp))));
        return if byte == 0 { None } else { Some(byte) };
    }
    // Fast path: read lock, hash lookup, early return on hit.
    //
    // PERF (negative-result memoization): the cache value `0u8` (NUL) is a
    // RESERVED SENTINEL meaning "resolved, but this (class, slot) has no
    // descriptor-aware type" — i.e. a CONFIRMED miss on a real, fully-loaded
    // hierarchy (the slot lands on no field, or its field carries no
    // descriptor byte). NUL is never a legitimate descriptor first byte (valid
    // JVM field descriptors begin with one of `B C D F I J S Z L [`), so this
    // overload is unambiguous. Caching the negative lets repeated lookups of
    // such a slot skip the class-hierarchy re-walk that the slow path performs
    // on every miss. TRANSIENT misses (unloaded class/ancestor, or a synthetic
    // stub that may be promoted) are deliberately NOT cached — see below.
    {
        let cache = shared.classes.field_descriptor_cache.read();
        if let Some(&b) = cache.get(&(class_id, slot_index)) {
            field_descriptor_remember(
                vm_key,
                class_id.as_u32(),
                slot_index,
                b,
                FIELD_DESCRIPTOR_DURABLE,
            );
            return if b == 0 { None } else { Some(b) };
        }
    }

    // Slow path: resolve via class metadata.
    //
    // CORRECTNESS: if the instance's concrete class is a synthetic stub,
    // its field descriptors are placeholder `Ljava/lang/Object;` entries
    // that do NOT reflect the real field type вЂ” panama, Unsafe, and other
    // natives store raw primitives into these "Object" slots and expect
    // them to round-trip as primitives. Descriptor-aware coercion on such
    // a slot would rewrite `Value::Long(0)` / `Value::Int(0)` as
    // `Value::Object(None)` (via the `b'L'` arm of
    // `coerce_field_value_by_descriptor`), silently corrupting the
    // native caller's view. The only safe short-circuit is to return
    // `None` immediately so the caller uses the raw descriptor-unaware
    // heap access вЂ” matching pre-T10.9.E behaviour for the stub case.
    // `cacheable` distinguishes a CONFIRMED miss (the hierarchy was fully
    // walked over real, non-stub classes and the slot legitimately has no
    // descriptor) from a TRANSIENT miss (the class — or an ancestor — is not
    // yet loaded, or is a synthetic stub that may later be promoted to the
    // real class). Only confirmed misses are memoized via the `0u8` sentinel;
    // transient misses must re-walk on the next call so they pick up the real
    // descriptor once the class is loaded/promoted.
    let mut cacheable = false;
    // The receiver's class is stored: a transient answer for an id with no
    // class behind it is not memoised under the narrow stamp (a definition
    // there moves neither half of it; wave 95, lane L7).
    let mut class_stored = false;
    let desc_byte = {
        let cm = shared.classes.class_manager.read();
        if let Some(concrete_cls) = cm.get_class(class_id) {
            class_stored = true;
            if concrete_cls.origin.is_compatibility_stub() {
                // Transient: stub may be promoted to the real class later.
                None
            } else {
                // Walk the class hierarchy вЂ” the slot may belong to an
                // ancestor. `field_at_index` returns `None` when the slot
                // is declared by a superclass, so we step up via
                // `superclass` in that case. If any ancestor is a
                // synthetic stub we also bail out, since that stub's
                // descriptors are unreliable in exactly the same way.
                //
                // BUG FIX (Cipher key binding): `Class::field_at_index`
                // indexes into the raw `fields` Vec which interleaves
                // statics and instance fields, so for a class like
                // `javax.crypto.spec.SecretKeySpec` whose declared layout
                // is `[serialVersionUID:J(static), key:[B, algorithm:Ljava/lang/String;]`,
                // calling `field_at_index(0)` returns the static
                // `serialVersionUID` (descriptor `J`) instead of the
                // instance field `key` (descriptor `[B`). The descriptor
                // cache then thinks slot 0 is `long`, which silently
                // mangles `byte[]` writes into long-coerced bits and
                // surfaces as `expected object reference, got double`
                // on read-back. We replicate `field_at_index`'s
                // first-field-index arithmetic here but skip static
                // fields when stepping through the slot index, which is
                // the same logic used by `resolve_field_index_in_hierarchy`.
                let mut cid_opt = Some(class_id);
                let mut found: Option<u8> = None;
                loop {
                    let Some(cid) = cid_opt else {
                        // Walked off the top of a fully-real hierarchy
                        // (e.g. `java/lang/Object.superclass == None`) without
                        // the slot landing in any class. This is a confirmed
                        // miss against the immutable real layout вЂ” memoize it.
                        cacheable = true;
                        break;
                    };
                    if let Some(cls) = cm.get_class(cid) {
                        if cls.origin.is_compatibility_stub() {
                            // Transient: an ancestor stub's descriptors are
                            // unreliable and may change on promotion. Do NOT
                            // memoize вЂ” `cacheable` stays false.
                            break;
                        }
                        // Slot is in this class iff slot >= first_field_index.
                        if slot_index >= cls.first_field_index {
                            let local_offset = slot_index - cls.first_field_index;
                            // Walk this class's fields skipping statics вЂ”
                            // matches the layout used by `getfield`/`putfield`.
                            let mut instance_idx = 0usize;
                            for field in &cls.fields {
                                if field.is_static() {
                                    continue;
                                }
                                if instance_idx == local_offset {
                                    if let Some(byte) = field.descriptor.as_bytes().first().copied()
                                    {
                                        found = Some(byte);
                                    }
                                    break;
                                }
                                instance_idx += 1;
                            }
                            // Owning class resolved on a real (non-stub) class:
                            // whether or not a descriptor byte was present, this
                            // is a definitive result against an immutable layout.
                            cacheable = true;
                            break;
                        }
                        cid_opt = cls.superclass;
                    } else {
                        // Transient: an ancestor class is not loaded yet; it may
                        // load later and supply the slot's real descriptor.
                        break;
                    }
                }
                found
            }
        } else {
            None
        }
    };

    // Write-back. A positive hit caches the real descriptor byte. A CONFIRMED
    // miss (`cacheable`) caches the `0u8` sentinel so the next lookup short-
    // circuits in the fast path instead of re-walking the hierarchy вЂ” this is
    // the common case for any real field/slot with no descriptor mapping, which
    // previously re-walked under the read lock on every single access.
    //
    // Correctness under class redefinition: JEP 109 / JVMTI redefinition
    // performs an in-place method-body swap and MUST preserve field layout, so
    // a cached descriptor (positive or negative) remains valid. As a belt-and-
    // braces guard, `NativeContextImpl::redefine_class` clears this cache.
    match desc_byte {
        Some(b) => {
            // Defensive: a real descriptor first byte is never NUL, so it can
            // never collide with the negative sentinel.
            debug_assert_ne!(b, 0, "descriptor first byte must not be NUL");
            cache_field_descriptor(shared, (class_id, slot_index), b);
            field_descriptor_remember(
                vm_key,
                class_id.as_u32(),
                slot_index,
                b,
                FIELD_DESCRIPTOR_DURABLE,
            );
        }
        None if cacheable => {
            cache_field_descriptor(shared, (class_id, slot_index), 0u8);
            field_descriptor_remember(
                vm_key,
                class_id.as_u32(),
                slot_index,
                0u8,
                FIELD_DESCRIPTOR_DURABLE,
            );
        }
        // TRANSIENT miss: the receiver's class — or an ancestor — is a
        // synthetic stub whose descriptors are placeholders, or is not loaded
        // yet. The answer may change (a stub can be promoted to the real class,
        // an ancestor can load), so it must NOT go into the SHARED cache, which
        // has no invalidation.
        //
        // PERF (testssl-testpost bulk TLS): it may, however, be memoized in the
        // THREAD-LOCAL tiers, because those are epoch-stamped and
        // `Class::set_origin` bumps the epoch on exactly the events that could
        // change the answer. Without this, every `NativeContext::get_field` on a
        // synthetic receiver re-took the process-global `class_manager` read
        // lock to re-derive an answer that never changes in practice — and
        // CratonVM's own stream/socket stand-ins (`SSLSocketInputStream` and
        // friends) are all synthetic. `TestSsl.testPost` reads 16 MiB one byte
        // at a time on each of 8 threads, so that lock was acquired ~134 million
        // times in one test; measured against a no-op native on the same
        // receiver, the field read cost 167 ns/call at 1 thread and 1166 ns/call
        // at 8 — the superlinear part being the shared-cacheline RMW of the read
        // acquire, not any real work.
        None if class_stored || !shared.config.class_memo_narrow_epoch => {
            field_descriptor_remember(vm_key, class_id.as_u32(), slot_index, 0u8, epoch)
        }
        None => {}
    }
    desc_byte
}

// ---------------------------------------------------------------------------
// NativeContextImpl вЂ” adapter for NativeContext trait on SharedVm + JvmThread
// ---------------------------------------------------------------------------

/// Adapter that implements [`NativeContext`] using `SharedVm` + `JvmThread`.
///
/// This replaces the previous `impl NativeContext for Vm`. Native methods
/// receive a `&mut NativeContextImpl` which provides access to the shared
/// VM state and the calling thread's state.
/// Does `value` disagree with the slot's declared descriptor across the
/// reference/primitive boundary?
///
/// A native that reaches a real JDK object by a slot index chosen against
/// CratonVM's own fabricated layout writes whatever its model says lives there.
/// When the model and the image disagree about *type class*, this is the tell:
/// a primitive landing in a reference-typed (`L`/`[`) slot, or a reference
/// landing in a primitive one. Real (verified) bytecode never does either.
/// Same-type-class tag normalization (Long-as-Double -> Long, `Uninitialized`
/// -> typed zero) is benign and is NOT flagged.
///
/// **L4 gap 3, 2026-08-05: `Object(None)` over a primitive now counts.** The
/// predicate used to require `Object(Some(_))`, on the reasoning that a null
/// coerces to a typed zero and so destroys nothing. That is true about the
/// *value* and false about the *defect*: the native still believed it was
/// clearing a reference field and still wrote a slot the image declares as an
/// `int`. `native_props_init` writing a null to `PROPS_FIELD_DEFAULTS` — which
/// is `loadFactor` on a real `java.util.Properties` — is the measured example,
/// and it never appeared in any census. The function is named for the mismatch
/// rather than for the damage because that is what it actually tests.
pub(in crate::vm::vm_exec) fn overlay_access_is_cross_type(value: Value, desc: u8) -> bool {
    match desc {
        b'L' | b'[' => matches!(
            value,
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)
        ),
        // `Object(_)`, not `Object(Some(_))`: see the gap-3 note above.
        // `Uninitialized` stays out — it is the allocator's "no value yet" tag,
        // not a claim by any native about what the slot holds.
        b'J' | b'D' | b'F' | b'I' | b'B' | b'C' | b'S' | b'Z' => {
            matches!(value, Value::Object(_))
        }
        _ => false,
    }
}

thread_local! {
    /// Per-class shadow-layout diffs, memoized so the access-site hunter's
    /// check is a map lookup plus an array index instead of a hierarchy walk
    /// under the class-manager lock. Stamped with `class_origin_epoch()` (a
    /// stub promoted to real bytes changes the answer) and with the VM identity
    /// (a `SharedVm` address can be recycled by a later VM in-process).
    ///
    /// Populated only when `CRATONVM_DBG_OVERLAY` is on, so it costs a default
    /// run nothing.
    pub(in crate::vm::vm_exec) static SHADOW_LAYOUT_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<(usize, u32), (u64, Option<std::sync::Arc<ShadowLayoutDiff>>)>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// The shadow-layout diff for `class_id`, or `None` when the class carries no
/// fabricated slot model or is itself a stub.
pub(in crate::vm::vm_exec) fn shadow_layout_for(
    shared: &SharedVm,
    class_id: ClassId,
) -> Option<std::sync::Arc<ShadowLayoutDiff>> {
    let vm_key = shared.vm_identity;
    let epoch = cratonvm_classloading::class_origin_epoch();
    let key = (vm_key, class_id.as_u32());
    if let Some(hit) = SHADOW_LAYOUT_CACHE.with(|c| {
        c.borrow()
            .get(&key)
            .filter(|(stamp, _)| *stamp == epoch)
            .map(|(_, v)| v.clone())
    }) {
        return hit;
    }
    // Deliberately computed with the lock taken and released here, not held
    // across the log below: `cold_log_overlay_access` re-acquires it, and two
    // sequential reads are safe where a nested one is not.
    let computed = {
        let cm = shared.classes.class_manager.read();
        cm.shadow_layout_diff(class_id).map(std::sync::Arc::new)
    };
    SHADOW_LAYOUT_CACHE.with(|c| {
        c.borrow_mut().insert(key, (epoch, computed.clone()));
    });
    computed
}

/// Why the hunter fired. A single access can trip more than one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::vm::vm_exec) enum OverlayReason {
    /// The value's type class disagrees with the slot's declared descriptor.
    /// The original signal: catches an `Int` over a reference and a reference
    /// (now including `null`) over a primitive.
    CrossType,
    /// The slot is one where CratonVM's fabricated model for this class and the
    /// loaded image disagree, whatever the value's type tag says. This is the
    /// only signal that reaches a same-kind wrong-slot access — an `Int` into
    /// the wrong `Int` slot type-checks and would otherwise pass silently.
    ModelSlot,
}

/// Cold path for the overlay hunter. Logs the suspect native field access: the
/// bound class (the real JDK class whose layout the access is addressing), the
/// slot, the value, why it fired, and the native caller's Java frames so the
/// offending native is identifiable. Gated by `CRATONVM_DBG_OVERLAY`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn cold_log_overlay_access(
    shared: &SharedVm,
    thread: &JvmThread,
    class_id: ClassId,
    index: usize,
    value: Value,
    desc: u8,
    is_read: bool,
    reasons: &[OverlayReason],
    shadow: Option<&ShadowLayoutDiff>,
) {
    let class_name = {
        let cm = shared.classes.class_manager.read();
        // Suppress the dominant benign case: a synthetic `<init>` native
        // writing placeholder `size`/`capacity` Ints to a `java.util.Map`
        // subtype. The map's real fields at those slots are references
        // (`values`/`table`), so coercion-to-null produces exactly the
        // null-initialized state real Map bytecode expects (S111r29) — the
        // map then runs real bytecode and works. Flagging these drowns the
        // genuinely-broken overlays. Set CRATONVM_DBG_OVERLAY_ALL=1 to see
        // them too.
        if None::<std::ffi::OsString>.is_none() {
            if let Some(map_id) = cm.get_loaded_class_id("java/util/Map") {
                if cm.is_subclass_of(class_id, map_id) {
                    return;
                }
            }
        }
        cm.get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<cid {}>", class_id.as_u32()))
    };
    let op = if is_read { "get_field" } else { "set_field" };
    let why = reasons
        .iter()
        .map(|r| match r {
            OverlayReason::CrossType => "cross-type",
            OverlayReason::ModelSlot => "model-slot",
        })
        .collect::<Vec<_>>()
        .join("+");
    // The model's own claim about the slot, when we have one. This is what
    // turns a same-kind report from "something is odd here" into a statement:
    // *our* model calls slot 4 `parallelLockMap`, the image calls it `classes`.
    let model = shadow
        .and_then(|s| s.slot(index))
        .map(|s| {
            format!(
                " model={}:{} real={}:{} verdict={}",
                s.model_name,
                s.model_desc,
                s.real_name.as_deref().unwrap_or("<none>"),
                s.real_desc.as_deref().unwrap_or("<none>"),
                s.verdict.tag(),
            )
        })
        .unwrap_or_default();
    eprintln!(
        "[OVERLAY] suspect native {op} [{why}]: class={class_name} slot={index} \
         value={value:?} real_field_desc='{}'{model} (overlay layout bound to a real JDK class)",
        desc as char,
    );
    for f in thread.frames.iter().rev().take(6) {
        eprintln!(
            "[OVERLAY]   at {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc,
        );
    }
    // `CRATONVM_DBG=overlay-bt` — the RUST writer, optionally filtered by a
    // substring of the class name (`overlay-bt=ClassLoaders`).
    //
    // The Java frames above name the method that was executing, which is often
    // not the code that wrote the field: a native reached from a `<clinit>`, or
    // from class-loading machinery triggered incidentally, prints a Java stack
    // that has nothing to do with the writer. The measured example is four
    // `Int` writes over reference slots on `ClassLoaders$AppClassLoader` whose
    // Java stack reads `BufferedWriter.initialBufferSize()`.
    //
    // Same lesson as the class-origin census's `requested_by`, which had to
    // learn to name the Rust call site because the Java frame was usually
    // absent: for a defect that lives in native code, the Rust backtrace is the
    // answer and the Java frame is context.
    if let Ok(spec) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        let want = spec.trim();
        if want.is_empty() || want == "1" || class_name.contains(want) {
            eprintln!(
                "[OVERLAY]   rust writer:\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
    }
}

/// Has this `(class, slot, read/write)` already produced a model-slot report?
///
/// Returns `true` exactly once per distinct site. Process-global on purpose —
/// a per-thread set would print one line per thread for the same site, which is
/// noise, not information. Only ever populated under `CRATONVM_DBG_OVERLAY`, so
/// a default run and every unit test leave it empty.
///
/// This is diagnostic de-duplication state, not `--real-jdk` feature state (the
/// contract's "no process globals" clause is about the latter), and it is the
/// same shape as the `static N: AtomicU32` print caps the two neighbouring
/// diagnostics in this file already use.
pub(in crate::vm::vm_exec) fn first_model_slot_report(class_id: ClassId, index: usize, is_read: bool) -> bool {
    // `CRATONVM_DBG=overlay-nodedup` turns the cap off, for when you want
    // per-site COUNTS rather than per-site presence.
    static NODEDUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *NODEDUP.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()
    }) {
        return true;
    }
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashSet<(u32, u32, bool)>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .insert((class_id.as_u32(), index as u32, is_read))
}

/// Decide whether a native positional field access is suspect, and report it.
///
/// One entry point for both halves — `NativeContextImpl::get_field` and
/// `::set_field` — because the two questions are the same question. The write
/// path has been instrumented since the hunter was written; **the read path had
/// not, and reads are arguably the more common half**: a native that reads slot
/// 3 of a real `java.util.Properties` gets a `float` where its model says a
/// reference lives, misbehaves, and nothing ever said so.
///
/// `value` is what is being written, or what the read is about to return.
#[inline]
pub(in crate::vm::vm_exec) fn overlay_check_access(
    shared: &SharedVm,
    thread: &JvmThread,
    class_id: ClassId,
    index: usize,
    value: Value,
    desc: u8,
    is_read: bool,
) {
    let mut reasons: [OverlayReason; 2] = [OverlayReason::CrossType; 2];
    let mut n = 0usize;
    if overlay_access_is_cross_type(value, desc) {
        reasons[n] = OverlayReason::CrossType;
        n += 1;
    }
    let shadow = shadow_layout_for(shared, class_id);
    let model_slot = shadow.as_ref().is_some_and(|s| s.is_disagreeing(index));
    if model_slot {
        reasons[n] = OverlayReason::ModelSlot;
        n += 1;
    }
    if n == 0 {
        return;
    }
    // Volume control, and only for the NEW signal. A cross-type access still
    // prints every time, so the pre-L4 census rows keep their counts and an A/B
    // against a pre-L4 binary compares like with like.
    //
    // A model-slot-only report is different in kind: the complete list of
    // disagreeing slots is already printed once per class by the shadow-layout
    // census, so the per-access line's whole job is to NAME the native, and one
    // occurrence does that. Without this, `java/lang/String` slot 1 alone —
    // model says reference, image says `byte coder` — buries the run.
    if n == 1
        && reasons[0] == OverlayReason::ModelSlot
        && !first_model_slot_report(class_id, index, is_read)
    {
        return;
    }
    cold_log_overlay_access(
        shared,
        thread,
        class_id,
        index,
        value,
        desc,
        is_read,
        &reasons[..n],
        shadow.as_deref(),
    );
}

/// Run a previously yielded virtual-thread continuation on a pool carrier.
///
/// The boxed `JvmThread` owns heap-resident Java frames, so returning from this
/// function releases the native carrier stack while preserving the Java stack.
pub(in crate::vm::vm_exec) fn resume_virtual_continuation(shared: std::sync::Arc<SharedVm>, vt_id: u64) {
    let Some((mut thread, resumed)) = shared
        .threads
        .virtual_thread_manager
        .take_runtime_for_mount(vt_id, vt_id)
    else {
        return;
    };
    let tid = thread.thread_id;
    // THREADLOCALS FOLLOW THE CONTINUATION (gc-common w4-b,
    // `docs/feature-designs/real-jdk-mode.md`).
    // The `ThreadLocal` intrinsics keep the running thread's map in an
    // OS-thread `thread_local!`, i.e. on THIS CARRIER. Install the virtual
    // thread's own state for the duration of the mount and keep the carrier's
    // aside; every exit below puts it back (yield: saved into the boxed
    // `JvmThread`; death: released first; the no-mirror bail: saved). Without
    // this, virtual threads sharing a carrier read each other's values, lost
    // theirs on migration, and a dead one's values stayed JNI global roots.
    let carrier_thread_locals = cratonvm_native_builtins::phases_early::swap_current_thread_locals(
        std::mem::take(&mut thread.detached_thread_locals),
    );
    // JIT round 20 wave 8 (lane huntFFM): the FFM verdicts and the compiled
    // segment door's memo are per OS thread and hold confined-owner answers
    // about the virtual thread mounted here before this one.
    cratonvm_native_builtins::ffm_fast::forget_thread_verdicts();
    crate::jit::helpers::forget_real_seg_entries();
    // CRASH DIAGNOSTICS (`startup-and-diagnostics.md` §6.3): publish this
    // continuation's frame trace into the CARRIER's thread-local cell for the
    // duration of the mount, so a fault while it is running renders *its* Java
    // stack instead of the primordial thread's. Deliberately here and not at
    // `install_runtime` (which runs on the spawning thread, not the carrier):
    // a carrier hosts many continuations over its life, and the report has to
    // name the one that was actually mounted. The guard restores the previous
    // occupant on every exit path — normal return, `ContinuationYield` unmount,
    // the missing-`java_thread_obj` bail — so the cell never outlives a mount.
    //
    // This is the crash class the primordial-only publication misses by
    // construction: virtual-thread resume heap corruption faults on a carrier.
    let _crash_frames = super::vm_util::PublishedFrameTrace::publish(
        &thread.name,
        tid.0,
        thread.frame_trace.clone(),
    );
    // Publish this mount's identity BEFORE leaving the blocked region.
    //
    // `check_post_block_gc` below turns this thread back into a COUNTED
    // mutator (`GcBarrier::leave_blocked_region_flagged`). From that instant a
    // new stop-the-world can be requested that *expects* us, and the takeover
    // / cross-thread scan paths identify us through the registry:
    // `set_os_tid_current` feeds the counted-set excusal and
    // `frozen_peer_thread_addrs`, which maps OS tids back to
    // `jvm_thread_addr`. A parked continuation is remounted on whichever
    // carrier picked it up, so `os_tid` genuinely differs from the previous
    // mount — becoming counted while it still names the *old* carrier would
    // point a takeover at the wrong OS thread. Hence: publish first, drain
    // second.
    shared.threads.thread_registry.set_os_tid_current(tid);
    // gen r5w2/roots6: on a machine stack again (see `ThreadEntry::mounted`).
    shared.threads.thread_registry.set_mounted(tid, true);
    shared
        .threads
        .thread_registry
        .set_jvm_thread_addr(tid, (&*thread as *const JvmThread) as usize);
    shared
        .threads
        .thread_registry
        .set_tlab_addr(tid, &thread.tlab as *const cratonvm_gc::Tlab as usize);
    // REMOUNT FIXUP (2026-07-26, `vt-resume-gc-fixup.md`).
    //
    // This used to be a bare
    //     gc_block_state.in_blocked_region.store(false, Release)
    // placed *above* the publishes, and it never drained the blocked-region
    // fixup. That is a heap corruptor, not a tidiness issue:
    //
    //  * `suspend_runtime`'s caller runs `deposit_root_snapshot()`, which
    //    RAISES `in_blocked_region`. So for the whole parked duration this
    //    thread is excluded from every STW census.
    //  * Every moving collection that runs meanwhile calls
    //    `ThreadRegistry::fold_pointer_map_into_blocked`, which remaps only the
    //    `root_snapshot` and *accumulates* the moves into
    //    `gc_block_state.fixup` (chained orig->cur) and `slot_origins`. It
    //    deliberately does NOT touch the parked thread's frames — the fold
    //    reaches us at all only because the registry entry shares our
    //    `Arc<GcBlockState>` / `root_snapshot` (see the `set_gc_block_state`
    //    call in `spawn_thread`'s `is_virtual` arm).
    //  * The ONLY code that drains and applies that accumulation to frame
    //    locals, operand stacks, `monitor_on_exit`, the thread mirror, JNI
    //    locals and the `slot_origins` write-back is
    //    `check_post_block_gc_refs`. `safepoint_check` does not — its own
    //    comment names "nothing ever applies its accumulated blocked-fixup" as
    //    the violation it *detects*.
    //
    // So a virtual thread parked across a young copying cycle resumed on
    // vacated addresses. It is reachable on the DEFAULT build: virtual threads
    // never enter JIT (`interpreter.rs` forces `env_disable_jit` for
    // `ThreadKind::Virtual`), so a virtual-thread workload publishes no
    // conservative JIT roots, `has_conservative_roots` is false,
    // `divert_non_moving` is false (`gc/src/gen_heap.rs`) and the moving Cheney
    // young cycle runs — in exactly the workload where continuations park.
    //
    // `check_post_block_gc` also replaces the raw `store(false)` with
    // `leave_blocked_region_flagged`, which clears the flag under the same
    // barrier-lock hold that confirmed no pause is active, closing the
    // excluded-but-running-mutator window the raw store left open.
    //
    // Drain exactly once: `check_post_block_gc_refs` `mem::take`s both `fixup`
    // and `slot_origins`, and once the flag is down no further fold can target
    // us, so a second call is a no-op.
    if resumed {
        // Only the resume path can have accumulated a fixup — `resumed` is
        // `vt.execution_started`, so `false` means this box came straight from
        // `install_runtime` with a virgin `GcBlockState` (flag down, fixup
        // empty). Calling the drain there would be worse than useless: the
        // first-mount arm below runs its own `arrive_and_wait_excluded`
        // handshake, and `leave_blocked_region_flagged` would classify an
        // unexcluded thread as participating and arrive a SECOND time for the
        // same pause, inflating `arrived` and releasing `wait_for_all` while a
        // counted mutator still runs.
        NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        }
        .check_post_block_gc();
    } else {
        // CENSUS-RECONCILE (2026-07-31): the bare store survives ONLY because
        // this arm's `GcBlockState` is virgin, which makes it a no-op — the
        // reasoning was prose, so pin it. If the precondition ever breaks, the
        // store becomes a genuine finding-1(c) flag clear performed outside the
        // barrier lock (a pause requested in the window both excludes this
        // thread and lets it run), and the accumulated fixup is dropped
        // unapplied. Both are silent; `check_post_block_gc` (the `resumed` arm
        // above) is the correct handler and the assert names it.
        debug_assert!(
            !thread
                .gc_block_state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire),
            "first-mount continuation must have a virgin GcBlockState \
             (in_blocked_region already down); a raised flag here needs \
             check_post_block_gc, not a bare store",
        );
        debug_assert!(
            thread.gc_block_state.fixup.lock().is_empty(),
            "first-mount continuation must have a virgin GcBlockState \
             (empty fixup); a non-empty one is a blocked-window remap this \
             bare store would drop unapplied",
        );
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, std::sync::atomic::Ordering::Release);
    }
    thread
        .gc_block_state
        .java_state
        .store(0, std::sync::atomic::Ordering::Release);

    // The carrier's 8 MiB native stack sizes the `StackOverflowError` guard on
    // BOTH arms: a carrier whose first mount is a resumed continuation (a
    // compensating carrier, or any carrier after a migration) used to keep the
    // thread-local default, sized for the 128 MiB main thread, so deep
    // recursion there overflowed the native stack instead of throwing
    // (interpreter round i1 wave 9, lane L5).
    crate::runtime::interpreter::init_thread_exec_depth_ceiling(8 * 1024 * 1024);
    let result = if resumed {
        if shared
            .mem
            .gc_barrier
            .stw_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            crate::runtime::interpreter::safepoint_check(&shared, &mut thread);
        }
        // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
        // `VirtualThreadMount` (HotSpot's extension event). gci i12/ev (item
        // 1c): on the continuation's own `JvmThread` handle.
        #[cfg(feature = "experimental-debug")]
        if shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::VirtualThread)
        {
            crate::runtime::interpreter::post_on_own_thread(&mut thread, || {
                crate::jvmti::native_env::post_virtual_thread(
                    &shared,
                    tid.0,
                    crate::jvmti::native_env::VirtualThreadEvent::Mount,
                )
            });
        }
        crate::runtime::interpreter::resume_continuation(&shared, &mut thread)
    } else {
        loop {
            let ready = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
                shared.threads.thread_registry.mark_stw_ready(tid);
            });
            if ready {
                break;
            }
            let pointer_map = shared.mem.gc_barrier.arrive_and_wait_excluded(tid);
            if !pointer_map.is_empty() {
                crate::runtime::interpreter::apply_pointer_map_to_thread(
                    &mut thread,
                    &pointer_map,
                    &shared.mem.heap,
                    &shared.classes.type_maps,
                );
            }
        }
        let Some(thread_obj) = shared.threads.thread_registry.java_thread_obj(tid) else {
            // No Java ran on this mount: hand the carrier its own state back.
            thread.detached_thread_locals =
                cratonvm_native_builtins::phases_early::swap_current_thread_locals(
                    carrier_thread_locals,
                );
            shared.threads.virtual_thread_manager.terminate(vt_id);
            return;
        };
        thread.java_thread_obj = Some(thread_obj);
        let recv_cid = shared.mem.heap.class_id_of(thread_obj);
        let _ = super::ensure_class_initialized_shared(&shared, &mut thread, recv_cid);
        // gen r4w4/rooting2 (residual item 3): `ensure_class_initialized_shared`
        // can run `<clinit>`, so the local above may name a vacated address.
        // The registry entry is remapped after every collection
        // (`update_thread_objs_after_gc`), exactly as the platform-thread
        // spawn path re-reads it; fall back to the local only on a miss.
        let thread_obj = shared
            .threads
            .thread_registry
            .java_thread_obj(tid)
            .unwrap_or(thread_obj);
        thread.java_thread_obj = Some(thread_obj);
        // Wave 63: `VirtualThreadStart`, then `VirtualThreadMount`, at the
        // first mount (MEASURED order).
        #[cfg(feature = "experimental-debug")]
        let thread_obj = if shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::VirtualThread)
        {
            use crate::jvmti::native_env::{post_virtual_thread, VirtualThreadEvent};
            // gci i12/ev (item 1c): on the continuation's own handle.
            crate::runtime::interpreter::post_on_own_thread(&mut thread, || {
                post_virtual_thread(&shared, tid.0, VirtualThreadEvent::Start);
                post_virtual_thread(&shared, tid.0, VirtualThreadEvent::Mount);
            });
            // The callbacks may have collected.
            let current = shared
                .threads
                .thread_registry
                .java_thread_obj(tid)
                .unwrap_or(thread_obj);
            thread.java_thread_obj = Some(current);
            current
        } else {
            thread_obj
        };
        invoke_on_class_shared(
            &shared,
            &mut thread,
            recv_cid,
            "run",
            "()V",
            &[Value::Object(Some(thread_obj))],
        )
    };
    if let Err(MethodCallFailed::InternalError(VmError::ContinuationYield { wake_after_nanos })) =
        &result
    {
        // Interpreter round i2 wave 63, lane L1: `VirtualThreadUnmount` (a
        // park), while the thread is still a mounted, running mutator. gci
        // i12/ev (item 1c): on the continuation's own handle.
        #[cfg(feature = "experimental-debug")]
        if shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::VirtualThread)
        {
            crate::runtime::interpreter::post_on_own_thread(&mut thread, || {
                crate::jvmti::native_env::post_virtual_thread(
                    &shared,
                    tid.0,
                    crate::jvmti::native_env::VirtualThreadEvent::Unmount,
                )
            });
        }
        // gen r5w2/roots6: the yield has unwound every Java frame of this
        // continuation off the carrier's stack (virtual threads never run
        // compiled code), so from here its last carrier's tid names a stack it
        // is not on. Lowered BEFORE the deposit below raises
        // `in_blocked_region`, so no pause ever sees it blocked AND mounted
        // while the carrier runs the rest of the unmount. See
        // `ThreadEntry::mounted`.
        shared.threads.thread_registry.set_mounted(tid, false);
        // `Thread.getState()` of the unmounted continuation (interpreter
        // round i1 wave 23, lane L7; page
        // `interpreter-L0-a-parked-virtual-thread-reports-runnable`). Every
        // yield is a park, a sleep or a wait -- untimed when it carries no
        // wake-up (`LockSupport.park`, `CountDownLatch.await`), timed
        // otherwise (`Thread.sleep`, `parkNanos`) -- and the registry's
        // `java_block_state` answers `java_state` exactly while
        // `in_blocked_region` is up, which the deposit below raises for the
        // whole parked duration. A platform thread gets the same two values
        // from `begin_blocking_region` / `begin_timed_blocking_region`; the
        // yield path set neither, so a parked virtual thread answered
        // RUNNABLE (HotSpot: WAITING / TIMED_WAITING). Stored before the
        // deposit so the flag never shows up over a stale state; the remount
        // above resets it to 0, and an unpark resets it while the
        // continuation waits for a carrier (`VirtualThreadManager`).
        thread.gc_block_state.java_state.store(
            if *wake_after_nanos == 0 { 1 } else { 3 },
            std::sync::atomic::Ordering::Release,
        );
        // gci i11/vt finding 1: retire the TLAB while still a COUNTED
        // mutator, before the deposit raises `in_blocked_region` -- no blocked
        // thread keeps a TLAB (`begin_blocking_region_with_state`'s CRIT note,
        // `ThreadRegistry::collect_reserved_tlab_tails`). `suspend_runtime`
        // retired it only after the flag was up and after the arrive below,
        // racing a pause that reads the tail and resets the young arena under
        // it; its retire is now an idempotent no-op backstop. After the
        // `VirtualThreadUnmount` callbacks above, which may allocate.
        thread.tlab.retire();
        NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
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
            let blocked = shared.mem.gc_barrier.enter_blocked();
            if blocked.pre_stw {
                let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
            }
        }
        // Unmount: the continuation takes its `ThreadLocal` state with it, and
        // the carrier gets its own back before it can mount anything else.
        thread.detached_thread_locals =
            cratonvm_native_builtins::phases_early::swap_current_thread_locals(
                carrier_thread_locals,
            );
        shared.threads.virtual_thread_manager.suspend_runtime(
            vt_id,
            thread,
            std::time::Duration::from_nanos(*wake_after_nanos),
        );
        return;
    }

    if let Err(error) = &result {
        if let MethodCallFailed::ExceptionThrown(exception) = error {
            // W1-C (real-JDK, 2026-07-27): same handler resolution as the
            // platform-thread death path in `thread_start` — side table -> real
            // `Thread.uncaughtExceptionHandler` field -> side-table default,
            // then `dispatchUncaughtException` only when nothing was found.
            // Mirroring the real field alone would NOT have fixed this path:
            // real `getUncaughtExceptionHandler()` gates on `isTerminated()`,
            // which resolves through the `holder.threadStatus` this VM never
            // advances. The helper owns the pin/re-read/truncate discipline
            // (including the `try_lambda_dispatch` routing a lambda handler
            // needs), so this site no longer pins the Throwable itself.
            if let Some(thread_obj) = shared.threads.thread_registry.java_thread_obj(tid) {
                let receiver_class = shared.mem.heap.class_id_of(thread_obj);
                let _ = dispatch_uncaught_exception_shared(
                    &shared,
                    &mut thread,
                    thread_obj,
                    *exception,
                    receiver_class,
                );
            }
        } else {
            eprintln!("Virtual thread {} terminated with error: {:?}", tid, error);
        }
    }
    // Java-side thread teardown, mirroring the platform-thread death path in
    // `thread_start` — same helper, same position (after the uncaught-handler
    // dispatch, before the JFR thread-end event and every teardown step that
    // follows), and likewise outside the `if let Err(error)` block so it covers
    // normal and abnormal termination alike. Re-read the `Thread` object from
    // the registry, which is remapped after every GC; if it has no entry there
    // is no receiver to clean up and the JFR/monitor sequence below already
    // handles that case by producing no `term_monitor`.
    // Interpreter round i2 wave 63, lane L1: `VirtualThreadUnmount`, then
    // `VirtualThreadEnd` (MEASURED order), before the thread's exit.
    #[cfg(feature = "experimental-debug")]
    if shared
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::VirtualThread)
    {
        use crate::jvmti::native_env::{post_virtual_thread, VirtualThreadEvent};
        // gci i12/ev (item 1c): on the continuation's own handle.
        crate::runtime::interpreter::post_on_own_thread(&mut thread, || {
            post_virtual_thread(&shared, tid.0, VirtualThreadEvent::Unmount);
            post_virtual_thread(&shared, tid.0, VirtualThreadEvent::End);
        });
    }
    // gci i12/ev (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): this virtual
    // thread's last own-handle point: its deferred `ClassLoad`s.
    #[cfg(feature = "experimental-debug")]
    if shared
        .debug
        .deferred_class_loads_pending
        .load(std::sync::atomic::Ordering::Acquire)
    {
        crate::runtime::interpreter::post_on_own_thread(&mut thread, || {
            crate::jvmti::native_env::flush_deferred_class_loads(&shared, tid.0)
        });
    }
    if let Some(exit_thread_obj) = shared.threads.thread_registry.java_thread_obj(tid) {
        run_thread_exit_shared(&shared, &mut thread, exit_thread_obj);
    }
    // `Thread.exit()` has run; no Java of this virtual thread runs again.
    // Release the JNI global roots behind its `ThreadLocal` values -- the
    // platform death path's step, which a virtual thread could not take while
    // its values lived in the carrier's shared map -- while its OWN map is
    // still the one installed, then give the carrier its state back. Before
    // `mark_dead()`: `current_thread_object` needs the registry entry.
    cratonvm_native_builtins::phases_early::release_current_thread_locals(&mut NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    });
    let _released =
        cratonvm_native_builtins::phases_early::swap_current_thread_locals(carrier_thread_locals);
    // An uncaught exception's debugger handle dies with the virtual thread.
    crate::runtime::interpreter::release_debugger_thread_state(&shared, &mut thread);
    // SBR5 lane jfr: free this virtual thread's JFR event buffer.
    cratonvm_native_builtins_jfr::jfr_vm_events::thread_buffer_released(&mut NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    });
    {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut recorder = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_thread_end_event_arc(
            &mut recorder,
            std::sync::Arc::from(thread.name.as_str()),
            tid.0,
            now_ns,
        );
    }
    shared.mem.heap.flush_thread_satb();

    let wake_obj = shared.threads.thread_registry.java_thread_obj(tid);
    let term_monitor = wake_obj.and_then(|thread_obj| {
        match shared
            .threads
            .monitors
            .enter_inflated_or_contend(thread_obj, tid)
        {
            Ok((monitor, contended)) => {
                if contended {
                    // Round 12 wave 1 (lane lock): the full
                    // `monitor_enter_blocking` protocol, as in the platform
                    // thread's termination (`thread_start`'s spawn closure):
                    // retire the TLAB before the deposit (a collection during
                    // the park may reset the arena it points into; the retire
                    // below would write its filler there) and wake through
                    // `check_post_block_gc` (apply the fixup of the
                    // collections slept through, and drop `in_blocked_region`
                    // before this thread writes to the heap again).
                    let mut ctx = NativeContextImpl {
                        shared: &shared,
                        thread: &mut thread,
                    };
                    ctx.thread.tlab.retire();
                    ctx.deposit_root_snapshot();
                    let blocked = shared.mem.gc_barrier.enter_blocked();
                    if blocked.pre_stw {
                        let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
                    }
                    monitor.block_enter(tid);
                    drop(blocked);
                    ctx.check_post_block_gc();
                }
                Some(monitor)
            }
            Err(_) => None,
        }
    });

    // SBR round 2 (lane `thread`): the platform death path's `ensure_join`
    // step (`thread_start`'s spawn closure): TERMINATED / `eetop = 0` while this
    // thread still owns its `Thread` object's monitor and is still a scanned
    // mutator, before the `notify_all` below.
    if let Some(mirror) = shared.threads.thread_registry.java_thread_obj(tid) {
        set_thread_state_fields(&shared, mirror, Some(0), Some(THREAD_STATUS_TERMINATED));
    }
    // `java_state` 4 = "terminating": the deposit below must not map a stale
    // WAITING/BLOCKED value over the TERMINATED word just written.
    thread
        .gc_block_state
        .java_state
        .store(JAVA_STATE_TERMINATING, std::sync::atomic::Ordering::Release);

    thread.tlab.retire();
    NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    }
    .deposit_root_snapshot();
    let blocked = shared.mem.gc_barrier.enter_blocked();
    if blocked.pre_stw {
        let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
    }
    blocked.finish_after(|| {
        shared.threads.thread_registry.clear_tlab_addr(tid);
        shared.threads.thread_registry.set_jvm_thread_addr(tid, 0);
        shared.threads.thread_registry.mark_dead(tid);
    });
    // After `mark_dead`, outside the barrier's transition lock (round 12
    // wave 1): see the platform thread's termination in `thread_start`.
    shared
        .threads
        .monitors
        .release_monitors_held_by_except(tid, term_monitor.as_ref());
    if let Some(monitor) = term_monitor {
        // Wave 89 (lane L6): unmounted virtual threads in `join()` too.
        shared.threads.monitors.vthread_notify_all_waiters_of(&monitor);
        let _ = monitor.notify_all(tid);
        let _ = monitor.exit(tid);
        shared
            .threads
            .monitors
            .vthread_hand_off_after_release(std::sync::Arc::as_ptr(&monitor) as usize);
    }
    shared.threads.virtual_thread_manager.terminate(vt_id);
}

/// What a GC-blocked region's entry read of the heap (interpreter round i2
/// wave 68, lane L6): the wake keeps the entry's root snapshot only while
/// all three still read so (`NativeContextImpl::check_post_block_gc_keeping`).
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnfoldedSince {
    /// `GcBarrier::gc_generation`: every completed pause moves it.
    pub(crate) gc_generation: u64,
    /// The heap's collection count.
    pub(crate) collections: u64,
    /// `class_redefinition_count`: a redefinition may convert frames.
    pub(crate) redefinitions: u64,
}

/// Run `f` on a registered VM mutator thread after ensuring the calling thread
/// is settled in a GC-blocked safepoint region, so joining a helper GC thread
/// cannot deadlock Stop-The-World pauses. Returns `None` if the OS thread is not
/// currently claimed by `shared` or has no published `JvmThread`.
pub(crate) fn with_mutator_pressure_gc_safepoint<R>(
    shared: &SharedVm,
    f: impl FnOnce() -> R,
) -> Option<R> {
    let registry = &shared.threads.thread_registry;
    let tid = registry.thread_id_for_current_os_tid()?;
    if registry.is_tid_in_blocked_region(tid) {
        return Some(f());
    }
    if crate::native::jni::installed_jni_thread_addr().is_some() {
        return None;
    }
    let addr = registry.own_jvm_thread_addr(tid)?;
    let thread_ptr = addr as *mut JvmThread;
    // SAFETY: `tid` is the alive claimant on this OS thread, its published
    // `jvm_thread_addr` points to the active `JvmThread` running this call,
    // and we cross-check `thread_id` before borrowing it to enter/leave the
    // blocking region.
    if unsafe { (*thread_ptr).thread_id } != tid {
        return None;
    }
    struct BlockingRegionReset<'a> {
        shared: &'a SharedVm,
        thread_ptr: *mut JvmThread,
    }
    impl Drop for BlockingRegionReset<'_> {
        fn drop(&mut self) {
            use cratonvm_native_api::NativeThreadAccess;
            // SAFETY: matches entry below on the same calling thread.
            let thread = unsafe { &mut *self.thread_ptr };
            NativeContextImpl {
                shared: self.shared,
                thread,
            }
            .end_blocking_region();
        }
    }
    {
        use cratonvm_native_api::NativeThreadAccess;
        let thread = unsafe { &mut *thread_ptr };
        NativeContextImpl { shared, thread }
            .begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    let _reset = BlockingRegionReset { shared, thread_ptr };
    Some(f())
}

pub struct NativeContextImpl<'a> {
    pub shared: &'a SharedVm,
    pub thread: &'a mut JvmThread,
}

impl NativeContextImpl<'_> {
    /// Charge a native-side allocation that did NOT come out of the calling
    /// thread's TLAB to the two allocation counters.
    ///
    /// # Why this exists
    ///
    /// `getThreadAllocatedBytes` / `getTotalThreadAllocatedBytes` are fed from
    /// exactly two places (see `cratonvm_gc::tlab`): the TLAB cursor, and
    /// `note_external_allocation` for everything that bypassed the TLAB. The
    /// interpreter's own slow paths (`alloc_object_shared`, `gc_alloc_array`)
    /// have always called the second. The NATIVE allocation funnels did not,
    /// on any of their non-TLAB arms -- and every collection, string, box and
    /// reflective object in this VM is built through them.
    ///
    /// The hole was invisible while the counter had a larger over-count on top
    /// of it, and it is not small. MEASURED, `Integer.valueOf(100000 + i)` a
    /// million times: 24.0 bytes per object retained on the heap, and **0.0**
    /// reported by BOTH counters on Generational and ZGC, against a correct
    /// 24.0 on G1 -- not because G1 counted better, but because G1 is the one
    /// backend with neither a native old-gen batch pool nor a disabled-by-
    /// default TLAB refill, so its native allocations happened to land on the
    /// one arm that was already counted. On the Hibernate HQL parse this
    /// record is about, the difference between the two is 72 MB against 191 MB
    /// for byte-identical bytecode.
    ///
    /// An under-count is the dangerous direction: it is what makes a byte
    /// budget pass vacuously. `HqlParserMemoryUsageTest` asserts one.
    ///
    /// Sized from the ALLOCATED OBJECT's own header, not from the slot count
    /// the caller asked for. The two differ: `alloc_object` clamps a native
    /// caller's slot count up to the class's real field count, and the heap
    /// rounds the result, so charging the request over-reported a boxed
    /// `Integer` at 32 bytes where its header says 24. Reading the header back
    /// is one load and cannot drift from what was actually laid down.
    #[inline]
    pub(in crate::vm::vm_exec) fn note_native_object_alloc(&mut self, obj: ObjectRef) {
        let bytes = self.native_object_footprint(obj);
        self.charge_external_allocation(bytes);
    }

    /// The one charge every `note_native_*_alloc` ends in: the calling
    /// thread's own total, and (in batches, see
    /// `cratonvm_gc::tlab::EXTERNAL_PUBLISH_BATCH_BYTES`) this VM's
    /// `thread_allocated_total`. Attaches the thread's buffer to that total
    /// first, so a thread that allocates only through natives, and never
    /// refills a TLAB, is still counted by its own VM and no other
    /// (gc-common w6-c, `common-f-process-global-allocation-state`).
    #[inline]
    pub(in crate::vm::vm_exec) fn charge_external_allocation(&mut self, bytes: usize) {
        let shared = self.shared;
        self.thread
            .tlab
            .ensure_vm_thread_allocation_total(&shared.mem.thread_allocated_total);
        self.thread.tlab.note_external_allocation(bytes);
    }

    /// Footprint of an allocated (non-array) object, read back from its header:
    /// the COMPACT body when the backend planned one (`GC_FLAG_COMPACT`), else
    /// the legacy `HEADER_SIZE + num_slots * SLOT_SIZE` --
    /// `cratonvm_gc::gc::object_total_size`, the rule the collectors walk with.
    ///
    /// gc-common w3-c: this charged the LEGACY size unconditionally, while
    /// Generational's and (since w3-c) G1's fallible entries plan the compact
    /// body for any class with a registered layout, so a native allocation of
    /// a reference-heavy class was over-charged up to ~2x -- the native twin of
    /// the interpreter's `common-f-shared-path-accounts-legacy-size` (fixed in
    /// w2-d by `shared_object_footprint`, whose clamp this repeats: a compact
    /// body is never larger than the legacy one, and a compact object whose
    /// layout cannot be found sizes as an "impossibly large" sentinel that
    /// must not reach a monitoring counter).
    #[inline]
    pub(in crate::vm::vm_exec) fn native_object_footprint(&self, obj: ObjectRef) -> usize {
        use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
        let header = self.shared.mem.heap.get_header(obj);
        let slots = header.num_slots() as usize;
        let legacy = HEADER_SIZE.saturating_add(slots.saturating_mul(SLOT_SIZE));
        let real = cratonvm_gc::gc::object_total_size(header);
        if (HEADER_SIZE..=legacy).contains(&real) {
            real
        } else {
            legacy
        }
    }

    /// [`Self::note_native_object_alloc`] for an array. The data area is sized
    /// by the same `array_data_size` the allocator used, so the charge is the
    /// footprint and not the element count.
    #[inline]
    pub(in crate::vm::vm_exec) fn note_native_array_alloc(&mut self, array: ObjectRef) {
        let bytes = self.native_array_footprint(array);
        self.charge_external_allocation(bytes);
    }

    /// Footprint of an allocated array, read back from the heap: header plus
    /// the `array_data_size` of its own length and element type. Shares the
    /// sizing rule with the allocator rather than restating it.
    ///
    /// Read straight off the header rather than through `array_length` /
    /// `element_type_of`. Those two are the defensive accessors: they run
    /// `is_object_address` first, which on ZGC is a registry lookup, and this
    /// runs on every native array allocation -- every collection resize in the
    /// process. The defence buys nothing here, because the only callers pass an
    /// array this method's own allocator returned microseconds earlier.
    #[inline]
    pub(in crate::vm::vm_exec) fn native_array_footprint(&self, array: ObjectRef) -> usize {
        use cratonvm_gc::heap::HEADER_SIZE;
        let header = self.shared.mem.heap.get_header(array);
        if header.kind() != ObjectKind::Array {
            // Not an array: charge nothing. Reachable only through the String
            // helper, whose field 0 is an array on every real layout but is a
            // duck-typed read -- and inventing a header's worth of allocation
            // for a String that turned out not to have a backing array would be
            // a bias in the over-reporting direction, which is the one this
            // whole record exists to remove.
            return 0;
        }
        let len = header.array_length() as usize;
        let data = cratonvm_types::array_data_size(len, header.element_type()).unwrap_or(0);
        HEADER_SIZE.saturating_add(data)
    }

    /// [`Self::note_native_object_alloc`] for a freshly built `java/lang/String`,
    /// charging BOTH the String object and its backing array.
    ///
    /// Sized by reading the object back rather than from the source text: the
    /// String's slot count and its backing array's element type (LATIN1 `byte[]`
    /// against UTF16, or a legacy `char[]`) are layout decisions made inside
    /// `alloc_java_string_object`, and re-deriving them here would be a second
    /// copy of that logic to keep in step.
    ///
    /// The caller decides WHETHER to call this, because the pooled constructor
    /// returns an existing object on an intern hit and that allocates nothing.
    pub(in crate::vm::vm_exec) fn note_native_string_alloc(&mut self, s: ObjectRef) {
        self.note_native_string_alloc_minus_tlab(s, 0);
    }

    pub(in crate::vm::vm_exec) fn note_native_string_alloc_minus_tlab(&mut self, s: ObjectRef, accounted: u64) {
        // The String object's own footprint as laid down (compact or legacy),
        // see `native_object_footprint`.
        let mut bytes = self.native_object_footprint(s);
        if let Value::Object(Some(backing)) = self.shared.mem.heap.get_field(s, 0) {
            // `native_array_footprint` screens the kind off the header itself.
            bytes = bytes.saturating_add(self.native_array_footprint(backing));
        }
        let accounted = usize::try_from(accounted).unwrap_or(usize::MAX);
        self.charge_external_allocation(bytes.saturating_sub(accounted));
    }

    /// Terminal object allocation for [`NativeContext::alloc_object`], with the
    /// heap-exhaustion unwind applied (see `crate::runtime::native_oom`).
    ///
    /// `try_alloc_object_full` walks the identical no-GC young -> old-gen spill
    /// path as the panicking `alloc_object`, so every allocation that fits is
    /// unchanged; the only divergence is that true double-exhaustion becomes a
    /// catchable `java.lang.OutOfMemoryError` rather than `abort()`. Outside a
    /// native callback (JIT helpers construct this context too, and a panic
    /// cannot cross a compiled frame) the historical abort is retained.
    #[inline]
    pub(in crate::vm::vm_exec) fn heap_alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        if crate::runtime::native_oom::unwind_ok() {
            match self
                .shared
                .mem
                .heap
                .try_alloc_object_full(class_id, num_fields)
            {
                Some(o) => return o,
                None => crate::runtime::native_oom::raise(num_fields, "object"),
            }
        }
        self.shared.mem.heap.alloc_object(class_id, num_fields)
    }

    /// The layout half of every native object allocation: the layout-alias
    /// census, the `ClassId::new(0)` substitution and the clamp of the
    /// requested slot count up to the class's real field count.
    ///
    /// Shared by the two object doors, `NativeContext::alloc_object` and
    /// `NativeContext::try_alloc_object` (gc-common w8-b), so they cannot
    /// disagree about what a native's `num_fields` means or about what the
    /// census sees. `native-api/tests/layout_alias_coverage.rs` reads THIS
    /// body for the census links, and checks that both doors call it before
    /// they allocate anything.
    ///
    /// Allocates nothing. `anon_cache_hit` reports the lock-free
    /// `AnonymousObject$N` fast path, on which the clamp is provably a no-op
    /// and was skipped.
    pub(in crate::vm::vm_exec) fn native_alloc_layout(&self, class_id: ClassId, num_fields: usize) -> NativeAllocLayout {
        // Defense-in-depth: a native caller must never allocate an object
        // with `ClassId::new(0)` (`java/lang/Object`, which declares zero
        // instance fields) yet a non-zero slot count. Such an object has an
        // "undersized layout" — its class says it has 0 fields but the
        // header reserves `num_fields` slots — and the GC's `get_field`
        // bounds guard then rejects (drops) every `getfield` on it, which is
        // exactly the `class_name=java/lang/Object class_id=ClassId(0)
        // real_field_count=Some(0)` failure WildFly's controller boot hit.
        //
        // Several native allocators still pass `ClassId::new(0)` on their
        // class-resolution-failed fallback path. Rather than let a broken
        // object reach the heap, substitute a synthetic class that declares
        // `num_fields` instance fields so the header's `class_id` agrees
        // with its slot count. Field-less Object allocations (`new Object()`
        // and array-element class hints) are unaffected.
        let class_id = if class_id == ClassId::new(0) && num_fields > 0 {
            // LAYOUT-ALIAS CENSUS, THE SHORT-OBJECT HALF (W7-73, 2026-08-12).
            // This observation must stay ABOVE the substitution below and above
            // the `anon_class_cache` early `return`, and both halves of that are
            // load-bearing.
            //
            // The substitution replaces `ClassId::new(0)` with
            // `cratonvm/synthetic/AnonymousObject$N`, which declares exactly
            // `N`. So every one of these allocations arrives at the census
            // downstream as `classify(n, n)` — perfect agreement — and prints
            // nothing in either direction. The caller, meanwhile, resolved a
            // class and FAILED, then handed the object out as an instance of
            // the class it named: 30 production sites in the native crates
            // spell `alloc_object(ClassId::new(0), N)`, and 16 of them name a
            // class whose real JDK 25 layout is WIDER than `N` — `ZipEntry` 6
            // against 14, `Pattern` 2 against 20, `ServiceLoader` 2 against 10,
            // and `java/lang/Thread` 5 against 19 at two sites that are not
            // even fallback arms. Those are the genuinely short objects, and
            // both directions of the census were structurally blind to all of
            // them: `under` cannot fire because the clamp runs first, and this
            // path cannot fire because the substitution makes the widths agree.
            //
            // Reported as `declared = 0` under `layout_alias::UNRESOLVED_CLASS`
            // rather than as the substitute's name, because the honest statement
            // is "no declared layout was available here", not "an
            // AnonymousObject$N was allocated at its exact width".
            //
            // OBSERVATION ONLY, and no `else`: the substitution, the cache, the
            // clamp and the returned object are byte-for-byte what they were.
            // With the flag off the cost is one `OnceLock` load and one
            // predictable branch — the same price the base observation below
            // pays, and paid only by allocations that already took this
            // untyped-allocation branch.
            if cratonvm_native_api::layout_alias::enabled() {
                let frames: Vec<String> = self
                    .thread
                    .frames
                    .iter()
                    .rev()
                    .take(4)
                    .map(|f| {
                        format!(
                            "{}.{}{}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor()
                        )
                    })
                    .collect();
                let site = if frames.is_empty() {
                    "<no java frame>".to_string()
                } else {
                    frames.join(" <- ")
                };
                let _ = cratonvm_native_api::layout_alias::observe(
                    cratonvm_native_api::layout_alias::UNRESOLVED_CLASS,
                    num_fields,
                    0,
                    cratonvm_native_api::layout_alias::AllocSite::Java(site.as_str()),
                );
            }
            // One synthetic class per distinct field count, shared across
            // all callers — keeps the class store from growing unbounded.
            //
            // Hot path (every HashMap/LinkedHashMap node, view backing, …):
            // the resolved `AnonymousObject$N` ClassId is cached lock-free in
            // `shared.classes.anon_class_cache`, so repeat allocations skip the name
            // `format!`, the `class_manager` write-lock, and the synthetic-class
            // hash probe. The stub declares exactly `num_fields` fields, so the
            // slot-count clamp below is provably a no-op and is skipped too —
            // we allocate directly. Env verdict is read once (OnceLock); when
            // `CRATONVM_DBG_ANONALLOC` is set we force the slow path so the
            // per-allocation stack dump still fires every time.
            let dbg = {
                static DBG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                *DBG.get_or_init(|| {
                    false
                })
            };
            if !dbg && num_fields < crate::vm::ANON_CLASS_CACHE_LEN {
                let cached = self.shared.classes.anon_class_cache[num_fields]
                    .load(std::sync::atomic::Ordering::Relaxed);
                if cached != 0 {
                    // The early exit carries the decision out, not an
                    // object: the caller allocates. `alloc_object` keeps its
                    // historical direct heap entry for this arm; see
                    // `NativeAllocLayout::anon_cache_hit`.
                    return NativeAllocLayout {
                        class_id: ClassId::new(cached),
                        slots: num_fields,
                        anon_cache_hit: true,
                    };
                }
            }
            let name = format!("cratonvm/synthetic/AnonymousObject${num_fields}");
            if dbg {
                let stack: Vec<String> = self
                    .thread
                    .frames
                    .iter()
                    .rev()
                    .take(12)
                    .map(|f| {
                        format!(
                            "    at {}.{}{}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor()
                        )
                    })
                    .collect();
                eprintln!(
                    "[ANONALLOC] alloc_object ClassId(0) num_fields={num_fields} -> {name}\n{}",
                    stack.join("\n")
                );
            }
            // `ensure_generated_class`, not `ensure_synthetic_class`: this is a
            // VM bookkeeping type, not a compatibility substitution. There is
            // no `cratonvm/synthetic/AnonymousObject$N` class file anywhere and
            // there never will be — it is the untyped allocation shape behind
            // every `HashMap`/`LinkedHashMap` node and friends, and contract §1
            // item 6 makes it legitimate in **both** modes.
            //
            // Stamping it `CompatibilityStub` (which `ensure_synthetic_class`
            // hard-codes) made contract §11's zero-stub acceptance criterion
            // unachievable by construction: a strict boot reported a
            // `compatibility-class-requested` violation for a class the
            // contract explicitly permits, so the census could never reach
            // zero however much real work was done. It also cost a
            // full-classpath rescan per fresh field count, looking for a class
            // file that cannot exist (the `is_synthetic` branch in
            // `fabricate_class`; `synthetic_upgrade_known_absent` memoised it
            // away after the first, but the first still ran).
            //
            // The deferral this replaces was correct at the time: the origin
            // feeds the derived `is_synthetic_stub` bool, and flipping it for a
            // class ~181 read sites reason about is a `Compatible`-mode
            // behaviour change wave 1 could not validate. What makes it safe
            // *here* is that this class is inert at every one of those sites —
            // nothing is registered as a native on it, neither
            // real-protected-stub allow-list names it, and `fabricate_class`'s
            // `Proxy$Instance` / collection-iterator special cases key on the
            // name, not the origin.
            let cid = self
                .shared
                .classes
                .class_manager_write_settling()
                .ensure_generated_class(
                    &name,
                    num_fields,
                    cratonvm_classloading::class_origin::ClassOrigin::VmInternal,
                );
            // Cache for the lock-free fast path above. Races are benign:
            // `ensure_generated_class` is idempotent, so any racing thread
            // stores the same id.
            if num_fields < crate::vm::ANON_CLASS_CACHE_LEN {
                self.shared.classes.anon_class_cache[num_fields]
                    .store(cid.as_u32(), std::sync::atomic::Ordering::Relaxed);
            }
            cid
        } else {
            class_id
        };
        // Layout-mismatch guard: many native allocators hard-code a
        // synthetic field count (e.g. `HashSet` => 1) that is SMALLER
        // than the real JDK class layout. When real-JDK bytecode later
        // executes `getfield`/`putfield` at the declared (inherited)
        // field indices, the access runs past the undersized object —
        // `gen_heap::get_field` then drops the read and the object's
        // state silently corrupts (observed as Kafka 3.7 boot failures:
        // `java/util/HashSet` allocated with num_slots=1 but
        // real_field_count=3).
        //
        // Clamp the requested slot count UP to the resolved class's real
        // declared instance-field count. This is the single, general fix
        // point: every native allocator routes through this trait method,
        // so individual call sites no longer need to remember to
        // `.max(class_num_total_fields(cid))` themselves. Synthetic
        // ClassIds not in the class manager report 0 here, so the
        // requested count is used unchanged for those.
        //
        // Hot path: this clamp runs on EVERY native allocation (notably each
        // out-of-cache autoboxed wrapper — ~19% of the 1M-put/get HashMap
        // probe sat in alloc_object, a large share of it in this RwLock
        // acquire + class-store lookup). A registered class's
        // `num_total_fields` is immutable for its ClassId except through
        // class redefinition, which bumps the global layout generation — the
        // same validation contract `gen_heap::compact_field_slot`'s cache
        // already relies on. Keep a tiny per-thread working set keyed by
        // (vm, class_id) and validated against that generation. Unregistered
        // ids (`get_class` → None) are deliberately NOT cached: a class id
        // observed mid-registration could otherwise pin a stale 0 clamp.
        struct TotalFieldsCache {
            // (vm_key, class_id, layout_generation, num_total_fields);
            // vm_key == 0 marks an empty slot.
            entries: [(usize, u32, u64, u32); 8],
            next: usize,
        }
        thread_local! {
            static TOTAL_FIELDS_CACHE: std::cell::RefCell<TotalFieldsCache> =
                const {
                    std::cell::RefCell::new(TotalFieldsCache {
                        entries: [(0, 0, 0, 0); 8],
                        next: 0,
                    })
                };
        }
        let vm_key = self.shared.vm_identity;
        let cid_u32 = class_id.as_u32();
        let layout_gen = cratonvm_types::layout_generation();
        let cached_fields = TOTAL_FIELDS_CACHE.with(|cell| {
            let cache = cell.borrow();
            cache.entries.iter().find_map(|&(vk, cid, gen, fields)| {
                (vk == vm_key && cid == cid_u32 && gen == layout_gen).then_some(fields)
            })
        });
        let real_fields = match cached_fields {
            Some(fields) => fields as usize,
            None => {
                let resolved = self
                    .shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.num_total_fields);
                if let Some(fields) = resolved {
                    // Cast: field counts are far below u32::MAX.
                    let fields_u32 = fields as u32;
                    TOTAL_FIELDS_CACHE.with(|cell| {
                        let mut cache = cell.borrow_mut();
                        let slot = cache.next;
                        cache.entries[slot] = (vm_key, cid_u32, layout_gen, fields_u32);
                        cache.next = (slot + 1) % cache.entries.len();
                    });
                }
                resolved.unwrap_or(0)
            }
        };
        // LAYOUT-ALIAS CENSUS (W7-59, 2026-08-12). This is the terminal every
        // native object allocation in every native crate reaches — the ~200
        // production `alloc_object` / `try_alloc_object_gc_safe` sites in
        // `native-builtins`, `native-io` and `native-collections` that bypass
        // `try_alloc_concurrent_synthetic`, and the funnel's own allocation too.
        // Until this line the detector sat on the funnel alone, so
        // `CRATONVM_DBG_LAYOUT_ALIAS=1` could not name a single direct site —
        // including `native-io`'s `AsynchronousSocketChannel`, allocated four
        // slots wide against a class declaring one, which is the widest LIVE
        // over-allocation W7-49-slot-index-recensus.md found.
        //
        // OBSERVATION ONLY. It cannot change what is allocated: `slots` below is
        // computed exactly as before, from exactly the same two integers, and
        // this block has no `else`. With the flag off (the default, and every
        // Compatible-mode run that has not opted in) the cost is one `OnceLock`
        // load and one predictable branch — no class name is resolved, no frame
        // is formatted, no lock is taken. The comparison itself is free because
        // `real_fields` is already in hand for the clamp; nothing is looked up
        // for the census's sake.
        //
        // `#[track_caller]` was considered and rejected here: it would add a
        // hidden argument to every native allocation plus a reify shim on the
        // `dyn NativeContext` vtable, paid in every run, to serve a flag that is
        // off in almost all of them. The Java frames are already a `Vec` on this
        // thread and are formatted only when a row is about to print — and they
        // answer the better question, because a Rust source location says a site
        // EXISTS while a Java frame says the path RAN, which is the
        // LIVE-versus-dead distinction last-write-wins registration makes
        // unanswerable from source alone.
        if cratonvm_native_api::layout_alias::enabled()
            && cratonvm_native_api::layout_alias::classify(num_fields, real_fields).is_some()
        {
            // `get_class` answering `None` is not a missing name — it is the
            // reason `real_fields` is 0, and it is one of the three shapes
            // `direction=undeclared` covers (a stale id, or a class observed
            // mid-registration, which is exactly why the field-count cache above
            // refuses to memoise a zero). An empty `class` field would have
            // reported it as a nameless nothing; say which id it was, so the
            // reader can tell it from an interface allocated at N slots.
            let class_name = self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| format!("<unregistered:ClassId({})>", class_id.as_u32()));
            let frames: Vec<String> = self
                .thread
                .frames
                .iter()
                .rev()
                .take(4)
                .map(|f| {
                    format!(
                        "{}.{}{}",
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor()
                    )
                })
                .collect();
            let site = if frames.is_empty() {
                "<no java frame>".to_string()
            } else {
                frames.join(" <- ")
            };
            let _ = cratonvm_native_api::layout_alias::observe(
                class_name.as_str(),
                num_fields,
                real_fields,
                cratonvm_native_api::layout_alias::AllocSite::Java(site.as_str()),
            );
        }
        let slots = num_fields.max(real_fields);
        NativeAllocLayout {
            class_id,
            slots,
            anon_cache_hit: false,
        }
    }

    /// The no-GC arms every native object allocation tries before the heap's
    /// own entry: the per-thread old-gen batch pool, the TLAB, and a fresh
    /// batch. `None` when none of them can serve `(class_id, slots)`; the
    /// caller then takes its terminal (`heap_alloc_object` for
    /// `alloc_object`, `try_alloc_object_full` for `try_alloc_object`).
    ///
    /// Charges the allocation counters exactly as before the split: the pool
    /// and batch arms charge the object they hand out, the TLAB arm charges
    /// nothing (the cursor's advance is already counted).
    pub(in crate::vm::vm_exec) fn native_alloc_no_gc_arms(&mut self, class_id: ClassId, slots: usize) -> Option<ObjectRef> {
        if self.thread.native_alloc_pool_layout == Some((class_id, slots)) {
            if let Some(obj) = self.thread.native_alloc_pool.pop() {
                if self.thread.native_alloc_pool.is_empty() {
                    self.thread.native_alloc_pool_layout = None;
                }
                // Charged as each object is HANDED OUT, not when the batch of
                // 2048 was carved: the batch is a lock-amortisation device, and
                // charging it up front would report 2048 objects' worth of
                // allocation to whichever caller happened to trip the refill.
                self.note_native_object_alloc(obj);
                return Some(obj);
            }
            self.thread.native_alloc_pool_layout = None;
        }
        // Native callbacks execute on the mutator's own `JvmThread`, so small
        // objects can use the same lock-free TLAB path as interpreted/JIT
        // `new`. Historically this method went straight to `GenHeap`, taking
        // the shared young-arena lock until young filled and then the old-gen
        // lock for every allocation. Autobox-heavy code (notably
        // HashMap<Integer, Integer>) therefore serialized millions of tiny
        // wrapper allocations through global locks despite an available TLAB.
        //
        // This path deliberately does not initiate GC from inside the native
        // callback: `tlab_alloc_object` only bumps/refills young space and
        // returns `None` when it cannot. The existing `heap.alloc_object`
        // fallback retains the previous spill/OOM behavior and native rooting
        // contract. Instead of collecting here, a TLAB failure flags
        // `young_spill_pressure` so the NEXT `safe_native_call` boundary —
        // where every argument is pinned and remappable — runs the
        // orchestrated GC this method cannot (see `safe_native_call_impl`).
        // The shape planner, not a bare legacy size -- see
        // `plan_tlab_object_shape`.
        let (requested_size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
            self.shared,
            class_id,
            slots,
            crate::runtime::interpreter::tlab_site::NATIVE,
        );
        if requested_size <= cratonvm_gc::tlab::tlab_max_alloc() {
            if let Some(obj) = crate::runtime::interpreter::tlab_alloc_object(
                self.thread,
                self.shared,
                class_id,
                slots,
                requested_size,
            ) {
                return Some(obj);
            }
            // Young could not supply another TLAB chunk: every allocation
            // below lands in old gen. The old-batch refill / heap spill arms
            // below signal the native-call boundary GC (`note_young_spill_
            // pressure` — advisability-gated, needs the old-gen lock those
            // slow paths already pay for; calling it here would add an
            // old-gen lock acquisition per allocation in spill mode).
        }
        // Once young space cannot provide another TLAB, amortize the
        // non-moving old-generation lock and free-list work across a chunk of
        // same-layout native objects. Unused entries remain rooted and are
        // remapped with their owning thread; the object popped here cannot be
        // moved before `safe_native_call` publishes its return root because a
        // native callback never initiates collection on this path.
        if self.thread.native_alloc_pool.is_empty() {
            let mut batch = self
                .shared
                .mem
                .heap
                .try_alloc_objects_old_batch(class_id, slots, 2048);
            if let Some(obj) = batch.pop() {
                self.thread.native_alloc_pool = batch;
                self.thread.native_alloc_pool_layout = if self.thread.native_alloc_pool.is_empty() {
                    None
                } else {
                    Some((class_id, slots))
                };
                // Only the object handed out here; the rest are charged as the
                // pool arm above pops them.
                self.note_native_object_alloc(obj);
                return Some(obj);
            }
        }
        None
    }

    /// `--nojit` only: an array allocation that leaves the heap wanting a
    /// collection asks the next native-call boundary for one
    /// (`native_array_gc_requested`). Shared by `new_array` and
    /// `new_ref_array`, on both their arms.
    #[inline]
    pub(in crate::vm::vm_exec) fn native_array_gc_request_after_alloc(&self) {
        if crate::runtime::env_cache::disable_jit() && self.shared.mem.heap.needs_gc() {
            self.shared
                .mem
                .native_array_gc_requested
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The TLAB arm of the four native array doors (`new_array`,
    /// `try_new_array`, `new_ref_array`, `try_new_ref_array`) under
    /// `CRATONVM_NATIVE_ARRAY_TLAB` (default OFF; round 16 wave 12, lane
    /// nativearr, proposal BN2-1). `None` -- the switch off, or anything the
    /// bump does not serve -- leaves the door's historical road, byte for
    /// byte. See [`Self::native_array_tlab_arm_at`].
    #[inline]
    pub(in crate::vm::vm_exec) fn native_array_tlab_arm(
        &mut self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let mode = crate::runtime::env_cache::native_array_tlab_mode();
        if mode == crate::runtime::env_cache::NATIVE_ARRAY_TLAB_OFF {
            return None;
        }
        self.native_array_tlab_arm_at(mode, class_id, element_type, length)
    }

    /// [`Self::native_array_tlab_arm`] for an explicit mode
    /// (`env_cache::NATIVE_ARRAY_TLAB_*`), so the tests pin each arm without
    /// the process environment.
    ///
    /// Why: every array a native allocated went `try_alloc_array_full` ->
    /// `try_alloc_young_initialized`, which takes the young arena's mutex per
    /// call, while the object door beside it (`native_alloc_no_gc_arms`) bumps
    /// the thread's TLAB. Each `BigInteger` a `java.math` native returns
    /// carries one such `int[]` (`compare-wide`: two per op).
    ///
    /// Never collects, in either mode: a native callback cannot (its caller's
    /// locals are not all pinned). `HIT` is a pure bump of the current buffer;
    /// `REFILL` also refills, without collecting, like the object door. The
    /// bytes are the cursor's advance, so the TLAB arm is NOT charged through
    /// `note_native_array_alloc` (that would count them twice).
    pub(in crate::vm::vm_exec) fn native_array_tlab_arm_at(
        &mut self,
        mode: u8,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        let refill = match mode {
            crate::runtime::env_cache::NATIVE_ARRAY_TLAB_HIT => false,
            crate::runtime::env_cache::NATIVE_ARRAY_TLAB_REFILL => true,
            _ => return None,
        };
        crate::runtime::interpreter::tlab_alloc_array_native(
            self.thread,
            self.shared,
            class_id,
            element_type,
            length,
            refill,
        )
    }
}

/// What `NativeContextImpl::native_alloc_layout` decided for one native
/// object allocation.
#[derive(Clone, Copy, Debug)]
pub(in crate::vm::vm_exec) struct NativeAllocLayout {
    /// The class to allocate: the caller's, or the `AnonymousObject$N`
    /// substitute for a `ClassId::new(0)` request with slots.
    pub(in crate::vm::vm_exec) class_id: ClassId,
    /// The slot count after the clamp to the class's real field count.
    pub(in crate::vm::vm_exec) slots: usize,
    /// The lock-free `AnonymousObject$N` cache answered. `alloc_object` has
    /// always sent this arm straight to the heap's entry (no pool, no TLAB)
    /// and keeps doing so byte-for-byte; `try_alloc_object` treats it like
    /// any other layout.
    pub(in crate::vm::vm_exec) anon_cache_hit: bool,
}




/// `class: message` for a Throwable, read out of the heap **without invoking
/// any Java code and without allocating a single Java object**.
///
/// Every caller is on a thread-death or double-fault path, and at least one of
/// them runs on a heap that has just refused an allocation. `toString()` would
/// need a `StringBuilder`, a `char[]` and a `String` to answer, so on exactly
/// the failure this exists to describe it would fail again — and a second
/// failure inside the reporter is what turns a legible error into a silent
/// one. So: class name from the class manager, `detailMessage` read straight
/// out of its field slot.
///
/// Mirrors the field walk in `runtime::exceptions::set_detail_message_by_name`
/// (the write side), including its `_fN` opaque-bootstrap-metadata fallback,
/// so the two cannot disagree about which slot holds the message.
pub(crate) fn describe_throwable(shared: &SharedVm, exc: ObjectRef) -> String {
    if shared
        .mem
        .heap
        .is_object_address(exc.as_ptr() as usize)
        .is_none()
    {
        return format!("<not a live object: {:p}>", exc.as_ptr());
    }
    let class_id = shared.mem.heap.class_id_of(exc);
    let (name, message_slot) = {
        let cm = shared.classes.class_manager.read();
        let name = cm
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<class_id={}>", class_id.as_u32()));
        let mut slot = None;
        let mut opaque = None;
        let mut walk = Some(class_id);
        while let Some(cid) = walk {
            let Some(cls) = cm.get_class(cid) else { break };
            if &*cls.name == "java/lang/Throwable"
                && cls.fields.len() >= 2
                && cls.fields.iter().take(2).all(|f| f.name.starts_with("_f"))
            {
                opaque = Some(cls.first_field_index + 1);
            }
            let mut inst = 0usize;
            for f in &cls.fields {
                if f.is_static() {
                    continue;
                }
                if &*f.name == "detailMessage" {
                    slot = Some(cls.first_field_index + inst);
                    break;
                }
                inst += 1;
            }
            if slot.is_some() {
                break;
            }
            walk = cls.superclass;
        }
        (name, slot.or(opaque))
    };
    let message = message_slot
        .map(|idx| shared.mem.heap.get_field(exc, idx))
        .and_then(|v| match v {
            Value::Object(Some(s)) => crate::vm::vm_object::read_java_string(&shared.mem.heap, s),
            _ => None,
        })
        // An implicit NPE's JEP 358 text lives in `extendedMessage`
        // (interpreter round i4 wave 99, lane L9): rendered as before.
        .or_else(|| {
            crate::runtime::exceptions::npe_computed_extended_message(shared, exc)
                .and_then(|s| crate::vm::vm_object::read_java_string(&shared.mem.heap, s))
        });
    match message {
        Some(m) => format!("{name}: {m}"),
        None => name,
    }
}

/// If `exc` is exactly a `java/lang/ExceptionInInitializerError` (the wrap
/// `initialize_class` applies per JVMS §5.5 — see vm_util.rs), read its
/// `cause` field and return THAT object instead. `describe_throwable` on the
/// EIIE itself would report just the class name (its constructor sets
/// `cause`, not `detailMessage`), which is not the shape `define_class_full`
/// wants to hand its callers — see that call site for the double-wrap this
/// avoids (an `initialize:true` `defineHiddenClass` re-describing an
/// already-wrapped EIIE, which the native-builtins reconstruction then wraps
/// AGAIN). Stops after one unwrap: a `<clinit>` failure is wrapped exactly
/// once, never nested, so a second level would mean the cause field is doing
/// something other than naming the original exception and is not safe to
/// keep unwrapping.
pub(crate) fn unwrap_exception_in_initializer_error(
    shared: &SharedVm,
    exc: ObjectRef,
) -> ObjectRef {
    let class_id = shared.mem.heap.class_id_of(exc);
    let cm = shared.classes.class_manager.read();
    let is_eiie = cm
        .get_class(class_id)
        .map(|c| &*c.name == "java/lang/ExceptionInInitializerError")
        .unwrap_or(false);
    if !is_eiie {
        return exc;
    }
    let mut cause_idx = None;
    let mut walk = Some(class_id);
    while let Some(cid) = walk {
        let Some(cls) = cm.get_class(cid) else { break };
        let mut inst = 0usize;
        for f in &cls.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == "cause" {
                cause_idx = Some(cls.first_field_index + inst);
                break;
            }
            inst += 1;
        }
        if cause_idx.is_some() {
            break;
        }
        walk = cls.superclass;
    }
    drop(cm);
    match cause_idx.map(|idx| shared.mem.heap.get_field(exc, idx)) {
        Some(Value::Object(Some(cause))) if cause != exc => cause,
        _ => exc,
    }
}

/// W1-C (real-JDK, 2026-07-27): run the `UncaughtExceptionHandler` for a
/// thread whose `run()` escaped with `exc`, falling back to the Java
/// `Thread.dispatchUncaughtException(Throwable)` when no handler is found.
///
/// Shared by BOTH thread-death paths — the platform-thread one in
/// `thread_start` and the virtual-thread one in the continuation runtime —
/// which previously each open-coded the `dispatchUncaughtException` invoke and
/// so would have drifted apart under this fix.
///
/// # Why this does not just call `dispatchUncaughtException`
///
/// `Thread.setUncaughtExceptionHandler` can land in either of two stores
/// depending on which side wins dispatch on the path the user's call took: the
/// Rust side table in `native_builtins::uncaught_handlers` (native won) or the
/// real `Thread.uncaughtExceptionHandler` field (real bytecode won). The real
/// `dispatchUncaughtException` bytecode can only ever see the second — so a
/// handler that reached only the side table was silently dropped, which is why
/// `setUncaughtExceptionHandler` had no observable effect in the default
/// real-JDK mode. The setters now mirror into BOTH stores; reading both here
/// closes the loop for handlers installed before that mirroring could run.
///
/// It also matters that this bypasses real `getUncaughtExceptionHandler()`,
/// whose JDK 25 bytecode opens with an `isTerminated()` guard that resolves
/// through `holder.threadStatus` — a field this VM never advances past 0 (see
/// `native-builtins/src/lib.rs`'s note on `getState`). Mirroring the field
/// alone would therefore not have been enough on either path.
///
/// # Handler precedence
///
/// Per-thread handler (side table, then real field) beats everything, matching
/// the Thread spec, so invoking it directly is what HotSpot would do. Without
/// one, the fallback runs the genuine `Thread.dispatchUncaughtException` ->
/// `ThreadGroup.uncaughtException` -> parent groups ->
/// `Thread.getDefaultUncaughtExceptionHandler` chain, as HotSpot does. There is
/// deliberately no DEFAULT tier here: calling the default handler directly
/// skipped a `ThreadGroup` subclass that overrides `uncaughtException`
/// whenever the side table held a default (interpreter round i1 wave 23, lane
/// L7; `L7W23ThreadMisc.java` `ueh group`: HotSpot runs
/// the group's override, which then delegates to the default).
///
/// # GC
///
/// `exc` and `thread_obj` arrive as bare Rust locals with no interpreter frame
/// covering them: `run()`'s frame has already popped and the handler's frame
/// does not exist yet. Every call below is re-entrant (lazy class init /
/// allocation on the way into the handler body), so all three refs are pinned
/// in `native_pin_roots` for the duration and RE-READ from their pinned slots
/// immediately before each call — a copy taken earlier can be stale. One
/// `truncate` restores the caller's pin depth on every exit path.
pub(in crate::vm::vm_exec) fn dispatch_uncaught_exception_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    thread_obj: ObjectRef,
    exc: ObjectRef,
    fallback_class: ClassId,
) -> MethodCallResult {
    const EXC: usize = 0;
    const THREAD_OBJ: usize = 1;
    const HANDLER: usize = 2;
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    thread.native_pin_roots.push(thread_obj);

    let handler_thread = thread.native_pin_roots[pin_base + THREAD_OBJ];
    let dbg = None::<std::ffi::OsString>.is_some();
    let ueh = {
        let nctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        // The tiers are separate bindings rather than one `.or_else` chain only
        // so the gated trace below can report WHICH tier answered; the
        // short-circuit order is identical.
        let side = cratonvm_native_builtins::uncaught_handlers::take_uncaught_handler(
            &nctx,
            handler_thread,
        );
        let field = if side.is_none() {
            cratonvm_native_builtins::uncaught_handlers::real_thread_handler(&nctx, handler_thread)
        } else {
            None
        };
        if dbg {
            eprintln!(
                "[dbg-uncaught] resolve: thread_obj={:p} ihash={} side_table={} real_field={}",
                handler_thread.as_ptr(),
                nctx.identity_hash_code(handler_thread),
                side.is_some(),
                field.is_some(),
            );
        }
        side.or(field)
    };
    if dbg && ueh.is_none() {
        eprintln!(
            "[dbg-uncaught] no handler found; falling back to Thread.dispatchUncaughtException"
        );
    }

    let result = if let Some(handler) = ueh {
        thread.native_pin_roots.push(handler);
        let handler_ref = thread.native_pin_roots[pin_base + HANDLER];
        let handler_cid = shared.mem.heap.class_id_of(handler_ref);
        // A lambda handler (`(t, e) -> ...`, the common shape) has a synthetic
        // class_id that is not in the class store: resolving `uncaughtException`
        // by name would land on the abstract interface method. Route it through
        // `try_lambda_dispatch`, exactly as the proxy-invoke path does. Bind the
        // table lookup to its own statement so the `lambda_proxies` read guard
        // is definitely released before the re-entrant dispatch runs.
        let handler_is_lambda = shared
            .classes
            .lambda_proxies
            .read()
            .contains_key(&handler_cid);
        let lambda_result = if handler_is_lambda {
            let sam_args = [
                Value::Object(Some(thread.native_pin_roots[pin_base + THREAD_OBJ])),
                Value::Object(Some(thread.native_pin_roots[pin_base + EXC])),
            ];
            match crate::runtime::interpreter::try_lambda_dispatch(
                shared,
                thread,
                handler_ref,
                handler_cid,
                "uncaughtException",
                "(Ljava/lang/Thread;Ljava/lang/Throwable;)V",
                &sam_args,
            ) {
                Ok(Some(v)) => Some(Ok(v)),
                // Not a SAM match after all — fall through to name resolution.
                Ok(None) => None,
                Err(le) => Some(Err(le)),
            }
        } else {
            None
        };
        match lambda_result {
            Some(r) => r,
            None => {
                // Copy the pinned slots into locals first: the call takes
                // `&mut thread`, so the argument list cannot also borrow it.
                let handler_ref = thread.native_pin_roots[pin_base + HANDLER];
                let thread_now = thread.native_pin_roots[pin_base + THREAD_OBJ];
                let exc_now = thread.native_pin_roots[pin_base + EXC];
                invoke_on_class_shared(
                    shared,
                    thread,
                    handler_cid,
                    "uncaughtException",
                    "(Ljava/lang/Thread;Ljava/lang/Throwable;)V",
                    &[
                        Value::Object(Some(handler_ref)),
                        Value::Object(Some(thread_now)),
                        Value::Object(Some(exc_now)),
                    ],
                )
            }
        }
    } else {
        let thread_now = thread.native_pin_roots[pin_base + THREAD_OBJ];
        let exc_now = thread.native_pin_roots[pin_base + EXC];
        invoke_on_class_shared(
            shared,
            thread,
            fallback_class,
            "dispatchUncaughtException",
            "(Ljava/lang/Throwable;)V",
            &[
                Value::Object(Some(thread_now)),
                Value::Object(Some(exc_now)),
            ],
        )
    };
    thread.native_pin_roots.truncate(pin_base);
    result
}

/// Write the Java `Thread.interrupted` field (SBR wave 6, lane `thread`).
///
/// The retired `Thread.isInterrupted()` / `interrupted()` bytecode reads this
/// field, HotSpot writes it wherever it raises or consumes an interrupt, and
/// this VM keeps its own flag in `JvmThread::interrupted`, so every place that
/// touches the flag has to touch the field too. Allocates nothing; a receiver
/// without the field (a synthetic `Thread` stub) is left alone.
///
/// Interpreter round i3 wave 77, lane L6 (`CRATONVM_INTERRUPT_FIELD_MEMO`,
/// default off; page
/// `i76-L6-proposal-interrupt-status-field-through-the-memo-20261109`): the
/// slot comes from the per-VM thread-layout memo
/// ([`thread_interrupted_field_slot_of`]), so the clear of every
/// consumed interrupt -- `is_interrupted(true)`, the monitor-wait checks, the
/// interpreter's sleep-interrupt throw -- takes no class-manager lock once the
/// memo is warm. Off: by name under that lock, as before.
pub(crate) fn set_thread_interrupted_field(shared: &SharedVm, thread_obj: ObjectRef, value: bool) {
    let memo = shared.threads.thread_registry.interrupt_field_memo();
    let slot = thread_interrupted_field_slot_of(shared, thread_obj, memo);
    store_thread_interrupted_field(shared, thread_obj, slot, value);
}

