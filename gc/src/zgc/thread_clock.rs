// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t6/ob4 (`zgc-zs3ac-collection-cpu-residual-shares-20261014.md` item 4):
//! an opt-in high-resolution thread CPU clock for Windows.
//!
//! [`super::mark::thread_cpu_nanos`] reads `GetThreadTimes` on Windows, which
//! advances in scheduler quanta (~15.6 ms): a worker span shorter than a
//! quantum reads 0 or one quantum, so the `[GC] zgc-cpu:` shares undercount
//! systematically there (Linux's `CLOCK_THREAD_CPUTIME_ID` is exact). Under
//! `CRATONVM_ZGC_THREAD_CPU_CYCLES=1` the reading is the thread's
//! `QueryThreadCycleTime` scaled by the time-stamp counter's rate instead.
//!
//! Why opt-in: Microsoft documents the cycle count as not convertible to
//! time. In practice it counts at the invariant TSC rate on x86-64 (the
//! counter `QueryThreadCycleTime` charges is the TSC sampled at each context
//! switch), which this module measures once against
//! `QueryPerformanceCounter`; a CPU without an invariant TSC would read wrong
//! figures. The deciding row is in the t6/ob4 report: a fixed CPU-bound span
//! measured both ways against the process clock.
//!
//! The one `static` (the calibrated TSC rate) is a property of the machine,
//! the same for every heap and VM in the process, not GC or compatibility
//! state; it lives here and not in `mark.rs`, which holds no process state.

/// `CRATONVM_ZGC_THREAD_CPU_CYCLES` -- default OFF (`=1` opt-in): on Windows
/// x86-64, the GC thread CPU clock is the cycle count scaled by the TSC rate.
/// Read per call (once per span edge, never per object).
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn cycle_clock_requested() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_THREAD_CPU_CYCLES")
}

/// `cycles` at `hz` cycles per second, in nanoseconds; `None` for a zero
/// rate or a result past `u64`. Pure.
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn cycles_to_nanos(cycles: u64, hz: u64) -> Option<u64> {
    if hz == 0 {
        return None;
    }
    u64::try_from(u128::from(cycles) * 1_000_000_000 / u128::from(hz)).ok()
}

/// The TSC rate from two `(tsc, qpc)` samples and the performance-counter
/// frequency `qpf`: `Δtsc * qpf / Δqpc`. `None` when either counter did not
/// advance, ran backwards, or `qpf` is 0. Pure.
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn tsc_hz_from(tsc: (u64, u64), qpc: (u64, u64), qpf: u64) -> Option<u64> {
    let dt = tsc.1.checked_sub(tsc.0)?;
    let dq = qpc.1.checked_sub(qpc.0)?;
    if dq == 0 || qpf == 0 || dt == 0 {
        return None;
    }
    u64::try_from(u128::from(dt) * u128::from(qpf) / u128::from(dq)).ok()
}

/// The calling thread's CPU time from its cycle count, in nanoseconds, or
/// `None` where this clock does not exist (not Windows x86-64) or could not
/// be calibrated or read.
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn thread_cycle_nanos() -> Option<u64> {
    imp::thread_cycle_nanos()
}

