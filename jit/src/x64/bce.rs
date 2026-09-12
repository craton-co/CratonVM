// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Array bounds-check elimination.
//!
//! Moved verbatim out of `x64.rs`'s `Array Bounds Check Elimination (BCE)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

use crate::loop_analysis::{analyze_counted_loop, decode_bound_expr, MinMax};
use crate::scev::{
    BoundSource, BoundTerm, BoundsProof, CountedLoop, ExitCmp, IndexExpr, IntRange, LoopForm,
    PreheaderGuard, RangeEnv, RefusalReason, Stride,
};

/// Whether speculative (runtime-guarded) BCE is disabled via
/// `CRATONVM_JIT_NO_SPEC_BCE`. Cached in a `OnceLock` like the other env gates
/// in this file (e.g. `precise_jit_maps_enabled`) so the lookup is paid once
/// rather than per loop header on every compile.
thread_local! {
    pub(super) static INCLUSIVE_SPEC_BCE_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only override for [`inclusive_spec_bce_enabled`].
pub fn __set_inclusive_spec_bce_override(v: Option<bool>) {
    INCLUSIVE_SPEC_BCE_TEST_OVERRIDE.with(|c| c.set(v));
}

/// Whether inclusive (`iv <= bound`) counted loops may take the SOUND
/// speculative BCE guard (`array.length > bound` via JBE + the
/// `bound != Integer.MAX_VALUE` entry check). **Default OFF**
/// (`CRATONVM_JIT_INCLUSIVE_BCE=1` opts in): the machinery is correct
/// (probe-verified against HotSpot incl. the `iv == bound == length`
/// boundary), but on the memory-homed template bodies the elision is a
/// measured NET LOSS for the Sieve OSR artifact (6.4s -> 12.7s, ~2x) —
/// the per-element check it removes is a predicted-never-taken branch and
/// a cache-hit length load (~free), while shrinking every unrolled loop
/// body reshuffles code layout that this frontend-sensitive kernel is
/// hostage to. Re-evaluate when the backend gets register-homed loop
/// bodies or an IR-level BCE.
pub(super) fn inclusive_spec_bce_enabled() -> bool {
    if let Some(v) = INCLUSIVE_SPEC_BCE_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_INCLUSIVE_BCE")
            .map(|v| {
                let v = v.trim();
                v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
            })
            .unwrap_or(false)
    })
}

pub(super) fn jit_no_spec_bce() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_SPEC_BCE"))
}

/// Speculative bounds check elimination: a deopt guard emitted at the loop header.
/// For counted loops where the IV increases by 1 up to N, we speculatively
/// eliminate per-element bounds checks and instead emit a range check at the
/// loop header: `if (iv < 0 || array.length < loop_bound) goto deopt;`. The
/// `iv >= 0` half runs once per header; the length half once per guarded array
/// (see the per-array note on `covered_pcs`).
#[derive(Clone)]
pub(super) struct SpeculativeBCEGuard {
    /// Bytecode PC of the loop header where the guard should be emitted.
    pub(super) loop_header: usize,
    /// Local holding the array reference.
    pub(super) array_local: usize,
    /// Local holding the loop bound (N in `for i in 0..N`).
    pub(super) bound_local: usize,
    /// Local holding the induction variable. The header guard proves `iv >= 0`
    /// at loop entry; combined with `find_induction_variable`'s +1-only-step
    /// invariant, every elided index is non-negative. Without this check a
    /// `for (i = start; i < n; i++)` loop with a negative runtime `start`
    /// would silently access below the array base.
    pub(super) iv_local: usize,
    /// The array-access bytecode PCs whose per-element bounds check was elided
    /// on the strength of THIS guard (one guard per distinct array local per
    /// header — the guard proves `array.length >= bound` for ITS array only;
    /// a loop guard on `a.length` says nothing about `out.length`, see
    /// docs/known-issues/jit-bce-multi-array-oob-store-20260711.md). If the
    /// guard is dropped (per-bci de-spec), these PCs MUST be removed from
    /// `bounds_safe_pcs` so their per-element checks are restored.
    pub(super) covered_pcs: Vec<usize>,
    /// Whether the loop's exit comparator is inclusive (`iv <= bound`). The
    /// length guard must then prove `array.length > bound` (JBE deopt) — a
    /// `>= bound` guard is stale by one at `iv == bound` (SECURITY FIX V17,
    /// now guarded soundly instead of refusing the whole loop). The preheader
    /// additionally proves `bound != Integer.MAX_VALUE` for inclusive loops
    /// (at `iv == bound == MAX` the increment wraps negative while the exit
    /// test keeps passing — the interpreter throws AIOOBE on the wrapped
    /// index; elided code must deopt instead of accessing).
    pub(super) inclusive: bool,
    /// For a variable-stride IV (canonical `iv += step` compound assignment):
    /// the STEP's local index. The preheader proves `step >= 0` and
    /// `step <= Integer.MAX_VALUE - bound` (deopt reason 2 on failure) so
    /// every elided index is monotonically non-decreasing from the checked
    /// non-negative entry value and can never wrap past the exit test —
    /// without this a runtime-NEGATIVE step walks the elided index below
    /// zero (an OOB write below the array base). `None` for `iinc iv, 1`.
    pub(super) step_local: Option<usize>,
}

