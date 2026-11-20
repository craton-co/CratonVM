#![allow(unsafe_code)]
// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JMX (Java Management Extensions) native method implementations.
//! Provides MBeanServer and platform MXBeans for runtime monitoring.

use cratonvm_native_builtins::lang_class::ClassTextName;
use cratonvm_native_api::{
    NativeContext, NativeHandle, NativeHandleScope, NativeKind, NativeMethodRegistry,
    ThreadJmxSnapshot,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::ClassId;
use cratonvm_types::{ObjectRef, Value};
use std::sync::OnceLock;
use std::time::Instant;

use cratonvm_native_builtins::{native_noop_with_this, obj_arg, try_alloc_concurrent_synthetic};
// The live-thread helpers live in `cratonvm_native_builtins::phases_late::management` rather than
// here: this module is gated on the `management` feature, that one is
// not, and both register the same `ThreadMXBean` triples. One copy is what
// stops the two registrations from drifting apart again.
use cratonvm_native_builtins::phases_late::management::{
    arbitrary_thread_cpu_time_supported, cpu_time_of_java_tid, daemon_thread_count,
    is_virtual_thread_mirror, live_platform_thread_count, live_thread_ids, peak_thread_count,
    platform_thread_object_for_java_tid, reset_peak_thread_count, synchronizer_usage_supported,
    thread_object_for_java_tid, total_started_thread_count,
};

/// The `com.sun.management.OperatingSystemMXBean` metrics, and the
/// `com.sun.management.internal.OperatingSystemImpl` `*0` natives behind the
/// real JDK's copy of the same bean.
///
/// One implementation for both surfaces, deliberately: they are two spellings
/// of one question, and the pattern this file keeps hitting is a pair that
/// drifts until one of them reports a number the other calls unavailable.
///
/// Everything here is `-1` off Linux — the JMM's documented "metric
/// unavailable", which callers distinguish from a real measurement — except
/// the process CPU time (zt t1/ob) and the memory, swap and committed-virtual
/// figures (zt t3/ob), which Windows answers too. On Linux
/// the numbers come from `/proc`, i.e. from the same place `libmanagement`
/// reads them, so they are measurements rather than plausible-looking
/// inventions. The two CPU-*load* doubles stay at the `-1.0` sentinel: they
/// are defined as a fraction over an interval, which needs sampling state this
/// bean does not keep, and a single-shot number would be a guess.
mod os_metrics {
    /// Value of a `key:  <n> kB` line in a `/proc` file, in **bytes**.
    #[cfg(target_os = "linux")]
    fn proc_kb_field(path: &str, key: &str) -> Option<i64> {
        let contents = std::fs::read_to_string(path).ok()?;
        for line in contents.lines() {
            let Some(rest) = line.strip_prefix(key) else {
                continue;
            };
            let rest = rest.strip_prefix(':')?;
            let kb: i64 = rest.split_whitespace().next()?.parse().ok()?;
            return kb.checked_mul(1024);
        }
        None
    }

    /// Total physical memory (`getTotalMemorySize` /
    /// `getTotalPhysicalMemorySize`).
    pub(super) fn total_physical_memory() -> i64 {
        #[cfg(windows)]
        {
            if let Some(s) = windows_memory_status() {
                return s[0];
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(bytes) = proc_kb_field("/proc/meminfo", "MemTotal") {
                return bytes;
            }
        }
        -1
    }

    /// zt t3/ob (`zgc-zt1ob-windows-os-metrics-answer-minus-one-FIXED-20261019.md`):
    /// `GlobalMemoryStatusEx`'s `[ullTotalPhys, ullAvailPhys, ullTotalPageFile,
    /// ullAvailPageFile]`, the four figures the JDK's Windows
    /// `OperatingSystemImpl` answers for the total / free memory and swap
    /// metrics; `None` on failure or under `CRATONVM_WINDOWS_OS_MEMORY_METRICS=0`
    /// (the -1 "unavailable" of before). The struct is `jfr.rs`'s
    /// `host_memory_totals` one, field for field (`clashing_extern_declarations`
    /// is denied).
    #[cfg(windows)]
    fn windows_memory_status() -> Option<[i64; 4]> {
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        extern "system" {
            fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a live, writable `MEMORYSTATUSEX` whose
        // `dwLength` is set, as the call requires.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return None;
        }
        let b = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        Some([
            b(status.ull_total_phys),
            b(status.ull_avail_phys),
            b(status.ull_total_page_file),
            b(status.ull_avail_page_file),
        ])
    }

    /// zt t3/ob: the process's `PROCESS_MEMORY_COUNTERS.PagefileUsage` (its
    /// private committed bytes), the JDK's Windows
    /// `getCommittedVirtualMemorySize0` figure; `None` on failure or under
    /// `CRATONVM_WINDOWS_OS_MEMORY_METRICS=0`. The struct is
    /// `phases_late/management.rs` `get_process_rss_bytes`' one, field for
    /// field.
    #[cfg(windows)]
    fn windows_pagefile_usage() -> Option<i64> {
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
        // SAFETY: an all-zero `PROCESS_MEMORY_COUNTERS` is valid (integers
        // only); `cb` is its size and the pointer a live, writable local; the
        // pseudo-handle needs no close.
        unsafe {
            let mut pmc = std::mem::zeroed::<ProcessMemoryCounters>();
            pmc.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) == 0 {
                return None;
            }
            Some(i64::try_from(pmc.PagefileUsage).unwrap_or(i64::MAX))
        }
    }

    /// Free physical memory (`getFreeMemorySize` / `getFreePhysicalMemorySize`).
    ///
    /// `MemAvailable` first — that is the kernel's own estimate of what a new
    /// allocation could actually get, and it is what a modern HotSpot reports.
    /// `MemFree` is the fallback for kernels too old to publish it.
    pub(super) fn free_physical_memory() -> i64 {
        #[cfg(windows)]
        {
            if let Some(s) = windows_memory_status() {
                return s[1];
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(bytes) = proc_kb_field("/proc/meminfo", "MemAvailable") {
                return bytes;
            }
            if let Some(bytes) = proc_kb_field("/proc/meminfo", "MemFree") {
                return bytes;
            }
        }
        -1
    }

    /// `getTotalSwapSpaceSize`.
    pub(super) fn total_swap() -> i64 {
        #[cfg(windows)]
        {
            if let Some(s) = windows_memory_status() {
                return s[2];
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(bytes) = proc_kb_field("/proc/meminfo", "SwapTotal") {
                return bytes;
            }
        }
        -1
    }

    /// `getFreeSwapSpaceSize`.
    pub(super) fn free_swap() -> i64 {
        #[cfg(windows)]
        {
            if let Some(s) = windows_memory_status() {
                return s[3];
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(bytes) = proc_kb_field("/proc/meminfo", "SwapFree") {
                return bytes;
            }
        }
        -1
    }

    /// `getCommittedVirtualMemorySize` — this process's whole virtual size,
    /// `VmSize` in `/proc/self/status`.
    pub(super) fn committed_virtual_memory() -> i64 {
        #[cfg(windows)]
        {
            if let Some(bytes) = windows_pagefile_usage() {
                return bytes;
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(bytes) = proc_kb_field("/proc/self/status", "VmSize") {
                return bytes;
            }
        }
        -1
    }

    /// `getProcessCpuTime`, in **nanoseconds**: user + system time for the
    /// whole process, from fields 14/15 of `/proc/self/stat`.
    ///
    /// The fields are in clock ticks. `sysconf(_SC_CLK_TCK)` is 100 on every
    /// Linux this VM targets and is not reachable without a libc binding, so
    /// it is assumed rather than read — the same constant the in-tree
    /// `/proc/…/stat` CPU-time reader for `getThreadCpuTime` already assumes.
    ///
    /// Parsing starts after the last `)`, because field 2 is the executable
    /// name in parentheses and may itself contain spaces.
    ///
    /// zt t1/ob: on Windows `GetProcessTimes`' kernel + user time (100 ns
    /// units), as the JDK's Windows `OperatingSystemImpl.getProcessCpuTime0`
    /// reports it; it answered -1 there, which a CPU-time probe on the
    /// Windows box read as "not supported". `CRATONVM_WINDOWS_PROCESS_CPU_TIME=0`
    /// restores the -1.
    pub(super) fn process_cpu_time_ns() -> i64 {
        #[cfg(windows)]
        {
            {
                if let Some(ns) = windows_process_cpu_ns() {
                    return ns;
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            const TICKS_PER_SEC: i64 = 100;
            const NS_PER_TICK: i64 = 1_000_000_000 / TICKS_PER_SEC;
            if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
                if let Some(after_comm) = stat.rfind(')').map(|i| &stat[i + 1..]) {
                    let fields: Vec<&str> = after_comm.split_whitespace().collect();
                    // After the `)` the first field is `state` (field 3), so
                    // utime (14) and stime (15) are indices 11 and 12.
                    if let (Some(utime), Some(stime)) = (fields.get(11), fields.get(12)) {
                        if let (Ok(u), Ok(s)) = (utime.parse::<i64>(), stime.parse::<i64>()) {
                            return u.saturating_add(s).saturating_mul(NS_PER_TICK);                        }                    }                }            }        }
        -1
    }

    /// zt t1/ob: the process's kernel + user CPU time in nanoseconds from
    /// `GetProcessTimes` (`FILETIME`s in 100 ns units), `None` on failure.
    #[cfg(windows)]
    fn windows_process_cpu_ns() -> Option<i64> {
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetProcessTimes(
                process: *mut std::ffi::c_void,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
        }
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always
        // valid and needs no close; every out pointer is a live, writable
        // `FILETIME` in this frame.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return None;
        }
        let ticks = |t: FileTime| (u64::from(t.high) << 32) | u64::from(t.low);
        let ns = ticks(kernel).saturating_add(ticks(user)).saturating_mul(100);
        Some(i64::try_from(ns).unwrap_or(i64::MAX))
    }

    /// `getOpenFileDescriptorCount` — entries in `/proc/self/fd`.
    ///
    /// The `readdir` handle itself is one of them, so the raw count is one
    /// too high; subtract it rather than report a number that grows every
    /// time it is asked for.
    pub(super) fn open_fd_count() -> i64 {
        #[cfg(target_os = "linux")]
        {
            if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
                let n = entries.count() as i64;
                return (n - 1).max(0);            }        }
        -1
    }

    /// `getMaxFileDescriptorCount` — the soft `NOFILE` limit, from
    /// `/proc/self/limits`.
    pub(super) fn max_fd_count() -> i64 {
        #[cfg(target_os = "linux")]
        {
            if let Ok(limits) = std::fs::read_to_string("/proc/self/limits") {
                for line in limits.lines() {
                    let Some(rest) = line.strip_prefix("Max open files") else {
                        continue;
                    };
                    if let Some(soft) = rest.split_whitespace().next() {
                        if let Ok(v) = soft.parse::<i64>() {
                            return v;
                        }
                        // "unlimited" — RLIM_INFINITY has no i64 spelling the
                        // JMM defines, so report it as unavailable rather than
                        // as some arbitrary large number.
                        return -1;
                    }
                }
            }
        }
        -1
    }
}

/// zs s3/ob2: is `pool` one of [`NativeContext::non_heap_memory_pools`]?
#[cfg(test)]
fn is_zgc_non_heap_pool(ctx: &dyn NativeContext, pool: &str) -> bool {
    ctx.non_heap_memory_pools().iter().any(|p| p.name == pool)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod jmx_tests;

/// gen r4w2/obs (2026-09-23): the per-generation JMX beans, driven through the
/// registered natives against a context that describes them the way the VM
/// does for the generational backend — and, as the negative control, against
/// one that does not (the historical answer must be unchanged).
#[cfg(test)]
mod gen_r4w2_obs_jmx_tests;

/// gen r4w3/obs2 (2026-09-23): the `com.sun.management` natives behind GC
/// notifications and `getLastGcInfo()`. The Java half (the JDK's
/// `GarbageCollectorExtImpl` / `GcInfoBuilder` / `GcInfo`) cannot run under the
/// mock; `GenR4W3GcNotificationProbe.java` drives it end to end.
#[cfg(test)]
mod gen_r4w3_obs2_gc_notification_tests;

/// gc-common w8-f: the platform MBeanServer is one per VM. Before, it was one
/// per process: a second VM's `getPlatformMBeanServer()` returned an object in
/// the first VM's heap, and every collector rooted it.
#[cfg(test)]
mod w8f_platform_mbean_server_tests;

#[cfg(test)]
mod w16f_jmx_tests;

#[cfg(test)]
mod w17e_jmx_tests;

#[cfg(test)]
mod w19e_jmx_tests;

/// gci i15/nh (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 7): HotSpot 25's non-heap pools and managers -- names, order, the
/// figures each is sampled from, the aggregate, and the bean natives.
#[cfg(test)]
mod gci_i15_nh_non_heap_tests;

/// gci i28/dg (`gci-i14mp-...` item 11b): the four `DiagnosticCommandImpl`
/// natives are registered as stated `Bridge`s, the support seed stays opt-in,
/// and a context without a command table answers HotSpot's refusals.
#[cfg(test)]
mod gci_i28_dg_dcmd_tests;

/// gci i47w/dh47 (`gci-i47w-j45-hotspotdiagnostic-dumpheap0-unsatisfied-link-FIXED-20261016`):
/// `HotSpotDiagnostic.dumpHeap0` is a stated `Bridge` on JDK 25's class in
/// both modes, and answers `jmm_DumpHeap0`'s exceptions.
#[cfg(test)]
mod gci_i47w_dh47_dump_heap_tests;

#[cfg(test)]
mod i101_l6_deadlock_walk_tests;

/// gci i44/sp: the safepoint natives' arithmetic and their registration.
#[cfg(test)]
mod gci_i44_sp_safepoint_figure_tests;

#[cfg(test)]
mod gci_i46_hc_tests;

/// zt t8/x5: `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE` was flipped to default ON by
/// the zt t7 orchestrator (`273734b221`) with no unit test of either arm.
#[cfg(test)]
mod zt8x5_soft_max_manageable_tests;

/// zt t24/x20: `CRATONVM_JMX_MANAGEABLE_HEAP_FREE_RATIOS` (default ON, `=0`
/// kill), and `getInternalFlagCount` / `getAllFlagNames` counting only what
/// `getFlags(null, ..)` lists.
#[cfg(test)]
mod zt24x20_manageable_flag_tests;

mod jmx_ioex;
#[allow(unused_imports)]
pub use jmx_ioex::*;
#[allow(unused_imports)]
use jmx_ioex::*;
mod register_garbage_collector_impl;
#[allow(unused_imports)]
pub use register_garbage_collector_impl::*;
#[allow(unused_imports)]
use register_garbage_collector_impl::*;
mod alloc_basic_thread_info;
#[allow(unused_imports)]
pub use alloc_basic_thread_info::*;
#[allow(unused_imports)]
use alloc_basic_thread_info::*;
