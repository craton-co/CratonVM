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
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_SPEC_BCE").is_some())
}

/// Info about a loop's induction variable and bounds.
///
/// SUPERSEDED by [`crate::scev::CountedLoop`] — `analyze_bounds_elimination`
/// no longer builds one. Kept because `analyze_loop_bound` (which returns it)
/// is still exercised by the x64 test suite as the reference decoding of the
/// two comparator shapes; nothing in the elision path reads it.
#[allow(dead_code)]
pub(super) struct LoopBoundsInfo {
    /// The local variable that serves as the induction variable (incremented by iinc +1).
    pub(super) induction_var: usize,
    /// The local variable used as the upper bound in the loop condition.
    /// If None, the bound is a constant.
    pub(super) bound_local: Option<usize>,
    /// Constant upper bound (if the bound is iconst/bipush/sipush).
    #[allow(dead_code)]
    pub(super) bound_const: Option<i32>,
    /// Whether the loop comparator is *inclusive* of `bound` (`if_icmpgt` exit
    /// or `if_icmple` continue, i.e. a `for (i = 0; i <= n; i++)` loop). When
    /// true the induction variable reaches `bound` itself, so the maximum index
    /// accessed is `bound`, requiring `array.length >= bound + 1`. The single
    /// header guard only proves `array.length >= bound` (SECURITY FIX V17), so
    /// BCE — both static and speculative — is REFUSED for inclusive loops to
    /// avoid an off-by-one out-of-bounds heap access at `index == bound`.
    pub(super) inclusive: bool,
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

/// How a loop's induction variable advances — the step provenance the
/// speculative BCE guard needs to bound every elided index from below (no
/// negative step) and above (no int wrap past the exit test).
///
/// SUPERSEDED by [`crate::scev::Stride`], which additionally carries non-unit
/// and negative constant strides and the `isub` spelling. Retained only as the
/// reference decoding the x64 test suite pins.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum IvStep {
    /// Canonical `iinc iv, 1`.
    UnitInc,
    /// Canonical compound `iv += step` (`iload iv; iload step; iadd;
    /// istore iv`); payload = the step's local index. The step's runtime
    /// SIGN and magnitude are unknown at compile time — the preheader guard
    /// must prove `0 <= step <= Integer.MAX_VALUE - bound` before any elided
    /// access runs.
    VarAdd(usize),
}

