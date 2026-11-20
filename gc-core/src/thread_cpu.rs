// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zt pl4 item 3 (G1): the GC worker threads' CPU time, HotSpot's
//! `CPUTimeCounters` (`sun.threads.cpu_time.gc_parallel_workers`,
//! `gc_conc_mark`, and their sum `sun.threads.total_gc_cpu_time`), which
//! HotSpot's G1 reads with `os::thread_cpu_time` over its worker threads.
//! Here each worker adds its own CPU around each piece of work
//! ([`CpuSpan`]), since one thread's clock cannot be read from another.
//! `CRATONVM_PERF_GC_THREAD_CPU` (default ON, `=0` kill: nothing measured).

use std::sync::atomic::{AtomicU64, Ordering};

/// The calling thread's CPU time in nanoseconds (Linux
/// `CLOCK_THREAD_CPUTIME_ID`, Windows `GetThreadTimes`), `None` where no
/// per-thread clock is wired up or without the `zgc` feature that owns the
/// reader (`thread_cpu_nanos`).
#[inline]
pub fn current_thread_cpu_ns() -> Option<u64> {
        thread_cpu_nanos()
}

/// `CRATONVM_PERF_GC_THREAD_CPU`, memoised (read per piece of work).
#[inline]
pub fn gc_thread_cpu_on() -> bool {
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

/// One piece of a GC worker's work: [`Self::begin`] reads the thread's CPU
/// clock, [`Self::end`] adds the difference to `total`. Inert under the kill
/// or without a clock.
pub struct CpuSpan(Option<u64>);

impl CpuSpan {
    #[inline]
    pub fn begin() -> Self {
        Self(if gc_thread_cpu_on() { current_thread_cpu_ns() } else { None })
    }

    #[inline]
    pub fn end(self, total: &AtomicU64) {
        total.fetch_add(self.elapsed_ns(), Ordering::Relaxed);
    }

    /// The CPU the span took, nanoseconds (0 when inert).
    #[inline]
    pub fn elapsed_ns(self) -> u64 {
        match (self.0, self.0.and_then(|_| current_thread_cpu_ns())) {
            (Some(start), Some(now)) => now.saturating_sub(start),
            _ => 0,
        }
    }
}

thread_local! {
    /// The CPU this thread spent inside collector work of the pause it is
    /// running ([`note_pause_work_cpu`]); taken by the GC barrier when the
    /// pause ends ([`take_pause_work_cpu`]).
    static PAUSE_WORK_CPU: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// zt pl4 item 3: the collecting thread spent `ns` of CPU inside a
/// collection HotSpot runs on GC worker threads (ZGC, G1), not on its VM
/// thread: the pause's `sun.threads.cpu_time.vm` share leaves it out
/// (`GcBarrier`, [`take_pause_work_cpu`]).
pub fn note_pause_work_cpu(ns: u64) {
    let _ = PAUSE_WORK_CPU.try_with(|c| c.set(c.get().saturating_add(ns)));
}

/// zt pl4 item 3: the CPU [`note_pause_work_cpu`] booked on this thread
/// since the last take, and reset to 0.
pub fn take_pause_work_cpu() -> u64 {
    PAUSE_WORK_CPU.try_with(|c| c.replace(0)).unwrap_or(0)
}

#[cfg(test)]
mod tests;

/// CPU time the CALLING thread has consumed, in nanoseconds, or `None` where
/// no per-thread clock is wired up (zr zs s1/mc). Linux:
/// `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`; Windows: `GetThreadTimes` (user +
/// kernel, scheduler-quantum resolution), or under the opt-in
/// `CRATONVM_ZGC_THREAD_CPU_CYCLES=1` the cycle count scaled by the TSC rate
/// (`thread_clock`, zt t6/ob4). The same construction as
/// `cratonvm_jit::tiered::current_thread_cpu_time`, which this crate cannot
/// depend on.
#[allow(unreachable_code)]
pub fn thread_cpu_nanos() -> Option<u64> {
    #[cfg(windows)]
    {
        // `thread_clock` lives in `cratonvm-gc-zgc` (`cycle_clock_requested()`
        // is off by default), so fall back to `GetThreadTimes` here.
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn GetThreadTimes(
                thread: *mut std::ffi::c_void,
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
        // SAFETY: `GetCurrentThread` returns a pseudo-handle that is always
        // valid for the calling thread and needs no close, and every out
        // pointer is a live, writable `FILETIME` in this frame.
        let ok = unsafe {
            GetThreadTimes(
                GetCurrentThread(),
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
        return Some(
            ticks(kernel)
                .saturating_add(ticks(user))
                .saturating_mul(100),
        );
    }
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        #[repr(C)]
        struct Timespec {
            tv_sec: i64,
            tv_nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
        }
        // `CLOCK_THREAD_CPUTIME_ID` from `<time.h>`, the same on every Linux
        // architecture.
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a live, writable `struct timespec` (two `long`s on
        // an LP64 target) and the clock id is a kernel-defined constant.
        let rc = unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        if rc != 0 {
            return None;
        }
        // Casts: a thread CPU clock is non-negative, and `tv_nsec < 1e9`.
        return Some((ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64);
    }
    None
}