/// Find induction variables in a loop body.
/// An induction variable is a local that is:
/// 1. Modified ONLY by `iinc local, 1` (increment by exactly +1)
/// 2. Not modified by any istore/astore
///
/// Returns the local index if found.
/// Collect the JVM local indexes that are the dead "high half" of a
/// `long`/`double` local.
///
/// A 64-bit local declared at index `N` reserves index `N+1`; the JVM never
/// addresses `N+1` directly. The JIT models 64-bit values as a single
/// register and likewise never reads `N+1`, but the register allocator still
/// assigns it a physical register. Those assignments are the source of an OSR
/// trampoline clobber (see the call site in `compile`), so OSR must know which
/// indexes are high-halves and skip them.
///
/// Detection scans for every wide load/store opcode (`lload`/`lstore`/
/// `dload`/`dstore`, both the `_0.._3` short forms and the `wide`-index
/// forms): a wide access at index `N` proves `N+1` is a high-half. This is
/// sufficient — a wide local that is never loaded or stored cannot hold a
/// value that OSR needs to preserve.
pub(super) fn wide_local_high_halves(code: &[u8], code_len: usize) -> Vec<usize> {
    let mut hi: Vec<usize> = Vec::new();
    let mut mark = |base: usize, set: &mut Vec<usize>| {
        let h = base + 1;
        if !set.contains(&h) {
            set.push(h);
        }
    };
    let mut pc = 0usize;
    while pc < code_len {
        match code[pc] {
            // lload_0..lload_3
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            0x1e..=0x21 => mark((code[pc] - 0x1e) as usize, &mut hi),
            // dload_0..dload_3
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            0x26..=0x29 => mark((code[pc] - 0x26) as usize, &mut hi),
            // lstore_0..lstore_3
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            0x3f..=0x42 => mark((code[pc] - 0x3f) as usize, &mut hi),
            // dstore_0..dstore_3
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            0x47..=0x4a => mark((code[pc] - 0x47) as usize, &mut hi),
            // lload (0x16) / dload (0x18), wide index
            // Widening: u8 -> wider int (bytecode operand byte, value fits)
            0x16 | 0x18 if pc + 1 < code_len => mark(code[pc + 1] as usize, &mut hi),
            // lstore (0x37) / dstore (0x39), wide index
            // Widening: u8 -> wider int (bytecode operand byte, value fits)
            0x37 | 0x39 if pc + 1 < code_len => mark(code[pc + 1] as usize, &mut hi),
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    hi
}

/// The subset of [`wide_local_high_halves`] whose OSR register home may be
/// stripped: the slots that are NOTHING BUT a cat-2 high half.
///
/// `wide_local_high_halves` is a whole-method scan — every `lstore N`/`dstore N`
/// anywhere marks `N+1` — so on its own it also names slots that are a live
/// cat-1 local in a disjoint range, which legal JVM slot reuse produces
/// constantly. Stripping one of those leaves the OSR trampoline seeding only its
/// FRAME slot while the compiled body keeps reading its REGISTER, and the loop
/// then runs on whatever the caller left there. That has now cost two separate
/// bugs — an `int` loop counter in `DualPivotQuicksort.mixedInsertionSort`
/// (`Arrays.sort(long[])` walked off the front of the array) and a `byte[]`
/// reference in Tomcat's annotation scan (a SIGSEGV on Windows, a silently wrong
/// checksum on Linux) — so the filter has ONE name, used by the publication in
/// `x64::osr` and asserted directly by the tests, rather than a predicate spelled
/// out at each.
///
/// [`classify_local_kinds`] already draws the distinction: an independently
/// accessed high half is [`LocalKind::Ambiguous`] (or its own kind, when the base
/// never settles on cat-2 either), an untouched one is [`LocalKind::HighHalf`].
pub(super) fn pure_high_halves(kinds: &[LocalKind], high_halves: &[usize]) -> Vec<usize> {
    high_halves
        .iter()
        .copied()
        .filter(|&hh| matches!(kinds.get(hh), Some(LocalKind::HighHalf)))
        .collect()
}

/// deopt-osr P2: the JVM value kind of a local slot, derived for the deopt
/// snapshot's width/type source. See [`classify_local_kinds`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LocalKind {
    /// Never loaded/stored in this method (a dead slot or untyped gap).
    Unknown,
    /// Cat-1 `int`/`boolean`/`byte`/`char`/`short`.
    Int,
    /// Cat-2 `long`.
    Long,
    /// Cat-1 `float`.
    Float,
    /// Cat-2 `double`.
    Double,
    /// Object reference (`a*` opcodes). The precise oop dataflow mask is the
    /// authority at a given bci; this is only a corroborating hint.
    Ref,
    /// The dead upper half of a cat-2 (`long`/`double`) local at the slot below.
    HighHalf,
    /// Accessed as more than one kind across the method (legal JVM slot reuse
    /// across disjoint live ranges). The kind at a given bci is unknowable from a
    /// whole-method scan, so the snapshot treats it as "re-run" rather than guess.
    Ambiguous,
}

/// deopt-osr P2: classify every local slot's JVM value kind from the method's
/// load/store opcodes — the only per-slot width/type signal the single-pass
/// backend has (it threads no method descriptor and runs no verification type
/// inference). The deopt snapshot uses this to emit a precisely-typed
/// `FrameValue` (`Long`/`Double`/`Float` vs `Int`/ref) instead of a width-blind
/// `Register`/`StackSlot` that would truncate a `long` (high 32 bits lost) or
/// mistype an FP value on resume.
///
/// A slot accessed as exactly one kind takes that kind; a slot accessed as more
/// than one is [`LocalKind::Ambiguous`]; a slot never accessed is
/// [`LocalKind::Unknown`]. Each `long`/`double` base additionally marks its
/// high-half slot [`LocalKind::HighHalf`] (so the snapshot records `Undefined`
/// there and the cat-2 locals collapse stays aligned). `Ambiguous` and
/// interior-`Unknown` slots make the snapshot fall back to the safe whole-method
/// re-run — never a guess.
pub(super) fn classify_local_kinds(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
) -> Vec<LocalKind> {
    let mut kinds = vec![LocalKind::Unknown; num_locals];
    fn vote(kinds: &mut [LocalKind], slot: usize, k: LocalKind) {
        if slot >= kinds.len() {
            return;
        }
        kinds[slot] = match kinds[slot] {
            LocalKind::Unknown => k,
            existing if existing == k => existing,
            _ => LocalKind::Ambiguous,
        };
    }

    let mut pc = 0usize;
    while pc < code_len {
        if let Some((k, slot)) = local_access_at(code, code_len, pc) {
            vote(&mut kinds, slot, k);
        }
        pc += bytecode_analysis::step(code, pc);
    }

    // Mark each cat-2 base's high-half slot. A high-half that is independently
    // accessed (slot reuse) becomes Ambiguous; an untouched one becomes HighHalf.
    for slot in 0..num_locals {
        if matches!(kinds[slot], LocalKind::Long | LocalKind::Double) {
            let hh = slot + 1;
            if hh < num_locals {
                kinds[hh] = match kinds[hh] {
                    LocalKind::Unknown | LocalKind::HighHalf => LocalKind::HighHalf,
                    _ => LocalKind::Ambiguous,
                };
            }
        }
    }
    kinds
}

/// The `(kind, slot)` of the local access at `pc`, or `None` if the opcode at
/// `pc` does not touch a local.
///
/// Shared by the whole-method classifier ([`classify_local_kinds`]) and the
/// per-bci refinement ([`refine_ambiguous_local_kinds`]) so the two can never
/// disagree about what an opcode does — a divergence there would silently make
/// the refinement unsound rather than merely imprecise.
pub(super) fn local_access_at(
    code: &[u8],
    code_len: usize,
    pc: usize,
) -> Option<(LocalKind, usize)> {
    let op = code[pc];
    match op {
        // Widening: u8 operand/opcode-relative index -> usize (value fits).
        0x15 if pc + 1 < code_len => Some((LocalKind::Int, code[pc + 1] as usize)),
        0x16 if pc + 1 < code_len => Some((LocalKind::Long, code[pc + 1] as usize)),
        0x17 if pc + 1 < code_len => Some((LocalKind::Float, code[pc + 1] as usize)),
        0x18 if pc + 1 < code_len => Some((LocalKind::Double, code[pc + 1] as usize)),
        0x19 if pc + 1 < code_len => Some((LocalKind::Ref, code[pc + 1] as usize)),
        0x1a..=0x1d => Some((LocalKind::Int, (op - 0x1a) as usize)),
        0x1e..=0x21 => Some((LocalKind::Long, (op - 0x1e) as usize)),
        0x22..=0x25 => Some((LocalKind::Float, (op - 0x22) as usize)),
        0x26..=0x29 => Some((LocalKind::Double, (op - 0x26) as usize)),
        0x2a..=0x2d => Some((LocalKind::Ref, (op - 0x2a) as usize)),
        0x36 if pc + 1 < code_len => Some((LocalKind::Int, code[pc + 1] as usize)),
        0x37 if pc + 1 < code_len => Some((LocalKind::Long, code[pc + 1] as usize)),
        0x38 if pc + 1 < code_len => Some((LocalKind::Float, code[pc + 1] as usize)),
        0x39 if pc + 1 < code_len => Some((LocalKind::Double, code[pc + 1] as usize)),
        0x3a if pc + 1 < code_len => Some((LocalKind::Ref, code[pc + 1] as usize)),
        0x3b..=0x3e => Some((LocalKind::Int, (op - 0x3b) as usize)),
        0x3f..=0x42 => Some((LocalKind::Long, (op - 0x3f) as usize)),
        0x43..=0x46 => Some((LocalKind::Float, (op - 0x43) as usize)),
        0x47..=0x4a => Some((LocalKind::Double, (op - 0x47) as usize)),
        0x4b..=0x4e => Some((LocalKind::Ref, (op - 0x4b) as usize)),
        // iinc reads+writes an int local.
        0x84 if pc + 1 < code_len => Some((LocalKind::Int, code[pc + 1] as usize)),
        // wide (0xc4): code[pc+1] is the real opcode, code[pc+2..4] the index.
        0xc4 if pc + 3 < code_len => {
            let real = code[pc + 1];
            let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
            match real {
                0x15 | 0x36 | 0x84 => Some((LocalKind::Int, idx)),
                0x16 | 0x37 => Some((LocalKind::Long, idx)),
                0x17 | 0x38 => Some((LocalKind::Float, idx)),
                0x18 | 0x39 => Some((LocalKind::Double, idx)),
                0x19 | 0x3a => Some((LocalKind::Ref, idx)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Per-bci kinds for the locals the whole-method classifier had to call
/// [`LocalKind::Ambiguous`].
///
/// Empty (`slots.is_empty()`) when the method has no ambiguous local, which is
/// the overwhelmingly common case — the pass then costs one scan of `kinds`.
#[derive(Default, Clone)]
pub(super) struct AmbiguousLocalKinds {
    /// The ambiguous local slots, ascending. Index into this is the column of
    /// [`Self::at`].
    pub(super) slots: Vec<usize>,
    /// `at[pc * slots.len() + col]` — the kind of `slots[col]` on entry to
    /// `pc`. [`LocalKind::Ambiguous`] where the dataflow could not agree, and
    /// [`LocalKind::Unknown`] on a pc the dataflow never reached.
    pub(super) at: Vec<LocalKind>,
    /// Is this pass's control-flow graph EXACTLY the verifier's?
    ///
    /// Only then may a caller read [`LocalKind::Ambiguous`] as "JVMS 4.10.1.6
    /// types this local `top` here, so no reachable bytecode can load it".
    /// Two constructs break the equality, in opposite directions:
    ///
    /// * **An exception range.** Handler entries are seeded
    ///   [`LocalKind::Ambiguous`] (TOP) because a handler is reachable from
    ///   any pc in its protected range. That is sound but COARSER than the
    ///   verifier, which merges the actual states of those pcs — so a slot
    ///   the verifier types precisely can read `Ambiguous` here, and treating
    ///   it as unreadable would drop a live value.
    /// * **`jsr`/`ret`.** [`super::licm::oop_dataflow_successors`] gives `ret`
    ///   no successors at all, which makes the graph NARROWER than the
    ///   verifier's and can settle a kind the verifier would merge further.
    ///
    /// False for either, and the callers that need the verifier equality then
    /// keep their conservative encoding.
    pub(super) cfg_is_exact: bool,
    /// Does this method contain `jsr`/`ret`?
    ///
    /// The half of [`Self::cfg_is_exact`] that makes the graph NARROWER than
    /// the verifier's, and therefore the only half a caller must check before
    /// trusting a kind this pass SETTLED on. The handler half widens instead:
    /// a TOP seed can only turn a settled kind into [`LocalKind::Ambiguous`],
    /// never manufacture one, so it cannot make a settled answer wrong.
    pub(super) has_jsr: bool,
}

impl AmbiguousLocalKinds {
    /// The refined kind of `slot` on entry to `pc`, or `None` when this slot is
    /// not tracked (it was never ambiguous) or the refinement did not settle.
    ///
    /// Never answers `Ref`: the flow-sensitive oop mask is the sole authority
    /// for ref-typed slots and has already had its say by the time a caller
    /// consults this, so a `Ref` here means "the mask could not prove it live
    /// as an oop" — the one case that must stay a safe re-run.
    /// The RAW dataflow state of `slot` on entry to `pc` — before
    /// [`Self::kind_at`]'s "only concrete non-ref kinds" filter.
    ///
    /// The two answers [`Self::kind_at`] collapses into `None` are different
    /// facts and want different fixes, and until this existed no caller could
    /// tell them apart:
    ///
    /// * [`LocalKind::Ambiguous`] — the dataflow REACHED this pc and two
    ///   different concrete kinds arrive on different paths. By JVMS 4.10.1.6
    ///   the verifier assigns such a local `top`, and a `top` local cannot be
    ///   the operand of any load, so no reachable bytecode reads it.
    /// * [`LocalKind::Unknown`] — the dataflow never reached this pc, or the
    ///   slot is undefined on every path here. No evidence either way.
    /// * [`LocalKind::Ref`] — the flow-sensitive oop mask, not this pass, is
    ///   the authority; a `Ref` here means the mask could not prove the slot
    ///   live as an oop.
    pub(super) fn raw_at(&self, pc: usize, slot: usize) -> Option<LocalKind> {
        if self.slots.is_empty() {
            return None;
        }
        let col = self.slots.iter().position(|&s| s == slot)?;
        self.at.get(pc * self.slots.len() + col).copied()
    }

    pub(super) fn kind_at(&self, pc: usize, slot: usize) -> Option<LocalKind> {
        if self.slots.is_empty() {
            return None;
        }
        let col = self.slots.iter().position(|&s| s == slot)?;
        let k = *self.at.get(pc * self.slots.len() + col)?;
        match k {
            LocalKind::Int | LocalKind::Long | LocalKind::Float | LocalKind::Double => Some(k),
            _ => None,
        }
    }
}

/// Forward reaching-kind dataflow for the ambiguous locals only.
///
/// See the module-level rationale on [`AmbiguousLocalKinds`] and the fix note
/// in the retired `h2-jitban-longtail1` write-up. Uses the same
/// successor relation as the precise oop-mask pass so the two agree about
/// control flow, and seeds every exception-handler entry TOP because a handler
/// is reachable from any point in its protected range.
pub(super) fn refine_ambiguous_local_kinds(
    code: &[u8],
    code_len: usize,
    kinds: &[LocalKind],
    exception_ranges: &[(usize, usize, usize)],
) -> AmbiguousLocalKinds {
    let slots: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| matches!(k, LocalKind::Ambiguous))
        .map(|(i, _)| i)
        .collect();
    if slots.is_empty() || code_len == 0 {
        return AmbiguousLocalKinds::default();
    }
    // See `AmbiguousLocalKinds::cfg_is_exact`. Scanned here rather than by the
    // caller so the flag can never disagree with the graph this pass walked.
    let has_jsr = {
        let mut pc = 0usize;
        let mut found = false;
        while pc < code_len {
            // jsr, ret, jsr_w, and the `wide ret` form.
            if matches!(code[pc], 0xa8 | 0xa9 | 0xc9)
                || (code[pc] == 0xc4 && pc + 1 < code_len && code[pc + 1] == 0xa9)
            {
                found = true;
                break;
            }
            pc += bytecode_analysis::step(code, pc);
        }
        found
    };
    let cfg_is_exact = exception_ranges.is_empty() && !has_jsr;
    let width = slots.len();
    let col_of = |slot: usize| slots.iter().position(|&s| s == slot);

    // `at` is the IN state; `reached` distinguishes "bottom" from "never seen"
    // so the first real predecessor seeds instead of merging with bottom.
    let mut at = vec![LocalKind::Unknown; code_len.saturating_mul(width)];
    let mut reached = vec![false; code_len];
    let mut work: Vec<usize> = Vec::new();

    reached[0] = true;
    work.push(0);
    // A handler can be entered from ANY pc in its protected range, so no kind
    // may be assumed on entry to one.
    for &(_start, _end, handler_pc) in exception_ranges {
        if handler_pc >= code_len {
            continue;
        }
        for col in 0..width {
            at[handler_pc * width + col] = LocalKind::Ambiguous;
        }
        if !reached[handler_pc] {
            reached[handler_pc] = true;
            work.push(handler_pc);
        }
    }

    // Bound iterations defensively against any decoding pathology, exactly as
    // `compute_local_oop_masks` does.
    let mut guard = code_len.saturating_mul(64).saturating_add(64);
    while let Some(pc) = work.pop() {
        if pc >= code_len {
            continue;
        }
        guard = guard.saturating_sub(1);
        if guard == 0 {
            // Ran out of budget: report nothing rather than a partial fixpoint.
            return AmbiguousLocalKinds::default();
        }
        let mut out: Vec<LocalKind> = at[pc * width..pc * width + width].to_vec();
        // A load proves the kind just as a store sets it — and a load is the
        // only signal for a slot whose value arrived as a parameter.
        if let Some((k, slot)) = local_access_at(code, code_len, pc) {
            if let Some(col) = col_of(slot) {
                out[col] = k;
            }
            // The dead upper half of a cat-2 store must not keep a stale kind
            // from the value that used to live there.
            if matches!(k, LocalKind::Long | LocalKind::Double) {
                if let Some(col) = col_of(slot + 1) {
                    out[col] = LocalKind::HighHalf;
                }
            }
        }
        for succ in oop_dataflow_successors(code, code_len, pc) {
            if succ >= code_len {
                continue;
            }
            let mut changed = false;
            for col in 0..width {
                let idx = succ * width + col;
                let merged = if reached[succ] {
                    merge_local_kind(at[idx], out[col])
                } else {
                    out[col]
                };
                if merged != at[idx] {
                    at[idx] = merged;
                    changed = true;
                }
            }
            if !reached[succ] || changed {
                reached[succ] = true;
                work.push(succ);
            }
        }
    }

    // An unreached pc keeps `Unknown`, which `kind_at` reports as "no answer".
    AmbiguousLocalKinds {
        slots,
        at,
        cfg_is_exact,
        has_jsr,
    }
}

/// Join of two reaching kinds. [`LocalKind::Unknown`] is the bottom (a path on
/// which the slot is undefined, and therefore — by JVMS verification — never
/// read); disagreement is [`LocalKind::Ambiguous`], the top.
pub(super) fn merge_local_kind(a: LocalKind, b: LocalKind) -> LocalKind {
    if a == b {
        return a;
    }
    match (a, b) {
        (LocalKind::Unknown, other) | (other, LocalKind::Unknown) => other,
        _ => LocalKind::Ambiguous,
    }
}

/// True iff `op` is a `long`/`float`/`double` bytecode — any op that can put a
/// cat-2 (`long`/`double`) or cat-1 `float` value onto the operand stack, or
/// proves one is in flight (load/store/const/arith/convert/compare/array/return).
pub(super) fn opcode_touches_long_float_double(op: u8) -> bool {
    matches!(
        op,
        // lconst_0/1, fconst_0/1/2, dconst_0/1 | ldc2_w
        0x09..=0x0f | 0x14
        // lload, fload, dload (wide-index)
        | 0x16..=0x18
        // lload_0-3, fload_0-3, dload_0-3
        | 0x1e..=0x29
        // laload, faload, daload
        | 0x2f..=0x31
        // lstore, fstore, dstore (wide-index)
        | 0x37..=0x39
        // lstore_0-3, fstore_0-3, dstore_0-3
        | 0x3f..=0x4a
        // lastore, fastore, dastore
        | 0x50..=0x52
        // lneg, fneg, dneg
        | 0x75..=0x77
        // lshl, lshr, lushr
        | 0x79 | 0x7b | 0x7d
        // land, lor, lxor
        | 0x7f | 0x81 | 0x83
        // i2l..d2f (every conversion involving a long/float/double)
        | 0x85..=0x90
        // lcmp, fcmpl, fcmpg, dcmpl, dcmpg
        | 0x94..=0x98
        // lreturn, freturn, dreturn
        | 0xad..=0xaf
    // add/sub/mul/div/rem: within each group of 4 (i,l,f,d at +0,+1,+2,+3) the
    // non-`i` members are long/float/double.
    ) || ((0x60..=0x73).contains(&op) && (op - 0x60) % 4 != 0)
}

/// deopt-osr FU2: true iff the method touches any `long`/`float`/`double` — the
/// (sound, complete) method-level gate for the operand-stack snapshot. The
/// abstract operand stack carries NO per-entry width source: a `long` (or a
/// spilled FP value) in a `Frame`/GPR stack slot is indistinguishable from an
/// `int` there, so it would be recorded as `StackSlot`/`Register` and TRUNCATE on
/// resume. Any wide/FP value on the operand stack implies one of these opcodes
/// somewhere in the method (it must be loaded / produced / consumed), so when
/// this is `false` every non-oop stack slot is provably a cat-1 `int`/`ref` and
/// keeps its precise encoding; when `true` the snapshot conservatively records
/// such slots as `Unsupported` (re-run) rather than risk a mistyped cat-2/FP
/// stack value. Pure-int/ref methods (the BCE pilot) are unaffected.
pub(super) fn code_uses_long_float_double(code: &[u8], code_len: usize) -> bool {
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        if opcode_touches_long_float_double(op) {
            return true;
        }
        // wide (0xc4) prefix: the real opcode follows.
        if op == 0xc4 && pc + 1 < code_len && opcode_touches_long_float_double(code[pc + 1]) {
            return true;
        }
        pc += bytecode_analysis::step(code, pc);
    }
    false
}

pub(super) fn find_induction_variable(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
) -> Option<usize> {
    let mut iinc_locals: Vec<(usize, i8)> = Vec::new(); // (local, increment)
    let mut stored_locals: u64 = 0; // bitmask of locals written by xstore
                                    // Track iadd+istore pattern: iload X; ...; iadd; istore X
    let mut iadd_store_locals: Vec<usize> = Vec::new();

    let mut pc = header;
    while pc < back_edge_end {
        match code[pc] {
            // iinc
            0x84 => {
                let local = code[pc + 1] as usize; // Widening: always safe
                let inc = code[pc + 2] as i8; // Widening: always safe
                iinc_locals.push((local, inc));
                pc += 3;
            }
            // istore_0..istore_3
            0x3b..=0x3e => {
                let local = (code[pc] - 0x3b) as usize; // Widening: always safe
                stored_locals |= 1 << local;
                // Check for iadd; istore X pattern (the iadd is right before)
                if pc >= 1 && code[pc - 1] == 0x60 {
                    iadd_store_locals.push(local);
                }
                pc += 1;
            }
            // istore (wide)
            0x36 => {
                let local = code[pc + 1] as usize; // Widening: always safe
                stored_locals |= 1u64 << local.min(63);
                // Check for iadd; istore X pattern
                if pc >= 1 && code[pc - 1] == 0x60 {
                    iadd_store_locals.push(local);
                }
                pc += 2;
            }
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                stored_locals |= 1 << (code[pc] - 0x3f);
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                stored_locals |= 1 << (code[pc] - 0x4b);
                pc += 1;
            }
            // lstore/fstore/dstore/astore (wide index)
            0x37..=0x3a => {
                stored_locals |= 1u64 << (code[pc + 1] as usize).min(63); // Widening: always safe
                pc += 2;
            }
            // fstore_0..fstore_3, dstore_0..dstore_3
            0x43..=0x4a => {
                stored_locals |= 1 << (code[pc] - 0x43);
                pc += 1;
            }
            _ => pc += bytecode_analysis::step(code, pc),
        }
    }

    // Priority 1: Find a local that has exactly one iinc +1 and no store
    for &(local, inc) in &iinc_locals {
        if inc == 1 && local < 64 && (stored_locals & (1u64 << local)) == 0 {
            // Verify this local only appears once in iinc list
            let count = iinc_locals.iter().filter(|&&(l, _)| l == local).count();
            if count == 1 {
                return Some(local);
            }
        }
    }

    // Priority 2: Find a local modified by iadd+istore pattern (non-unit stride)
    // This handles `j += i` patterns in Sieve's inner loop
    for &local in &iadd_store_locals {
        if local < 64 {
            // Verify: the local should be loaded before iadd (iload X; iload Y; iadd; istore X)
            // and only modified by this one iadd+istore in the loop
            let iadd_count = iadd_store_locals.iter().filter(|&&l| l == local).count();
            let iinc_count = iinc_locals.iter().filter(|&&(l, _)| l == local).count();
            if iadd_count == 1 && iinc_count == 0 {
                return Some(local);
            }
        }
    }

    None
}

/// Soundly identify the `(array_local, index_local)` operands consumed by each
/// array load/store in a counted loop, via operand-stack *producer* tracking.
///
/// The legacy positional heuristics (`find_store_index_pc` /
/// `find_preceding_iload` / `find_preceding_aload`) guessed the index/array by
/// counting bytecode instructions backward from the access ("the index is the
/// load 2 instructions before the store"). That is unsound the moment the
/// value or index expression spans more than one instruction. The canonical
/// scatter store `result[off + i] = src[i]` is the textbook break: the inner
/// `iload i` (the *src* index) sits exactly two instructions before `iastore`,
/// so the heuristic decided `index == i` (the IV) and elided the store's bounds
/// check — while the real index `off + i` runs past `result.length`, turning
/// the store into an out-of-bounds heap write that overwrites a neighbouring
/// object's header (the "kind=Object but array_length set" corruption seen
/// across the BC / Spring / JUnit JIT crashes). The array heuristic was
/// likewise wrong: it would guard `src.length` while the store targeted
/// `result`.
///
/// This pass simulates the operand stack as a vector of *producer PCs*, started
/// empty at the loop header (the stack-empty point for javac counted loops). At
/// each array access the array and index operands are read from their exact
/// stack positions, so an access is reported only when its index is genuinely
/// produced by a bare `iload` and its array by a bare `aload`. Anything the
/// simulator cannot model precisely — method calls, `dup2`/`swap`/`dup_x*`,
/// switches, `wide`, or a control-flow join where the linear stack is no longer
/// authoritative — makes it STOP, after which no further access in the loop is
/// reported. STOP/underflow is always conservative: the per-element bounds
/// check is kept.
pub(super) fn analyze_array_access_operands(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
) -> FxHashMap<usize, (usize, usize)> {
    let mut out: FxHashMap<usize, (usize, usize)> = FxHashMap::default();
    let code_len = code.len();
    if header >= back_edge_end || back_edge_end > code_len {
        return out;
    }

    // Precompute forward conditional-branch targets that land inside the loop.
    // These are control-flow *join* points where the linearly-simulated
    // producer stack is no longer guaranteed to match every predecessor, so the
    // walk must stop before analysing them. (Backward targets re-enter at the
    // same stack height by the JVM's structural constraint and need no special
    // handling; goto/switch/return are not modelled and stop the walk anyway.)
    let mut join_targets: FxHashSet<usize> = FxHashSet::default();
    {
        let mut pc = header;
        while pc < back_edge_end {
            let op = code[pc];
            if matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) && pc + 2 < code_len {
                // Cast: value to i32 (encoding immediate/displacement)
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                // Cast: value to i32 (encoding immediate/displacement)
                let target = pc as i32 + off;
                // Cast: non-negative index/count to usize
                if target > pc as i32 && (target as usize) < back_edge_end {
                    // Cast: non-negative index/count to usize
                    join_targets.insert(target as usize);
                }
            }
            pc += bytecode_analysis::step(code, pc);
        }
    }

    let mut producers: Vec<usize> = Vec::new();
    let mut pc = header;
    while pc < back_edge_end {
        // A join: the operand stack here may differ per predecessor path.
        if pc != header && join_targets.contains(&pc) {
            break;
        }
        let op = code[pc];
        match op {
            // Array loads: [.., array, index] -> [.., value].
            0x2e..=0x35 => {
                let n = producers.len();
                if n < 2 {
                    break;
                }
                let array_pc = producers[n - 2];
                let index_pc = producers[n - 1];
                if let (Some(al), Some(il)) = (
                    extract_aload_local(code, array_pc),
                    extract_iload_local(code, index_pc),
                ) {
                    out.insert(pc, (al, il));
                }
                producers.truncate(n - 2);
                producers.push(pc);
            }
            // Array stores: [.., array, index, value] -> [..].
            0x4f..=0x56 => {
                let n = producers.len();
                if n < 3 {
                    break;
                }
                let array_pc = producers[n - 3];
                let index_pc = producers[n - 2];
                if let (Some(al), Some(il)) = (
                    extract_aload_local(code, array_pc),
                    extract_iload_local(code, index_pc),
                ) {
                    out.insert(pc, (al, il));
                }
                producers.truncate(n - 3);
            }
            // Pure pushes — consume 0, produce exactly 1 value (one entry,
            // category-1 or -2 alike: a long/double is a single producer here).
            0x01..=0x14    // aconst_null..ldc2_w
            | 0x15..=0x19  // iload/lload/fload/dload/aload (wide index)
            | 0x1a..=0x2d  // *load_0.._3
            | 0xb2         // getstatic — pushes exactly one value
            | 0xbb         // new
            => {
                producers.push(pc);
            }
            // Consume 1, produce 1.
            0x74..=0x77    // ineg/lneg/fneg/dneg
            | 0x85..=0x93  // i2l..i2s conversions
            | 0xb4         // getfield  (objref -> value)
            | 0xbc | 0xbd  // newarray/anewarray  (count -> arrayref)
            | 0xbe         // arraylength
            | 0xc0 | 0xc1  // checkcast/instanceof
            => {
                let n = producers.len();
                if n < 1 {
                    break;
                }
                producers.truncate(n - 1);
                producers.push(pc);
            }
            // Consume 2, produce 1.
            0x60..=0x73    // i/l/f/d add/sub/mul/div/rem
            | 0x78..=0x83  // shifts + and/or/xor (int & long)
            | 0x94..=0x98  // lcmp / fcmp* / dcmp*
            => {
                let n = producers.len();
                if n < 2 {
                    break;
                }
                producers.truncate(n - 2);
                producers.push(pc);
            }
            // Consume 1, produce 0.
            0x36..=0x3a    // istore/lstore/fstore/dstore/astore (wide index)
            | 0x3b..=0x4e  // *store_0.._3
            | 0x57         // pop
            | 0xb3         // putstatic
            | 0xc2 | 0xc3  // monitorenter/monitorexit
            // Conditional single-operand branches — fall-through continues.
            | 0x99..=0x9e  // if<cond>
            | 0xc6 | 0xc7  // ifnull/ifnonnull
            => {
                let n = producers.len();
                if n < 1 {
                    break;
                }
                producers.truncate(n - 1);
            }
            // Consume 2, produce 0.
            0xb5           // putfield  (objref + value)
            | 0x9f..=0xa6  // if_icmp<cond> / if_acmp<cond>
            => {
                let n = producers.len();
                if n < 2 {
                    break;
                }
                producers.truncate(n - 2);
            }
            // dup — duplicate the top producer.
            0x59 => match producers.last().copied() {
                Some(t) => producers.push(t),
                None => break,
            },
            // iinc / nop — no stack effect.
            0x84 | 0x00 => {}
            // Everything else (goto, switches, returns, athrow, invoke*,
            // dup2/swap/dup_x*, pop2, wide, multianewarray, jsr/ret, ...) is not
            // modelled: stop so no access is reported on a desynchronised stack.
            _ => break,
        }
        pc += bytecode_analysis::step(code, pc);
    }

    out
}

/// Extract the local variable index from an iload instruction at `pc`.
pub(super) fn extract_iload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x1a => Some(0),
        0x1b => Some(1),
        0x1c => Some(2),
        0x1d => Some(3),
        // Cast: non-negative index/count to usize
        0x15 => code.get(pc + 1).map(|&b| b as usize), // iload
        _ => None,
    }
}

