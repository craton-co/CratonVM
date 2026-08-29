// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cycle breakdown of ONE interpreted `invokestatic`, gated by
//! `CRATONVM_DBG_INVOKE_PHASES=1`.
//!
//! # Why this exists
//!
//! `probes/InvokeFrameCostProbe.java` established that a CratonVM interpreted
//! call carries a **fixed ~206ns** that does not depend on the callee's shape,
//! against HotSpot's ~6.9ns — roughly 30x, where the same host runs `iadd` at
//! only 4.1x. It also REJECTED the two shape-dependent explanations (frame
//! locals-init and operand-stack depth both cost CratonVM proportionally LESS
//! than HotSpot). So the remaining cost is per-call work that no Java-level
//! probe can subdivide: varying the callee cannot separate the inline-cache
//! lookup from the frame build from the push.
//!
//! Reading the code produced three candidate culprits and no way to rank them,
//! and reading-derived rankings have already cost this workspace one withdrawn
//! claim ("final dispatch is 3.8x", which turned out to be host drift). Hence a
//! measurement.
//!
//! # What it is honest about
//!
//! `rdtsc` is not free — roughly 20-30 cycles per read, against a call that
//! costs ~600 cycles at 3GHz. With five reads the instrument inflates the call
//! it measures by something like 20%, and it inflates every phase EQUALLY in
//! absolute terms, which biases the *shares* toward the short phases.
//!
//! So this is a **ranking instrument, not a costing one**. A phase that reads
//! 40% here is genuinely the largest; a phase that reads 6% against another at
//! 9% is not meaningfully smaller. Any fix it points at still has to be proven
//! by a one-binary A/B on the real workload, exactly as the argument-scan hoist
//! was — that change looked like a clean win on per-argument cost and was a
//! net pessimisation until its fixed cost was found and removed.
//!
//! Every phase is also charged to the FIRST executed call as well as the
//! steady-state ones, so a short run over-reports the cold path. The probe runs
//! millions of calls; the cold ones are noise at that count.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Prologue: function entry through the inline-cache probe and its entry
/// `clone()` — the `FxHashMap<(ClassId, u16, bool)>` hash, the bucket probe,
/// and the ~40-64 byte enum copy with its 1-2 `Arc` refcount bumps. This is the
/// standing structural suspicion: the field/cast/new site caches are
/// direct-mapped arrays that index without hashing or cloning.
pub const P_IC_LOOKUP: usize = 0;
/// Inline-cache hit through to the bytecode dispatch arm: the redefine/JVMTI
/// staleness gate, the native-shadow and loader checks, and the `match`.
pub const P_GUARDS: usize = 1;
/// Argument popping and coercion — `ParamTags::of` plus the per-argument
/// `pop_arg_for_descriptor_checked`. Measured separately at ~34ns/arg, so on a
/// ZERO-argument call this phase should be near-empty; if it is not, the
/// per-call part of argument handling is bigger than the per-argument part.
pub const P_ARGS: usize = 2;
/// `Frame::new_pooled_cached` — pool pops, locals init, the `code` and cached
/// `Arc` clones, and building a **296-byte** `Frame`.
pub const P_FRAME_BUILD: usize = 3;
/// `push_frame_and_fire_entry` — moving those 296 bytes into the frame stack,
/// plus two gated listener checks.
pub const P_PUSH: usize = 4;

/// The whole `ireturn`/`return` arm: popping the return value, the JVMTI
/// method-exit hook, the frame recycle, and pushing the value to the caller.
///
/// This exists because the call-side phases summed to only about HALF an
/// uninstrumented call, and the remainder was attributed to "the callee body
/// and the return path" without either being measured. An unmeasured half is
/// not a small residual; it is where the answer might be.
pub const P_RET_TOTAL: usize = 5;
/// `pop_and_recycle_frame` alone, the dominant suspect inside `P_RET_TOTAL`:
/// `Frame`'s `Drop` (two `Arc` decrements — `code` and the cached method) plus
/// routing four `Vec` headers back to the thread-local pools.
pub const P_RET_RECYCLE: usize = 6;
/// CALIBRATION: two back-to-back `now()` calls measuring nothing at all.
///
/// Every other phase is inflated by roughly one `rdtsc` latency, and
/// `P_RET_TOTAL` by THREE, because it brackets the nested `P_RET_RECYCLE`
/// pair. Comparing a nested phase against a flat one without accounting for
/// that is not like-for-like, and at a plausible ~25 cyc per read it is enough
/// to invert the ranking. So the overhead is measured on this workload rather
/// than assumed, and `dump` prints each phase both raw and
/// overhead-corrected.
pub const P_CALIB: usize = 7;

