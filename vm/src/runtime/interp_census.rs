//! Two censuses that answer "what is the interpreter actually running, and why
//! was it never nominated for compilation".
//!
//! Both are DEFAULT-OFF and cost one relaxed `AtomicBool` load each on their
//! hot path. They exist because the composition page
//! (`known-issues/perf/completablefuture-composition-is-20x-and-5-percent-compiled-20260901.md`)
//! reached a profile whose largest bucket is "the interpreter and the plumbing
//! around it" with no instrument able to name a single METHOD in that bucket:
//! `jit-method-stats` ranges only over NOMINATED methods, so its population is
//! the answer's complement.
//!
//! * `CRATONVM_DBG_INTERP_FRAMES=1` — one row per method per interpreted frame
//!   push. This is the census of what runs interpreted.
//! * `CRATONVM_DBG_TIERUP_DECLINE=1` — one row per cached `invokevirtual`
//!   dispatch, keyed by the FIRST condition in `execute_invokevirtual_cached`'s
//!   tier-up chain that refused it. A method that appears here with a large
//!   count and a reason other than `admitted` is a method the invocation
//!   counter never saw, which is why lowering `CRATONVM_JIT_THRESHOLD` cannot
//!   reach it.
//!
//! Both dump at exit from `vm-cli`'s report block, sorted by count.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

static INTERP_FRAMES_ON: AtomicU8 = AtomicU8::new(0);
static TIERUP_DECLINE_ON: AtomicU8 = AtomicU8::new(0);

