// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A self-sampling Java execution profiler, driven from the interpreter's
//! safepoint poll.
//!
//! # Why this exists
//!
//! There was no way to ask this VM where a workload's time goes.
//!
//! * `jdk.ExecutionSample` is fully defined in `cratonvm-jfr` — event type,
//!   field layout, two emitters — and **has no caller anywhere in `vm/`**. JFR
//!   execution profiling is wired and has never run. (`grep -rn
//!   'emit_execution_sample' vm/src` returns nothing.)
//! * The platform profiler is not available unelevated: `wpr -start CPU` fails
//!   with "Failed to enable the policy to profile system performance", CPU
//!   sampling on Windows is kernel-side, and `whoami /priv` carries no
//!   `SeSystemProfilePrivilege`.
//!
//! So an H2 measurement that wanted to know why a contended workload runs 21x
//! slower than HotSpot had to reason from ablation and code reading. Two
//! hypotheses died that way in one session — `--Xmx` as a speed lever, and
//! monitor tick-quantization — each after real time spent. This is the
//! instrument that would have answered both in one run.
//!
//! # How it samples, and the bias that follows
//!
//! Each thread samples ITSELF, at its own safepoint poll. That is what makes it
//! safe with no new synchronisation: `JvmThread::frames` is owned by the
//! running thread, and walking a PEER's frames outside a stop-the-world pause
//! is exactly the hazard `memory::roots` spends a hundred lines guarding.
//!
//! **State the bias, because it decides what the output means.** A thread is
//! sampled only when it reaches a safepoint poll, so:
//!
//! * a thread blocked in a native call, parked, or waiting on a monitor
//!   contributes NOTHING. This is a CPU profile, not a wall-clock profile. A
//!   workload that is slow because it waits will look idle here — and knowing
//!   that is itself the answer, because the totals will not add up to the
//!   wall clock.
//! * compiled code polls less often than the interpreter, so interpreted
//!   frames are over-represented relative to their true share of CPU. Read the
//!   RANKING of Java methods, not the absolute percentages, and do not use
//!   this to compare the interpreter against the JIT.
//!
//! `samples_total` and the wall clock together are what make both biases
//! legible: if the run took 400 s and the sampler took 900 samples at a
//! requested 10 ms interval, roughly 90 s of CPU was on a safepoint-polling
//! path and the rest was somewhere this instrument cannot see.
//!
//! # Cost when off
//!
//! One relaxed load of a cached `Option<u64>`. The interval is resolved once
//! through `OnceLock`; an unset flag leaves `None` and every call returns
//! before touching a clock or a frame.
//!
//! `CRATONVM_PROFILE_SAMPLE_MS=<n>` turns it on with an `n`-millisecond target
//! interval. 10 is a reasonable starting point for a multi-second workload.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use rustc_hash::FxHashMap;

use crate::threading::jvm_thread::JvmThread;

/// Requested sampling interval in nanoseconds, or `None` when the profiler is
/// off. Resolved once; see the module doc for the cost of the off case.
fn interval_ns() -> Option<u64> {
    static IV: OnceLock<Option<u64>> = OnceLock::new();
    *IV.get_or_init(|| {
        let raw = cratonvm_types::flags::runtime_var("CRATONVM_PROFILE_SAMPLE_MS").ok()?;
        let ms: u64 = raw.trim().parse().ok()?;
        (ms > 0).then(|| ms.saturating_mul(1_000_000))
    })
}

/// `Class.method` -> sample count. Keyed by the TOP frame only.
///
/// A top-frame histogram answers "which method is running", which is the
/// question a first profile should answer. Full stacks would answer "who
/// called it" and cost an allocation per sample; that is a second instrument,
/// and this one should not become it by accident.
static HISTOGRAM: OnceLock<Mutex<FxHashMap<String, u64>>> = OnceLock::new();

/// Samples taken, including ones with no Java frame on the stack.
static SAMPLES_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Samples where the thread held no Java frame at all — VM-internal work
/// reached through a safepoint poll with an empty stack. Counted rather than
/// dropped, so the histogram's percentages have an honest denominator.
static SAMPLES_NO_FRAME: AtomicU64 = AtomicU64::new(0);

fn histogram() -> &'static Mutex<FxHashMap<String, u64>> {
    HISTOGRAM.get_or_init(|| Mutex::new(FxHashMap::default()))
}

