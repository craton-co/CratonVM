// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.management` natives: ManagementFactory and the MXBean surface.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// java.lang.management — ManagementFactory, MemoryMXBean, ThreadMXBean,
//                         RuntimeMXBean, OperatingSystemMXBean
// =============================================================================
//
// DISPATCH NOTE (measured, not assumed — read the call sites before editing):
// almost every triple registered below is registered AGAIN by
// `crate::jmx::register_jmx_natives`, and LAST REGISTRATION WINS. In
// `register_synthetic_overrides` (native-builtins/src/lib.rs) this module runs
// via `register_phase59_natives` and `register_jmx_natives` runs afterwards,
// so the `jmx.rs` version is the one that dispatches. In real-JDK mode this
// module is not registered at all (`register_synthetic_overrides` is skipped),
// while `register_jmx_natives` IS called from `vm/src/vm/vm_init.rs`.
// `management` is a DEFAULT feature, so both paths are the normal build.
//
// Consequence: fixing a value here alone changes nothing. The `jmx.rs`
// counterpart has to be fixed too — that is why `MEMORY_MX_VERBOSE` below is
// shared with `jmx.rs` rather than private to this module.

/// Global flag for MemoryMXBean.setVerbose / isVerbose.
///
/// Shared with `crate::jmx`, which registers the winning `isVerbose` for the
/// `MemoryMXBean` interface as well as the `sun.management.MemoryImpl`
/// `setVerboseGC`/`isVerbose` pair the real-JDK bytecode routes through. One
/// cell keeps every one of those surfaces reporting the same `-verbose:gc`
/// state; while `jmx.rs` answered a hard-coded 0, the `setVerbose` below wrote
/// a flag nothing read.
pub(crate) static MEMORY_MX_VERBOSE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether a live `java.lang.Thread` is a daemon.
///
/// Real-JDK 25 keeps the flag on `Thread.holder` (a `FieldHolder`) — the same
/// place `vm_exec.rs::read_thread_daemon_flag` reads it from for the VM's own
/// shutdown decisions. A synthetic `Thread` has neither field and is treated
/// as non-daemon, exactly as that helper does. Field reads only: nothing here
/// allocates, so the caller's `ObjectRef`s cannot move underneath it.
pub(crate) fn thread_is_daemon(ctx: &dyn NativeContext, thread: ObjectRef) -> bool {
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(thread, "holder") {
        if let Value::Int(v) = ctx.get_field_by_name(holder, "daemon") {
            return v != 0;
        }
    }
    matches!(ctx.get_field_by_name(thread, "daemon"), Value::Int(v) if v != 0)
}

/// Live daemon-thread count — the real datum behind
/// `ThreadMXBean.getDaemonThreadCount()`, which both this module and `jmx.rs`
/// answered with a hard-coded 0, making every JVM look like it was running no
/// daemon threads at all.
pub(crate) fn daemon_thread_count(ctx: &dyn NativeContext) -> i32 {
    let mut count = 0i32;
    for thread in ctx.enumerate_threads(usize::MAX) {
        if thread_is_daemon(ctx, thread) {
            count += 1;
        }
    }
    count
}

/// Process-wide high-water mark of the live-thread count.
///
/// Zero-initialised and only ever raised by [`peak_thread_count`] or set
/// outright by [`reset_peak_thread_count`], so it is meaningful from the first
/// read regardless of which surface asks first.
static PEAK_THREAD_COUNT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The JMM peak live-thread count — a real high-water mark, sampled on every
/// read.
///
/// Every surface used to answer `getPeakThreadCount()` with the CURRENT live
/// count, which meant the "peak" could go DOWN when threads exited (a peak may
/// never do that) and made `resetPeakThreadCount()` unobservable — it had
/// nothing to reset. Sampling here is monotone by construction and is still a
/// lower bound on the true peak (a spike between two reads is missed), which is
/// exactly what the old code claimed to be but was not.
pub(crate) fn peak_thread_count(ctx: &dyn NativeContext) -> i32 {
    let live = ctx.active_thread_count();
    PEAK_THREAD_COUNT
        .fetch_max(live, std::sync::atomic::Ordering::Relaxed)
        .max(live)
}

