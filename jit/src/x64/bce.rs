// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Array bounds-check elimination.
//!
//! Moved verbatim out of `x64.rs`'s `Array Bounds Check Elimination (BCE)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;


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

/// Find array accesses in a loop that are STATICALLY provably safe — no
/// runtime guard needed. Returns the set of such bytecode PCs.
///
/// The proof (SECURITY FIX: per-array, see
/// docs/known-issues/jit-bce-multi-array-oob-store-20260711.md): the access
/// `arr[iv]` under an exclusive loop test `iv < bound` is in range iff
/// `0 <= iv < bound <= arr.length` at every execution. This function
/// therefore requires ALL of:
///   1. `bound_from_array == Some(arr_local)` — whole-method arraylength
///      provenance (`find_bound_arraylength_provenance`) proving the bound IS
///      this very array's length. The loop guard `iv < a.length` bounds ONLY
///      accesses into `a`; any other array indexed by the same IV (the `out`
///      store of `out[i] = a[i] + b[i]`) gets NO static elision and falls to
///      the speculative per-array header guard instead.
///   2. `iv_start_nonneg` — whole-method proof the IV can never be negative
///      (`find_iv_nonneg_start`).
///   3. The IV is the access index and is only stepped +1
///      (`find_induction_variable`), the loop comparator is exclusive, and
///      both the array local and the bound local are loop-invariant.
///
/// Operand identification is delegated to `analyze_array_access_operands`
/// (sound producer-stack tracking); `operands` maps each analysable array
/// access PC to its `(array_local, index_local)`.
pub(super) fn find_safe_array_accesses(
    bounds: &LoopBoundsInfo,
    modified: u64,
    operands: &FxHashMap<usize, (usize, usize)>,
    bound_from_array: Option<usize>,
    iv_start_nonneg: bool,
) -> FxHashSet<usize> {
    let mut safe_pcs = FxHashSet::default();

    // SECURITY FIX (V17): an inclusive comparator (`if_icmpgt` exit /
    // `if_icmple` continue) lets the induction variable reach `bound` itself, so
    // the maximum index accessed is `bound`, requiring `array.length >= bound +
    // 1`. Provenance only proves `array.length == bound`, which is off-by-one
    // for `index == bound` (an OOB heap read/write one element past the end).
    // Refuse to mark any access safe for inclusive loops so the per-element
    // check is always kept.
    if bounds.inclusive {
        return safe_pcs;
    }

    // Static elision needs the arraylength provenance and the non-negative IV
    // start; anything unproven is left for the speculative guard path.
    let bound_arr = match bound_from_array {
        Some(a) if iv_start_nonneg => a,
        _ => return safe_pcs,
    };

    // Loop-invariance of the bound: if the body raised `bound` after entry,
    // the per-iteration exit test `iv < bound` could admit `iv >= array.length`
    // on a later trip (SECURITY FIX V16). Provenance's single-store rule
    // already implies this; kept as defense in depth.
    match bounds.bound_local {
        Some(bl) if bl < 64 && (modified & (1u64 << bl)) == 0 => {}
        _ => return safe_pcs,
    }

    for (&pc, &(arr_local, idx_local)) in operands {
        // Index must be the induction variable; the array must be THE array
        // whose length the bound was taken from, and loop-invariant.
        if idx_local == bounds.induction_var
            && arr_local == bound_arr
            && arr_local < 64
            && (modified & (1u64 << arr_local)) == 0
        {
            safe_pcs.insert(pc);
        }
    }

    safe_pcs
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
/// `SpeculativeBCEGuard`). Before this proof existed, `find_safe_array_accesses`
/// treated the loop guard `iv < bound` as bounding EVERY array indexed by the
/// IV — eliding the store check of `out[i] = a[i] + b[i]` from a guard on
/// `a.length`, a silent out-of-bounds heap write when `out` is shorter
/// (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md).
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

/// Perform bounds check elimination analysis for all loops in the method.
/// Returns a set of bytecode PCs where bounds checks can be safely skipped,
/// and a list of speculative BCE guards to emit at loop headers.
pub(super) fn analyze_bounds_elimination(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>) {
    let mut safe_pcs = FxHashSet::default();
    let mut speculative_guards: Vec<SpeculativeBCEGuard> = Vec::new();

    for &(header, back_edge) in loops {
        let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
        if back_edge_end > code_len {
            continue;
        }

        // Step 1: Find the induction variable
        let induction_var = match find_induction_variable(code, header, back_edge_end) {
            Some(iv) => iv,
            None => continue,
        };

        // Step 2: Analyze the loop bound
        let bounds = match analyze_loop_bound(code, header, back_edge, back_edge_end, induction_var)
        {
            Some(b) => b,
            None => continue,
        };

        // Step 3: Find modified locals in loop body
        let modified = find_modified_locals(code, header, back_edge_end);

        // Step 3b: Soundly identify the (array_local, index_local) consumed by
        // each analysable array access via operand-stack producer tracking.
        // Both the static and speculative passes below consult this map instead
        // of the old positional heuristics that mis-identified scatter stores.
        let operands = analyze_array_access_operands(code, header, back_edge_end);

        // Step 3c: Whole-method provenance facts for the static (guard-less)
        // path — which array's length the bound provably IS, and whether the
        // IV provably starts non-negative. Static elision of `arr[iv]` is
        // per-array: it requires `bound == arr.length` for THAT array
        // (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md).
        let bound_from_array = bounds
            .bound_local
            .and_then(|bl| find_bound_arraylength_provenance(code, code_len, bl));
        let iv_start_nonneg = find_iv_nonneg_start(code, code_len, induction_var);
        // Step 3d: step provenance. `find_induction_variable` admits
        // `iadd;istore` IVs (the Sieve `j += i` inner loop) without naming the
        // step operand; every elision below needs the step's identity (to
        // guard its sign/magnitude) or the `iinc +1` proof. An unprovable
        // step refuses the loop entirely.
        let iv_step = find_iv_step_provenance(code, header, back_edge_end, induction_var);

        // Step 4: Find safe array accesses (statically proven). Only the
        // canonical +1 step qualifies: the static proof has no step-sign /
        // no-wrap guard, so a variable-stride IV (whose runtime step could be
        // negative) must go through the guarded speculative path below.
        let loop_safe = if matches!(iv_step, Some(IvStep::UnitInc)) {
            find_safe_array_accesses(
                &bounds,
                modified,
                &operands,
                bound_from_array,
                iv_start_nonneg,
            )
        } else {
            FxHashSet::default()
        };
        safe_pcs.extend(&loop_safe);

        // Step 5: Speculative BCE — for counted loops with IV from 0..N step 1,
        // find array accesses using IV as index that weren't already proven safe.
        // For these, we emit a single range guard at the loop header and mark
        // all such accesses as safe.
        //
        // SECURITY FIX (V16) SOUNDNESS INVARIANT: the header guard proves
        // `array.length >= bound_local` exactly ONCE on loop entry, then every
        // per-element bounds check is elided. For that single guard to keep
        // every elided access in range, three locals must be loop-invariant
        // *after* the guard:
        //   1. the IV is `0..bound` step 1 — enforced by
        //      `find_induction_variable` (modified only by one canonical
        //      iinc/iadd-istore, no conflicting xstore).
        //   2. the array local is not reassigned — enforced inside
        //      `find_speculative_array_accesses` (`modified & (1<<al)==0`).
        //   3. the BOUND local is not raised inside the loop. If it were, a
        //      later iteration's exit test `iv < bound` could pass with
        //      `iv >= array.length` — an OOB access past the stale guard.
        // (1) and (2) were already checked; (3) was NOT. Enforce it here so the
        // speculative guard is only installed when `bound_local` is invariant.
        //
        // SECURITY FIX (V17), sound-guard form (2026-07-18): an inclusive
        // comparator reaches `index == bound`, which a `array.length >= bound`
        // header guard does NOT cover. Instead of refusing the loop, the
        // guard emission now proves `array.length > bound` (JBE deopt) plus
        // `bound != Integer.MAX_VALUE` for inclusive loops — see
        // `SpeculativeBCEGuard::inclusive`. (`find_safe_array_accesses` still
        // refuses the guard-less STATIC elisions for inclusive loops.)
        //
        // Step-provenance guard (2026-07-18): a variable-stride IV is only
        // admitted when the step local is identified, loop-invariant, and
        // < 64 (representable in `modified`); the preheader then proves
        // `0 <= step <= Integer.MAX_VALUE - bound` at runtime. `None` (an
        // unprovable step shape) refuses the speculative path entirely —
        // `find_induction_variable`'s `iadd;istore` admission alone said
        // nothing about the step's sign, so a runtime-negative step could
        // walk an elided index below the array base.
        let step_guard: Option<Option<usize>> = match iv_step {
            Some(IvStep::UnitInc) => Some(None),
            Some(IvStep::VarAdd(sl)) if sl < 64 && (modified & (1u64 << sl)) == 0 => Some(Some(sl)),
            _ => None,
        };
        let bound_invariant = (!bounds.inclusive || inclusive_spec_bce_enabled())
            && step_guard.is_some()
            && bounds
                .bound_local
                .map(|bl| bl < 64 && (modified & (1u64 << bl)) == 0)
                .unwrap_or(false);
        // DBG (env-gated): CRATONVM_JIT_NO_SPEC_BCE disables ONLY the speculative
        // (runtime-guarded) BCE, keeping the statically-proven elisions — to
        // isolate whether the speculative guard is the unsound corruptor.
        // Cached in a OnceLock so the env lookup is paid once, not per loop.
        let no_spec_bce = jit_no_spec_bce();
        if let Some(bound_local) = bounds
            .bound_local
            .filter(|_| bound_invariant && !no_spec_bce)
        {
            let mut speculative_accesses =
                find_speculative_array_accesses(&bounds, modified, &loop_safe, &operands);
            // `operands` is a hash map, so the access order is nondeterministic;
            // sort so guard emission order (and thus codegen) is reproducible.
            speculative_accesses.sort_unstable();
            if !speculative_accesses.is_empty() {
                // One guard per DISTINCT array local: each guard proves
                // `its_array.length >= bound` for its own array only, and
                // records which access PCs its pass justifies (`covered_pcs`)
                // so a later de-spec can restore exactly those checks.
                let mut guard_arrays: Vec<(usize, Vec<usize>)> = Vec::new();
                for &(access_pc, arr_local) in &speculative_accesses {
                    safe_pcs.insert(access_pc);
                    match guard_arrays.iter_mut().find(|(a, _)| *a == arr_local) {
                        Some((_, pcs)) => pcs.push(access_pc),
                        None => guard_arrays.push((arr_local, vec![access_pc])),
                    }
                }
                for (arr_local, covered_pcs) in guard_arrays {
                    speculative_guards.push(SpeculativeBCEGuard {
                        loop_header: header,
                        array_local: arr_local,
                        bound_local,
                        iv_local: induction_var,
                        covered_pcs,
                        inclusive: bounds.inclusive,
                        step_local: step_guard.flatten(),
                    });
                }
            }
        }
    }

    (safe_pcs, speculative_guards)
}

/// Find array accesses in a counted loop that use the IV as index but were NOT
/// already proven safe by `find_safe_array_accesses`. These are candidates for
/// speculative BCE with a deopt guard at the loop header.
///
/// Returns vec of (bytecode_pc_of_access, array_local).
///
/// Operand identification comes from `analyze_array_access_operands` via the
/// `operands` map (sound producer-stack tracking) — the old positional
/// heuristics mis-identified scatter stores and elided the wrong array's
/// bounds check.
pub(super) fn find_speculative_array_accesses(
    bounds: &LoopBoundsInfo,
    modified: u64,
    already_safe: &FxHashSet<usize>,
    operands: &FxHashMap<usize, (usize, usize)>,
) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    for (&pc, &(arr_local, idx_local)) in operands {
        if already_safe.contains(&pc) {
            continue;
        }
        if idx_local == bounds.induction_var {
            // SECURITY FIX (V16): array-local invariance. `al < 64` is
            // load-bearing, not just a bitmask bound: locals >= 64 cannot be
            // represented in the `modified` u64, so we conservatively refuse to
            // elide their checks. A modified array local is likewise rejected,
            // so the header guard's `array.length` cannot go stale via
            // reassignment. IV invariance is guaranteed by
            // `find_induction_variable`; bound-local invariance is enforced by
            // the caller (`analyze_bounds_elimination`) before this function is
            // invoked.
            if arr_local < 64 && (modified & (1u64 << arr_local)) == 0 {
                result.push((pc, arr_local));
            }
        }
    }
    result
}
