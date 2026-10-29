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
/// Installing the callee's frame: pool pops, locals init, the `code` and
/// cached `Arc` clones, and getting a `Frame` into the stack.
///
/// Originally `Frame::new_pooled_cached` alone, with `P_PUSH` measuring the
/// move that followed. Since 2026-09-03 the general dispatchers install the
/// frame **in place** — rebuilding the retired slot at this depth, or writing
/// the struct into the next one — so there is no move to measure separately
/// and no boundary to bracket. The two phases were re-scoped rather than
/// merged, because what `P_PUSH` covered is exactly what has been removed and
/// a phase silently absorbing another's cycles is worse than one reading zero:
/// this is now everything up to the frame being live, and `P_PUSH` is the
/// tail that runs after it. `CRATONVM_JIT_NO_FRAME_EMPLACE` restores the
/// by-value build, and under it these two mean what they always did.
pub const P_FRAME_BUILD: usize = 3;
/// The post-install tail: the synchronized callee's monitor handoff, the frame
/// trace, the JVMTI `MethodEntry` event and the push-time diagnostics.
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

/// Every cached frame install inside `FrameStack` (wave 29, lane L7): the
/// retired-slot rebuilds (`push_cached_compact_reusing`,
/// `push_cached_value_reusing`) and the first-call-at-a-depth emplaces
/// (`emplace_cached_compact_args`, `emplace_cached_value_args`), whichever
/// door or dispatcher called them. The phases above bracket
/// `execute_invokestatic_cached` alone, which the fast doors bypass: on the
/// wave-29 stage-0 run it saw 344 of 11 M calls. This one sees them all, so the
/// slot slab's A/B (`CRATONVM_JIT_NO_LOCALS_SLAB`) is measured where it acts.
/// Its count is [`P_SLOT_BUILD_N`]; it is not part of the table's total.
pub const P_SLOT_BUILD: usize = 8;
/// How many installs [`P_SLOT_BUILD`] charged (a count, not cycles).
pub const P_SLOT_BUILD_N: usize = 9;
/// Every interpreter-frame pop through `pop_and_recycle_frame_with_reason`:
/// the recycle itself (`JvmThread::recycle_top_frame_in_place` -- the retire,
/// or the pooled harvest and truncate), for every return and unwind of every
/// path. Its count is [`P_SLOT_RET_N`].
pub const P_SLOT_RET: usize = 10;
/// How many pops [`P_SLOT_RET`] charged.
pub const P_SLOT_RET_N: usize = 11;

/// Stage 2 of the contiguous interpreter stack (argument overlap, wave 37
/// lane L7, `CRATONVM_JIT_OVERLAP_ARGS`): frames installed with their locals
/// on the caller's argument slots and nothing copied (a count).
pub const P_OVERLAP_N: usize = 12;
/// ... overlapped, but laid again from the door's copy because an argument
/// is a `long` / `double` (one operand-stack slot, two local slots).
pub const P_OVERLAP_RELAID_N: usize = 13;
/// ... asked for and refused (an owned caller, another slab chunk, no room);
/// the stage-1 window was built instead.
pub const P_OVERLAP_DECLINED_N: usize = 14;
/// Value returns that released an overlapped callee's view of its slots
/// before pushing into the caller (`Frame::release_caller_overlap`).
pub const P_OVERLAP_RELEASED_N: usize = 15;
/// Of the overlapped installs (both counts above), those laid IN PLACE with
/// no copy of the arguments (stage 2b, `FrameStack::push_cached_compact_in_place`,
/// interpreter round i1 wave 38, lane L7): the static and virtual doors with
/// `CRATONVM_JIT_OVERLAP_ARGS=1`.
pub const P_OVERLAP_IN_PLACE_N: usize = 16;