/// `ThreadMXBean.resetPeakThreadCount()` — "resets the peak thread count to
/// the current number of live threads" (JMM). A real write now: the previous
/// no-op silently discarded the call on every JMX surface.
pub(crate) fn reset_peak_thread_count(ctx: &dyn NativeContext) {
    PEAK_THREAD_COUNT.store(
        ctx.active_thread_count(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Ids of the live threads, read from each `java.lang.Thread`'s own `tid`.
///
/// That is the identity `getThreadInfo(long)` resolves against, so the two
/// agree — a caller can feed any id from here straight back into
/// `getThreadInfo` and get a real `ThreadInfo`. Ids come back as plain `i64`
/// so the caller can allocate its result array afterwards: `new_array` can GC
/// and the thread references are not pinned.
pub(crate) fn live_thread_ids(ctx: &mut dyn NativeContext) -> Vec<i64> {
    let mut ids = Vec::new();
    for thread in ctx.enumerate_threads(usize::MAX) {
        match ctx.get_field_by_name(thread, "tid") {
            Value::Long(id) if id > 0 => ids.push(id),
            Value::Int(id) if id > 0 => ids.push(id as i64),
            _ => {}
        }
    }
    if ids.is_empty() {
        // The calling thread is alive by definition, so an empty answer would
        // break the JMM invariant. Read its real `tid` rather than inventing
        // one; if that is unreadable too, an empty array is the honest answer.
        let current = ctx.current_thread_object();
        if let Value::Long(id) = ctx.get_field_by_name(current, "tid") {
            if id > 0 {
                ids.push(id);
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// Arbitrary-thread CPU time
// ---------------------------------------------------------------------------
//
// These live here rather than in `crate::jmx` for the same reason
// `daemon_thread_count` / `live_thread_ids` do: `jmx.rs` is gated on the
// `management` feature and this module is not, yet both register the same
// `ThreadMXBean` triples. One copy of the platform code is what stops the two
// registrations from drifting apart.

/// Windows per-thread CPU clock. One `extern` block for both the current-thread
/// and the arbitrary-thread read, so the two can never be declared with
/// different signatures.
#[cfg(target_os = "windows")]
mod win_thread_times {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn OpenThread(desired_access: u32, inherit_handle: i32, thread_id: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetThreadTimes(
            thread: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    /// `FILETIME` counts 100-nanosecond intervals.
    fn to_ns(t: FileTime) -> i64 {
        ((((t.high as u64) << 32) | (t.low as u64)).saturating_mul(100)) as i64
    }

    /// `(cpu_ns, user_ns)` for an already-open thread handle.
    ///
    /// # Safety
    /// `handle` must be a live thread handle opened with at least
    /// `THREAD_QUERY_LIMITED_INFORMATION`, or a pseudo-handle.
    unsafe fn times_of(handle: *mut c_void) -> Option<(i64, i64)> {
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        if GetThreadTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return None;
        }
        let user_ns = to_ns(user);
        Some((to_ns(kernel).saturating_add(user_ns), user_ns))
    }

    /// The CALLING thread, via the `GetCurrentThread()` pseudo-handle (which
    /// needs no `OpenThread` and no `CloseHandle`).
    pub(super) fn current() -> Option<(i64, i64)> {
        unsafe { times_of(GetCurrentThread()) }
    }

    /// An arbitrary OS thread in this process.
    pub(super) fn of_os_tid(os_tid: u32) -> Option<(i64, i64)> {
        // `GetThreadTimes` documents THREAD_QUERY_INFORMATION; Vista+ also
        // accepts the LIMITED right, which some hardened processes are capped
        // at. Try the specific one first and fall back rather than reporting
        // "unavailable" for a thread we can in fact read.
        const THREAD_QUERY_INFORMATION: u32 = 0x0040;
        const THREAD_QUERY_LIMITED_INFORMATION: u32 = 0x0800;
        for access in [THREAD_QUERY_INFORMATION, THREAD_QUERY_LIMITED_INFORMATION] {
            unsafe {
                let handle = OpenThread(access, 0, os_tid);
                if handle.is_null() {
                    continue;
                }
                let times = times_of(handle);
                CloseHandle(handle);
                return times;
            }
        }
        None
    }
}

/// The CALLING thread's `(cpu_ns, user_ns)` on Windows — see
/// [`win_thread_times`]. Exposed so `crate::jmx` shares the one `extern` block.
#[cfg(target_os = "windows")]
pub(crate) fn windows_current_thread_cpu_time_ns() -> Option<(i64, i64)> {
    win_thread_times::current()
}

/// `(cpu_ns, user_ns)` for the task `/proc/self/task/<tid>`.
///
/// Fields 14 (`utime`) and 15 (`stime`) of `stat`, in clock ticks. Field 2
/// (`comm`) is parenthesised and may itself contain spaces and `)`, so the
/// split starts after the LAST `)`: what follows is field 3 onwards.
#[cfg(target_os = "linux")]
fn linux_task_cpu_time_ns(os_tid: u32) -> Option<(i64, i64)> {
    let stat = std::fs::read_to_string(format!("/proc/self/task/{os_tid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `fields[0]` is field 3, so utime (14) is index 11 and stime (15) index 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let to_ns = |ticks: u64| -> i64 {
        ((i128::from(ticks) * 1_000_000_000i128) / i128::from(hz))
            .try_into()
            .unwrap_or(i64::MAX)
    };
    let user_ns = to_ns(utime);
    Some((user_ns.saturating_add(to_ns(stime)), user_ns))
}

/// CPU and user time consumed by an ARBITRARY OS thread of this process, in
/// nanoseconds — the datum `ThreadMXBean.getThreadCpuTime(long)` needs for an
/// id that is not the caller's.
///
/// `None` on a platform whose per-thread clock we do not read, or for a tid
/// that has already exited: both are the JMM's "measurement not available"
/// case, which callers map to `-1`.
pub(crate) fn os_thread_cpu_time_ns(os_tid: u32) -> Option<(i64, i64)> {
    #[cfg(target_os = "windows")]
    {
        win_thread_times::of_os_tid(os_tid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_task_cpu_time_ns(os_tid)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = os_tid;
        None
    }
}

/// The live `java.lang.Thread` whose `tid` is `java_tid`.
///
/// Same resolution `getThreadInfo(long)` performs, so any id from
/// [`live_thread_ids`] / `getAllThreadIds()` round-trips here.
pub(crate) fn thread_object_for_java_tid(
    ctx: &mut dyn NativeContext,
    java_tid: i64,
) -> Option<ObjectRef> {
    ctx.enumerate_threads(usize::MAX)
        .into_iter()
        .find(|thread| match ctx.get_field_by_name(*thread, "tid") {
            Value::Long(id) => id == java_tid,
            Value::Int(id) => i64::from(id) == java_tid,
            _ => false,
        })
}

/// Arbitrary-thread CPU time for a Java thread id, or `None` when it cannot be
/// measured (unknown id, no OS tid on record, or a platform we do not read).
pub(crate) fn cpu_time_of_java_tid(
    ctx: &mut dyn NativeContext,
    java_tid: i64,
) -> Option<(i64, i64)> {
    let thread = thread_object_for_java_tid(ctx, java_tid)?;
    let os_tid = ctx.thread_os_tid(thread)?;
    os_thread_cpu_time_ns(os_tid)
}

/// Whether `ThreadMXBean.isThreadCpuTimeSupported()` may honestly answer true.
///
/// Not a platform guess: it exercises the ARBITRARY-thread route end to end on
/// the calling thread — resolve its own mirror to an OS tid via
/// `NativeContext::thread_os_tid`, then read that tid through exactly the
/// platform path `getThreadCpuTime(long)` uses for a foreign id. If either step
/// fails, the honest answer is still `false`.
pub(crate) fn arbitrary_thread_cpu_time_supported(ctx: &mut dyn NativeContext) -> bool {
    let current = ctx.current_thread_object();
    match ctx.thread_os_tid(current) {
        Some(os_tid) => os_thread_cpu_time_ns(os_tid).is_some(),
        None => false,
    }
}

/// Whether `isSynchronizerUsageSupported()` may honestly answer true.
///
/// The write side is `AbstractOwnableSynchronizer.setExclusiveOwnerThread`,
/// which `crate::jmx::register_thread_impl` intercepts and forwards to
/// `NativeContext::record_jmx_owned_synchronizer` (a real VM override since
/// 2026-07-28) → `ThreadRegistry::set_jmx_owned_synchronizer`. That is the
/// JDK's single authoritative ownership transition for every ownable
/// synchronizer, so the resulting `lockedSynchronizers` list is complete —
/// but ONLY while the real AQS is in use. Under `CRATONVM_SYNTHETIC_AQS`,
/// `ReentrantLock.lock()` is itself a native (`util_concurrent_ext`) that never
/// reaches `setExclusiveOwnerThread`, so the list would be silently empty and
/// `true` would be a false claim. Mirrors that module's own gate expression.
pub(crate) fn synchronizer_usage_supported() -> bool {
    !crate::nbflags().synthetic_aqs || crate::nbflags().real_aqs
}

pub(crate) fn register_p59_management(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let mf = "java/lang/management/ManagementFactory";

    // ManagementFactory static getters — each returns a synthetic MXBean
    r.register(
        mf,
        "getMemoryMXBean",
        "()Ljava/lang/management/MemoryMXBean;",
        p59_get_memory_mxbean,
    );
    r.register(
        mf,
        "getThreadMXBean",
        "()Ljava/lang/management/ThreadMXBean;",
        p59_get_thread_mxbean,
    );
    r.register(
        mf,
        "getRuntimeMXBean",
        "()Ljava/lang/management/RuntimeMXBean;",
        p59_get_runtime_mxbean,
    );
    r.register(
        mf,
        "getOperatingSystemMXBean",
        "()Ljava/lang/management/OperatingSystemMXBean;",
        p59_get_os_mxbean,
    );
    r.register(
        mf,
        "getClassLoadingMXBean",
        "()Ljava/lang/management/ClassLoadingMXBean;",
        p59_get_classloading_mxbean,
    );
    r.register(
        mf,
        "getCompilationMXBean",
        "()Ljava/lang/management/CompilationMXBean;",
        p59_get_compilation_mxbean,
    );

    // MemoryMXBean = 0-field synthetic
    let mem = "java/lang/management/MemoryMXBean";
    r.register(
        mem,
        "getHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        p59_heap_usage,
    );
    r.register(
        mem,
        "getNonHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        p59_nonheap_usage,
    );
    // REAL: the GC's finalization backlog, via the `NativeContext` accessor
    // that exposes `ReferenceProcessor::pending_finalization_count()`
    // (gc/src/reference.rs). CratonVM DOES finalize —
    // `SharedVm::register_finalizable` feeds `ref_processor.finalization_queue`,
    // which `drain_finalizers` hands to a real `FinalizerThread` — so the
    // previous 0 was a floor, not a measurement. Identical body to the winning
    // `jmx.rs` registration, which is the point: the two must not drift.
    r.register(
        mem,
        "getObjectPendingFinalizationCount",
        "()I",
        |ctx, _args| Ok(Some(Value::Int(ctx.pending_finalization_count()))),
    );
    r.register(mem, "isVerbose", "()Z", |_ctx, _args| {
        let on = MEMORY_MX_VERBOSE.load(std::sync::atomic::Ordering::Relaxed);
        Ok(Some(Value::Int(if on { 1 } else { 0 })))
    });
    r.register(mem, "setVerbose", "(Z)V", |_ctx, args| {
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        MEMORY_MX_VERBOSE.store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(None)
    });

    // MemoryUsage = 4-field synthetic (init=0, used=1, committed=2, max=3 — all Long)
    let mu = "java/lang/management/MemoryUsage";
    r.register(mu, "getInit", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(mu, "getUsed", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(mu, "getCommitted", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(mu, "getMax", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });

    // THE ThreadMXBean BLOCK WAS REMOVED HERE 2026-08-24. All 8 triples it
    // registered also come from `jmx.rs::register_thread_mxbean_for`, which a
    // shipping binary reaches and this pass is not -- so the two modes ran
    // different code for every one of them, and this side won only under
    // `--features synthetic-jdk`. The shipping pass registers all 8 on a
    // SUPERSET of the class names (it also covers `com/sun/management/
    // ThreadMXBean`), so nothing here was load-bearing.
    //
    // Three of the 8 were better on THIS side and the fix went the other way:
    // `getThreadCount` / `getTotalStartedThreadCount` / `getDaemonThreadCount`
    // read live counts here and frozen `<init>` slots there. Those three are
    // now live in `jmx.rs` too.
    //
    // Two were WORSE on this side, which is why reading both bodies mattered:
    // this block answered `isThreadContentionMonitoringSupported()` false, on
    // the stated premise that nothing in the VM records blocked/waited
    // durations. That premise expired -- `ThreadJmxSnapshot::blocked_time_ms`
    // and `waited_time_ms` are real, filled by `vm_exec.rs` from the registry
    // and read back into `ThreadInfo.blockedTime`. The shipping `true` is
    // correct, so synthetic-JDK mode had been denying a capability this VM has.

    // RuntimeMXBean = 0-field synthetic — wire to real uptime
    let rmx = "java/lang/management/RuntimeMXBean";
    r.register(rmx, "getUptime", "()J", |_ctx, _args| {
        // Use a static start time to compute real uptime
        use std::sync::OnceLock;
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        let start = START.get_or_init(std::time::Instant::now);
        Ok(Some(Value::Long(start.elapsed().as_millis() as i64)))
    });
    r.register(rmx, "getName", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("1@cratonvm");
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(rmx, "getVmName", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("CratonVM");
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(rmx, "getVmVersion", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("25.0");
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(
        rmx,
        "getSpecVersion",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("25");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(rmx, "getStartTime", "()J", |_ctx, _args| {
        use std::sync::OnceLock;
        static START_TIME: OnceLock<i64> = OnceLock::new();
        let t = *START_TIME.get_or_init(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        });
        Ok(Some(Value::Long(t)))
    });

    // OperatingSystemMXBean = 0-field synthetic
    let osx = "java/lang/management/OperatingSystemMXBean";
    r.register(osx, "getName", "()Ljava/lang/String;", |ctx, _args| {
        let name = if cfg!(windows) {
            "Windows"
        } else if cfg!(target_os = "macos") {
            "Mac OS X"
        } else {
            "Linux"
        };
        let s = ctx.create_string(name);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(osx, "getArch", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string(std::env::consts::ARCH);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(osx, "getAvailableProcessors", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(
            std::thread::available_parallelism()
                .map(|p| p.get() as i32)
                .unwrap_or(1),
        )))
    });
    r.register(osx, "getSystemLoadAverage", "()D", |_ctx, _args| {
        #[cfg(target_os = "linux")]
        {
            if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
                if let Some(first) = loadavg.split_whitespace().next() {
                    if let Ok(avg) = first.parse::<f64>() {
                        return Ok(Some(Value::Double(avg)));
                    }
                }
            }
        }
        Ok(Some(Value::Double(-1.0))) // unsupported on non-Linux
    });
    // REAL: the `os.version` system property the VM populates at startup —
    // the same source the winning `jmx.rs` registration reads. The old "1.0"
    // was a fabricated version string that matched no operating system.
    // "unknown" only when the VM itself never learned the version.
    r.register(osx, "getVersion", "()Ljava/lang/String;", |ctx, _args| {
        let version = ctx
            .get_system_property("os.version")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| String::from("unknown"));
        let s = ctx.create_string(&version);
        Ok(Some(Value::Object(Some(s))))
    });

    // ClassLoadingMXBean = 0-field synthetic — wire to real class loading stats
    let clmx = "java/lang/management/ClassLoadingMXBean";
    r.register(clmx, "getLoadedClassCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(ctx.loaded_class_count() as i32)))
    });
    r.register(clmx, "getTotalLoadedClassCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(
            ctx.loaded_class_count() as i64 + ctx.unloaded_class_count() as i64,
        )))
    });
    r.register(clmx, "getUnloadedClassCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(ctx.unloaded_class_count() as i64)))
    });

    // CompilationMXBean = 0-field synthetic
    let cmx = "java/lang/management/CompilationMXBean";
    r.register(cmx, "getName", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("CratonVM Native Compiler");
        Ok(Some(Value::Object(Some(s))))
    });
    // REAL, as a self-consistent pair, in lock-step with the winning `jmx.rs`
    // registrations. `jit/src/tiered.rs` keeps
    // `CompilationStats::total_compile_time_ms` in the JMX spec's own unit, and
    // `NativeContext::jit_total_compile_time_ms` (added 2026-07-28) is the route
    // this crate was missing. Both natives read that ONE accessor, so the
    // support flag can never claim a number that is not kept: `None` (JIT
    // disabled) keeps the old 0 / `false`, and `Some(ms)` reports the real
    // total with `true`.
    r.register(cmx, "getTotalCompilationTime", "()J", |ctx, _args| {
        Ok(Some(Value::Long(
            ctx.jit_total_compile_time_ms()
                .map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX)),
        )))
    });
    r.register(
        cmx,
        "isCompilationTimeMonitoringSupported",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                ctx.jit_total_compile_time_ms().is_some(),
            ))))
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p59_get_memory_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryMXBean", 0)?;
    Ok(Some(Value::Object(Some(bean))))
}

/// Delegates to `jmx.rs` rather than fabricating its own bean.
///
/// Two reasons, both of which this pair had already drifted on. The bean must
/// be typed `com.sun.management.ThreadMXBean` so the platform-extension
/// `instanceof` answers true (see `jmx::alloc_extension_mxbean`), and it must
/// be allocated with the six slots the count getters read by index — this
/// function asked for **zero**. Per the DISPATCH NOTE at the top of this
/// module the `jmx.rs` registration is the one that wins, so neither defect
/// was reachable; sharing the allocator is what stops the next edit from
/// making one of them reachable again.
#[cfg(feature = "management")]
pub(crate) fn p59_get_thread_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // `crate::jmx` is `#[cfg(feature = "management")]`, and this call was not
    // — so `cargo check -p cratonvm-native-builtins` (the crate's own default
    // features, which do not include `management`) did not compile at all, and
    // with it every `cargo test` for this crate. Invisible from the CLI binary,
    // which does enable the feature. The plain synthetic bean is what this
    // function returned before it started delegating; keeping it as the
    // feature-off arm restores the build without changing the answer on any
    // configuration that could observe one.
    #[cfg(feature = "management")]
    let bean = crate::jmx::alloc_thread_mxbean(ctx)?;
    #[cfg(not(feature = "management"))]
    let bean = try_alloc_concurrent_synthetic(ctx, "java/lang/management/ThreadMXBean", 6)?;
    Ok(Some(Value::Object(Some(bean))))
}