/// Not Windows x86-64: no cycle clock.
#[cfg(not(all(windows, target_arch = "x86_64")))]
#[allow(dead_code)]
pub(crate) fn thread_cycle_nanos() -> Option<u64> {
    None
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn QueryThreadCycleTime(thread: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
    }

    /// The calibrated TSC rate (see the module note for why a `static`).
    static TSC_HZ: OnceLock<Option<u64>> = OnceLock::new();

    /// The calibration window: 2 ms of the performance counter.
    const CALIBRATION_DIVISOR: u64 = 500;

    fn qpc() -> Option<u64> {
        let mut v = 0i64;
        // SAFETY: `v` is a live, writable `LARGE_INTEGER`-sized local.
        let ok = unsafe { QueryPerformanceCounter(&mut v) };
        if ok == 0 {
            return None;
        }
        u64::try_from(v).ok()
    }

    fn tsc() -> u64 {
        // SAFETY: `_rdtsc` reads the time-stamp counter and touches no memory.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// Measure the TSC rate against the performance counter over
    /// [`CALIBRATION_DIVISOR`]'s window, once per process (the first span
    /// edge under the opt-in pays ~2 ms).
    fn calibrate() -> Option<u64> {
        let mut f = 0i64;
        // SAFETY: `f` is a live, writable `LARGE_INTEGER`-sized local.
        if unsafe { QueryPerformanceFrequency(&mut f) } == 0 {
            return None;
        }
        let qpf = u64::try_from(f).ok().filter(|f| *f > 0)?;
        let window = (qpf / CALIBRATION_DIVISOR).max(1);
        let (t0, q0) = (tsc(), qpc()?);
        let mut q1 = q0;
        // Bounded: the counter advances by `qpf` per second, so this ends
        // after ~2 ms; the iteration cap guards a counter that is stuck.
        for _ in 0..100_000_000u64 {
            q1 = qpc()?;
            if q1.saturating_sub(q0) >= window {
                break;
            }
            std::hint::spin_loop();
        }
        let t1 = tsc();
        super::tsc_hz_from((t0, t1), (q0, q1), qpf)
    }

    pub(super) fn thread_cycle_nanos() -> Option<u64> {
        let hz = (*TSC_HZ.get_or_init(calibrate))?;
        let mut cycles = 0u64;
        // SAFETY: `GetCurrentThread` returns a pseudo-handle valid for the
        // calling thread that needs no close; `cycles` is a live, writable
        // `ULONG64` in this frame.
        let ok = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
        if ok == 0 {
            return None;
        }
        super::cycles_to_nanos(cycles, hz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// zt t6/ob4: the scaling arithmetic, its refusals, and no overflow at
    /// a large cycle count.
    #[test]
    fn zt6ob4_cycle_scaling_and_tsc_rate_arithmetic() {
        assert_eq!(cycles_to_nanos(3_000_000_000, 3_000_000_000), Some(1_000_000_000));
        assert_eq!(cycles_to_nanos(2_000, 2_000_000_000), Some(1_000));
        assert_eq!(cycles_to_nanos(5, 0), None);
        assert_eq!(cycles_to_nanos(u64::MAX, 4_000_000_000), Some(4_611_686_018_427_387_903));
        // 2 ms of a 10 MHz counter against 4 million TSC ticks: 2 GHz.
        assert_eq!(
            tsc_hz_from((1_000, 4_001_000), (50, 20_050), 10_000_000),
            Some(2_000_000_000)
        );
        assert_eq!(tsc_hz_from((5, 9), (7, 7), 10_000_000), None, "no QPC step");
        assert_eq!(tsc_hz_from((9, 5), (7, 70), 10_000_000), None, "TSC ran back");
        assert_eq!(tsc_hz_from((5, 9), (7, 70), 0), None);
        assert_eq!(tsc_hz_from((5, 5), (7, 70), 10_000_000), None);
    }

    /// zt t6/ob4: the switch is opt-in (`=1`), and on Windows x86-64 the
    /// cycle clock reads a busy span shorter than one scheduler quantum as
    /// more than zero (the quantum clock reads such a span as 0 or ~15.6 ms).
    #[test]
    fn zt6ob4_cycle_clock_is_opt_in_and_reads_a_sub_quantum_span() {
        use cratonvm_types::flags::with_thread_overrides;
        assert!(!with_thread_overrides(
            &[("CRATONVM_ZGC_THREAD_CPU_CYCLES", None)],
            cycle_clock_requested
        ));
        assert!(with_thread_overrides(
            &[("CRATONVM_ZGC_THREAD_CPU_CYCLES", Some("1"))],
            cycle_clock_requested
        ));
        let Some(a) = thread_cycle_nanos() else {
            assert!(!cfg!(all(windows, target_arch = "x86_64")), "no reading on Windows x86-64");
            return;
        };
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed() < std::time::Duration::from_millis(2) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let b = thread_cycle_nanos().expect("a second reading");
        assert!(b > a, "a 2 ms busy span reads {} ns", b.saturating_sub(a));
        assert!(b - a < 60_000_000_000, "an absurd span: {} ns", b - a);
    }
}