/// Prove how the induction variable `iv` advances inside `[header,
/// back_edge_end)`. Returns `None` when the step shape is anything but the
/// two canonical forms — the caller must then refuse every bounds-check
/// elision for the loop (`find_induction_variable` alone admits `iadd;istore`
/// IVs without identifying the step operand, which is not enough to reason
/// about sign or wrap).
///
/// SUPERSEDED by [`crate::loop_analysis::find_iv_stride`].
#[allow(dead_code)]
pub(super) fn find_iv_step_provenance(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
    iv: usize,
) -> Option<IvStep> {
    let mut unit_incs = 0usize;
    let mut var_adds = 0usize;
    let mut var_step: Option<usize> = None;
    // PCs of the previous three instruction starts (linear order).
    let mut prev: [Option<usize>; 3] = [None, None, None];
    let mut pc = header;
    while pc < back_edge_end {
        let op = code[pc];
        match op {
            // iinc
            0x84 => {
                if code[pc + 1] as usize == iv {
                    if code[pc + 2] as i8 == 1 {
                        unit_incs += 1;
                    } else {
                        return None; // non-unit iinc — unsupported stride
                    }
                }
            }
            // wide istore/iinc aliasing the IV via a 2-byte index — unprovable.
            0xc4 => {
                if pc + 3 < back_edge_end {
                    let real = code[pc + 1];
                    // Widening: operand bytes -> usize index (value fits)
                    let idx = ((code[pc + 2] as usize) << 8) | code[pc + 3] as usize;
                    if (real == 0x36 || real == 0x84) && idx == iv {
                        return None;
                    }
                }
            }
            _ => {
                // Widening: u8 operand/opcode-relative index -> usize
                let istore_target = match op {
                    0x36 => Some(code[pc + 1] as usize),
                    0x3b..=0x3e => Some((op - 0x3b) as usize),
                    _ => None,
                };
                if istore_target == Some(iv) {
                    // Must be the canonical `iload iv; iload step; iadd;
                    // istore iv` (javac's `iv += step`). Anything else —
                    // including the commuted `step + iv` — is refused.
                    let (Some(p1), Some(p2), Some(p3)) = (prev[0], prev[1], prev[2]) else {
                        return None;
                    };
                    if code[p1] != 0x60 {
                        return None;
                    }
                    let step = extract_iload_local(code, p2)?;
                    let base = extract_iload_local(code, p3)?;
                    if base != iv || step == iv {
                        return None;
                    }
                    if var_step.is_some_and(|s| s != step) {
                        return None;
                    }
                    var_step = Some(step);
                    var_adds += 1;
                }
            }
        }
        prev = [Some(pc), prev[0], prev[1]];
        pc += bytecode_len_at(code, pc);
    }
    match (unit_incs, var_adds, var_step) {
        (1, 0, None) => Some(IvStep::UnitInc),
        (0, 1, Some(step)) => Some(IvStep::VarAdd(step)),
        _ => None,
    }
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
        pc += bytecode_len_at(code, pc);
    }
    hi
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
pub(super) fn classify_local_kinds(code: &[u8], code_len: usize, num_locals: usize) -> Vec<LocalKind> {
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
        pc += bytecode_len_at(code, pc);
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
pub(super) fn local_access_at(code: &[u8], code_len: usize, pc: usize) -> Option<(LocalKind, usize)> {
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
}

impl AmbiguousLocalKinds {
    /// The refined kind of `slot` on entry to `pc`, or `None` when this slot is
    /// not tracked (it was never ambiguous) or the refinement did not settle.
    ///
    /// Never answers `Ref`: the flow-sensitive oop mask is the sole authority
    /// for ref-typed slots and has already had its say by the time a caller
    /// consults this, so a `Ref` here means "the mask could not prove it live
    /// as an oop" — the one case that must stay a safe re-run.
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
/// in `docs/known-issues/h2/h2-jitban-residuals-20260726.md`. Uses the same
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
    AmbiguousLocalKinds { slots, at }
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
        pc += bytecode_len_at(code, pc);
    }
    false
}