/// Without the `management` feature `jmx.rs` is not compiled at all, so there
/// is no shared allocator to delegate to — and neither are the count getters
/// that read these slots, so nothing can observe their values.
///
/// The slot COUNT is still matched to `jmx::alloc_thread_mxbean`'s, which is
/// the half of the original defect that survives the feature being off: a
/// zero-slot bean is an out-of-bounds read waiting for the first getter that
/// does get compiled in. The type stays the plain
/// `java.lang.management.ThreadMXBean` rather than the `com.sun` extension,
/// because `alloc_extension_mxbean`'s reason for the extension type — making
/// the platform-extension `instanceof` answer true — is itself part of the
/// gated surface.
#[cfg(not(feature = "management"))]
pub(crate) fn p59_get_thread_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = try_alloc_concurrent_synthetic(ctx, "java/lang/management/ThreadMXBean", 6)?;
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_runtime_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = try_alloc_concurrent_synthetic(ctx, "java/lang/management/RuntimeMXBean", 0)?;
    Ok(Some(Value::Object(Some(bean))))
}

/// Delegates to `jmx.rs` — same reasoning as [`p59_get_thread_mxbean`]: the
/// extension type and the five slots `getName`/`getArch`/… read by index.
#[cfg(feature = "management")]
pub(crate) fn p59_get_os_mxbean(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // See `p59_get_thread_mxbean` for why the delegation is feature-gated.
    #[cfg(feature = "management")]
    let bean = crate::jmx::alloc_os_mxbean(ctx)?;
    #[cfg(not(feature = "management"))]
    let bean =
        try_alloc_concurrent_synthetic(ctx, "java/lang/management/OperatingSystemMXBean", 5)?;
    Ok(Some(Value::Object(Some(bean))))
}