/// Extract the local variable index from an aload instruction at `pc`.
pub(super) fn extract_aload_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        0x2a => Some(0),
        0x2b => Some(1),
        0x2c => Some(2),
        0x2d => Some(3),
        // Cast: non-negative index/count to usize
        0x19 => code.get(pc + 1).map(|&b| b as usize), // aload
        _ => None,
    }
}

/// Collect every branch target of the i16-offset branch family (`if*`,
/// `if_icmp*`, `if_acmp*`, `ifnull`/`ifnonnull`, `goto`) across the whole
/// method. Returns `None` — "cannot analyze" — when the method contains an
/// opcode whose targets this scan does not model (`tableswitch`,
/// `lookupswitch`, `jsr`/`ret`, `goto_w`/`jsr_w`), so callers stay
/// conservative instead of trusting an incomplete target set.
///
/// Used by the whole-method provenance proofs below to reject a pattern that
/// is *linearly* adjacent but not *control-flow* adjacent (a branch landing
/// between `arraylength` and its `istore` could store a different value).
/// Exception-handler entries need no modeling: a handler starts with the
/// thrown ref as the only stack value, so verified bytecode cannot enter a
/// pattern at its `arraylength` (needs an array) or value-consuming `istore`
/// (needs an int) — only at or before the producing `aload`/`iconst`, which
/// re-executes the whole pattern.
pub(super) fn collect_i16_branch_targets(code: &[u8], code_len: usize) -> Option<FxHashSet<usize>> {
    let mut targets = FxHashSet::default();
    let mut pc = 0usize;
    while pc < code_len {
        match code[pc] {
            0xaa | 0xab | 0xa8 | 0xa9 | 0xc8 | 0xc9 => return None,
            op if matches!(op, 0x99..=0xa7 | 0xc6 | 0xc7) && pc + 2 < code_len => {
                // Cast: value to i32 (branch displacement arithmetic)
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                let target = pc as i32 + off; // Cast: value to i32
                if target >= 0 && (target as usize) < code_len {
                    // Cast: non-negative index to usize
                    targets.insert(target as usize);
                }
            }
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    Some(targets)
}

/// Whole-method proof that `bound_local` holds `A.length` for some array
/// local `A` — the proof obligation for a *static* (guard-less) bounds-check
/// elision of `arr[iv]` under a loop test `iv < bound`: it is only sound for
/// `arr == A`. Returns `Some(A)` when ALL of:
///
/// 1. the method contains EXACTLY ONE store to `bound_local`, and it is the
///    canonical `aload A ; arraylength ; istore bound` triple (javac's
///    `int n = a.length;`). JVM definite-assignment then guarantees that
///    single store dominates every read of `bound_local`, so no dominator
///    tree is needed;
/// 2. no branch lands on the `arraylength` or the `istore` (which could
///    deliver a different stack value into the store — see
///    `collect_i16_branch_targets` for why handlers cannot);
/// 3. `bound_local` is never `iinc`'d, and no `wide`-indexed store aliases
///    it; and
/// 4. `A` is NEVER reassigned (no `astore A` anywhere), so the array whose
///    length was read is the same object at every access — in practice `A`
///    is a parameter or the single-assignment result javac emits.
///
/// Everything that fails this proof falls back to the *speculative* per-array
/// header guard, which checks the runtime lengths instead (see
/// `SpeculativeBCEGuard`). Before this proof existed, the static-elision pass
/// treated the loop guard `iv < bound` as bounding EVERY array indexed by the
/// IV — eliding the store check of `out[i] = a[i] + b[i]` from a guard on
/// `a.length`, a silent out-of-bounds heap write when `out` is shorter
/// (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md).
///
/// `recognise_loop` now consumes this by REWRITING the limit: a `Local(bl)`
/// whose provenance is `A.length` becomes `BoundSource::ArrayLength(A)`, and
/// `prove_index_in_bounds_of` discharges `A[iv]` against the resulting
/// tautology while leaving every other array's guard in place.
pub(super) fn find_bound_arraylength_provenance(
    code: &[u8],
    code_len: usize,
    bound_local: usize,
) -> Option<usize> {
    let branch_targets = collect_i16_branch_targets(code, code_len)?;
    // (istore pc, previous instruction pc, the one before that)
    let mut stores: Vec<(usize, Option<usize>, Option<usize>)> = Vec::new();
    let mut astored: FxHashSet<usize> = FxHashSet::default();
    let mut prev1: Option<usize> = None;
    let mut prev2: Option<usize> = None;
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        // Widening: u8 operand/opcode-relative index -> usize (value fits)
        let istore_target = match op {
            0x36 if pc + 1 < code_len => Some(code[pc + 1] as usize),
            0x3b..=0x3e => Some((op - 0x3b) as usize),
            _ => None,
        };
        if istore_target == Some(bound_local) {
            stores.push((pc, prev1, prev2));
        }
        // Any iinc on the bound makes its value diverge from the recorded
        // arraylength — refuse.
        if op == 0x84 && pc + 1 < code_len && code[pc + 1] as usize == bound_local {
            return None;
        }
        // wide-indexed forms can alias the bound (or an array local) with a
        // 2-byte index; treat a wide istore/iinc on the bound as unprovable
        // and record wide astores like the short forms.
        if op == 0xc4 && pc + 3 < code_len {
            let real = code[pc + 1];
            // Widening: operand bytes -> usize index (value fits)
            let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
            if (real == 0x36 || real == 0x84) && idx == bound_local {
                return None;
            }
            if real == 0x3a {
                astored.insert(idx);
            }
        }
        match op {
            // Widening: u8 operand/opcode-relative index -> usize (value fits)
            0x3a if pc + 1 < code_len => {
                astored.insert(code[pc + 1] as usize);
            }
            0x4b..=0x4e => {
                astored.insert((op - 0x4b) as usize);
            }
            _ => {}
        }
        prev2 = prev1;
        prev1 = Some(pc);
        pc += bytecode_analysis::step(code, pc);
    }

    let (store_pc, p1, p2) = match stores.as_slice() {
        [(s, Some(p1), Some(p2))] => (*s, *p1, *p2),
        _ => return None,
    };
    if code[p1] != 0xbe {
        return None; // not arraylength
    }
    let array_local = extract_aload_local(code, p2)?;
    if branch_targets.contains(&p1) || branch_targets.contains(&store_pc) {
        return None;
    }
    if astored.contains(&array_local) {
        return None;
    }
    Some(array_local)
}

/// Whole-method proof that the induction variable can never be negative: its
/// only `istore` is a single dominating (definite-assignment) store of a
/// non-negative constant (`iconst_0..5` / non-negative `bipush`/`sipush`),
/// no branch lands on that `istore`, and every `iinc` on it is non-negative
/// (`find_induction_variable` separately guarantees the in-loop step is
/// exactly +1). Required for the *static* elision path: an elided access
/// assumes `0 <= iv`, and a `for (i = start; ...)` loop with a negative
/// `start` would otherwise silently access below the array base. The
/// speculative path needs no such proof — its header guard tests the runtime
/// `iv >= 0` at loop entry instead.
pub(super) fn find_iv_nonneg_start(code: &[u8], code_len: usize, iv_local: usize) -> bool {
    let Some(branch_targets) = collect_i16_branch_targets(code, code_len) else {
        return false;
    };
    let mut stores: Vec<(usize, Option<usize>)> = Vec::new();
    let mut prev1: Option<usize> = None;
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        // Widening: u8 operand/opcode-relative index -> usize (value fits)
        let istore_target = match op {
            0x36 if pc + 1 < code_len => Some(code[pc + 1] as usize),
            0x3b..=0x3e => Some((op - 0x3b) as usize),
            _ => None,
        };
        if istore_target == Some(iv_local) {
            stores.push((pc, prev1));
        }
        // A negative iinc could take the IV below its non-negative start.
        // Cast: operand byte reinterpreted as the signed iinc constant
        if op == 0x84
            && pc + 2 < code_len
            && code[pc + 1] as usize == iv_local
            && (code[pc + 2] as i8) < 0
        {
            return false;
        }
        if op == 0xc4 && pc + 3 < code_len {
            let real = code[pc + 1];
            // Widening: operand bytes -> usize index (value fits)
            let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
            if (real == 0x36 || real == 0x84) && idx == iv_local {
                return false;
            }
        }
        prev1 = Some(pc);
        pc += bytecode_analysis::step(code, pc);
    }

    let (store_pc, p1) = match stores.as_slice() {
        [(s, Some(p1))] => (*s, *p1),
        _ => return false,
    };
    let nonneg_const = match code[p1] {
        0x03..=0x08 => true, // iconst_0..iconst_5
        // Cast: operand byte reinterpreted as the signed bipush immediate
        0x10 if p1 + 1 < code_len => (code[p1 + 1] as i8) >= 0,
        0x11 if p1 + 2 < code_len => i16::from_be_bytes([code[p1 + 1], code[p1 + 2]]) >= 0,
        _ => false,
    };
    nonneg_const && !branch_targets.contains(&store_pc)
}

// ===========================================================================
// Range-analysis-backed bounds-check elimination
//
// `analyze_bounds_elimination` used to be a stack of one-off bytecode
// patterns: `find_induction_variable` (+1 only), `analyze_loop_bound` (limit
// must be a bare `iload`), `find_iv_step_provenance` (two step shapes),
// `find_bound_arraylength_provenance` / `find_iv_nonneg_start` (whole-method
// proofs), plus a hard refusal of every inclusive loop. Those are now one
// call into `loop_analysis::analyze_counted_loop` and one call per access into
// `CountedLoop::prove_index_in_bounds_of` — see `docs/jit/range-analysis.md`
// for the subsumption table and `docs/jit/bce-range-integration.md` for what
// this consumer can and cannot discharge.
//
// The load-bearing rule here is the one the proof engine states and cannot
// enforce: **a consumer that cannot emit every returned `PreheaderGuard` must
// treat the whole proof as refused.** The pre-header emitter in `x64.rs`
// (`compile_op`, the "Speculative BCE" block) has a FIXED repertoire — an
// `iv >= 0` test, a `bound != Integer.MAX_VALUE` test, a
// `0 <= step <= MAX - bound` pair, and one `array.length` vs `bound` compare
// per guarded array. [`GuardShape::covers`] is the explicit statement of what
// that repertoire proves; anything outside it refuses rather than silently
// eliding on an obligation nobody discharges.
// ===========================================================================

/// A loop's exit test, located and *verified* here rather than taken on trust.
///
/// [`crate::loop_analysis::analyze_counted_loop`] accepts the first
/// `iload x; <limit>; if_icmp*` triple it meets inside the loop and never
/// checks that the branch actually leaves the loop, so an ordinary in-body
/// `if (i >= limit)` would be read as the loop's exit condition — which would
/// let the proof claim every executed iteration satisfies `i < limit`. This
/// struct is produced only from a branch that provably is the loop's exit
/// (Pattern A) or its back edge (Pattern B).
struct LoopExitTest {
    /// The local the test compares.
    iv_local: usize,
    /// The limit it is compared against.
    bound: BoundSource,
    /// The comparison in `if_icmp*` **exit-when-true** polarity, which is what
    /// [`ExitCmp`] means. A continue-branch (Pattern B) is negated into it.
    cmp: ExitCmp,
    /// Whether that test dominates the body.
    form: LoopForm,
}

/// A recognised loop plus the JVM local a pre-header guard has to load to
/// evaluate the limit at runtime.
struct RecognisedLoop {
    /// The claim the range analysis reasons about.
    counted: CountedLoop,
    /// `Some(bl)` when the exit test read its limit out of local `bl`.
    /// `None` when the limit has no local home — an inline `a.length`, a
    /// constant, a field, a `Math.min`. Such a loop can still take STATIC
    /// elisions, but never a guarded one: [`SpeculativeBCEGuard`] carries a
    /// `bound_local` and the emitter has nothing else to load.
    bound_local: Option<usize>,
}

/// The exit-when-true comparison of the negation of `cmp`.
///
/// Pattern B's branch is taken to *continue*, so the loop's exit condition is
/// the complement of the opcode's own test.
fn negate_exit_cmp(cmp: ExitCmp) -> ExitCmp {
    match cmp {
        ExitCmp::Lt => ExitCmp::Ge,
        ExitCmp::Ge => ExitCmp::Lt,
        ExitCmp::Gt => ExitCmp::Le,
        ExitCmp::Le => ExitCmp::Gt,
    }
}

