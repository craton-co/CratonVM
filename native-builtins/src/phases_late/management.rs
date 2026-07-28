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
// `experimental-jmx` is a DEFAULT feature, so both paths are the normal build.
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
    // FLAGGED-0 (was justified as spec-correct on a false premise): CratonVM
    // DOES finalize — `SharedVm::register_finalizable` feeds
    // `ref_processor.finalization_queue`, which `drain_finalizers` hands to a
    // real `FinalizerThread` — and `ReferenceProcessor::pending_finalization_count()`
    // (gc/src/reference.rs) is the exact datum this method wants. It is simply
    // not reachable: `NativeContext` exposes no accessor for it. 0 is therefore
    // a floor, not a measurement. Needs a one-line `NativeContext` accessor to
    // fix — out of scope for a native-builtins-only change. Same situation as
    // the winning `jmx.rs` registration.
    r.register(
        mem,
        "getObjectPendingFinalizationCount",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(0))),
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

    // ThreadMXBean = 0-field synthetic — wire to real VM thread stats
    let tmx = "java/lang/management/ThreadMXBean";
    r.register(tmx, "getThreadCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(ctx.active_thread_count() as i32)))
    });
    // REAL: a monotone high-water mark (see `peak_thread_count`), not the
    // instantaneous live count the previous body returned — which could go
    // DOWN and left `resetPeakThreadCount()` with nothing to reset.
    r.register(tmx, "getPeakThreadCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(peak_thread_count(ctx))))
    });
    r.register(tmx, "getTotalStartedThreadCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(ctx.active_thread_count().max(1) as i64)))
    });
    // REAL: count the live threads carrying `Thread.holder.daemon` — the same
    // flag the VM's own shutdown logic reads. The hard-coded 0 made every JVM
    // look like it ran no daemon threads at all. Shares `jmx.rs`'s helper so
    // both registrations of this triple agree.
    r.register(tmx, "getDaemonThreadCount", "()I", |ctx, _args| {
        Ok(Some(Value::Int(daemon_thread_count(ctx))))
    });
    // REAL thread ids. The previous body allocated one slot per live thread
    // but then filled them with the loop INDEX (`i + 1`) rather than the
    // thread's id, so the array's contents were fabricated even though its
    // length was real — and `getThreadInfo(long)` resolves threads by matching
    // `Thread.tid`, so those ids did not round-trip. Read the real `tid`.
    r.register(tmx, "getAllThreadIds", "()[J", |ctx, _args| {
        // Ids are collected as plain i64 first: `new_array` can GC, and the
        // thread references behind them are not pinned.
        let ids = live_thread_ids(ctx);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, ids.len());
        for (i, id) in ids.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Long(*id));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    // KEEP: false is the measurement, and it is specifically about OTHER
    // threads. `isThreadCpuTimeSupported()` promises `getThreadCpuTime(id)`
    // works for an arbitrary id, which needs an OS handle for a thread we are
    // not running on — CratonVM's thread table carries no such handle. (The
    // CURRENT thread's CPU time IS readable and the winning `jmx.rs`
    // registration now reports it; the spec explicitly allows exactly this
    // asymmetry, and this interface registers no current-thread getter at all.)
    r.register(tmx, "isThreadCpuTimeSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    // KEEP: false is the measurement — CratonVM times no monitor contention, so
    // `getThreadInfo(..).getBlockedTime()` has no source and the spec's answer
    // for an unsupported optional feature is exactly `false`.
    // ES-FAIL-05 — `HotThreads.initializeRuntimeMonitoring()` (ESTestCase.<clinit>)
    // calls isThreadContentionMonitoringSupported(); unregistered → AbstractMethodError
    // blocking ~every server test. Report false (HotThreads then no-ops).
    r.register(
        tmx,
        "isThreadContentionMonitoringSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    // KEEP: false is the measurement — CratonVM does no contention timing, and
    // "not enabled" is the state a HotSpot boots in anyway.
    r.register(
        tmx,
        "isThreadContentionMonitoringEnabled",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

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
    // KEEP, as a self-consistent pair. The JIT keeps no cumulative wall-clock
    // compile timer, and per the JMX spec `getTotalCompilationTime()` is only
    // meaningful when monitoring is supported — so reporting `false` below is
    // what keeps the 0 above legible as "not measured" rather than "measured
    // and zero". FLAGGED for follow-up if the JIT gains compile-time
    // accounting.
    r.register(cmx, "getTotalCompilationTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    });
    r.register(
        cmx,
        "isCompilationTimeMonitoringSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p59_get_memory_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryMXBean", 0);
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_thread_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = alloc_concurrent_synthetic(ctx, "java/lang/management/ThreadMXBean", 0);
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_runtime_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = alloc_concurrent_synthetic(ctx, "java/lang/management/RuntimeMXBean", 0);
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_os_mxbean(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let bean = alloc_concurrent_synthetic(ctx, "java/lang/management/OperatingSystemMXBean", 0);
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_classloading_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = alloc_concurrent_synthetic(ctx, "java/lang/management/ClassLoadingMXBean", 0);
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_get_compilation_mxbean(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let bean = alloc_concurrent_synthetic(ctx, "java/lang/management/CompilationMXBean", 0);
    Ok(Some(Value::Object(Some(bean))))
}

pub(crate) fn p59_heap_usage(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let mu = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4);
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
    let mu = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4);
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