/// See [`p59_get_thread_mxbean`]'s `not(management)` twin for why this exists
/// and why it matches only the slot count.
#[cfg(not(feature = "management"))]
pub(crate) fn p59_get_os_mxbean(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let bean =
        try_alloc_concurrent_synthetic(ctx, "java/lang/management/OperatingSystemMXBean", 5)?;
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_classloading_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = try_alloc_concurrent_synthetic(ctx, "java/lang/management/ClassLoadingMXBean", 0)?;
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_compilation_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = try_alloc_concurrent_synthetic(ctx, "java/lang/management/CompilationMXBean", 0)?;
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_heap_usage(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let mu = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4)?;
    let used = ctx.heap_allocated_bytes() as i64;
    let rss = get_process_rss_bytes();
    let committed = if rss > 0 {
        rss
    } else {
        used + (4 * 1024 * 1024)
    };
    let max = committed * 2; // max = 2x committed
    ctx.set_field(mu, 0, Value::Long(used.min(committed))); // init = first seen used
    ctx.set_field(mu, 1, Value::Long(used.max(1024))); // used
    ctx.set_field(mu, 2, Value::Long(committed));
    ctx.set_field(mu, 3, Value::Long(max));
    Ok(Some(Value::Object(Some(mu))))
}

pub(crate) fn p59_nonheap_usage(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let mu = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4)?;
    // Non-heap: metaspace/code cache estimate
    let rss = get_process_rss_bytes();
    let heap_used = ctx.heap_allocated_bytes() as i64;
    let nonheap_used = if rss > heap_used {
        (rss - heap_used).max(512 * 1024)
    } else {
        512 * 1024
    };
    ctx.set_field(mu, 0, Value::Long(256 * 1024)); // initial
    ctx.set_field(mu, 1, Value::Long(nonheap_used));
    ctx.set_field(mu, 2, Value::Long(nonheap_used + 1024 * 1024));
    ctx.set_field(mu, 3, Value::Long(-1)); // -1 means undefined
    Ok(Some(Value::Object(Some(mu))))
}