/// Every `(source, target)` branch edge in the method, or `None` when the
/// method contains control flow this scan does not model (`tableswitch`,
/// `lookupswitch`, `jsr`/`ret`, `goto_w`/`jsr_w`). Sibling of
/// `collect_i16_branch_targets`, which keeps only the targets; the entry-edge
/// question below needs to know where an edge came *from*.
pub(super) fn branch_edges(code: &[u8], code_len: usize) -> Option<Vec<(usize, usize)>> {
    let mut edges = Vec::new();
    let code_len = code_len.min(code.len());
    let mut pc = 0usize;
    while pc < code_len {
        match code[pc] {
            0xaa | 0xab | 0xa8 | 0xa9 | 0xc8 | 0xc9 => return None,
            op if matches!(op, 0x99..=0xa7 | 0xc6 | 0xc7) && pc + 2 < code_len => {
                // Cast: value to i32 (branch displacement arithmetic)
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                let target = pc as i32 + off; // Cast: value to i32
                if target >= 0 && (target as usize) < code_len {
                    // Cast: non-negative index to usize
                    edges.push((pc, target as usize));
                }
            }
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    Some(edges)
}

/// Whether control can fall through into `header` from the instruction that
/// linearly precedes it. Answers `true` — the pessimistic answer — for a
/// header at pc 0 and for any decode that does not land exactly on `header`.
fn falls_through_into(code: &[u8], code_len: usize, header: usize) -> bool {
    let code_len = code_len.min(code.len());
    let mut prev: Option<usize> = None;
    let mut pc = 0usize;
    while pc < header && pc < code_len {
        prev = Some(pc);
        pc += bytecode_analysis::step(code, pc);
    }
    if pc != header {
        return true; // desynchronised decode: assume the entry exists
    }
    match prev {
        None => true,
        // goto / jsr / switches / *return / athrow / goto_w / jsr_w do not
        // fall through; everything else does.
        Some(p) => !matches!(
            code[p],
            0xa7 | 0xa8 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf | 0xc8 | 0xc9
        ),
    }
}

/// Whether `[from, to)` contains any array load or store opcode.
fn range_contains_array_access(code: &[u8], from: usize, to: usize) -> bool {
    let mut pc = from;
    while pc < to && pc < code.len() {
        if matches!(code[pc], 0x2e..=0x35 | 0x4f..=0x56) {
            return true;
        }
        pc += bytecode_analysis::step(code, pc);
    }
    false
}

/// Decide whether a Pattern-B loop (exit test AT the back edge) is pre-tested.
///
/// This is the one place adopting the range analysis could *change* today's
/// behaviour rather than extend it: `analyze_loop_bound` treats the
/// `if_icmplt <body>` continue-branch as though the test always dominated the
/// body, which is true for javac's / ecj's `goto cond` rotation and false for a
/// hand-built `do { } while` with the same shape. The assumption was never
/// checked. It is checked here.
///
/// The criterion is not "was the loop rotated" but the weaker fact the elision
/// actually needs: **no array access can execute before the first test**. Every
/// edge that enters `[header, back_edge_end)` from outside is enumerated; the
/// loop is pre-tested when each of them lands at or after the comparison's
/// first instruction, or lands earlier but with no array access between the
/// landing point and the comparison. A `do { } while` fails that immediately
/// (its entry is the header, and its accesses precede the test), so it is
/// correctly reported [`LoopForm::PostTested`] and the proof folds the untested
/// first iteration into the span.
///
/// Fail-closed: unmodelled control flow answers [`LoopForm::PostTested`].
fn pattern_b_loop_form(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge_end: usize,
    cmp_start: usize,
) -> LoopForm {
    let Some(edges) = branch_edges(code, code_len) else {
        return LoopForm::PostTested;
    };
    let mut entries: Vec<usize> = Vec::new();
    for (src, target) in edges {
        let target_inside = target >= header && target < back_edge_end;
        let src_inside = src >= header && src < back_edge_end;
        if target_inside && !src_inside {
            entries.push(target);
        }
    }
    if falls_through_into(code, code_len, header) {
        entries.push(header);
    }
    for t in entries {
        if t >= cmp_start {
            continue; // enters at or after the test: the test still runs first
        }
        if range_contains_array_access(code, t, cmp_start) {
            return LoopForm::PostTested;
        }
    }
    LoopForm::PreTested
}

/// Locate and verify the loop's exit test.
///
/// Two shapes, and only two:
///
/// * **Pattern A** — the test is the FIRST instruction at the header and its
///   branch leaves `[header, back_edge_end)`. Because the header dominates the
///   loop body, a test sitting on it dominates every access, so the loop is
///   [`LoopForm::PreTested`] with no entry-edge question to answer. (The old
///   `analyze_loop_bound` accepted a Pattern-A-shaped compare *anywhere* in the
///   body, which admitted a loop whose accesses run before its test.)
/// * **Pattern B** — the back edge itself is the comparison, branching back
///   into the loop to continue. The comparison is negated into exit polarity
///   and the loop form is settled by `pattern_b_loop_form`.
///
/// The limit is decoded by [`crate::loop_analysis::decode_bound_expr`], which
/// accepts a constant, a local, an inline `a.length`, a field, or a
/// `Math.min`/`Math.max`. The `Math` resolver answers `None` here: this module
/// carries no constant pool, so a min/max limit simply is not recognised
/// (losing the shape, never mis-decoding it).
fn locate_exit_test(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    back_edge_end: usize,
) -> Option<LoopExitTest> {
    let no_math = |_: u16| -> Option<MinMax> { None };
    if header >= code_len || back_edge >= code_len || back_edge_end > code_len {
        return None;
    }

    // ---- Pattern A: the test is the header ------------------------------
    if let Some(iv) = extract_iload_local(code, header) {
        let after = header + if code[header] == 0x15 { 2 } else { 1 };
        if let Some((bound, q)) = decode_bound_expr(code, after, back_edge_end, &no_math) {
            if q + 2 < code_len {
                if let Some(cmp) = ExitCmp::from_opcode(code[q]) {
                    // Cast: value to i32 (branch displacement arithmetic)
                    let off = i16::from_be_bytes([code[q + 1], code[q + 2]]) as i32;
                    let target = q as i32 + off; // Cast: value to i32
                    if target < header as i32 || target >= back_edge_end as i32 {
                        return Some(LoopExitTest {
                            iv_local: iv,
                            bound,
                            cmp,
                            form: LoopForm::PreTested,
                        });
                    }
                }
            }
        }
    }

    // ---- Pattern B: the back edge is the test ---------------------------
    let cmp = ExitCmp::from_opcode(code[back_edge])?;
    if back_edge + 2 >= code_len {
        return None;
    }
    // Cast: value to i32 (branch displacement arithmetic)
    let off = i16::from_be_bytes([code[back_edge + 1], code[back_edge + 2]]) as i32;
    let target = back_edge as i32 + off; // Cast: value to i32
    if target < header as i32 || target >= back_edge_end as i32 {
        return None; // not a continue-branch into this loop
    }
    // The comparison's operands are the last `iload iv; <limit>` pair that ends
    // exactly at the back edge. Scanned instruction-aligned from the header so
    // a byte-misaligned coincidence cannot be mistaken for the real operands.
    let mut found: Option<(usize, usize, BoundSource)> = None;
    let mut s = header;
    while s < back_edge {
        if let Some(iv) = extract_iload_local(code, s) {
            let after = s + if code[s] == 0x15 { 2 } else { 1 };
            if let Some((bound, q)) = decode_bound_expr(code, after, back_edge_end, &no_math) {
                if q == back_edge {
                    found = Some((s, iv, bound));
                }
            }
        }
        s += bytecode_analysis::step(code, s);
    }
    let (cmp_start, iv, bound) = found?;
    Some(LoopExitTest {
        iv_local: iv,
        bound,
        cmp: negate_exit_cmp(cmp),
        form: pattern_b_loop_form(code, code_len, header, back_edge_end, cmp_start),
    })
}

/// Recognise `(header, back_edge)` as a [`CountedLoop`] the range analysis can
/// reason about, with a verified comparator and loop form.
///
/// This is the single call that replaces steps 1-3d of the old
/// `analyze_bounds_elimination` (induction variable, loop bound, modified
/// locals, arraylength provenance, non-negative start, step provenance).
fn recognise_loop(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
) -> Option<RecognisedLoop> {
    let back_edge_end = back_edge + bytecode_analysis::step(code, back_edge);
    if back_edge_end > code_len {
        return None;
    }
    let test = locate_exit_test(code, code_len, header, back_edge, back_edge_end)?;
    let mut counted =
        analyze_counted_loop(code, code_len, header, back_edge, test.form, &|_| None)?;
    // `analyze_counted_loop` decodes the FIRST `iload x; <limit>; if_icmp*`
    // triple in the body and maps the opcode straight through
    // `ExitCmp::from_opcode`, without checking that the branch leaves the loop.
    // Accept its answer only when it describes the same `(iv, limit)` the
    // verified exit test does, and take the polarity and the loop form from the
    // verified test — a Pattern-B continue-branch decodes to the OPPOSITE
    // comparison under `from_opcode`'s exit-when-true convention.
    if counted.iv.local != test.iv_local || counted.bound != test.bound {
        return None;
    }
    counted.cmp = test.cmp;
    counted.form = test.form;

    let bound_local = match &counted.bound {
        BoundSource::Local(bl) => Some(*bl),
        _ => None,
    };
    // Whole-method arraylength provenance, expressed in the limit itself rather
    // than as a separate per-access side condition: when the limit local
    // provably holds `A.length`, `prove_index_in_bounds_of` discharges `A[iv]`
    // against the `length >= A.length` tautology and needs no guard at all.
    // Naming a DIFFERENT array changes nothing — which is exactly the
    // multi-array out-of-bounds store this provenance pass exists to prevent
    // (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md).
    if let Some(bl) = bound_local {
        if let Some(a) = find_bound_arraylength_provenance(code, code_len, bl) {
            counted.bound = BoundSource::ArrayLength(a);
        }
    }
    Some(RecognisedLoop {
        counted,
        bound_local,
    })
}

/// What the loop-header pre-header emitter in `x64.rs` can actually prove.
///
/// The emitter's repertoire is fixed and per-header (see the "Speculative BCE"
/// block in `x64.rs`, and [`SpeculativeBCEGuard`]):
///
/// | emitted | proves |
/// |---|---|
/// | `TEST iv,iv; JS deopt` | `iv_entry >= 0` |
/// | `CMP bound, MAX; JE deopt` (inclusive only) | `bound <= i32::MAX - 1` |
/// | `TEST step,step; JS` + `CMP step, MAX-bound; JG` | `0 <= step <= MAX - bound` |
/// | `CMP a.length, bound; JB`/`JBE` (per array) | `a.length >= bound + addend + 1` |
///
/// `Self::covers` is the statement of which [`PreheaderGuard`] each of those
/// discharges. A guard that is not covered means the whole proof is refused:
/// a partially-emitted guard set proves nothing, and an elision resting on an
/// obligation nobody discharged is a silent out-of-bounds access.
struct GuardShape {
    /// Local holding the induction variable (the `iv >= 0` test's operand).
    iv_local: usize,
    /// Local the emitter loads to materialise the limit. `None` refuses every
    /// guarded elision for this loop.
    bound_local: Option<usize>,
    /// The limit as the proof spells it, so a returned guard's term can be
    /// checked to be *this* limit rather than some other runtime value.
    bound_term: BoundTerm,
    /// [`CountedLoop::bound_addend`] — `-1` exclusive, `0` inclusive. Selects
    /// `JB` vs `JBE`, and gates the `bound != Integer.MAX_VALUE` test.
    addend: i32,
    /// Local holding a runtime stride, when there is one.
    step_local: Option<usize>,
}

impl GuardShape {
    /// Whether the emitted pre-header proves `g`.
    fn covers(&self, g: &PreheaderGuard) -> bool {
        match g {
            // The `iv >= 0` header test, and nothing else. A non-negativity
            // obligation on any other term (a symbolic limit, a decreasing
            // loop's `bound + 1` endpoint) has no emitter.
            PreheaderGuard::NonNegative(t) => {
                t.base == BoundTerm::IvEntry(self.iv_local) && t.addend == 0
            }
            // The per-array length compare proves `length >= bound + addend + 1`
            // (`JB` for `addend == -1`, `JBE` for `addend == 0`), so it
            // discharges any demand no stronger than that.
            //
            // NOTE — 64-bit endpoint. The emitted compare is currently a 32-bit
            // *unsigned* one, which is sound for the two addends above (the
            // endpoint is never materialised: `JBE` expresses `>= bound + 1`
            // without computing it, and a negative bound reads as a huge
            // unsigned and deopts). `PreheaderGuard::LengthAtLeast` is
            // nevertheless specified to be evaluated in 64 bits so
            // `base + addend` cannot wrap for any other addend; see
            // `docs/jit/bce-range-integration.md` for the exact x64.rs edit that
            // makes the compare a sign-extended 64-bit one. Until it lands,
            // this arm admits only the addends the current encoding proves.
            PreheaderGuard::LengthAtLeast(t) => {
                self.bound_local.is_some()
                    && t.base == self.bound_term
                    && (t.addend as i64) <= self.addend as i64 + 1
            }
            // The `bound != Integer.MAX_VALUE` entry test, emitted only for the
            // inclusive comparator. It proves `bound <= i32::MAX - 1`, hence
            // `bound + t.addend <= i32::MAX - 1 + t.addend`.
            PreheaderGuard::AtMost { term, limit } => {
                self.addend == 0
                    && self.bound_local.is_some()
                    && term.base == self.bound_term
                    && (i32::MAX as i64 - 1) + term.addend as i64 <= *limit as i64
            }
            // A decreasing loop's `i32::MIN` obligation has no emitter at all.
            PreheaderGuard::AtLeast { .. } => false,
            // `0 <= step` and `step <= MAX - bound`. The proof asks for
            // `step <= MAX - (bound + addend)`; with `addend <= 0` the emitted
            // form is the stricter of the two, so it discharges the demand.
            PreheaderGuard::StrideInRange { local, headroom } => {
                self.step_local == Some(*local)
                    && self.bound_local.is_some()
                    && headroom.base == self.bound_term
                    && headroom.addend <= 0
            }
            // A minimum-trip-count obligation has no emitter here. It exists
            // for the vectorization gate ("this loop runs at least VF times");
            // BCE never asks for one, so an unemitted guard must read as NOT
            // covered rather than as vacuously satisfied.
            PreheaderGuard::TripCountAtLeast { .. } => false,
        }
    }
}

/// For each loop, the innermost other loop whose PC extent strictly contains
/// it — the nesting an inner limit that mentions the outer induction variable
/// needs (`RangeEnv::with_loop_iv`).
fn innermost_enclosing(code: &[u8], loops: &[(usize, usize)]) -> Vec<Option<usize>> {
    let spans: Vec<(usize, usize)> = loops
        .iter()
        .map(|&(h, b)| (h, b + bytecode_analysis::step(code, b)))
        .collect();
    (0..loops.len())
        .map(|i| {
            let (s, e) = spans[i];
            let mut best: Option<usize> = None;
            for (j, &(js, je)) in spans.iter().enumerate() {
                if i == j || (js, je) == (s, e) {
                    continue;
                }
                if js <= s && e <= je {
                    best = Some(match best {
                        Some(b) if spans[b].0 >= js => b,
                        _ => j,
                    });
                }
            }
            best
        })
        .collect()
}

/// Perform bounds check elimination analysis for all loops in the method.
/// Returns a set of bytecode PCs where bounds checks can be safely skipped,
/// and a list of speculative BCE guards to emit at loop headers.
///
/// Each loop is recognised once (`recognise_loop`) and each array access
/// proved once ([`CountedLoop::prove_index_in_bounds_of`]). The verdicts map
/// onto the existing output exactly as they did before:
///
/// * [`BoundsProof::Static`] → the PC joins `safe_pcs` with no guard;
/// * [`BoundsProof::Guarded`] → **every** guard must be covered by
///   `GuardShape::covers`, and the PC then joins `safe_pcs` and its array's
///   `covered_pcs`;
/// * [`BoundsProof::Refused`] → the per-element check stays.
///
/// Guards stay attributed per array. `LengthAtLeast` names no array by
/// contract, so three arrays produce three identical-looking guards and they
/// must NOT be merged: discharging the shortest array's obligation with the
/// longest array's length is the multi-array out-of-bounds store
/// (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md).
///
/// This is the **counted-loop** reason and the only one this entry point can
/// run. The second, independent reason — the guard-dominated range analysis —
/// needs the method's exception table to be sound and is therefore only
/// available through [`analyze_bounds_elimination_with_handlers`]; see that
/// function's doc comment for why, and `docs/jit/range-analysis.md` for the
/// argument in full.
pub(super) fn analyze_bounds_elimination(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>) {
    analyze_bounds_elimination_with_handlers(code, code_len, loops, None)
}

/// [`analyze_bounds_elimination`] plus the **guard-dominated range** reason.
///
/// `handlers` is the method's exception table as `(start_pc, end_pc,
/// handler_pc)`, exactly the shape `refine_ambiguous_local_kinds` already
/// takes. `None` means "this caller does not know the table", and the range
/// reason is then **switched off entirely** rather than run on an assumption:
///
/// > A flow-sensitive fact ("local `i` is non-negative and below
/// > `a.length` here") is a claim about every way control can arrive at a
/// > program point. An exception edge is one of those ways, it can originate
/// > at *any* throwing instruction in a protected range, and it lands with the
/// > operand stack reset to `[throwable]`. Without the table the analysis
/// > cannot see those edges, and a fact that is true on every edge it *can*
/// > see is simply not a fact. `None` therefore refuses.
///
/// It is not possible to recover the table from the bytecode: a handler entry
/// need not be a branch target, and the only structural signature it leaves —
/// entry stack depth exactly one, holding a reference — is shared by the
/// middle of every ordinary two-operand expression.
pub(super) fn analyze_bounds_elimination_with_handlers(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    handlers: Option<&[(usize, usize, usize)]>,
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>) {
    let (mut safe_pcs, guards) = analyze_counted_loop_bce(code, code_len, loops);
    if let Some(h) = handlers {
        if range_bce_enabled() {
            for pc in range_safe_pcs(code, code_len, h) {
                safe_pcs.insert(pc);
            }
        }
    }
    (safe_pcs, guards)
}

/// The counted-loop reason, unchanged. See
/// [`analyze_bounds_elimination`] for the contract.
fn analyze_counted_loop_bce(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>) {
    let mut safe_pcs = FxHashSet::default();
    let mut speculative_guards: Vec<SpeculativeBCEGuard> = Vec::new();
    // DBG (env-gated): CRATONVM_JIT_NO_SPEC_BCE disables ONLY the speculative
    // (runtime-guarded) BCE, keeping the statically-proven elisions — to
    // isolate whether the speculative guard is the unsound corruptor.
    // Cached in a OnceLock so the env lookup is paid once, not per loop.
    let no_spec_bce = jit_no_spec_bce();

    // Recognise every loop up front so an inner loop can be proved in an
    // environment that already knows its enclosing loop's IV range.
    let recognised: Vec<Option<RecognisedLoop>> = loops
        .iter()
        .map(|&(header, back_edge)| recognise_loop(code, code_len, header, back_edge))
        .collect();
    let enclosing = innermost_enclosing(code, loops);

    for (li, &(header, back_edge)) in loops.iter().enumerate() {
        let Some(rl) = recognised[li].as_ref() else {
            continue;
        };
        let loop_ = &rl.counted;
        let back_edge_end = back_edge + bytecode_analysis::step(code, back_edge);

        // The inclusive comparator is no longer a CORRECTNESS refusal — the
        // proof handles it as `bound_addend() == 0`, a `length >= bound + 1`
        // guard and a `bound <= MAX - 1` entry test. It stays gated on
        // `inclusive_spec_bce_enabled` (default OFF) because inclusive elision
        // measured a ~2x NET LOSS on the Sieve OSR artifact — a code-layout
        // effect that generalising the proof does not change. See the flag's
        // own doc comment.
        if loop_.is_inclusive() && !inclusive_spec_bce_enabled() {
            continue;
        }

        // Producer obligation the proof engine states but cannot check: a
        // runtime stride's pre-header guard is evaluated once, so the step
        // local must be loop-invariant (and representable in `modified_locals`)
        // or the guard goes stale on the iteration that changes it.
        if let Stride::Variable(s) = loop_.iv.stride {
            if s >= 64 || (loop_.modified_locals & (1u64 << s)) != 0 {
                continue;
            }
        }
        // Same obligation for the limit's own local: the emitter re-loads
        // `bound_local` in the pre-header, so a body that raises it would let a
        // later exit test admit an index past the guarded length. (For an
        // `ArrayLength` limit `BoundSource::is_invariant` already covers the
        // array local; this covers the local the emitter actually reads.)
        let bound_local = rl
            .bound_local
            .filter(|bl| *bl < 64 && (loop_.modified_locals & (1u64 << *bl)) == 0);

        let env = match enclosing[li].and_then(|pi| recognised[pi].as_ref()) {
            Some(outer) => RangeEnv::new().with_loop_iv(&outer.counted),
            None => RangeEnv::new(),
        };

        // Soundly identify the (array_local, index_local) consumed by each
        // analysable array access via operand-stack producer tracking. NOT
        // subsumed by the range analysis: this is the *producer* of the index
        // expression, and its STOP-on-anything-unmodelled discipline stays
        // load-bearing.
        let operands = analyze_array_access_operands(code, header, back_edge_end);
        // `operands` is a hash map, so its iteration order is nondeterministic;
        // sort so guard emission order (and thus codegen) is reproducible.
        let mut accesses: Vec<(usize, usize, usize)> = operands
            .iter()
            .map(|(&pc, &(arr, idx))| (pc, arr, idx))
            .collect();
        accesses.sort_unstable();

        let addend = loop_.bound_addend();
        let step_local = match loop_.iv.stride {
            Stride::Variable(s) => Some(s),
            Stride::Const(_) => None,
        };
        let shape = GuardShape {
            iv_local: loop_.iv.local,
            bound_local,
            bound_term: BoundTerm::Bound(loop_.bound.clone()),
            addend,
            step_local,
        };

        // One guard per DISTINCT array local, recording which access PCs its
        // pass justifies (`covered_pcs`) so a later de-spec restores exactly
        // those checks.
        let mut guard_arrays: Vec<(usize, Vec<usize>)> = Vec::new();
        for (pc, arr_local, idx_local) in accesses {
            // The index must be this loop's induction variable. There is no
            // producer for a non-identity `IndexExpr` yet — `scale`/`offset`
            // are available in the proof but nothing computes them here — so a
            // derived index keeps its per-element check.
            if idx_local != loop_.iv.local {
                continue;
            }
            // Array-local invariance. `< 64` is load-bearing, not just a
            // bitmask bound: a local the `u64` cannot represent is refused
            // rather than assumed unmodified.
            if arr_local >= 64 || (loop_.modified_locals & (1u64 << arr_local)) != 0 {
                continue;
            }
            let idx = IndexExpr::identity(loop_.iv.local);
            let len = IntRange::array_length();
            let mut proof = loop_.prove_index_in_bounds_of(&idx, Some(arr_local), len, &env);
            // A statically-known NEGATIVE entry value is a refusal for the
            // proof (it can show the first index is out of range) but not for
            // this consumer: the emitted `iv >= 0` header test re-checks the
            // entry value at runtime and deopts, which discharges the same
            // obligation without trusting the constant. Retry once with the
            // entry value widened to unknown — a strictly weaker assumption,
            // so the retry can only prove less.
            if matches!(
                proof,
                BoundsProof::Refused(RefusalReason::IndexMayBeNegative)
            ) {
                let mut widened = loop_.clone();
                widened.iv.init = IntRange::unknown();
                proof = widened.prove_index_in_bounds_of(&idx, Some(arr_local), len, &env);
            }
            match proof {
                BoundsProof::Static => {
                    safe_pcs.insert(pc);
                }
                BoundsProof::Guarded(guards) => {
                    if no_spec_bce || bound_local.is_none() {
                        continue;
                    }
                    // ALL or NOTHING. One guard this emitter cannot discharge
                    // makes the whole proof worthless.
                    if !guards.iter().all(|g| shape.covers(g)) {
                        continue;
                    }
                    safe_pcs.insert(pc);
                    match guard_arrays.iter_mut().find(|(a, _)| *a == arr_local) {
                        Some((_, pcs)) => pcs.push(pc),
                        None => guard_arrays.push((arr_local, vec![pc])),
                    }
                }
                BoundsProof::Refused(_) => {}
            }
        }

        if let Some(bl) = bound_local {
            for (arr_local, covered_pcs) in guard_arrays {
                speculative_guards.push(SpeculativeBCEGuard {
                    loop_header: header,
                    array_local: arr_local,
                    bound_local: bl,
                    iv_local: loop_.iv.local,
                    covered_pcs,
                    inclusive: addend == 0,
                    step_local,
                });
            }
        }
    }

    (safe_pcs, speculative_guards)
}

// ===========================================================================
// Guard-dominated bounds-check elimination — the range-analysis reason
//
// The counted-loop reason above answers exactly one question: "is this index
// the induction variable of a loop whose exit test bounds it?". Everything
// else keeps its per-element check, including the single commonest guarded
// access in Java:
//
//     if (i >= 0 && i < a.length) { … a[i] … }
//
// This pass is the second, independent reason. It runs a small flow-sensitive
// analysis over the method whose abstract state is
//
//   * a `range_analysis::Range` per JVM local — the numeric half, carrying the
//     machine-integer overflow discipline (`iinc i, 1` on an unbounded `i`
//     yields TOP, never a shifted interval); and
//   * a set of *symbolic* facts `i < a.length` — the half a numeric interval
//     cannot express, because `a.length` is a runtime value.
//
// Both halves are required. `i >= 0` alone proves nothing (the unsigned
// bounds compare `emit_bounds_check` emits catches negatives, so the range
// half is not even the interesting one); `i < a.length` alone permits a
// negative index straight below the array base.
//
// Every design decision here is fail-closed:
//
//   * unmodelled control flow (`tableswitch`, `lookupswitch`, `jsr`/`ret`,
//     `goto_w`) refuses the WHOLE method — the same rule
//     `collect_i16_branch_targets` already enforces;
//   * an exception-handler entry is seeded TOP-with-no-facts, so nothing
//     downstream of a handler inherits a fact the exception edge did not
//     establish;
//   * the iteration is bounded and widened, and exhausting the budget reports
//     NOTHING rather than a partial fixpoint;
//   * a fact is killed by any write to either local it mentions, and an
//     `iinc` that cannot be proved not to wrap kills it too.
//
// It deliberately proves less than it could: only accesses whose array and
// index are bare locals, and only where the guard is a literal comparison
// against `a.length` or a constant. Widening that repertoire is a follow-up;
// widening it wrongly is an out-of-bounds heap write.
// ===========================================================================

use crate::range_analysis::{IntWidth, Range};

thread_local! {
    /// Test-only override of the [`range_bce_enabled`] gate, mirroring
    /// [`INCLUSIVE_SPEC_BCE_TEST_OVERRIDE`].
    pub(super) static RANGE_BCE_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only override for [`range_bce_enabled`].
pub fn __set_range_bce_override(v: Option<bool>) {
    RANGE_BCE_TEST_OVERRIDE.with(|c| c.set(v));
}

/// Whether the guard-dominated range reason may contribute PCs.
///
/// **Default OFF**, enabled by `CRATONVM_JIT_RANGE_BCE=1`.
///
/// This reason landed with an opt-OUT switch (`CRATONVM_JIT_NO_RANGE_BCE`),
/// matching `CRATONVM_JIT_NO_SPEC_BCE`'s polarity. The polarity was flipped
/// deliberately at merge: this is a brand-new reason for DELETING a bounds
/// check, it has never been benchmarked or differentially tested, and a wrong
/// elision is an out-of-bounds heap write — the worst failure this compiler
/// can produce. Its own author's note applies: "the proof got stronger" is not
/// evidence the elision is a win, and inclusive counted-loop elision measured
/// a ~2x net LOSS on one OSR artifact for pure code-layout reasons.
///
/// Flip it back to opt-out once it has a differential run behind it.
/// `CRATONVM_JIT_NO_BCE` still kills every reason including this one, and
/// `CRATONVM_JIT_NO_SPEC_BCE` still kills exactly the speculative one, so the
/// bisection story the two narrow switches provide is unchanged.
pub(super) fn range_bce_enabled() -> bool {
    if let Some(v) = RANGE_BCE_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_RANGE_BCE"))
}

/// Largest method this pass will analyse, in bytecode bytes.
///
/// The state is one `Range` per local per reached instruction; the cap bounds
/// both compile time and the peak allocation (at the limits below,
/// ~8 K instructions x 64 locals x 16 bytes = 8 MB worst case, and in practice
/// far less because only reached instruction starts hold a state).
const RANGE_MAX_CODE_LEN: usize = 8192;

/// Largest local slot index the pass will model. A method with a wider frame
/// is refused outright rather than analysed with some locals invisible.
const RANGE_MAX_LOCALS: usize = 64;

/// Most `i < a.length` facts one state carries. A state that would exceed it
/// drops the new fact — sound, because a missing fact only costs an elision.
const RANGE_MAX_FACTS: usize = 64;

/// Merges into one PC before its numeric ranges start widening.
const RANGE_WIDEN_AFTER: u32 = 3;

/// Sentinel for "no instruction start here" in the `prev`/`next` tables.
const NO_START: usize = usize::MAX;

/// The abstract state on entry to one bytecode PC.
#[derive(Clone, PartialEq, Debug)]
struct RangeState {
    /// One `int`-width range per JVM local slot. A slot holding a reference or
    /// a `long` simply carries a range nobody reads.
    locals: Vec<Range>,
    /// `(index_local, array_local)` pairs proven `index < array.length` on
    /// every path reaching this PC. Sorted and deduplicated so the merge is a
    /// linear intersection.
    lt_len: Vec<(u16, u16)>,
}

impl RangeState {
    /// The state that proves nothing: every local unknown, no facts. Also the
    /// seed for method entry and for every exception-handler entry.
    fn top(n_locals: usize) -> RangeState {
        RangeState {
            locals: vec![Range::top(IntWidth::W32); n_locals],
            lt_len: Vec::new(),
        }
    }

    /// Merge `other` in as an additional predecessor. Ranges join (hull),
    /// facts intersect (a fact must hold on *every* path). Returns whether
    /// anything changed.
    ///
    /// `widen` throws outward-moving range endpoints to the extremes; it is
    /// what makes a loop's merge reach a fixpoint in a bounded number of
    /// visits instead of crawling one integer at a time.
    fn merge(&mut self, other: &RangeState, widen: bool) -> bool {
        let mut changed = false;
        for (i, r) in self.locals.iter_mut().enumerate() {
            let o = match other.locals.get(i) {
                Some(o) => *o,
                None => Range::top(IntWidth::W32),
            };
            let joined = r.join(o);
            let next = if widen { r.widen(joined) } else { joined };
            if next != *r {
                *r = next;
                changed = true;
            }
        }
        let before = self.lt_len.len();
        self.lt_len
            .retain(|f| other.lt_len.binary_search(f).is_ok());
        if self.lt_len.len() != before {
            changed = true;
        }
        changed
    }

    /// Record `index_local < array_local.length`.
    fn add_lt_len(&mut self, index_local: usize, array_local: usize) {
        if index_local >= RANGE_MAX_LOCALS
            || array_local >= RANGE_MAX_LOCALS
            || self.lt_len.len() >= RANGE_MAX_FACTS
        {
            return;
        }
        // Cast: both indexes are below `RANGE_MAX_LOCALS`.
        let f = (index_local as u16, array_local as u16);
        if let Err(at) = self.lt_len.binary_search(&f) {
            self.lt_len.insert(at, f);
        }
    }

    /// Forget everything that mentions local `slot` — it has just been
    /// overwritten, so neither its range nor any fact naming it survives.
    fn kill(&mut self, slot: usize) {
        if let Some(r) = self.locals.get_mut(slot) {
            *r = Range::top(IntWidth::W32);
        }
        // Cast: slot indexes are below `RANGE_MAX_LOCALS` by construction.
        let s = slot.min(u16::MAX as usize) as u16;
        self.lt_len.retain(|&(i, a)| i != s && a != s);
    }

    /// Forget only the facts in which `slot` is the *index* — used when the
    /// local's value is known to have moved but not to have been replaced.
    fn kill_index_facts(&mut self, slot: usize) {
        // Cast: as in `kill`.
        let s = slot.min(u16::MAX as usize) as u16;
        self.lt_len.retain(|&(i, _)| i != s);
    }

    /// Whether `index_local < array_local.length` is proven here.
    fn proves_lt_len(&self, index_local: usize, array_local: usize) -> bool {
        if index_local >= RANGE_MAX_LOCALS || array_local >= RANGE_MAX_LOCALS {
            return false;
        }
        // Cast: bounds-checked immediately above.
        let f = (index_local as u16, array_local as u16);
        self.lt_len.binary_search(&f).is_ok()
    }
}

/// An operand of a comparison, as far as this pass can read it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operand {
    /// The current value of an `int` local.
    Local(usize),
    /// `a.length` for array local `a`.
    Len(usize),
    /// A compile-time constant.
    Const(i64),
}

/// A comparison, in **taken-branch** polarity: the branch is taken iff
/// `lhs REL rhs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rel {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The relation that holds when `rel` does not — i.e. on the fall-through
/// edge of a branch whose taken edge is `rel`.
fn negate_rel(rel: Rel) -> Rel {
    match rel {
        Rel::Eq => Rel::Ne,
        Rel::Ne => Rel::Eq,
        Rel::Lt => Rel::Ge,
        Rel::Ge => Rel::Lt,
        Rel::Gt => Rel::Le,
        Rel::Le => Rel::Gt,
    }
}

/// `a REL b` restated as `b SWAP(REL) a`.
fn swap_rel(rel: Rel) -> Rel {
    match rel {
        Rel::Eq => Rel::Eq,
        Rel::Ne => Rel::Ne,
        Rel::Lt => Rel::Gt,
        Rel::Gt => Rel::Lt,
        Rel::Le => Rel::Ge,
        Rel::Ge => Rel::Le,
    }
}

/// The `(arity, taken-relation)` of a conditional branch opcode, or `None`
/// when the opcode is not an integer comparison (`if_acmp*`, `ifnull`,
/// `ifnonnull`, anything else).
fn branch_relation(op: u8) -> Option<(u8, Rel)> {
    Some(match op {
        0x99 => (1, Rel::Eq),
        0x9a => (1, Rel::Ne),
        0x9b => (1, Rel::Lt),
        0x9c => (1, Rel::Ge),
        0x9d => (1, Rel::Gt),
        0x9e => (1, Rel::Le),
        0x9f => (2, Rel::Eq),
        0xa0 => (2, Rel::Ne),
        0xa1 => (2, Rel::Lt),
        0xa2 => (2, Rel::Ge),
        0xa3 => (2, Rel::Gt),
        0xa4 => (2, Rel::Le),
        _ => return None,
    })
}

/// Narrow `r` by the fact `value REL c`.
///
/// The `c - 1` / `c + 1` endpoints are computed in `i64` so `c == i32::MIN`
/// (`x < Integer.MIN_VALUE`, impossible) collapses to bottom rather than
/// wrapping to a range that admits everything.
fn narrow_by(r: Range, rel: Rel, c: i64) -> Range {
    let w = IntWidth::W32;
    // Widening: the i32 endpoints into the i64 interval domain.
    let (min, max) = (i32::MIN as i64, i32::MAX as i64);
    let bound = match rel {
        Rel::Eq => return r.meet(Range::constant(w, c)),
        // An interval lattice cannot express "everything but c".
        Rel::Ne => return r,
        Rel::Lt => Range::exact(w, min, c - 1),
        Rel::Le => Range::exact(w, min, c),
        Rel::Gt => Range::exact(w, c + 1, max),
        Rel::Ge => Range::exact(w, c, max),
    };
    r.meet(bound)
}

/// A constant push, decoded to its value. `ldc` is excluded — this module
/// carries no constant pool.
fn range_const_push(code: &[u8], pc: usize) -> Option<i64> {
    let op = *code.get(pc)?;
    match op {
        // Widening: the opcode-relative constant into i64.
        0x02 => Some(-1),
        0x03..=0x08 => Some((op - 0x03) as i64),
        // Cast: the bipush operand is a signed byte.
        0x10 => code.get(pc + 1).map(|&b| b as i8 as i64),
        // Widening: the sipush operand is a signed 16-bit immediate.
        0x11 => Some(i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]) as i64),
        _ => None,
    }
}

