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

/// Global flag for MemoryMXBean.setVerbose / isVerbose.
pub(crate) static MEMORY_MX_VERBOSE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    r.register(tmx, "getPeakThreadCount", "()I", |ctx, _args| {
        // Peak is at least current
        Ok(Some(Value::Int(ctx.active_thread_count().max(1) as i32)))
    });
    r.register(tmx, "getTotalStartedThreadCount", "()J", |ctx, _args| {
        Ok(Some(Value::Long(ctx.active_thread_count().max(1) as i64)))
    });
    r.register(tmx, "getDaemonThreadCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(tmx, "getAllThreadIds", "()[J", |ctx, _args| {
        let threads = ctx.enumerate_threads(256);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, threads.len().max(1));
        if threads.is_empty() {
            ctx.set_array_element(arr, 0, Value::Long(1));
        } else {
            for (i, _tid) in threads.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Long((i + 1) as i64));
            }
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(tmx, "isThreadCpuTimeSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    // ES-FAIL-05 — `HotThreads.initializeRuntimeMonitoring()` (ESTestCase.<clinit>)
    // calls isThreadContentionMonitoringSupported(); unregistered → AbstractMethodError
    // blocking ~every server test. Report false (HotThreads then no-ops).
    r.register(
        tmx,
        "isThreadContentionMonitoringSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
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
    r.register(osx, "getVersion", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("1.0");
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
