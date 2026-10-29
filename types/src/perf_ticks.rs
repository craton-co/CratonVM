// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t21/pl10 (`zgc-zt12pl4-perf-counters-what-is-left-*` item 10): the
//! unit of this VM's perf-data tick counters (`sun.os.hrt.frequency`, every
//! `Ticks` counter) and of `jdk.internal.perf.Perf.highResCounter` /
//! `highResFrequency`.
//!
//! HotSpot 25 counts `os::elapsed_counter()` ticks at `os::elapsed_frequency()`:
//! - Linux and macOS: nanoseconds (`javaTimeNanos`), frequency 1000000000;
//! - Windows: the performance counter (`QueryPerformanceCounter`), frequency
//!   `QueryPerformanceFrequency` (10000000 on Windows 10 and 11; the t14 rows
//!   read `info gc freq=10000000`).
//!
//! This VM keeps its times in nanoseconds (`Instant`; on Windows `Instant` is
//! itself the performance counter scaled to nanoseconds). So the tick writers
//! scale nanoseconds to this unit with [`ticks_from_nanos`] /
//! [`ticks_from_millis`], and the image, `Perf.highResFrequency` and every tick
//! counter agree on one frequency, [`hrt_frequency`]. With the 10 MHz counter
//! the scaling is exact (one tick is 100 ns, `Instant`'s own grain on
//! Windows), so `ticks * 1000 / frequency` gives the same whole milliseconds
//! the beans answer (`CompilationMXBean.getTotalCompilationTime()`, the
//! collectors' times).
//!
//! `CRATONVM_PERF_HRT_QPC_FREQUENCY` (default ON, `=0` kill; Windows only):
//! `=0` keeps nanoseconds on every OS, as before. Other OSes: nanoseconds,
//! HotSpot's own unit there.

/// Nanoseconds per second: the tick frequency off Windows (HotSpot's
/// `NANOSECS_PER_SEC`), and under `CRATONVM_PERF_HRT_QPC_FREQUENCY=0`.
pub const NANOS_FREQUENCY: i64 = 1_000_000_000;

/// The tick frequency (`sun.os.hrt.frequency`, `Perf.highResFrequency`): the
/// performance counter's on Windows, [`NANOS_FREQUENCY`] elsewhere, under the
/// kill, or if the OS refuses the query. The OS figure is read once per
/// process (it is fixed at boot); the switch is read per call.
pub fn hrt_frequency() -> i64 {
    #[cfg(windows)]
    {
        if crate::flags::runtime_flag_default_on("CRATONVM_PERF_HRT_QPC_FREQUENCY") {
            if let Some(frequency) = imp::performance_frequency() {
                return frequency;
            }
        }
    }
    NANOS_FREQUENCY
}

/// `nanos` in ticks at `frequency` (rounded down, saturating). Pure.
pub fn ticks_from_nanos_at(nanos: u64, frequency: i64) -> i64 {
    scale(u128::from(nanos), frequency, 1_000_000_000)
}

/// `millis` in ticks at `frequency` (saturating). Pure.
pub fn ticks_from_millis_at(millis: u64, frequency: i64) -> i64 {
    scale(u128::from(millis), frequency, 1_000)
}

/// `nanos` in ticks at [`hrt_frequency`].
pub fn ticks_from_nanos(nanos: u64) -> i64 {
    ticks_from_nanos_at(nanos, hrt_frequency())
}

/// `millis` in ticks at [`hrt_frequency`].
pub fn ticks_from_millis(millis: u64) -> i64 {
    ticks_from_millis_at(millis, hrt_frequency())
}

/// `value * frequency / per_second`, a non-positive frequency read as 1.
fn scale(value: u128, frequency: i64, per_second: u128) -> i64 {
    let frequency = u128::try_from(frequency).unwrap_or(0).max(1);
    i64::try_from(value.saturating_mul(frequency) / per_second).unwrap_or(i64::MAX)
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
    }

    /// A machine constant (fixed at boot and the same for every VM in the
    /// process, as `gc/src/zgc/thread_clock.rs`' TSC rate), not per-VM state.
    static FREQUENCY: OnceLock<Option<i64>> = OnceLock::new();

    pub(super) fn performance_frequency() -> Option<i64> {
        *FREQUENCY.get_or_init(|| {
            let mut frequency = 0i64;
            // SAFETY: `frequency` is a live, writable `LARGE_INTEGER`-sized local.
            let ok = unsafe { QueryPerformanceFrequency(&mut frequency) };
            (ok != 0 && frequency > 0).then_some(frequency)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// zt t21/pl10: the scaling is exact at HotSpot's Windows frequency
    /// (100 ns ticks) and the identity at nanoseconds; milliseconds scale
    /// the same way, and `ticks * 1000 / frequency` is the floor of the
    /// milliseconds, as `Management::ticks_to_ms` reads it.
    #[test]
    fn zt21pl10_ticks_scale_nanoseconds_to_the_frequency() {
        assert_eq!(
            ticks_from_nanos_at(1_234_567_891, NANOS_FREQUENCY),
            1_234_567_891
        );
        assert_eq!(ticks_from_nanos_at(1_234_567_891, 10_000_000), 12_345_678);
        assert_eq!(ticks_from_millis_at(20, 10_000_000), 200_000);
        assert_eq!(ticks_from_millis_at(20, NANOS_FREQUENCY), 20_000_000);
        for ns in [0u64, 99, 100, 999_999, 1_000_000, 470_123_456, 1 << 50] {
            let ticks = ticks_from_nanos_at(ns, 10_000_000);
            assert_eq!(
                u64::try_from(ticks).unwrap() * 1000 / 10_000_000,
                ns / 1_000_000,
                "ns={ns}"
            );
        }
        assert_eq!(
            ticks_from_nanos_at(u64::MAX, i64::MAX),
            i64::MAX,
            "saturates"
        );
        assert_eq!(
            ticks_from_nanos_at(5, 0),
            0,
            "a non-positive frequency is 1"
        );
    }

    /// zt t21/pl10: nanoseconds under the kill on every OS; off Windows
    /// always.
    #[test]
    fn zt21pl10_the_kill_keeps_nanoseconds() {
        crate::flags::with_thread_overrides(
            &[("CRATONVM_PERF_HRT_QPC_FREQUENCY", Some("0"))],
            || assert_eq!(hrt_frequency(), NANOS_FREQUENCY),
        );
        let on = crate::flags::with_thread_overrides(
            &[("CRATONVM_PERF_HRT_QPC_FREQUENCY", None)],
            hrt_frequency,
        );
        if cfg!(windows) {
            assert!(on > 0, "the performance counter's frequency: {on}");
        } else {
            assert_eq!(on, NANOS_FREQUENCY);
        }
    }
}