/// True for an opcode that pushes exactly one operand-stack entry and pops
/// none: the constants, the `ldc` family and every `*load`.
///
/// Used to recognise the *value* operand of an array store without simulating
/// the operand stack. It is the same contiguity argument the comparison decode
/// uses: three consecutive single-push instructions followed by an `xastore`
/// leave exactly those three values on top, whatever is beneath them.
fn is_single_push(op: u8) -> bool {
    matches!(op, 0x01..=0x14 | 0x15..=0x19 | 0x1a..=0x2d)
}

/// A write to a JVM local.
#[derive(Clone, Copy, Debug)]
enum RangeWrite {
    /// `*store` — the slot's value is replaced. `wide` marks a `long`/`double`
    /// store, which also clobbers the following slot.
    Store { slot: usize, wide: bool },
    /// `iinc slot, delta`.
    Iinc { slot: usize, delta: i64 },
}

/// The local write performed by the instruction at `pc`, if any.
///
/// Deliberately independent of [`local_access_at`], which answers a different
/// question (it reports *loads* too, and it does not carry the `iinc` delta or
/// the cat-2 clobber). The overlap is one match arm per opcode family; the
/// alternative — teaching `local_access_at` to answer both — would make the
/// deopt-snapshot classifier depend on this pass's needs.
fn range_local_write(code: &[u8], code_len: usize, pc: usize) -> Option<RangeWrite> {
    if pc >= code_len {
        return None;
    }
    let op = *code.get(pc)?;
    // Widening: every operand/opcode-relative local index fits a usize.
    Some(match op {
        // istore / fstore / astore (wide index)
        0x36 | 0x38 | 0x3a => RangeWrite::Store {
            slot: *code.get(pc + 1)? as usize,
            wide: false,
        },
        // lstore / dstore (wide index)
        0x37 | 0x39 => RangeWrite::Store {
            slot: *code.get(pc + 1)? as usize,
            wide: true,
        },
        0x3b..=0x3e => RangeWrite::Store {
            slot: (op - 0x3b) as usize,
            wide: false,
        },
        0x3f..=0x42 => RangeWrite::Store {
            slot: (op - 0x3f) as usize,
            wide: true,
        },
        0x43..=0x46 => RangeWrite::Store {
            slot: (op - 0x43) as usize,
            wide: false,
        },
        0x47..=0x4a => RangeWrite::Store {
            slot: (op - 0x47) as usize,
            wide: true,
        },
        0x4b..=0x4e => RangeWrite::Store {
            slot: (op - 0x4b) as usize,
            wide: false,
        },
        // Cast: the iinc delta is a signed byte.
        0x84 => RangeWrite::Iinc {
            slot: *code.get(pc + 1)? as usize,
            delta: *code.get(pc + 2)? as i8 as i64,
        },
        0xc4 => {
            let real = *code.get(pc + 1)?;
            let idx = ((*code.get(pc + 2)? as usize) << 8) | *code.get(pc + 3)? as usize;
            match real {
                0x36 | 0x38 | 0x3a => RangeWrite::Store {
                    slot: idx,
                    wide: false,
                },
                0x37 | 0x39 => RangeWrite::Store {
                    slot: idx,
                    wide: true,
                },
                // Widening: the wide-iinc delta is a signed 16-bit immediate.
                0x84 => RangeWrite::Iinc {
                    slot: idx,
                    delta: i16::from_be_bytes([*code.get(pc + 4)?, *code.get(pc + 5)?]) as i64,
                },
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// Instruction-start tables for one method: `prev[pc]` / `next[pc]` are the
/// linearly adjacent instruction starts ([`NO_START`] at the ends), and
/// `is_start[pc]` marks a decoded boundary.
struct StartTables {
    prev: Vec<usize>,
    next: Vec<usize>,
    is_start: Vec<bool>,
}

/// Decode `code` linearly into [`StartTables`]. `None` when the decode does
/// not make progress, which would otherwise loop forever.
fn range_start_tables(code: &[u8], code_len: usize) -> Option<StartTables> {
    let mut t = StartTables {
        prev: vec![NO_START; code_len],
        next: vec![NO_START; code_len],
        is_start: vec![false; code_len],
    };
    let mut pc = 0usize;
    let mut prev = NO_START;
    while pc < code_len {
        t.is_start[pc] = true;
        t.prev[pc] = prev;
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            return None;
        }
        let nxt = pc + len;
        if nxt < code_len {
            t.next[pc] = nxt;
        }
        prev = pc;
        pc = nxt;
    }
    Some(t)
}

/// Whether every instruction start strictly after `start` and no later than
/// `end` is free of branch targets.
///
/// This is what licenses reading a multi-instruction pattern as one atom: a
/// branch landing in the middle of `iload i; aload a; arraylength; if_icmplt`
/// would deliver *different* values to the comparison. Landing on `start`
/// itself is harmless — the whole pattern then re-executes.
fn range_span_is_atomic(
    t: &StartTables,
    branch_targets: &FxHashSet<usize>,
    start: usize,
    end: usize,
) -> bool {
    let mut s = start;
    while s < end {
        let nxt = t.next.get(s).copied().unwrap_or(NO_START);
        if nxt == NO_START || nxt > end {
            return false;
        }
        if branch_targets.contains(&nxt) {
            return false;
        }
        s = nxt;
    }
    s == end
}

/// Decode the operand whose producer *ends* at instruction `p`, returning it
/// with the PC its producer *starts* at.
fn decode_operand_ending_at(code: &[u8], t: &StartTables, p: usize) -> Option<(Operand, usize)> {
    let op = *code.get(p)?;
    // `aload a; arraylength` — two instructions, one operand.
    if op == 0xbe {
        let q = *t.prev.get(p)?;
        if q == NO_START {
            return None;
        }
        let a = extract_aload_local(code, q)?;
        return Some((Operand::Len(a), q));
    }
    if let Some(l) = extract_iload_local(code, p) {
        return Some((Operand::Local(l), p));
    }
    if let Some(c) = range_const_push(code, p) {
        return Some((Operand::Const(c), p));
    }
    None
}

/// Decode the comparison performed by the conditional branch at `pc`, in
/// taken-branch polarity.
///
/// Returns `None` unless the operands are a contiguous, branch-target-free run
/// of instructions ending at `pc` — see [`range_span_is_atomic`].
fn decode_compare(
    code: &[u8],
    t: &StartTables,
    branch_targets: &FxHashSet<usize>,
    pc: usize,
) -> Option<(Operand, Operand, Rel)> {
    let (arity, rel) = branch_relation(*code.get(pc)?)?;
    let p1 = *t.prev.get(pc)?;
    if p1 == NO_START {
        return None;
    }
    if arity == 1 {
        let (lhs, start) = decode_operand_ending_at(code, t, p1)?;
        if !range_span_is_atomic(t, branch_targets, start, pc) {
            return None;
        }
        return Some((lhs, Operand::Const(0), rel));
    }
    let (rhs, rhs_start) = decode_operand_ending_at(code, t, p1)?;
    let p2 = *t.prev.get(rhs_start)?;
    if p2 == NO_START {
        return None;
    }
    let (lhs, lhs_start) = decode_operand_ending_at(code, t, p2)?;
    if !range_span_is_atomic(t, branch_targets, lhs_start, pc) {
        return None;
    }
    Some((lhs, rhs, rel))
}

/// Apply `lhs REL rhs` to `st`.
///
/// Two independent refinements, both optional: a numeric narrowing when one
/// side is a constant, and the symbolic `< length` fact when the comparison is
/// literally an index against an array length.
fn apply_rel(st: &mut RangeState, lhs: Operand, rhs: Operand, rel: Rel) {
    match (lhs, rhs) {
        (Operand::Local(l), Operand::Const(c)) => {
            if let Some(r) = st.locals.get_mut(l) {
                *r = narrow_by(*r, rel, c);
            }
        }
        (Operand::Const(c), Operand::Local(l)) => {
            if let Some(r) = st.locals.get_mut(l) {
                *r = narrow_by(*r, swap_rel(rel), c);
            }
        }
        _ => {}
    }
    match (lhs, rhs, rel) {
        // `i < a.length`
        (Operand::Local(l), Operand::Len(a), Rel::Lt) => st.add_lt_len(l, a),
        // `a.length > i` — the same fact, written the other way round.
        (Operand::Len(a), Operand::Local(l), Rel::Gt) => st.add_lt_len(l, a),
        _ => {}
    }
}

/// The array-element access at `pc`, as `(array_local, index_local)`.
///
/// Recognised without simulating the operand stack, by the contiguity argument
/// spelled out on [`is_single_push`]: `aload a; iload i; xaload` (and
/// `aload a; iload i; <one push>; xastore`) put exactly those values on top of
/// whatever the stack already held. `range_span_is_atomic` rejects a pattern a
/// branch can enter part-way through.
fn decode_array_access(
    code: &[u8],
    t: &StartTables,
    branch_targets: &FxHashSet<usize>,
    pc: usize,
) -> Option<(usize, usize)> {
    let op = *code.get(pc)?;
    let idx_pc = match op {
        // xaload: [array, index] -> [value]
        0x2e..=0x35 => *t.prev.get(pc)?,
        // xastore: [array, index, value] -> []
        0x4f..=0x56 => {
            let value_pc = *t.prev.get(pc)?;
            if value_pc == NO_START || !is_single_push(*code.get(value_pc)?) {
                return None;
            }
            *t.prev.get(value_pc)?
        }
        _ => return None,
    };
    if idx_pc == NO_START {
        return None;
    }
    let index_local = extract_iload_local(code, idx_pc)?;
    let arr_pc = *t.prev.get(idx_pc)?;
    if arr_pc == NO_START {
        return None;
    }
    let array_local = extract_aload_local(code, arr_pc)?;
    if !range_span_is_atomic(t, branch_targets, arr_pc, pc) {
        return None;
    }
    Some((array_local, index_local))
}

/// The number of local slots to model, or `None` when the method's frame is
/// wider than [`RANGE_MAX_LOCALS`] (refused rather than partially modelled).
fn range_num_locals(code: &[u8], code_len: usize, t: &StartTables) -> Option<usize> {
    let mut max_slot = 0usize;
    let mut pc = 0usize;
    while pc < code_len {
        if t.is_start[pc] {
            if let Some((_, slot)) = local_access_at(code, code_len, pc) {
                if slot >= RANGE_MAX_LOCALS {
                    return None;
                }
                max_slot = max_slot.max(slot);
            }
        }
        pc += 1;
    }
    // +2 so a cat-2 store at the highest slot still has its clobbered high
    // half inside the vector.
    Some((max_slot + 2).min(RANGE_MAX_LOCALS))
}

/// Merge `s` into the state on entry to `succ` and re-queue it if that changed
/// anything.
///
/// `visits` counts merges per PC; past [`RANGE_WIDEN_AFTER`] the merge widens,
/// which is what bounds the number of times a loop header's state can move up
/// the lattice. Without it the walk is a `2^32`-tall descent that pins the
/// compiler thread — reported by this VM's watchdog as a *VM hang*.
fn range_push_to(
    t: &StartTables,
    code_len: usize,
    visits: &mut [u32],
    state: &mut [Option<RangeState>],
    work: &mut Vec<usize>,
    succ: usize,
    s: RangeState,
) {
    if succ >= code_len || !t.is_start[succ] {
        return;
    }
    visits[succ] = visits[succ].saturating_add(1);
    let widen = visits[succ] > RANGE_WIDEN_AFTER;
    if state[succ].is_none() {
        state[succ] = Some(s);
        work.push(succ);
        return;
    }
    if let Some(cur) = state[succ].as_mut() {
        if cur.merge(&s, widen) {
            work.push(succ);
        }
    }
}

/// Bytecode PCs whose array bounds check is discharged by a *dominating guard*
/// rather than by a counted loop.
///
/// Returns an empty set — never a partial answer — whenever anything about the
/// method cannot be modelled: unmodelled control flow, an over-wide frame, an
/// over-long method, a decode that does not advance, or an exhausted iteration
/// budget.
pub(super) fn range_safe_pcs(
    code: &[u8],
    code_len: usize,
    handlers: &[(usize, usize, usize)],
) -> FxHashSet<usize> {
    if code_len == 0 || code_len > RANGE_MAX_CODE_LEN || code_len > code.len() {
        return FxHashSet::default();
    }
    // Refuses `tableswitch`/`lookupswitch`/`jsr`/`ret`/`goto_w`/`jsr_w`: their
    // successors are not modelled, so a fact could survive an edge this pass
    // never walked.
    let Some(branch_targets) = collect_i16_branch_targets(code, code_len) else {
        return FxHashSet::default();
    };
    let Some(t) = range_start_tables(code, code_len) else {
        return FxHashSet::default();
    };
    let Some(n_locals) = range_num_locals(code, code_len, &t) else {
        return FxHashSet::default();
    };
    // Every branch must land inside the method, on a decoded instruction
    // start. `collect_i16_branch_targets` silently DROPS a target that does
    // not — which for this walk would be an edge whose state never reaches the
    // join, leaving a fact standing that the unfollowed path would have
    // killed. Re-derive the targets and refuse instead.
    {
        let mut pc = 0usize;
        while pc < code_len {
            if t.is_start[pc] && matches!(code[pc], 0x99..=0xa7 | 0xc6 | 0xc7) {
                if pc + 2 >= code_len {
                    return FxHashSet::default();
                }
                // Cast: branch displacement arithmetic.
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                let target = pc as i32 + off;
                // Cast: bounds-checked immediately.
                if target < 0 || target as usize >= code_len || !t.is_start[target as usize] {
                    return FxHashSet::default();
                }
            }
            pc += 1;
        }
    }

    let mut state: Vec<Option<RangeState>> = vec![None; code_len];
    let mut visits: Vec<u32> = vec![0; code_len];
    let mut work: Vec<usize> = Vec::new();

    // Method entry, and every handler entry. A handler is reachable from ANY
    // throwing instruction in its protected range, with the operand stack
    // reset — so it starts from the state that proves nothing, and everything
    // downstream of it inherits that until a real guard re-establishes a fact.
    let top = RangeState::top(n_locals);
    range_push_to(
        &t,
        code_len,
        &mut visits,
        &mut state,
        &mut work,
        0,
        top.clone(),
    );
    for &(_start, _end, handler_pc) in handlers {
        range_push_to(
            &t,
            code_len,
            &mut visits,
            &mut state,
            &mut work,
            handler_pc,
            top.clone(),
        );
    }

    // Bounded exactly as `refine_ambiguous_local_kinds` bounds its own
    // worklist. Exhausting it reports nothing: a partial fixpoint of a
    // must-analysis claims facts it has not finished intersecting away.
    let mut budget = code_len.saturating_mul(64).saturating_add(64);

    while let Some(pc) = work.pop() {
        budget = budget.saturating_sub(1);
        if budget == 0 {
            return FxHashSet::default();
        }
        let Some(here) = state[pc].clone() else {
            continue;
        };
        let op = match code.get(pc) {
            Some(&op) => op,
            None => continue,
        };

        // The instruction's own effect on the state.
        let mut out = here;
        if let Some(w) = range_local_write(code, code_len, pc) {
            match w {
                RangeWrite::Store { slot, wide } => {
                    out.kill(slot);
                    if wide {
                        out.kill(slot + 1);
                    }
                    // `<const>; istore l` is the one store whose value this
                    // pass can read without simulating the stack.
                    let src = t.prev.get(pc).copied().unwrap_or(NO_START);
                    if src != NO_START
                        && !branch_targets.contains(&pc)
                        && matches!(op, 0x36 | 0x3b..=0x3e)
                    {
                        if let Some(c) = range_const_push(code, src) {
                            if let Some(r) = out.locals.get_mut(slot) {
                                *r = Range::constant(IntWidth::W32, c);
                            }
                        }
                    }
                }
                RangeWrite::Iinc { slot, delta } => {
                    let cur = out.locals.get(slot).copied();
                    match cur.and_then(|r| r.add_no_wrap(Range::constant(IntWidth::W32, delta))) {
                        // The advance provably does not wrap. A non-positive
                        // delta cannot invalidate `slot < a.length`; a positive
                        // one can, so its facts go.
                        Some(next) => {
                            if let Some(r) = out.locals.get_mut(slot) {
                                *r = next;
                            }
                            if delta > 0 {
                                out.kill_index_facts(slot);
                            }
                        }
                        // Unproven wrap: `slot` could reappear anywhere.
                        None => out.kill(slot),
                    }
                }
            }
        }

        let fall = t.next.get(pc).copied().unwrap_or(NO_START);
        // A branch whose 16-bit displacement is truncated by `code_len` cannot
        // be decoded; treat it as having no successors at all rather than
        // reading past the method.
        let branch_target = if matches!(op, 0x99..=0xa7 | 0xc6 | 0xc7) {
            if pc + 2 >= code_len {
                None
            } else {
                // Cast: branch displacement arithmetic.
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                let target = pc as i32 + off;
                // Cast: non-negative branch target to usize.
                if target >= 0 {
                    Some(target as usize)
                } else {
                    None
                }
            }
        } else {
            None
        };

        match op {
            // goto — one successor, no fall-through.
            0xa7 => {
                if let Some(target) = branch_target {
                    range_push_to(
                        &t,
                        code_len,
                        &mut visits,
                        &mut state,
                        &mut work,
                        target,
                        out,
                    );
                }
            }
            // *return / athrow — no successor inside the method.
            0xac..=0xb1 | 0xbf => {}
            // Conditional branches: two edges, refined independently.
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                let (mut taken, mut not_taken) = (out.clone(), out);
                if let Some((lhs, rhs, rel)) = decode_compare(code, &t, &branch_targets, pc) {
                    apply_rel(&mut taken, lhs, rhs, rel);
                    apply_rel(&mut not_taken, lhs, rhs, negate_rel(rel));
                }
                if let Some(target) = branch_target {
                    range_push_to(
                        &t,
                        code_len,
                        &mut visits,
                        &mut state,
                        &mut work,
                        target,
                        taken,
                    );
                }
                range_push_to(
                    &t,
                    code_len,
                    &mut visits,
                    &mut state,
                    &mut work,
                    fall,
                    not_taken,
                );
            }
            _ => range_push_to(&t, code_len, &mut visits, &mut state, &mut work, fall, out),
        }
    }

    // Harvest. An access is discharged only when BOTH halves hold: the index
    // is provably non-negative AND provably below THIS array's length.
    let mut safe = FxHashSet::default();
    let mut pc = 0usize;
    while pc < code_len {
        if !t.is_start[pc] {
            pc += 1;
            continue;
        }
        if let Some((array_local, index_local)) = decode_array_access(code, &t, &branch_targets, pc)
        {
            if let Some(st) = state[pc].as_ref() {
                let idx_range = st
                    .locals
                    .get(index_local)
                    .copied()
                    .unwrap_or_else(|| Range::top(IntWidth::W32));
                let non_negative = !idx_range.is_empty() && idx_range.is_non_negative();
                if non_negative && st.proves_lt_len(index_local, array_local) {
                    safe.insert(pc);
                }
            }
        }
        pc += 1;
    }
    safe
}

#[cfg(test)]
mod ambiguous_local_cfg_exactness_tests {
    use super::*;

    /// `AmbiguousLocalKinds::cfg_is_exact` is the whole soundness condition
    /// behind reading `Ambiguous` as "the verifier types this local `top`".
    /// Both of the constructs that break the CFG equality must clear it, and
    /// the ordinary method must set it — a predicate that answered `true`
    /// everywhere would silently license dropping live locals, and one that
    /// answered `false` everywhere would look like a working gate while
    /// buying nothing.
    ///
    /// The bytecode below reuses slot 0 as an `int` (`istore_0`) on the taken
    /// side of a branch and as a `double` (`dstore_0`) on the other, so slot 0
    /// really is `Ambiguous` for the method and the pass has something to
    /// track. Verified by BREAKING it: dropping the `exception_ranges`
    /// conjunct makes the second case read `true` and this test fails.
    #[test]
    fn cfg_exactness_tracks_handlers_and_jsr() {
        // Slot 0 is an `int` on one arm of a branch and a `double` on the
        // other, so the whole-method classifier must call it `Ambiguous` and
        // the refinement has something to track:
        //   0: iconst_0  1: ifeq ->9  4: iconst_1  5: istore_0
        //   6: goto ->11  9: dconst_0 10: dstore_0 11: return
        let code: &[u8] = &[
            0x03, 0x99, 0x00, 0x08, 0x04, 0x3b, 0xa7, 0x00, 0x05, 0x0e, 0x47, 0xb1,
        ];
        // The same method with the trailing `return` replaced by `jsr; return`.
        let jsr_code: &[u8] = &[
            0x03, 0x99, 0x00, 0x08, 0x04, 0x3b, 0xa7, 0x00, 0x05, 0x0e, 0x47, 0xa8, 0x00, 0x03,
            0xb1,
        ];
        let kinds = classify_local_kinds(code, code.len(), 4);
        assert!(
            matches!(kinds[0], LocalKind::Ambiguous),
            "the fixture must actually produce an ambiguous slot, or every                assertion below passes vacuously"
        );
        let clean = refine_ambiguous_local_kinds(code, code.len(), &kinds, &[]);
        let with_handler = refine_ambiguous_local_kinds(code, code.len(), &kinds, &[(0, 4, 11)]);
        let jsr_kinds = classify_local_kinds(jsr_code, jsr_code.len(), 4);
        let with_jsr = refine_ambiguous_local_kinds(jsr_code, jsr_code.len(), &jsr_kinds, &[]);

        assert!(
            clean.cfg_is_exact && !clean.has_jsr,
            "an exception-free, jsr-free method's CFG IS the verifier's, and the                gate must say so or the relaxation it guards is dead code"
        );
        assert!(
            !with_handler.cfg_is_exact,
            "a handler entry is seeded TOP, which is COARSER than the verifier's                merge of the protected range -- `Ambiguous` there does not imply                `top` and must not license dropping the slot"
        );
        assert!(
            !with_handler.has_jsr,
            "`has_jsr` is the half a caller may check ALONE before trusting a                SETTLED kind; folding the handler half into it would refuse every                method that merely has a try block"
        );
        assert!(
            with_jsr.has_jsr && !with_jsr.cfg_is_exact,
            "`ret` is given no successors at all, which makes the graph NARROWER                than the verifier's, and that is the half a settled kind cannot                survive"
        );
    }
}

#[cfg(test)]
mod range_analysis_bce_tests {

    use super::*;

    /// Run the guard-dominated pass on a handler-free method.
    fn run(code: &[u8]) -> Vec<usize> {
        let mut v: Vec<usize> = range_safe_pcs(code, code.len(), &[]).into_iter().collect();
        v.sort_unstable();
        v
    }

    /// `int f(int[] a, int i) { if (i >= 0 && i < a.length) return a[i]; return -1; }`
    ///
    /// The canonical guarded access, and the one the counted-loop reason
    /// cannot touch at all: there is no loop.
    ///
    /// ```text
    ///  0: iload_1            (i)
    ///  1: iflt   +14 -> 15
    ///  4: iload_1
    ///  5: aload_0
    ///  6: arraylength
    ///  7: if_icmpge +8 -> 15
    /// 10: aload_0
    /// 11: iload_1
    /// 12: iaload             <-- the access
    /// 13: ireturn
    /// 15: iconst_m1
    /// 16: ireturn
    /// ```
    fn guarded_load() -> Vec<u8> {
        vec![
            0x1b, 0x9b, 0x00, 0x0e, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x08, 0x2a, 0x1b, 0x2e, 0xac,
            0x02, 0xac,
        ]
    }

    #[test]
    fn both_halves_of_the_guard_eliminate_the_check() {
        assert_eq!(run(&guarded_load()), vec![12]);
    }

    /// MUST REFUSE: drop the `i >= 0` half. `i < a.length` alone admits a
    /// negative index, which is an access *below* the array base — strictly
    /// worse than the overrun the check normally catches.
    #[test]
    fn upper_bound_alone_is_not_enough() {
        // Same as `guarded_load` with the `iflt` replaced by two `nop`s plus a
        // `pop` of the loaded `i`, so the length guard still dominates.
        //  0: iload_1 ; 1: pop ; 2: nop ; 3: nop
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +8 -> 15
        // 10: aload_0 ; 11: iload_1 ; 12: iaload ; 13: ireturn
        // 15: iconst_m1 ; 16: ireturn
        let code: Vec<u8> = vec![
            0x1b, 0x57, 0x00, 0x00, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x08, 0x2a, 0x1b, 0x2e, 0xac,
            0x02, 0xac,
        ];
        assert!(
            run(&code).is_empty(),
            "an unbounded-below index must keep its check"
        );
    }

    /// MUST REFUSE: drop the length half, keep `i >= 0`.
    #[test]
    fn lower_bound_alone_is_not_enough() {
        //  0: iload_1 ; 1: iflt +12 -> 13
        //  4: aload_0 ; 5: iload_1 ; 6: iaload ; 7: ireturn
        //  ... padding so the branch target is a real instruction start
        let code: Vec<u8> = vec![
            0x1b, 0x9b, 0x00, 0x0c, 0x2a, 0x1b, 0x2e, 0xac, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
            0xac,
        ];
        assert!(run(&code).is_empty(), "no upper bound, no elision");
    }

    /// MUST REFUSE: the guard names a DIFFERENT array from the access. This is
    /// the multi-array out-of-bounds store
    /// (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md) in its
    /// guard-dominated spelling — `a.length` says nothing about `b.length`.
    #[test]
    fn a_guard_on_another_array_proves_nothing() {
        // locals: 0=a 1=b 2=i
        //  0: iload_2 ; 1: iflt +15 -> 16
        //  4: iload_2 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +9 -> 16
        // 10: aload_1 ; 11: iload_2 ; 12: iaload ; 13: pop ; 14: nop ; 15: nop
        // 16: return
        let code: Vec<u8> = vec![
            0x1c, 0x9b, 0x00, 0x0f, 0x1c, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x2b, 0x1c, 0x2e, 0x57,
            0x00, 0x00, 0xb1,
        ];
        assert!(
            run(&code).is_empty(),
            "b[i] under a guard on a.length must keep its check"
        );
        // …and the SAME method with the access on `a` does eliminate, so the
        // refusal above is about the array identity and not about the shape.
        let mut on_a = code.clone();
        on_a[10] = 0x2a; // aload_1 -> aload_0
        assert_eq!(run(&on_a), vec![12]);
    }

    /// MUST REFUSE: the guard is *after* the access.
    #[test]
    fn a_guard_that_does_not_dominate_proves_nothing() {
        //  0: aload_0 ; 1: iload_1 ; 2: iaload ; 3: pop      (unguarded access)
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +4 -> 11
        // 10: nop ; 11: return
        let code: Vec<u8> = vec![
            0x2a, 0x1b, 0x2e, 0x57, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x04, 0x00, 0xb1,
        ];
        assert!(run(&code).is_empty());
    }

    /// MUST REFUSE: the index is reassigned between the guard and the access.
    #[test]
    fn a_write_to_the_index_kills_the_fact() {
        //  0: iload_1 ; 1: iflt +16 -> 17
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +10 -> 17
        // 10: sipush 9999 ; 13: istore_1          <-- i is now anything
        // 14: aload_0 ; 15: iload_1 ; 16: iaload
        // 17: return
        let code: Vec<u8> = vec![
            0x1b, 0x9b, 0x00, 0x10, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0a, 0x11, 0x27, 0x0f, 0x3c,
            0x2a, 0x1b, 0x2e, 0xb1,
        ];
        assert!(
            run(&code).is_empty(),
            "the guarded value is gone; so is the guard"
        );
    }

    /// MUST REFUSE: the ARRAY local is reassigned between the guard and the
    /// access. The fact was about the object `a` referred to then.
    #[test]
    fn a_write_to_the_array_kills_the_fact() {
        // locals: 0=a 1=i 2=other
        //  0: iload_1 ; 1: iflt +14 -> 15
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +8 -> 15
        // 10: aload_2 ; 11: astore_0                <-- a := other
        // 12: aload_0 ; 13: iload_1 ; 14: iaload
        // 15: return
        let code: Vec<u8> = vec![
            0x1b, 0x9b, 0x00, 0x0f, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x2c, 0x4b, 0x2a, 0x1b,
            0x2e, 0x00, 0xb1,
        ];
        assert!(run(&code).is_empty());
    }

    /// `iinc` is where the machine-integer discipline earns its keep.
    ///
    /// `i++` after the guard invalidates `i < a.length`; `i--` does not (the
    /// value only moved down, and `add_no_wrap` proved the step representable),
    /// but it *can* break `i >= 0`, so the numeric half has to catch it.
    /// Either way the access must keep its check — the point of the test is
    /// that both halves are consulted, and neither is assumed.
    #[test]
    fn an_increment_after_the_guard_invalidates_it() {
        //  0: iload_1 ; 1: iflt +15 -> 16
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +9 -> 16
        // 10: iinc 1,<d>
        // 13: aload_0 ; 14: iload_1 ; 15: iaload
        // 16: return
        let mk = |d: u8| -> Vec<u8> {
            vec![
                0x1b, 0x9b, 0x00, 0x0f, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, d, 0x2a,
                0x1b, 0x2e, 0xb1,
            ]
        };
        assert!(
            run(&mk(1)).is_empty(),
            "i++ can walk off the end: the upper-bound fact is dead"
        );
        // Cast: 0xff is `-1` as the signed iinc delta.
        assert!(
            run(&mk(0xff)).is_empty(),
            "i-- can walk below zero: the lower-bound fact is dead"
        );
    }

    /// A constant index needs no `< length` fact of its own — but it does not
    /// GET one either, so it keeps its check. Pinned so a future "constants are
    /// obviously fine" shortcut cannot be added without a length proof.
    #[test]
    fn a_constant_index_without_a_length_proof_is_refused() {
        // 0: aload_0 ; 1: iconst_0 ; 2: iaload ; 3: pop ; 4: return
        let code: Vec<u8> = vec![0x2a, 0x03, 0x2e, 0x57, 0xb1];
        assert!(run(&code).is_empty());
    }

    /// An array STORE is eliminated on the same evidence — with the extra
    /// requirement that the stored value comes from a single pushing
    /// instruction, since anything longer would desynchronise the positional
    /// operand decode.
    #[test]
    fn a_guarded_store_eliminates_but_a_computed_value_does_not() {
        // locals: 0=a 1=i 2=v
        //  0: iload_1 ; 1: iflt +14 -> 15
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +8 -> 15
        // 10: aload_0 ; 11: iload_1 ; 12: iload_2 ; 13: iastore
        // 15: return   (14 is a nop)
        let simple: Vec<u8> = vec![
            0x1b, 0x9b, 0x00, 0x0e, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x08, 0x2a, 0x1b, 0x1c, 0x4f,
            0x00, 0xb1,
        ];
        assert_eq!(run(&simple), vec![13]);

        // The same store with a two-instruction value expression (`v + v`):
        // the instruction before `iastore` is now `iadd`, so the positional
        // decode refuses rather than guessing.
        //  0: iload_1 ; 1: iflt +16 -> 17
        //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +10 -> 17
        // 10: aload_0 ; 11: iload_1 ; 12: iload_2 ; 13: iload_2 ; 14: iadd
        // 15: iastore ; 16: nop ; 17: return
        let computed: Vec<u8> = vec![
            0x1b, 0x9b, 0x00, 0x10, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0a, 0x2a, 0x1b, 0x1c, 0x1c,
            0x60, 0x4f, 0x00, 0xb1,
        ];
        assert!(run(&computed).is_empty());
    }

    /// An exception-handler entry resets the analysis. The handler here starts
    /// *inside* the guarded region, so control can arrive at the access having
    /// skipped the guard entirely — the fact must not survive.
    #[test]
    fn a_handler_entry_inside_the_guarded_region_kills_the_facts() {
        let code = guarded_load();
        // No handlers: eliminated (the control test).
        assert_eq!(run(&code), vec![12]);
        // A handler landing on pc 10 (the `aload_0` of the access) means
        // control can reach pc 12 without ever running either guard.
        let with_handler = range_safe_pcs(&code, code.len(), &[(0, 10, 10)]);
        assert!(
            with_handler.is_empty(),
            "a handler entry ahead of the access invalidates the guard, got {with_handler:?}"
        );
        // A handler landing after the access does not.
        let after = range_safe_pcs(&code, code.len(), &[(0, 16, 15)]);
        assert_eq!(after.into_iter().collect::<Vec<_>>(), vec![12]);
    }

    /// Unmodelled control flow refuses the whole method rather than the one
    /// construct: a `tableswitch`'s successors are not walked, so a fact could
    /// survive an edge this pass never saw.
    #[test]
    fn unmodelled_control_flow_refuses_the_method() {
        let mut code = guarded_load();
        // Overwrite the trailing `iconst_m1; ireturn` with a `lookupswitch`
        // opcode. The decode never reaches it, but the whole-method scan does.
        let n = code.len();
        code[n - 2] = 0xab;
        assert!(run(&code).is_empty());
    }

    /// The pass refuses an over-long method outright, so the analysis can
    /// never become the compile-time outlier.
    #[test]
    fn an_over_long_method_is_refused() {
        let mut code = guarded_load();
        code.resize(RANGE_MAX_CODE_LEN + 1, 0x00);
        assert!(range_safe_pcs(&code, code.len(), &[]).is_empty());
    }

    /// The gate is a real switch in both directions, and it is the ONLY thing
    /// that decides whether the range reason contributes.
    #[test]
    fn the_gate_switches_the_range_reason_only() {
        let code = guarded_load();
        let loops = super::super::detect_loops(&code, code.len());

        __set_range_bce_override(Some(true));
        let (on, _) =
            analyze_bounds_elimination_with_handlers(&code, code.len(), &loops, Some(&[]));
        __set_range_bce_override(Some(false));
        let (off, _) =
            analyze_bounds_elimination_with_handlers(&code, code.len(), &loops, Some(&[]));
        // The handler-blind entry point never runs the range reason at all.
        __set_range_bce_override(Some(true));
        let (blind, _) = analyze_bounds_elimination(&code, code.len(), &loops);
        __set_range_bce_override(None);

        assert!(on.contains(&12));
        assert!(!off.contains(&12));
        assert!(
            !blind.contains(&12),
            "an unknown exception table must refuse, not assume there is none"
        );
    }

    /// The counted-loop reason is untouched by any of this: the same fixture
    /// the loop tests use must still produce the same verdict through the new
    /// handler-aware entry point.
    #[test]
    fn the_counted_loop_reason_is_unchanged() {
        // `for (i = 0; i < a.length; i++) a[i];` — see
        // `range_bce_tests::inline_arraylength_limit_eliminates_statically`.
        let code: Vec<u8> = vec![
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01,
            0x01, 0xa7, 0xff, 0xf3, 0xb1,
        ];
        let loops = super::super::detect_loops(&code, code.len());
        __set_range_bce_override(Some(false));
        let (safe, guards) =
            analyze_bounds_elimination_with_handlers(&code, code.len(), &loops, Some(&[]));
        __set_range_bce_override(None);
        let mut safe: Vec<usize> = safe.into_iter().collect();
        safe.sort_unstable();
        assert_eq!(safe, vec![10]);
        assert!(guards.is_empty());
    }

    // ---- the state lattice itself ----------------------------------------

    #[test]
    fn merge_intersects_facts_and_hulls_ranges() {
        let mut a = RangeState::top(4);
        a.locals[1] = Range::exact(IntWidth::W32, 0, 10);
        a.add_lt_len(1, 0);
        a.add_lt_len(2, 0);
        let mut b = RangeState::top(4);
        b.locals[1] = Range::exact(IntWidth::W32, 20, 30);
        b.add_lt_len(1, 0);

        assert!(a.merge(&b, false));
        assert_eq!(a.locals[1], Range::exact(IntWidth::W32, 0, 30));
        assert!(a.proves_lt_len(1, 0), "held on both paths");
        assert!(!a.proves_lt_len(2, 0), "held on only one path");
        // Idempotent: merging the same state again changes nothing.
        let snapshot = a.clone();
        assert!(!a.merge(&snapshot, false));
    }

    #[test]
    fn merge_widens_once_asked_to() {
        let mut a = RangeState::top(2);
        a.locals[0] = Range::exact(IntWidth::W32, 0, 10);
        let mut b = RangeState::top(2);
        b.locals[0] = Range::exact(IntWidth::W32, 0, 11);
        a.merge(&b, true);
        assert_eq!(
            a.locals[0],
            // Widening: the i32 endpoints into the i64 interval domain.
            Range::exact(IntWidth::W32, 0, i32::MAX as i64),
            "an outward-moving endpoint goes straight to the extreme"
        );
    }

    #[test]
    fn kill_removes_the_range_and_every_fact_naming_the_slot() {
        let mut s = RangeState::top(4);
        s.locals[1] = Range::exact(IntWidth::W32, 0, 5);
        s.add_lt_len(1, 0); // i=1 indexes a=0
        s.add_lt_len(2, 1); // j=2 indexes a=1
        s.kill(1);
        assert!(s.locals[1].is_top());
        assert!(!s.proves_lt_len(1, 0), "killed as the index");
        assert!(!s.proves_lt_len(2, 1), "killed as the array");
    }

    #[test]
    fn narrow_by_handles_the_endpoints_without_wrapping() {
        let top = Range::top(IntWidth::W32);
        // Widening: the i32 endpoints into the i64 interval domain.
        let (min, max) = (i32::MIN as i64, i32::MAX as i64);
        assert_eq!(
            narrow_by(top, Rel::Ge, 0),
            Range::exact(IntWidth::W32, 0, max)
        );
        assert_eq!(
            narrow_by(top, Rel::Lt, 10),
            Range::exact(IntWidth::W32, min, 9)
        );
        // `x < Integer.MIN_VALUE` is impossible — bottom, not a wrapped range.
        assert!(narrow_by(top, Rel::Lt, min).is_empty());
        // `x > Integer.MAX_VALUE` likewise.
        assert!(narrow_by(top, Rel::Gt, max).is_empty());
        // `!=` cannot be expressed by an interval, so it must narrow nothing.
        assert_eq!(narrow_by(top, Rel::Ne, 5), top);
    }

    #[test]
    fn relation_algebra() {
        for r in [Rel::Eq, Rel::Ne, Rel::Lt, Rel::Le, Rel::Gt, Rel::Ge] {
            assert_eq!(negate_rel(negate_rel(r)), r);
            assert_eq!(swap_rel(swap_rel(r)), r);
        }
        assert_eq!(negate_rel(Rel::Lt), Rel::Ge);
        assert_eq!(swap_rel(Rel::Lt), Rel::Gt);
        // `if_icmpge` not taken means `<`, which is where the fall-through
        // edge of javac's guard gets its fact.
        assert_eq!(branch_relation(0xa2), Some((2, Rel::Ge)));
        assert_eq!(negate_rel(Rel::Ge), Rel::Lt);
        // `if_acmpeq` / `ifnull` carry no integer relation.
        assert_eq!(branch_relation(0xa5), None);
        assert_eq!(branch_relation(0xc6), None);
    }
}

#[cfg(test)]
mod range_bce_tests {
    use super::super::detect_loops;
    use super::*;
    use crate::scev::{BoundSource, BoundTerm, PreheaderGuard, SymBound};

    /// A guard flattened to `(header, array, bound, iv, covered)`.
    type GuardTuple = (usize, usize, usize, usize, Vec<usize>);

    /// `(safe_pcs sorted, guards sorted)`.
    fn run(code: &[u8], code_len: usize) -> (Vec<usize>, Vec<GuardTuple>) {
        let loops = detect_loops(code, code_len);
        let (safe, guards) = analyze_bounds_elimination(code, code_len, &loops);
        let mut safe: Vec<usize> = safe.into_iter().collect();
        safe.sort_unstable();
        let mut g: Vec<GuardTuple> = guards
            .iter()
            .map(|g| {
                (
                    g.loop_header,
                    g.array_local,
                    g.bound_local,
                    g.iv_local,
                    g.covered_pcs.clone(),
                )
            })
            .collect();
        g.sort();
        (safe, g)
    }

    /// `for (int i = 0; i < a.length; i++) a[i];` with the length read INLINE
    /// in the exit test — the single most common Java loop, and one the old
    /// `analyze_loop_bound` refused outright because it demanded the limit be a
    /// bare `iload`. The limit IS the accessed array's length, so
    /// `prove_index_in_bounds_of` discharges it against its own tautology and
    /// the elision is STATIC: no guard, no deopt.
    fn inline_length_loop() -> (Vec<u8>, usize) {
        // 0: iconst_0 ; 1: istore_1                       (i = 0)
        // 2: iload_1 ; 3: aload_0 ; 4: arraylength        (header)
        // 5: if_icmpge +13 -> 18
        // 8: aload_0 ; 9: iload_1 ; 10: iaload ; 11: pop  (a[i])
        // 12: iinc 1,1 ; 15: goto -13 -> 2 ; 18: return
        let code = vec![
            0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01,
            0x01, 0xa7, 0xff, 0xf3, 0xb1,
        ];
        let len = code.len();
        (code, len)
    }

    #[test]
    fn inline_arraylength_limit_eliminates_statically() {
        let (code, code_len) = inline_length_loop();
        assert_eq!(detect_loops(&code, code_len)[0], (2, 15));
        let (safe, guards) = run(&code, code_len);
        assert_eq!(
            safe,
            vec![10],
            "a[i] under `i < a.length` is statically safe"
        );
        assert!(
            guards.is_empty(),
            "the limit IS this array's length — nothing left to guard, got {guards:?}"
        );
    }

    /// MUST REFUSE counterpart of the above: the same loop counting DOWN.
    /// `for (i = n; i > 0; i--) a[i]` puts the length obligation on the IV's
    /// entry value (`length >= i_entry + 1`) and the non-negativity obligation
    /// on `bound + 1`; the pre-header emitter has a compare for neither, so the
    /// proof's guards are uncoverable and the per-element check stays.
    #[test]
    fn decreasing_loop_refuses_no_emitter_for_its_guards() {
        // 0: iload_1 ; 1: istore_2                        (i = n)
        // 2: iload_2 ; 3: iconst_0 ; 4: if_icmple +13 -> 17   (exit i <= 0)
        // 7: aload_0 ; 8: iload_2 ; 9: iaload ; 10: pop
        // 11: iinc 2,-1 ; 14: goto -12 -> 2 ; 17: return
        let code: Vec<u8> = vec![
            0x1b, 0x3d, 0x1c, 0x03, 0xa4, 0x00, 0x0d, 0x2a, 0x1c, 0x2e, 0x57, 0x84, 0x02, 0xff,
            0xa7, 0xff, 0xf4, 0xb1,
        ];
        let code_len = code.len();
        assert_eq!(detect_loops(&code, code_len)[0], (2, 14));
        let (safe, guards) = run(&code, code_len);
        assert!(
            safe.is_empty(),
            "decreasing loop must keep its check, got {safe:?}"
        );
        assert!(guards.is_empty());
    }

    /// A provably zero-trip loop (`for (i = 5; i < 3; i++)`) has an EMPTY index
    /// span: the body never runs, so there is no access to check and the
    /// verdict is `Static`. This is the lattice's bottom doing real work — the
    /// old pattern set had no way to express it.
    #[test]
    fn zero_trip_loop_is_vacuously_safe() {
        // 0: iconst_5 ; 1: istore_1                       (i = 5)
        // 2: iload_1 ; 3: iconst_3 ; 4: if_icmpge +13 -> 17
        // 7: aload_0 ; 8: iload_1 ; 9: iaload ; 10: pop
        // 11: iinc 1,1 ; 14: goto -12 -> 2 ; 17: return
        let code: Vec<u8> = vec![
            0x08, 0x3c, 0x1b, 0x06, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01, 0x01,
            0xa7, 0xff, 0xf4, 0xb1,
        ];
        let code_len = code.len();
        assert_eq!(detect_loops(&code, code_len)[0], (2, 14));
        let (safe, guards) = run(&code, code_len);
        assert_eq!(safe, vec![9], "a provably zero-trip body needs no check");
        assert!(guards.is_empty(), "and no guard, got {guards:?}");
    }

    /// `for (i = 0; i < n; i++) c[i] = a[i] + b[i];` with `n` a parameter.
    /// Three arrays, three SEPARATE guards. `LengthAtLeast` names no array by
    /// contract, so all three read identically — merging them would discharge
    /// the shortest array's obligation with the longest array's length, which
    /// is exactly the multi-array out-of-bounds store
    /// (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md).
    #[test]
    fn multi_array_loop_keeps_one_guard_per_array() {
        // locals: 0=a 1=b 2=c 3=n 4=i
        // 0: iconst_0 ; 1: istore 4
        // 3: iload 4 ; 5: iload_3 ; 6: if_icmpge +22 -> 28   (header)
        // 9: aload_2 ; 10: iload 4
        // 12: aload_0 ; 13: iload 4 ; 15: iaload
        // 16: aload_1 ; 17: iload 4 ; 19: iaload
        // 20: iadd ; 21: iastore
        // 22: iinc 4,1 ; 25: goto -22 -> 3 ; 28: return
        let code: Vec<u8> = vec![
            0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x16, 0x2c, 0x15, 0x04, 0x2a, 0x15,
            0x04, 0x2e, 0x2b, 0x15, 0x04, 0x2e, 0x60, 0x4f, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xea,
            0xb1,
        ];
        let code_len = code.len();
        assert_eq!(detect_loops(&code, code_len)[0], (3, 25));
        let (safe, guards) = run(&code, code_len);
        assert_eq!(safe, vec![15, 19, 21]);
        assert_eq!(
            guards,
            vec![
                (3, 0, 3, 4, vec![15]),
                (3, 1, 3, 4, vec![19]),
                (3, 2, 3, 4, vec![21]),
            ],
            "one guard per array, each covering only its own access"
        );
    }

    /// MUST ELIMINATE / MUST REFUSE pair on the stride.
    ///
    /// `iinc i, 1` eliminates behind the ordinary guard. `iinc i, 2` is a stride
    /// the *proof* now accepts (`Stride::Const(2)`) but whose no-wrap obligation
    /// comes back as `PreheaderGuard::AtMost { bound, i32::MAX - 1 }` — and the
    /// pre-header emitter only emits its `bound != Integer.MAX_VALUE` test for
    /// the INCLUSIVE comparator. One uncoverable guard refuses the whole proof.
    #[test]
    fn unit_stride_eliminates_non_unit_stride_refuses() {
        // 0: iload_0 ; 1: iload_2 ; 2: if_icmpge +13 -> 15
        // 5: aload_1 ; 6: iload_0 ; 7: iaload ; 8: pop
        // 9: iinc 0,<step> ; 12: goto -12 -> 0 ; 15: return
        let mk = |step: u8| -> Vec<u8> {
            vec![
                0x1a, 0x1c, 0xa2, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00, step, 0xa7, 0xff,
                0xf4, 0xb1,
            ]
        };
        let unit = mk(1);
        let (safe, guards) = run(&unit, unit.len());
        assert_eq!(safe, vec![7], "unit stride still eliminates");
        assert_eq!(guards, vec![(0, 1, 2, 0, vec![7])]);

        let wide = mk(2);
        let (safe2, guards2) = run(&wide, wide.len());
        assert!(
            safe2.is_empty(),
            "a stride whose wrap guard has no emitter must refuse, got {safe2:?}"
        );
        assert!(guards2.is_empty());
    }

    /// MUST ELIMINATE / MUST REFUSE pair on the limit's shape.
    ///
    /// A limit held in a LOCAL eliminates behind `a.length >= n`. The same loop
    /// with a `getfield` limit is proved just as well by the range analysis
    /// (`BoundSource::Field`, invariant because the body is heap-stable) but has
    /// no local home for the pre-header to load, so the guard cannot be emitted
    /// and the check stays.
    #[test]
    fn local_limit_eliminates_field_limit_refuses() {
        // local limit: 0: iload_0 ; 1: iload_2 ; 2: if_icmpge +13 -> 15 ; ...
        let local_bound: Vec<u8> = vec![
            0x1a, 0x1c, 0xa2, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00, 0x01, 0xa7, 0xff,
            0xf4, 0xb1,
        ];
        let (safe, guards) = run(&local_bound, local_bound.len());
        assert_eq!(safe, vec![7]);
        assert_eq!(guards, vec![(0, 1, 2, 0, vec![7])]);

        // field limit: 0: iload_0 ; 1: aload_3 ; 2: getfield #7 ;
        //              5: if_icmpge +13 -> 18 ; 8: aload_1 ; 9: iload_0 ;
        //              10: iaload ; 11: pop ; 12: iinc 0,1 ; 15: goto -15 -> 0 ;
        //              18: return
        let field_bound: Vec<u8> = vec![
            0x1a, 0x2d, 0xb4, 0x00, 0x07, 0xa2, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00,
            0x01, 0xa7, 0xff, 0xf1, 0xb1,
        ];
        let (safe2, guards2) = run(&field_bound, field_bound.len());
        assert!(
            safe2.is_empty(),
            "a field limit has no local for the pre-header to load, got {safe2:?}"
        );
        assert!(guards2.is_empty());
    }

    /// The inclusive comparator, both sides of its flag.
    ///
    /// The proof handles `i <= n` as `bound_addend() == 0`: a `length >= n + 1`
    /// guard (emitted as JBE) plus `PreheaderGuard::AtMost { n, MAX - 1 }` (the
    /// `n != Integer.MAX_VALUE` entry test). It is nevertheless gated OFF by
    /// default — the elision measured a ~2x NET LOSS on the Sieve OSR artifact,
    /// a code-layout effect that generalising the proof does not change.
    #[test]
    fn inclusive_loop_is_provable_but_stays_flag_gated() {
        // 0: iload_0 ; 1: iload_2 ; 2: if_icmpgt +13 -> 15   (i <= n)
        // 5: aload_1 ; 6: iload_0 ; 7: iaload ; 8: pop
        // 9: iinc 0,1 ; 12: goto -12 -> 0 ; 15: return
        let code: Vec<u8> = vec![
            0x1a, 0x1c, 0xa3, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00, 0x01, 0xa7, 0xff,
            0xf4, 0xb1,
        ];
        let code_len = code.len();

        __set_inclusive_spec_bce_override(Some(false));
        let (off_safe, off_guards) = run(&code, code_len);
        assert!(off_safe.is_empty(), "default-off keeps the check");
        assert!(off_guards.is_empty());

        __set_inclusive_spec_bce_override(Some(true));
        let loops = detect_loops(&code, code_len);
        let (safe, guards) = analyze_bounds_elimination(&code, code_len, &loops);
        __set_inclusive_spec_bce_override(None);
        assert!(safe.contains(&7), "opt-in elides the inclusive access");
        assert_eq!(guards.len(), 1);
        assert!(
            guards[0].inclusive,
            "the guard must record the inclusive form (JBE + `n != MAX_VALUE`)"
        );
        assert_eq!(guards[0].step_local, None);
    }

    /// The exclusive/inclusive distinction is the `bound_addend`, and it is the
    /// ONLY thing that changes the emitted length guard. Pinned here because
    /// `GuardShape::covers` reads the addend directly.
    #[test]
    fn addend_is_the_whole_inclusive_question() {
        let excl = GuardShape {
            iv_local: 0,
            bound_local: Some(2),
            bound_term: BoundTerm::Bound(BoundSource::Local(2)),
            addend: -1,
            step_local: None,
        };
        let incl = GuardShape {
            addend: 0,
            ..excl_clone(&excl)
        };
        let need = |k: i32| {
            PreheaderGuard::LengthAtLeast(SymBound {
                base: BoundTerm::Bound(BoundSource::Local(2)),
                addend: k,
            })
        };
        // Exclusive emits JB: proves `length >= n`, not `length >= n + 1`.
        assert!(excl.covers(&need(0)));
        assert!(!excl.covers(&need(1)));
        // Inclusive emits JBE: proves `length >= n + 1`.
        assert!(incl.covers(&need(0)));
        assert!(incl.covers(&need(1)));
        assert!(!incl.covers(&need(2)));
    }

    fn excl_clone(g: &GuardShape) -> GuardShape {
        GuardShape {
            iv_local: g.iv_local,
            bound_local: g.bound_local,
            bound_term: g.bound_term.clone(),
            addend: g.addend,
            step_local: g.step_local,
        }
    }

    /// The all-or-nothing contract, stated directly.
    ///
    /// Every guard shape the pre-header cannot emit — a non-negativity
    /// obligation on anything but the IV's entry value, a length obligation on
    /// a DIFFERENT limit than the one the emitter loads, the decreasing loop's
    /// `AtLeast`, a stride guard naming a local that is not the recorded step,
    /// and the exclusive loop's `AtMost` — must answer `false`, because one
    /// uncovered guard refuses the whole proof.
    #[test]
    fn uncoverable_guards_are_refused() {
        let shape = GuardShape {
            iv_local: 0,
            bound_local: Some(2),
            bound_term: BoundTerm::Bound(BoundSource::Local(2)),
            addend: -1,
            step_local: Some(3),
        };
        let sym = |base: BoundTerm, addend: i32| SymBound { base, addend };

        // Covered: the `iv >= 0` header test.
        assert!(shape.covers(&PreheaderGuard::NonNegative(sym(BoundTerm::IvEntry(0), 0))));
        // Not covered: non-negativity of anything else.
        assert!(!shape.covers(&PreheaderGuard::NonNegative(sym(BoundTerm::IvEntry(1), 0))));
        assert!(!shape.covers(&PreheaderGuard::NonNegative(sym(
            BoundTerm::Bound(BoundSource::Local(2)),
            1
        ))));
        // Not covered: a length obligation against a limit this pre-header does
        // not load. This is the cross-array / foreign-limit case.
        assert!(!shape.covers(&PreheaderGuard::LengthAtLeast(sym(
            BoundTerm::Bound(BoundSource::Local(5)),
            0
        ))));
        assert!(!shape.covers(&PreheaderGuard::LengthAtLeast(sym(
            BoundTerm::Bound(BoundSource::ArrayLength(1)),
            0
        ))));
        // Not covered: the exclusive comparator emits no `n != MAX_VALUE` test,
        // so an IV that can wrap has nothing discharging it.
        assert!(!shape.covers(&PreheaderGuard::AtMost {
            term: sym(BoundTerm::Bound(BoundSource::Local(2)), 0),
            limit: i32::MAX - 1,
        }));
        // Not covered at all: the decreasing loop's `i32::MIN` obligation.
        assert!(!shape.covers(&PreheaderGuard::AtLeast {
            term: sym(BoundTerm::Bound(BoundSource::Local(2)), 0),
            limit: i32::MIN + 1,
        }));
        // Covered: the recorded step, headroom against this loop's own limit.
        assert!(shape.covers(&PreheaderGuard::StrideInRange {
            local: 3,
            headroom: sym(BoundTerm::Bound(BoundSource::Local(2)), -1),
        }));
        // Not covered: a stride guard on a different local.
        assert!(!shape.covers(&PreheaderGuard::StrideInRange {
            local: 4,
            headroom: sym(BoundTerm::Bound(BoundSource::Local(2)), -1),
        }));
        // Not covered: a limit with no local home cannot be loaded at all.
        let homeless = GuardShape {
            bound_local: None,
            ..excl_clone(&shape)
        };
        assert!(!homeless.covers(&PreheaderGuard::LengthAtLeast(sym(
            BoundTerm::Bound(BoundSource::Local(2)),
            0
        ))));
    }

    /// Pattern B (`if_icmplt <body>` as the back edge) is pre-tested ONLY
    /// because the loop is entered through a `goto` that lands at or after the
    /// comparison. `analyze_loop_bound` assumed that and never checked it.
    /// Here both shapes are built from the same body: the rotated one keeps its
    /// elision, the `do { } while` one is reported post-tested and refuses
    /// (its untested first iteration puts an unbounded entry value in the span,
    /// which needs a `length >= 1` guard the pre-header cannot emit).
    #[test]
    fn pattern_b_rotation_is_verified_not_assumed() {
        // Rotated (`goto cond`), locals 0=?, 1=arr, 2=n, 3=i:
        // 0: iconst_0 ; 1: istore_3 ; 2: goto +10 -> 12
        // 5: aload_1 ; 6: iload_3 ; 7: iaload ; 8: pop      (body, header=5)
        // 9: iinc 3,1
        // 12: iload_3 ; 13: iload_2 ; 14: if_icmplt -9 -> 5 (back edge)
        // 17: return
        let rotated: Vec<u8> = vec![
            0x03, 0x3e, 0xa7, 0x00, 0x0a, 0x2b, 0x1d, 0x2e, 0x57, 0x84, 0x03, 0x01, 0x1d, 0x1c,
            0xa1, 0xff, 0xf7, 0xb1,
        ];
        let code_len = rotated.len();
        assert_eq!(detect_loops(&rotated, code_len)[0], (5, 14));
        let (safe, guards) = run(&rotated, code_len);
        assert_eq!(safe, vec![7], "the rotated entry lands on the test");
        assert_eq!(guards, vec![(5, 1, 2, 3, vec![7])]);

        // `do { } while`: identical bytes except the entry `goto` is replaced
        // by three `nop`s, so control falls through into the header and the
        // body runs once BEFORE the first test.
        let mut do_while = rotated.clone();
        do_while[2] = 0x00; // nop
        do_while[3] = 0x00; // nop
        do_while[4] = 0x00; // nop
        let (safe2, guards2) = run(&do_while, code_len);
        assert!(
            safe2.is_empty(),
            "a post-tested body executes a[i] before any test, got {safe2:?}"
        );
        assert!(guards2.is_empty());
    }
}