/// The phases of the invokestatic table (and its total).
const N: usize = 8;
/// Every counter, the frame-lifecycle pair and the overlap counts included.
const N_ALL: usize = 17;
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
static CYCLES: [AtomicU64; N_ALL] = [ZERO; N_ALL];
static CALLS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn on() -> bool {
    // Read up to five times per value return; one relaxed byte load, init cold.
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
    let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVOKE_PHASES").is_some();
    state.store(if on { 2 } else { 1 }, Ordering::Relaxed);
    on
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

/// Charge a frame install inside `FrameStack` that started at `start` (a
/// `now()` taken at its entry; 0 when the instrument is off, which makes this
/// one register test).
#[inline(always)]
pub fn charge_slot_build(start: u64) {
    if start != 0 {
        charge_counted(P_SLOT_BUILD, P_SLOT_BUILD_N, start);
    }
}

/// [`charge_slot_build`] for a frame pop.
#[inline(always)]
pub fn charge_slot_ret(start: u64) {
    if start != 0 {
        charge_counted(P_SLOT_RET, P_SLOT_RET_N, start);
    }
}

/// Count one argument-overlap outcome (`P_OVERLAP_*_N`) for an install that
/// started at `start` (0 when the instrument is off: one register test).
#[inline(always)]
pub fn note_overlap(start: u64, which: usize) {
    if start != 0 {
        bump(which);
    }
}

/// Count one [`P_OVERLAP_RELEASED_N`]. Called only on an overlapped frame's
/// value return, so the gate's byte load is paid only with the switch on.
#[inline]
pub fn note_overlap_released() {
    if on() {
        bump(P_OVERLAP_RELEASED_N);
    }
}

#[cold]
#[inline(never)]
fn bump(which: usize) {
    CYCLES[which].fetch_add(1, Ordering::Relaxed);
}

#[cold]
#[inline(never)]
fn charge_counted(phase: usize, count: usize, start: u64) {
    let end = now();
    if end > start {
        CYCLES[phase].fetch_add(end - start, Ordering::Relaxed);
    }
    CYCLES[count].fetch_add(1, Ordering::Relaxed);
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
    } else {
        dump_invokestatic_table(calls);
    }
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
    crate::runtime::frame::dump_frame_shape_census();
    dump_frame_lifecycle(calls);
}

/// The frame install and pop cost over EVERY cached install and every pop
/// ([`P_SLOT_BUILD`], [`P_SLOT_RET`]). Printed even when the invokestatic
/// table is empty, with zeros too.
fn dump_frame_lifecycle(calls: u64) {
    let per = |cyc: usize, n: usize| {
        let c = CYCLES[cyc].load(Ordering::Relaxed);
        let k = CYCLES[n].load(Ordering::Relaxed);
        (k, if k == 0 { 0.0 } else { c as f64 / k as f64 })
    };
    let (builds, build_cyc) = per(P_SLOT_BUILD, P_SLOT_BUILD_N);
    let (pops, pop_cyc) = per(P_SLOT_RET, P_SLOT_RET_N);
    // One `now()` pair per charge; the calibration row exists only when the
    // invokestatic table ran.
    let calib = if calls == 0 {
        0.0
    } else {
        CYCLES[P_CALIB].load(Ordering::Relaxed) as f64 / calls as f64
    };
    eprintln!(
        "[invoke-phases] frame lifecycle: installs={builds} install_cyc={build_cyc:.1} \
         pops={pops} pop_cyc={pop_cyc:.1} per event (raw; subtract CALIB={calib:.1} each)"
    );
    // Stage 2's positive control (wave 37, lane L7). All zero unless the VM
    // runs with `CRATONVM_JIT_OVERLAP_ARGS=1`; printed with zeros too.
    let n = |i: usize| CYCLES[i].load(Ordering::Relaxed);
    eprintln!(
        "[invoke-phases] arg overlap: overlapped={} relaid_cat2={} declined={} \
         released_at_return={} in_place={} (installs above include them)",
        n(P_OVERLAP_N),
        n(P_OVERLAP_RELAID_N),
        n(P_OVERLAP_DECLINED_N),
        n(P_OVERLAP_RELEASED_N),
        n(P_OVERLAP_IN_PLACE_N),
    );
}

/// The invokestatic phase table (everything `dump` printed before wave 29
/// besides the frame lines).
fn dump_invokestatic_table(calls: u64) {
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
}