/// Get the resident set size (RSS) of the current process in bytes.
/// Returns 0 if unable to determine.
pub(crate) fn get_process_rss_bytes() -> i64 {
    #[cfg(target_os = "windows")]
    {
        // On Windows, use GetProcessMemoryInfo via Win32 API
        #[repr(C)]
        #[allow(non_snake_case)]
        struct ProcessMemoryCounters {
            cb: u32,
            PageFaultCount: u32,
            PeakWorkingSetSize: usize,
            WorkingSetSize: usize,
            QuotaPeakPagedPoolUsage: usize,
            QuotaPagedPoolUsage: usize,
            QuotaPeakNonPagedPoolUsage: usize,
            QuotaNonPagedPoolUsage: usize,
            PagefileUsage: usize,
            PeakPagefileUsage: usize,
        }
        extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn K32GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                ppmc: *mut ProcessMemoryCounters,
                cb: u32,
            ) -> i32;
        }
        unsafe {
            let mut pmc = std::mem::zeroed::<ProcessMemoryCounters>();
            pmc.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
                return pmc.WorkingSetSize as i64;
            }
        }
        0
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(kb_str) = line.strip_prefix("VmRSS:") {
                    let kb_str = kb_str.trim().trim_end_matches(" kB").trim();
                    if let Ok(kb) = kb_str.parse::<i64>() {
                        return kb * 1024;
                    }
                }
            }
        }
        0
    }
    #[cfg(target_os = "macos")]
    {
        // Approximate with rusage
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
                return usage.ru_maxrss; // on macOS this is bytes
            }
        }
        0
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        0
    }
}