pub(super) fn find_induction_variable(code: &[u8], header: usize, back_edge_end: usize) -> Option<usize> {
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
            _ => pc += bytecode_len_at(code, pc),
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

/// Analyze the loop condition to find the upper bound.
///
/// Looks for patterns like:
/// - `iload iv; iload bound; if_icmpge exit` → bound is in local `bound`
/// - `iload iv; arraylength; if_icmpge exit` → bound is array length (implicit)
///
/// Returns LoopBoundsInfo if the pattern is recognized.
///
/// SUPERSEDED by `locate_exit_test` + [`crate::loop_analysis::decode_bound_expr`],
/// which accept a constant / `arraylength` / field / `Math.min`-`max` limit as
/// well as a bare local, and — unlike this function — *verify* that the branch
/// they decode is really the loop's exit or back edge. Retained because the
/// x64 test suite pins its inclusive/exclusive classification.
#[allow(dead_code)]
pub(super) fn analyze_loop_bound(
    code: &[u8],
    header: usize,
    back_edge: usize,
    back_edge_end: usize,
    induction_var: usize,
) -> Option<LoopBoundsInfo> {
    // Pattern 1: Loop controlled by `goto header` at back_edge
    // The loop condition is typically at the header or just before the goto
    // Common Java for-loop pattern:
    //   header: iload iv; iload bound; if_icmpge exit; ... ; goto header
    //
    // Pattern 2: Loop controlled by conditional branch at back_edge
    //   header: ...; iload iv; iload bound; if_icmplt header

    // Check if back_edge is a conditional branch (do-while pattern)
    let back_op = code[back_edge];
    if matches!(back_op, 0x99..=0xa4 | 0xc6 | 0xc7) {
        // Conditional branch as back-edge — look for the comparison pattern just before
        // We need: iload iv; iload bound; if_icmplt/le/etc header
        // Scan backwards from back_edge to find the comparison setup
        // This is simpler if we scan forward from header
    }

    // Scan the loop body looking for the comparison pattern with the induction variable
    let mut pc = header;
    while pc < back_edge_end {
        // Match: iload <iv>; iload <bound>; if_icmpge/if_icmpgt <target>
        // where <target> is outside the loop (exit condition)
        let iv_local = match code[pc] {
            0x1a if induction_var == 0 => Some(0usize),
            0x1b if induction_var == 1 => Some(1),
            0x1c if induction_var == 2 => Some(2),
            0x1d if induction_var == 3 => Some(3),
            // Widening: u8 -> wider int (bytecode operand byte, value fits)
            0x15 if pc + 1 < back_edge_end && code[pc + 1] as usize == induction_var => {
                // Widening: always safe
                Some(induction_var)
            }
            _ => None,
        };

        if let Some(_iv) = iv_local {
            let next_pc = if code[pc] == 0x15 { pc + 2 } else { pc + 1 };
            if next_pc >= back_edge_end {
                pc += bytecode_len_at(code, pc);
                continue;
            }

            // Check if next instruction loads the bound
            let (bound_local, after_bound) = match code[next_pc] {
                0x1a => (Some(0usize), next_pc + 1),
                0x1b => (Some(1), next_pc + 1),
                0x1c => (Some(2), next_pc + 1),
                0x1d => (Some(3), next_pc + 1),
                0x15 if next_pc + 1 < back_edge_end => {
                    (Some(code[next_pc + 1] as usize), next_pc + 2) // Widening: always safe
                }
                _ => (None, next_pc),
            };

            if let Some(bound) = bound_local {
                if after_bound < back_edge_end && after_bound + 2 < back_edge_end {
                    let cmp_op = code[after_bound];
                    let offset =
                        i16::from_be_bytes([code[after_bound + 1], code[after_bound + 2]]) as i32; // Widening: always safe
                    let target = (after_bound as i32 + offset) as usize; // Cast: x86-64 immediate encoding

                    // Pattern A: Exit condition — if_icmpge/if_icmpgt with target OUTSIDE loop
                    // e.g. `iload i; iload n; if_icmpge exit` at loop header.
                    // `if_icmpgt` (0xa3) exits only when `iv > bound`, so the loop
                    // body still runs at `iv == bound` → inclusive (index reaches
                    // `bound`). `if_icmpge` (0xa2) exits at `iv == bound` →
                    // exclusive (max index `bound - 1`).
                    if matches!(cmp_op, 0xa2 | 0xa3) && (target > back_edge || target < header) {
                        return Some(LoopBoundsInfo {
                            induction_var,
                            bound_local: Some(bound),
                            bound_const: None,
                            inclusive: cmp_op == 0xa3,
                        });
                    }

                    // Pattern B: Continue condition — if_icmplt/if_icmple with target INSIDE loop
                    // e.g. `iload i; iload n; if_icmplt loop_body` (standard javac for-loop pattern).
                    // `if_icmple` (0xa4) continues while `iv <= bound`, so the body
                    // runs at `iv == bound` → inclusive. `if_icmplt` (0xa1)
                    // continues while `iv < bound` → exclusive.
                    if matches!(cmp_op, 0xa1 | 0xa4) && target >= header && target <= back_edge {
                        return Some(LoopBoundsInfo {
                            induction_var,
                            bound_local: Some(bound),
                            bound_const: None,
                            inclusive: cmp_op == 0xa4,
                        });
                    }
                }
            }
        }
        pc += bytecode_len_at(code, pc);
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
            pc += bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
fn branch_edges(code: &[u8], code_len: usize) -> Option<Vec<(usize, usize)>> {
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
        pc += bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
        s += bytecode_len_at(code, s);
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
    let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
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
        .map(|&(h, b)| (h, b + bytecode_len_at(code, b)))
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
pub(super) fn analyze_bounds_elimination(
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
        let back_edge_end = back_edge + bytecode_len_at(code, back_edge);

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
        assert_eq!(safe, vec![10], "a[i] under `i < a.length` is statically safe");
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
        assert!(safe.is_empty(), "decreasing loop must keep its check, got {safe:?}");
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
        let incl = GuardShape { addend: 0, ..excl_clone(&excl) };
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
        assert!(shape.covers(&PreheaderGuard::NonNegative(sym(
            BoundTerm::IvEntry(0),
            0
        ))));
        // Not covered: non-negativity of anything else.
        assert!(!shape.covers(&PreheaderGuard::NonNegative(sym(
            BoundTerm::IvEntry(1),
            0
        ))));
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

