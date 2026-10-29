// SPDX-License-Identifier: Apache-2.0
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
/// reader (`crate::zgc::mark::thread_cpu_nanos`).
#[inline]
pub fn current_thread_cpu_ns() -> Option<u64> {
    #[cfg(feature = "zgc")]
    {
        crate::zgc::mark::thread_cpu_nanos()
    }
    #[cfg(not(feature = "zgc"))]
    {
        None
    }
}

/// `CRATONVM_PERF_GC_THREAD_CPU`, memoised (read per piece of work).
#[inline]
pub fn gc_thread_cpu_on() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PERF_GC_THREAD_CPU");
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
mod tests {
    use super::*;

    /// A span that burns CPU adds a positive figure where a clock exists.
    /// Windows charges thread CPU per clock tick (~15.6 ms), so the span
    /// burns until the thread's clock moves (bounded by wall time).
    #[test]
    fn a_span_adds_the_threads_cpu() {
        let total = AtomicU64::new(0);
        let start = current_thread_cpu_ns();
        let span = CpuSpan::begin();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut acc = 0u64;
        loop {
            for i in 0..1_000_000u64 {
                acc = acc.wrapping_mul(31).wrapping_add(i);
            }
            std::hint::black_box(acc);
            if current_thread_cpu_ns() != start || std::time::Instant::now() > deadline {
                break;
            }
        }
        span.end(&total);
        if current_thread_cpu_ns().is_some() && gc_thread_cpu_on() {
            assert!(total.load(Ordering::Relaxed) > 0);
        }
    }
}