/// 0 = not yet read, 1 = off, 2 = on. Self-initialising rather than wired into
/// `SharedVm::new`: the two hot sites are reached from several entry points
/// (including the ones a unit test drives directly), and a gate that depends on
/// an init call it might not get is a gate that reads OFF for reasons that have
/// nothing to do with the switch.
#[inline(always)]
fn gate(cell: &AtomicU8, var: &str) -> bool {
    match cell.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_var_os(var).is_some();
            cell.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

#[inline(always)]
pub fn interp_frames_enabled() -> bool {
    gate(&INTERP_FRAMES_ON, "CRATONVM_DBG_INTERP_FRAMES")
}

#[inline(always)]
pub fn tierup_decline_enabled() -> bool {
    gate(&TIERUP_DECLINE_ON, "CRATONVM_DBG_TIERUP_DECLINE")
}

static DIRECT_BINDS_ON: AtomicU8 = AtomicU8::new(0);

/// `CRATONVM_DBG_DIRECT_BINDS=1` — the thin-direct-call engagement census.
#[inline(always)]
pub fn direct_binds_enabled() -> bool {
    gate(&DIRECT_BINDS_ON, "CRATONVM_DBG_DIRECT_BINDS")
}

/// `Integer.intValue()` thin-direct-call engagement.
///
/// `sites_bound` counts BIND EVENTS, not distinct call sites: a method compiled
/// at C1 and again at C2 binds its sites twice. `HibfixComposeProbe2` has two
/// `intValue` sites in `lambda$chain$0` and reports 4.
///
/// `[0]` calls served by the helper's own field-0 read, `[1]` calls the helper
/// declined back to `jit_invoke_dispatch`. The compile-time site count lives in
/// the `jit` crate (`cratonvm_jit::integer_int_value_direct_sites`), because
/// two of the three doors that bind it are there.
pub static INT_VALUE_DIRECT: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `Long.longValue()` sibling of [`INT_VALUE_DIRECT`].
pub static LONG_VALUE_DIRECT: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Count one served (`declined = false`) or declined (`true`) direct call.
/// Caller has already tested [`direct_binds_enabled`].
#[inline]
pub fn note_int_value_direct(declined: bool) {
    INT_VALUE_DIRECT[usize::from(declined)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// [`note_int_value_direct`] for the `Long.longValue()` helper.
#[inline]
pub fn note_long_value_direct(declined: bool) {
    LONG_VALUE_DIRECT[usize::from(declined)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

type Census = Mutex<HashMap<String, u64>>;

fn interp_census() -> &'static Census {
    static C: OnceLock<Census> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn decline_census() -> &'static Census {
    static C: OnceLock<Census> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one interpreted frame push. Caller has already tested
/// [`interp_frames_enabled`].
#[cold]
pub fn record_interp_frame(class: &str, method: &str, descriptor: &str) {
    let key = format!("{class}.{method}{descriptor}");
    if let Ok(mut m) = interp_census().lock() {
        *m.entry(key).or_insert(0) += 1;
    }
}

/// Record one cached `invokevirtual` dispatch and the first tier-up condition
/// that refused it (`"admitted"` when none did). Caller has already tested
/// [`tierup_decline_enabled`].
#[cold]
pub fn record_tierup_decline(reason: &str, class: &str, method: &str, descriptor: &str) {
    let key = format!("{reason} {class}.{method}{descriptor}");
    if let Ok(mut m) = decline_census().lock() {
        *m.entry(key).or_insert(0) += 1;
    }
}

fn dump(label: &str, c: &Census, top: usize) {
    let Ok(m) = c.lock() else { return };
    if m.is_empty() {
        return;
    }
    let mut rows: Vec<(&String, &u64)> = m.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let total: u64 = rows.iter().map(|(_, n)| **n).sum();
    eprintln!("[{label}] {} distinct rows, {total} total", rows.len());
    for (k, n) in rows.iter().take(top) {
        eprintln!("[{label}] {n:>12} {k}");
    }
}

/// Print both censuses. Called from `vm-cli`'s exit report block; a census
/// that was never armed prints nothing.
/// A trapped compiled frame the sinks could not rebuild re-ran its method FROM
/// ENTRY — side effects included. Say so, always.
///
/// # Why this one is not behind a debug flag
///
/// Everything else in this file is a diagnostic: you turn it on because you are
/// already looking. This is not that. It is a silent WRONG ANSWER — a store, a
/// call or a monitor action that happened twice because a deopt could not be
/// resumed and the sink re-entered the method at bci 0
/// (`jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`). The
/// 2026-09-07 fix resumes wherever the frame CAN be rebuilt, which is the
/// common case; what is left is this, and leaving it behind
/// `CRATONVM_DBG_JITC` would keep the residual exactly as invisible as the
/// defect was.
///
/// One line, only when the count is non-zero, naming the reasons. A clean run
/// prints nothing.
fn report_unrebuildable_frames() {
    let bails = crate::runtime::interpreter::deopt_frame_bail_counts();
    let total: u64 = bails.iter().map(|(_, n)| *n).sum();
    if total == 0 {
        return;
    }
    let detail = bails
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(why, n)| format!("{why}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "[cratonvm] WARNING: {total} trapped compiled frame(s) could not be rebuilt and their \
         methods RE-RAN FROM ENTRY, repeating any side effect committed before the trap: \
         {detail}. See internal/fixed-bugs/\
         jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md."
    );
}

pub fn report_at_exit() {
    report_unrebuildable_frames();
    dump("interp-frames", interp_census(), 60);
    dump("tierup-decline", decline_census(), 60);
    // C1→C2 supersede engagement. `unchanged`/`first-publish` are the two
    // outcomes that cannot invalidate anything, and `ic_evictions` is what the
    // epoch bump actually costs — the number of `Jit` invoke-cache entries it
    // threw away process-wide. Printed together because the second is the only
    // thing that makes the first two worth acting on.
    let (first_publish, unchanged, changed) =
        crate::runtime::interpreter::jit_bridge::supersede_census();
    if crate::runtime::env_cache::dbg_jitc() && first_publish + unchanged + changed > 0 {
        eprintln!(
            "[c2-supersede] publishes: first_publish={first_publish} unchanged={unchanged} changed={changed}; ic_evictions_from_epoch={}",
            cratonvm_classloading::epoch_stale_evictions(),
        );
        let (ft_n, ft_us, ok_n, ok_us) =
            crate::runtime::interpreter::jit_bridge::c2_compile_census();
        eprintln!(
            "[c2-supersede] compiles: lowered={ok_n} ({} ms) fell_through_to_single_pass={ft_n} ({} ms)",
            ok_us / 1000,
            ft_us / 1000,
        );
        // Allocation lowering, so a run that reports "no crash" can be told
        // apart from a run where the path never executed. `emit_inline_tlab_new_ir`
        // is unreachable unless `CRATONVM_JIT_C2_ALLOC_UPGRADE=1` lets an
        // allocation-bearing method reach the optimizing tier at all, and a
        // clean result from a sequence that never ran is the shape of a zero
        // that means nothing.
        let (inline_bump, stub_only) = cratonvm_jit::runtime_lowering::ir_alloc_site_counts();
        eprintln!(
            "[c2-supersede] ir alloc sites: inline_tlab_bump={inline_bump} stub_only={stub_only}"
        );
        for (why, n) in cratonvm_jit::runtime_lowering::inline_tlab_declines() {
            eprintln!("[c2-supersede]   inline-TLAB declined {n}x: {why}");
        }
        // Site-level refusals replaced by an uncommon trap, and call-site
        // intrinsics lowered as arithmetic. Both always printed, including all
        // three trap rows as zeros: a zero on one row alone cannot be told from
        // "this workload has no such site", and the refused count is the work
        // list for whoever extends `try_ir_scalar_intrinsic`.
        let traps = cratonvm_jit::ir::ir_trap_census();
        let trap_line = traps
            .iter()
            .map(|(cause, n)| format!("{cause}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        // REFUSED, beside PLANTED, for the same reason PLANTED is printed as
        // all three rows including zeros: a planted count on its own cannot
        // tell "this workload has no such site" from "every such site was
        // declined", and those want opposite next steps.
        //
        // It is also the price tag of `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD`.
        // Every refusal is one optimizing body handed back to the single-pass
        // tier, and the guard's own doc claims that cost is "countable rather
        // than argued about" — which was not true while nothing read the
        // counter. `ir_trap_refusal_census` landed with no reader in
        // 7ade4a87c; this is that reader.
        let refused_line = cratonvm_jit::ir::ir_trap_refusal_census()
            .iter()
            .map(|(cause, n)| format!("{cause}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        // TAKEN, beside PLANTED. The planting doc promised this half and did
        // not have it: "a cause whose taken count is not ~0 has had its
        // coldness argument refuted". A non-zero number means a trap sat on a
        // LIVE path, which is the falsifiable form of that claim.
        eprintln!(
            "[c2-supersede] ir site traps planted: {trap_line} | REFUSED as unresumable: \
             {refused_line} | TAKEN at runtime: {}",
            cratonvm_jit::ir::site_traps_taken(),
        );
        let repeats = cratonvm_jit::ir::site_trap_repeats();
        if repeats > 0 {
            eprintln!(
                "[c2-supersede] ir site traps re-fired after the decision: {repeats} (a caller frame still holds a baked CALL to the trapping body)"
            );
        }
        // Trapped frames the sinks could NOT rebuild, confirmed at zero.
        //
        // The non-zero case is reported unconditionally by
        // `report_unrebuildable_frames` above and is NOT repeated here; this
        // line exists so a diagnostic run can tell "the residual is empty"
        // apart from "the census is not wired up", which are the same silence.
        if crate::runtime::interpreter::deopt_frame_bail_total() == 0 {
            eprintln!(
                "[c2-supersede] trapped frames that could not be rebuilt: 0 (every trap this \
                 run resumed precisely)"
            );
        }
        let (lowered, refused) = cratonvm_jit::ir::scalar_intrinsic_census();
        eprintln!(
            "[c2-supersede] call-site intrinsics: lowered_as_arithmetic={lowered} refused_method={refused}"
        );
        // The guarded slot-0 accessors (`Op::Unbox`): the unboxing pair and the
        // four `Atomic*` families. Counted on their own line rather than folded
        // into `lowered_as_arithmetic`, because they are not arithmetic -- they
        // are guarded memory ops, and three of them WRITE. They came off the
        // `refused_method` work list, so the two numbers have to be readable
        // against each other.
        //
        // `note_unbox_lowered` existed from the day the op landed and nothing
        // printed it, which is the "instrument armed where nobody reads it"
        // shape this census exists to avoid: a zero here now means the emitter
        // found no sites, and that is a different statement from silence.
        eprintln!(
            "[c2-supersede] unbox accessors: lowered_inline={}",
            cratonvm_jit::ir::unbox_lowered()
        );
        // The branch-profile window. `still_open` at exit should be ~0: a
        // nomination that opens the window and never closes it pins branch
        // recording on for the rest of the process, which is the global cost
        // the window exists to avoid.
        let (wo, wc, ws) = cratonvm_jit::profile::c2_branch_window_census();
        eprintln!(
            "[c2-supersede] branch-profile window: opened={wo} closed={wc} still_open={ws}"
        );
        // The C1->C2 acceptance gate. `unjudged` is the one to read first: a
        // non-zero there means the per-compile evidence slot was not armed on
        // the thread that compiled, so the gate accepted without judging --
        // which is the fail-open this design chose deliberately, and a number
        // that should be zero.
        let (acc, ref_ev, ref_pol, unjudged) = cratonvm_jit::ir_evidence::census();
        eprintln!(
            "[c2-supersede] acceptance: accepted={acc} refused_no_evidence={ref_ev} refused_by_policy={ref_pol} unjudged={unjudged} memo_skips={} supersedes_abandoned={}",
            cratonvm_jit::ir_evidence::memo_skips(),
            cratonvm_jit::ir_evidence::supersedes_abandoned(),
        );
        // The split the refusal count needs beside it: of the bodies refused
        // for want of evidence, how many had `ir_optimize` actually remove
        // nodes? A large `simplified` means the evidence list is too narrow;
        // a large `inert` means the tier really did nothing on those methods.
        let (ref_simpl, ref_inert) = cratonvm_jit::ir_evidence::refusal_split();
        eprintln!(
            "[c2-supersede] refusals by optimizer activity: simplified={ref_simpl} inert={ref_inert}"
        );
        // The third refusal reason, and the one that means the opposite of the
        // other two: these bodies DID carry evidence the list accepts and were
        // refused anyway, because the transform they carried made them slower.
        // A non-zero count is the priced gate catching what the transform list
        // alone published -- see `ir_evidence`'s header for the 897 ms against
        // 338 ms that motivated pricing it.
        let (cost_ref, cost_ns) = cratonvm_jit::ir_evidence::cost_regression_census();
        eprintln!(
            "[c2-supersede] refused as a cost regression: bodies={cost_ref} est_ns_per_execution_declined={cost_ns}"
        );
        // Array guard elision. Elided AND emitted on both rows, always: an
        // elision count alone cannot tell a working pass from a workload that
        // compiles no array accesses in this tier.
        let (ne, nm, be, bm) = cratonvm_jit::ir_check_elim::census();
        eprintln!(
            "[c2-supersede] ir array guards: null_elided={ne} null_emitted={nm} bounds_elided={be} bounds_emitted={bm}"
        );
        // Split the elisions by which pass proved them. Dominating redundancy
        // can only ever remove a SECOND access; the range pass is the only one
        // that removes a first. A single total moves for either reason.
        eprintln!(
            "[c2-supersede] ir bounds elisions by range proof: {} (rest are dominating-redundancy)",
            cratonvm_jit::ir_check_elim::range_census(),
        );
        // WHY the rest were not provable. "Extend the range pass" is four
        // separate decisions with very different costs, and this says which
        // one is actually holding the checks.
        let refusals = cratonvm_jit::ir_check_elim::refusal_census();
        if !refusals.is_empty() {
            let body: Vec<String> = refusals
                .iter()
                .map(|(name, n)| format!("{name}={n}"))
                .collect();
            eprintln!(
                "[c2-supersede] ir bounds range refusals: {}",
                body.join(" ")
            );
        }
        eprintln!(
            "[c2-supersede] ir aastore sites lowered: {}",
            cratonvm_jit::ir_lower::ir_aastore_census(),
        );
        // Block-exit shape. Read as a RATIO: `elided` alone cannot separate a
        // layout that is working from a method whose blocks were already in
        // source order, and until the elision existed every edge ended in an
        // explicit `JMP` — so frequency-driven block layout could not pay,
        // whatever it reordered.
        let (ft_elided, ft_jmps) = cratonvm_jit::ir_lower::ir_fallthrough_census();
        eprintln!(
            "[c2-supersede] ir block exits: fell_through={ft_elided} jmp_emitted={ft_jmps}"
        );
        // Speculation. A zero with `CRATONVM_JIT_IR_SPECULATE=1` means no
        // branch in this workload was one-sided over the sample — a fact about
        // the program, not about the pass — and that is precisely what a bare
        // "nothing happened" cannot tell you.
        eprintln!(
            "[c2-supersede] ir cold branch arms pruned: {}",
            cratonvm_jit::ir::branch_prune_census(),
        );
        // Multi-return splicing. `bodies=0` is the DEFAULT reading — the
        // feature is off. With `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1` a zero
        // means no admitted callee had a second reachable `return`, which is a
        // fact about the workload rather than about the feature.
        let (mr_bodies, mr_edges) = cratonvm_jit::ir::multi_return_splice_census();
        eprintln!(
            "[c2-supersede] ir multi-return spliced bodies: bodies={mr_bodies} return_edges={mr_edges}"
        );
        // Reference residency. `admitted=0` under
        // `CRATONVM_JIT_IR_REF_RESIDENCY=1` means no reference in this workload
        // was worth a register; `admitted>0 dropped=0` would mean the
        // invalidation is not wired, which is the one reading that must never
        // be silent — it is a stale-oop bug, not a missed optimization.
        let (ref_admitted, ref_dropped) = cratonvm_jit::ir_lower::ir_ref_residency_census();
        eprintln!(
            "[c2-supersede] ir reference residency: admitted={ref_admitted} copies_dropped={ref_dropped}"
        );
        // Calls this tier lowered to `jit_invoke_dispatch` -- a name
        // resolution per execution. `in_splice` is the row to act on: a splice
        // exists to delete a frame, and a resolution costs far more than the
        // frame it removed, so a non-zero here means the optimizing body is
        // very likely SLOWER than the single-pass one for that method. It read
        // non-zero for every spliced statically-bound call until 2026-09-09;
        // see `c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md`.
        let (bd_own, bd_splice) = cratonvm_jit::ir_lower::ir_blind_dispatch_census();
        eprintln!(
            "[c2-supersede] ir blind dispatches: own_code={bd_own} in_splice={bd_splice}"
        );
        let (held, spent, retired) = cratonvm_jit::deferred_new_retry_census();
        eprintln!(
            "[c2-supersede] deferred-new retries: held={held} spent={spent} retired={retired} re_offered={}",
            crate::runtime::interpreter::jit_bridge::deferred_new_reoffered(),
        );
    }
    if direct_binds_enabled() {
        eprintln!(
            "[direct-binds] Integer.intValue: sites_bound={} served={} declined_to_dispatch={}",
            cratonvm_jit::integer_int_value_direct_sites(),
            INT_VALUE_DIRECT[0].load(Ordering::Relaxed),
            INT_VALUE_DIRECT[1].load(Ordering::Relaxed),
        );
        eprintln!(
            "[direct-binds] Long.longValue: sites_bound={} served={} declined_to_dispatch={}",
            cratonvm_jit::long_long_value_direct_sites(),
            LONG_VALUE_DIRECT[0].load(Ordering::Relaxed),
            LONG_VALUE_DIRECT[1].load(Ordering::Relaxed),
        );
    }
}