thread_local! {
    /// This thread's last sample, as a monotonic instant. Thread-local so the
    /// interval is per-thread: N threads sampling at 10 ms give N samples per
    /// 10 ms, which is what makes a contended workload's distribution across
    /// threads visible rather than aliased onto whichever thread polls first.
    static LAST: std::cell::Cell<Option<std::time::Instant>> = const {
        std::cell::Cell::new(None)
    };
}

/// Sample this thread if its interval has elapsed. Called from the
/// interpreter's safepoint poll.
///
/// Takes `&JvmThread` rather than `&mut` deliberately: this must never be able
/// to perturb the thread it is measuring.
#[inline]
pub fn maybe_sample(thread: &JvmThread) {
    let Some(iv) = interval_ns() else {
        return;
    };
    let now = std::time::Instant::now();
    let due = LAST.with(|c| match c.get() {
        Some(prev) if (now.duration_since(prev).as_nanos() as u64) < iv => false,
        _ => {
            c.set(Some(now));
            true
        }
    });
    if !due {
        return;
    }
    SAMPLES_TOTAL.fetch_add(1, Ordering::Relaxed);
    let Some(frame) = thread.frames.last() else {
        SAMPLES_NO_FRAME.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // One allocation per SAMPLE, not per safepoint poll — at 10 ms that is a
    // hundred per thread-second, which is noise beside what it measures.
    let key = format!("{}.{}", frame.class_name(), frame.method_name());
    if let Ok(mut h) = histogram().lock() {
        *h.entry(key).or_insert(0) += 1;
    }
}

/// `(total, no_frame)` — exposed so a caller can state the denominator.
pub fn sample_counts() -> (u64, u64) {
    (
        SAMPLES_TOTAL.load(Ordering::Relaxed),
        SAMPLES_NO_FRAME.load(Ordering::Relaxed),
    )
}

/// Print the profile at exit. Silent unless the profiler was armed.
///
/// Prints the DENOMINATOR first and unconditionally once armed: a ranking whose
/// total is unstated invites reading 40% of samples as 40% of the run, and on a
/// workload that spends its time blocked those are very different numbers.
pub fn report_at_exit() {
    if interval_ns().is_none() {
        return;
    }
    let (total, no_frame) = sample_counts();
    let iv_ms = interval_ns().unwrap_or(0) / 1_000_000;
    eprintln!(
        "[profile] execution samples: total={total} no_java_frame={no_frame} \
interval_ms={iv_ms} (CPU profile taken at safepoint polls -- a thread parked, \
blocked on a monitor or inside a native call contributes NOTHING, so \
total*interval UNDER-counts wall clock by exactly the time spent waiting)"
    );
    if total == 0 {
        eprintln!(
            "[profile] no samples: the workload never reached an interpreter \
safepoint poll with the profiler armed"
        );
        return;
    }
    let Ok(h) = histogram().lock() else {
        return;
    };
    let mut rows: Vec<(&String, &u64)> = h.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let attributed: u64 = rows.iter().map(|(_, c)| **c).sum();
    for (name, count) in rows.iter().take(25) {
        let pct = (**count as f64) * 100.0 / (attributed.max(1) as f64);
        eprintln!("[profile]   {pct:6.2}%  {count:>8}  {name}");
    }
    eprintln!(
        "[profile] {} distinct methods, {attributed} attributed samples",
        rows.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The off path must not read a clock.
    ///
    /// Not a timing assertion — those are unreliable on a loaded box, and this
    /// file's whole purpose is measuring one. It asserts the property that
    /// makes the off path free: the interval resolves to `None` when the flag
    /// is unset, which is the branch `maybe_sample` returns on.
    #[test]
    fn the_profiler_is_off_without_its_flag() {
        if cratonvm_types::flags::runtime_var("CRATONVM_PROFILE_SAMPLE_MS").is_ok() {
            return; // armed in this environment; nothing to assert
        }
        assert!(interval_ns().is_none());
        let (total, _) = sample_counts();
        assert_eq!(total, 0, "an unarmed profiler must take no samples");
    }

    /// A zero or unparseable interval is OFF, not a divide-by-zero or a
    /// sample-every-poll storm.
    #[test]
    fn a_zero_interval_reads_as_off() {
        // `interval_ns` is latched, so this exercises the parse rule directly
        // rather than through the cache.
        let parse = |s: &str| -> Option<u64> {
            let ms: u64 = s.trim().parse().ok()?;
            (ms > 0).then(|| ms.saturating_mul(1_000_000))
        };
        assert_eq!(parse("0"), None);
        assert_eq!(parse("nonsense"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("10"), Some(10_000_000));
    }
}