const N: usize = 8;
const NAMES: [&str; N] = [
    "ic_lookup   ",
    "guards      ",
    "args        ",
    "frame_build ",
    "frame_push  ",
    "ret_total   ",
    "  ret_recycle",
    "CALIB(noop) ",
];

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static CYCLES: [AtomicU64; N] = [ZERO; N];
static CALLS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVOKE_PHASES").is_some()
    })
}

/// Current cycle counter, or 0 when the instrument is off.
///
/// Not serialising: a bare `rdtsc` can be reordered by the CPU, so a single
/// phase boundary is approximate. Over millions of calls the reordering is
/// noise against the totals, and the alternative (`lfence`/`cpuid` around every
/// read) would cost far more than the phases being measured.
#[inline]
pub fn now() -> u64 {
    if !on() {
        return 0;
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `_rdtsc` reads the timestamp counter and touches no memory. It is
    // `unsafe` only because it is a target intrinsic.
    unsafe {
        std::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

/// Charge `end - start` to `phase`. No-op when off, and when the counter went
/// backwards (a migration between cores with unsynchronised TSCs).
#[inline]
pub fn charge(phase: usize, start: u64, end: u64) {
    if !on() || end <= start {
        return;
    }
    CYCLES[phase].fetch_add(end - start, Ordering::Relaxed);
}

#[inline]
pub fn count_call() {
    if on() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Final tally. Prints **zeros too** — a phase reading 0 is the finding that it
/// never executed, which is exactly what a silent instrument would hide.
pub fn dump() {
    if !on() {
        return;
    }
    let calls = CALLS.load(Ordering::Relaxed);
    if calls == 0 {
        eprintln!("[invoke-phases] calls=0 — the instrumented path never ran");
        return;
    }
    // `P_RET_RECYCLE` is NESTED inside `P_RET_TOTAL`, so it is excluded from the
    // total and its percentage is of the total rather than an additional slice.
    // Summing all seven would double-count it and quietly inflate the whole
    // table.
    let total: u64 = (0..N)
        .filter(|i| *i != P_RET_RECYCLE && *i != P_CALIB)
        .map(|i| CYCLES[i].load(Ordering::Relaxed))
        .sum();
    // Measured cost of ONE `now()` pair on this workload. Each flat phase
    // carries one; `P_RET_TOTAL` carries three, because it brackets the nested
    // `P_RET_RECYCLE` pair.
    let calib = CYCLES[P_CALIB].load(Ordering::Relaxed) as f64 / calls as f64;
    eprintln!("[invoke-phases] instrumented invokestatic calls={calls} total_cycles={total}");
    for i in 0..N {
        let c = CYCLES[i].load(Ordering::Relaxed);
        let per = c as f64 / calls as f64;
        let pct = if total == 0 {
            0.0
        } else {
            100.0 * c as f64 / total as f64
        };
        let overhead = if i == P_CALIB {
            0.0
        } else if i == P_RET_TOTAL {
            calib * 3.0
        } else {
            calib
        };
        let corrected = (per - overhead).max(0.0);
        eprintln!(
            "[invoke-phases]   {} {per:8.1} raw  {corrected:8.1} corrected  {pct:5.1}% raw",
            NAMES[i]
        );
    }
    eprintln!(
        "[invoke-phases]   {:8.1} cyc/call measured (rdtsc overhead INCLUDED; ranking, not costing)",
        total as f64 / calls as f64
    );
    // Frame-kind split. `OwnedFrameMeta` is boxed, which trades one heap
    // allocation per `Owned` frame for 64 bytes off every frame — a trade that
    // is only correct while `Owned` stays rare. Printed so that stays checked.
    let (owned, cached) = crate::runtime::frame::frame_kind_counts();
    let tot = owned + cached;
    let pct = if tot == 0 {
        0.0
    } else {
        100.0 * owned as f64 / tot as f64
    };
    eprintln!("[invoke-phases] frames: owned={owned} cached={cached} owned_share={pct:.3}%");
}
