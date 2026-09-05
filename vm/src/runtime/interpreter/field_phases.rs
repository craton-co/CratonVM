// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cycle breakdown of ONE quickened `getfield`, gated by
//! `CRATONVM_DBG_FIELD_PHASES=1`.
//!
//! # Why this exists
//!
//! Instance field access is the interpreter's worst remaining ratio, and
//! **four** structural explanations for it have now been proposed from reading
//! the code and refuted by measurement:
//!
//! 1. the object-start bitmap probe is a cache miss — refuted, the probe is
//!    worth 1-4 ns and the interpreter's own per-bytecode cost hides a miss;
//! 2. eliding that probe restores parity with the handler — refuted, and
//!    worse than refuted: `op_getfield` reaches `load_and_forward`, which
//!    performs the same probe, so the elide removed a real check;
//! 3. the compact body layout costs more than the legacy one — refuted, no
//!    separation, the control moved as much as the arms;
//! 4. the `SiteCache` is a hashed 1024-slot table so cost should rise with the
//!    number of distinct sites — refuted, `probes/SiteSpread.java` puts 256
//!    sites against 1 at **0.874x**, against HotSpot's flat 1.000 control.
//!
//! Four hypotheses, four refutations, zero attribution. That is the signature
//! of a missing instrument, not of a hard problem: varying the Java program
//! cannot separate the site lookup from the header compares from the read,
//! because every arm of the fast path runs on every access.
//!
//! `invoke_phases` is the same instrument for the call path, written after the
//! same thing happened there — its own note says "reading-derived rankings
//! have already cost this workspace one withdrawn claim". This is that lesson
//! applied one opcode over.
//!
//! # What it is honest about
//!
//! `rdtsc` costs roughly 20-30 cycles against a field access of perhaps 100,
//! so four reads inflate what they measure substantially and inflate every
//! phase EQUALLY in absolute terms — which biases the *shares* toward the
//! short phases. This is a **ranking instrument, not a costing one**. A phase
//! reading 40% is genuinely the largest; 6% against 9% is not meaningfully
//! smaller. [`P_CALIB`] measures two back-to-back reads on this workload so
//! the overhead is subtracted rather than assumed, and [`dump`] prints both.
//!
//! Anything it points at still needs a one-binary A/B on a real workload. On
//! this branch that discipline caught a change that looked clean and measured
//! nothing (the array autobox latch) and one that measured well and was
//! removing a safety check (the field registry elide).

use std::sync::atomic::{AtomicU64, Ordering};

/// Entry through the JVMTI watchpoint gate, the stack-depth check and the
/// receiver peek. All of it is per-access work that no site can memoize.
pub const P_GATES: usize = 0;
/// `FastFieldSiteCache::get`: the redefine latch, the two epoch loads and
/// their compares, the multiply-shift slot index, and the tag comparison
/// against the entry. The refuted site-count hypothesis was about this phase;
/// if it reads small, that refutation has a second, independent witness.
pub const P_SITE: usize = 1;
/// `field_ptr_for`: the object-start registry probe plus the class id, slot
/// count and compact-flag comparisons against the site.
pub const P_PTR: usize = 2;
/// The read itself and the operand-stack traffic around it: the storage-kind
/// match, the load at the field's width, and the `pop_compact` /
/// `push_compact` pair with their bounds checks and kind writes.
pub const P_READ: usize = 3;
/// CALIBRATION: two back-to-back [`now`] calls measuring nothing.
pub const P_CALIB: usize = 4;

const N: usize = 5;
const NAMES: [&str; N] = [
    "gates       ",
    "site_lookup ",
    "field_ptr   ",
    "read+stack  ",
    "CALIB(noop) ",
];

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static CYCLES: [AtomicU64; N] = [ZERO; N];
static ACCESSES: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn on() -> bool {
    static STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    let s = STATE.load(Ordering::Relaxed);
    if s != 0 {
        return s == 2;
    }
    on_init(&STATE)
}

#[cold]
#[inline(never)]
fn on_init(state: &std::sync::atomic::AtomicU8) -> bool {
    let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FIELD_PHASES").is_some();
    state.store(if on { 2 } else { 1 }, Ordering::Relaxed);
    on
}

/// Current cycle counter, or 0 when the instrument is off. Not serialising,
/// for [`crate::runtime::interpreter::invoke_phases::now`]'s reason: an
/// `lfence` around every read would cost more than the phases being measured.
#[inline]
pub fn now() -> u64 {
    if !on() {
        return 0;
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `_rdtsc` reads the timestamp counter and touches no memory; it
    // is `unsafe` only because it is a target intrinsic.
    unsafe {
        std::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

thread_local! {
    /// Per-thread accumulation, flushed to the shared counters in batches.
    ///
    /// # Why this is not a shared `fetch_add`
    ///
    /// The first version charged straight into [`CYCLES`]. A Relaxed
    /// `fetch_add` on x86 is a `lock xadd` — tens of cycles — and there are
    /// four of them per access on a path that costs about a hundred. The
    /// instrument was a large fraction of what it measured, and it showed:
    /// every phase read within one cycle of every other (50.1 / 49.2 / 49.5 /
    /// 49.9, four equal quarters) for phases that do visibly different amounts
    /// of work. Four equal quarters is what an instrument reports when it is
    /// measuring its own boundaries rather than the code between them.
    ///
    /// `Cell<u64>` accumulation costs a load, an add and a store, and the
    /// shared atomics are touched once per [`FLUSH_EVERY`] accesses.
    static LOCAL: [std::cell::Cell<u64>; N] = [const { std::cell::Cell::new(0) }; N];
    static LOCAL_N: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Accesses between flushes of the thread-local accumulators into the shared
/// counters. A thread that exits mid-batch loses at most this many accesses'
/// worth, which is noise against the millions these probes run.
const FLUSH_EVERY: u64 = 4096;

/// Charge `end - start` to `phase`. No-op when off, and when the counter went
/// backwards (a migration between cores with unsynchronised TSCs).
#[inline]
pub fn charge(phase: usize, start: u64, end: u64) {
    if !on() || end <= start {
        return;
    }
    let d = end - start;
    LOCAL.with(|l| l[phase].set(l[phase].get() + d));
}

#[inline]
pub fn count_access() {
    if !on() {
        return;
    }
    let n = LOCAL_N.with(|c| {
        let v = c.get() + 1;
        c.set(v);
        v
    });
    if n % FLUSH_EVERY == 0 {
        flush();
    }
}

/// Fold this thread's accumulators into the shared counters and reset them.
#[inline(never)]
#[cold]
fn flush() {
    LOCAL.with(|l| {
        for (i, c) in l.iter().enumerate() {
            let v = c.replace(0);
            if v != 0 {
                CYCLES[i].fetch_add(v, Ordering::Relaxed);
            }
        }
    });
    let n = LOCAL_N.with(|c| c.replace(0));
    if n != 0 {
        ACCESSES.fetch_add(n, Ordering::Relaxed);
    }
}

/// Final tally. Prints **zeros too** — a phase reading 0 is the finding that
/// it never executed, which is exactly what a silent instrument would hide.
pub fn dump() {
    if !on() {
        return;
    }
    // The dumping thread's own partial batch; other threads' tails are lost,
    // which is bounded by `FLUSH_EVERY` per thread.
    flush();
    let n = ACCESSES.load(Ordering::Relaxed);
    if n == 0 {
        eprintln!(
            "[field-phases] accesses=0 — the quickened arm never ran. Check \
             `CRATONVM_DBG_FIELD_SITE=1` for whether it was declining, and on \
             which screen."
        );
        return;
    }
    let total: u64 = (0..N - 1).map(|i| CYCLES[i].load(Ordering::Relaxed)).sum();
    let calib = CYCLES[P_CALIB].load(Ordering::Relaxed) as f64 / n as f64;
    eprintln!("[field-phases] quickened getfield accesses={n} total_cycles={total}");
    for (i, name) in NAMES.iter().enumerate() {
        let per = CYCLES[i].load(Ordering::Relaxed) as f64 / n as f64;
        let corrected = (per - calib).max(0.0);
        let pct = if total > 0 && i != P_CALIB {
            per * n as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        eprintln!("[field-phases]   {name} {per:8.1} raw  {corrected:8.1} corrected  {pct:5.1}% raw");
    }
    eprintln!(
        "[field-phases]   {:8.1} cyc/access measured (rdtsc overhead INCLUDED; ranking, not costing)",
        total as f64 / n as f64
    );
}
