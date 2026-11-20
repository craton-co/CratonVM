// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Array bounds-check elimination.
//!
//! Moved verbatim out of `x64.rs`'s `Array Bounds Check Elimination (BCE)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

use crate::loop_analysis::{
    decode_bound_expr_with_hoisted_fields, HoistedFieldSite, LoopMethodTables, MinMax,
};
use crate::scev::{
    BoundSource, BoundTerm, BoundsProof, CountedLoop, ExitCmp, IndexExpr, IntRange, LoopForm,
    PreheaderGuard, RangeEnv, RefusalReason, Stride,
};

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
///
/// Read once per [`analyze_counted_loop_bce`]; no `static` (t1 wave 2, lane
/// Y: this was a process `OnceLock`, so the first VM's answer -- or a test's
/// per-thread override seen first -- stuck for every later VM in the
/// process). The value parser is unchanged (`1` / `true` / `on`).
pub(super) fn inclusive_spec_bce_enabled() -> bool {
    if let Some(v) = INCLUSIVE_SPEC_BCE_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false)
}

/// Whether speculative (runtime-guarded) BCE is disabled via
/// `CRATONVM_JIT_NO_SPEC_BCE`. Read once per [`analyze_counted_loop_bce`] (a
/// compile-time path, never per loop header), like the newer BCE switches
/// below; no `static` (t1 wave 2, lane Y: it was a process `OnceLock`).
pub(super) fn jit_no_spec_bce() -> bool {
    false
}

/// `CRATONVM_JIT_INLINE_LENGTH_BCE` -- **default OFF** (JIT round 21 wave 4,
/// lane bce, BCE21-1): a counted loop whose exit test reads its limit inline as
/// `aload a; arraylength` (no local home) may take guarded elisions for OTHER
/// arrays: the pre-header loads `a` (null deopts), reads its length and
/// compares each guarded array's length against it ([`BceGuardLimit::ArrayLength`]).
/// Read once per [`analyze_counted_loop_bce`]; no `static`.
pub(super) fn inline_length_bce_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_DOWN_COUNT_BCE` -- **default OFF** (JIT round 21 wave 4, lane
/// bce, BCE21-3): a counted loop stepping DOWN by a constant may take guarded
/// elisions whose only length demand is `length >= iv_entry + 1` (a down count
/// to a constant floor `>= 0`): the pre-header compares each guarded array's
/// length against the IV's entry value ([`BceGuardLimit::IvEntry`]). Also lets
/// such a loop's inclusive test (`i >= 0`) through the
/// `CRATONVM_JIT_INCLUSIVE_BCE` screen, which is about increasing loops. Read
/// once per [`analyze_counted_loop_bce`]; no `static`.
///
/// The same switch admits the zero-compare exit test (`iload iv; iflt`, the
/// shape javac gives `i >= 0`) in [`locate_exit_test`] and in
/// `loop_analysis::LoopMethodTables::analyze_counted_loop_with_hoisted_fields`
/// (whose only caller is [`recognise_loop`]): one reader,
/// `loop_analysis::zero_compare_exit_enabled`.
pub(super) fn down_count_bce_enabled() -> bool {
    crate::loop_analysis::zero_compare_exit_enabled()
}

/// `CRATONVM_JIT_LONG_INDUCTION_BCE` -- **default ON since j27 2026-10-28**,
/// `=0` kills (t1 wave 8, lane BCE, BCE21-4's single-pass arm; perf): `a[(int) i]` in `for (long i = 0; i <
/// a.length; i++)` is proved STATICALLY ([`long_induction_static_pcs`]), so no
/// pre-header guard and no emitter change. The IR tier's twin is
/// `CRATONVM_JIT_IR_BCE_LONG_INDUCTION`. Read once per
/// [`analyze_counted_loop_bce`]; no `static`.
pub(super) fn long_induction_bce_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// What a speculative-BCE guard compares each guarded array's length against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BceGuardLimit {
    /// The loop limit in `bound_local` (or its `int` field hoist): `JB` /
    /// `JBE` against it, as since the guard existed.
    Bound,
    /// BCE21-1: `<local>.length` -- the exit test's inline limit. The
    /// pre-header loads the local, deopts on null (reason 2) and compares
    /// against its length with `JB`. `bound_local` is `usize::MAX` (no local).
    ArrayLength(usize),
    /// BCE21-3: the IV's entry value of a down count. `JBE` (deopt when
    /// `length <= iv_entry`, unsigned, so a negative entry deopts too) proves
    /// `0 <= iv_entry < length`, and every elided index lies in
    /// `[0, iv_entry]`. `bound_local` is `usize::MAX`.
    ///
    /// `zero_trip_floor` (review rev-x64 item 7): `Some(f)` for a PRE-tested
    /// loop whose limit is the constant floor `f` (it runs while `iv >= f`).
    /// An entry value below `f` is a loop that does not run at all -- its
    /// first test exits before any body pc, elided or not -- so the
    /// pre-header jumps over the guards instead of deopting
    /// (`for (i = a.length - 1; i >= 0; i--)` over an empty array enters with
    /// `-1` on every call). `None` (a post-tested loop, whose first iteration
    /// is untested, or a non-constant limit) keeps the deopt.
    IvEntry {
        /// See the variant doc.
        zero_trip_floor: Option<i32>,
    },
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
    /// docs/feature-designs/jit-ir-optimizations.md). If the
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
    /// Round 13 wave 3 (lane loopcall): `Some(site)` when `array_local` is
    /// not a JVM local but the SYNTHETIC local of an X2 field-array hoist
    /// ([`FieldHoistAlias`]); `site` is that hoist's key, its first `aload`
    /// pc (`LoopHoist::seq_start`). The pre-header loads the array from the
    /// hoist's slot, after the hoist block has written it, and never from
    /// `local_offset(array_local)`.
    pub(super) array_field_hoist: Option<usize>,
    /// The same for `bound_local`: `Some(site)` names an `int` field hoist
    /// (`ArrayLenHoist::int_field`, keyed by its first site's `aload` pc)
    /// whose slot the exit test reads.
    pub(super) bound_field_hoist: Option<usize>,
    /// What the per-array compare reads (JIT round 21 wave 4, lane bce):
    /// [`BceGuardLimit::Bound`] except under BCE21-1 / BCE21-3. Every guard of
    /// one header has the same limit.
    pub(super) limit: BceGuardLimit,
}

impl SpeculativeBCEGuard {
    /// Whether the pre-header can evaluate this guard only after the field
    /// hoists of its loop have written their slots.
    pub(super) fn reads_a_field_hoist(&self) -> bool {
        self.array_field_hoist.is_some() || self.bound_field_hoist.is_some()
    }
}

/// Kill switch for the field-array bounds-check elimination (round 13 wave 3,
/// lane loopcall; `r12w4-vec2-x2-field-array-bce-not-landed-FIXED-20260928.md`):
/// `CRATONVM_JIT_FIELD_ARRAY_BCE=0` hands the BCE analysis no field hoists, so
/// every `this.data[i]` keeps its per-element check exactly as in round 12.
pub(crate) const FIELD_ARRAY_BCE_FLAG: &str = "CRATONVM_JIT_FIELD_ARRAY_BCE";

/// One field hoist the BCE analysis may read through (round 13 wave 3, lane
/// loopcall): every `aload R; getfield F` site of it in `[loop_header,
/// loop_end)` pushes the value its loop's pre-header read ONCE into a slot, so
/// for any loop inside that span the site is the synthetic local `synthetic`
/// (`loop_analysis::HoistedFieldSite`).
///
/// **What the driver guarantees for each alias it builds**, and what makes
/// eliding a check against it sound:
///
/// * every listed site is REPLACED by the slot load in the walk
///   (`aaload_hoist_at` / `len_hoist_at`), so the exit test's bound and the
///   element access read the same once-read value -- a racing store to the
///   field (JLS 17.4 lets the loop miss it, as HotSpot's hoisted load does)
///   cannot make the two name different arrays or sizes;
/// * the hoist's admission refused every loop that can write the field
///   (any `putfield`, any invoke, allocation, typecheck, monitor, volatile
///   access or class initialization), so no in-loop store through another
///   reference aliases it;
/// * the hoist's pre-header runs on every entry into the span (bypassable
///   headers are filtered before either exists, and no OSR entry lands inside
///   a hoisted span), and it is written BEFORE any guard that reads it
///   (`Compiler::emit_relocatable_preheader` defers such guards);
/// * `synthetic` is at or above `max_locals` and below 64, so no bytecode
///   stores it and its `modified_locals` bit is a fact;
/// * a hoist the driver drops after this analysis ran (the scalar-replacement
///   filter) takes every elision that read it with it (`field_dependent_pcs`
///   and the guards' `reads_a_field_hoist`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FieldHoistAlias {
    /// The hoisted loop's header and first pc past its back edge.
    pub(super) loop_header: usize,
    pub(super) loop_end: usize,
    /// The hoist's key: its first site's `aload` pc.
    pub(super) key: usize,
    /// The synthetic local that names the slot.
    pub(super) synthetic: usize,
    /// `true` for an array field (`LoopHoist::field`), `false` for an `int`
    /// field (`ArrayLenHoist::int_field`).
    pub(super) is_array: bool,
    /// Every `(aload pc, pc after the getfield)` site the walk replaces.
    pub(super) sites: Vec<(usize, usize)>,
}

impl FieldHoistAlias {
    /// Whether the hoist's slot is live across all of `[header, end)`.
    fn covers_loop(&self, header: usize, end: usize) -> bool {
        self.loop_header <= header && end <= self.loop_end
    }

    /// The hoisted-site answer for the site whose `aload` is at `pc`.
    fn site_at(&self, pc: usize) -> Option<HoistedFieldSite> {
        if !self.sites.iter().any(|&(start, _)| start == pc) {
            return None;
        }
        Some(if self.is_array {
            HoistedFieldSite::Array(self.synthetic)
        } else {
            HoistedFieldSite::Int(self.synthetic)
        })
    }
}

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

/// Type the primitive parameters [`classify_local_kinds`] left
/// [`LocalKind::Unknown`] — a parameter the method never loads or stores —
/// from the method descriptor's per-slot tags
/// ([`crate::param_slot_kind_tags`]). Interpreter round i1 wave 22, lane L1.
///
/// Only an `Unknown` slot is seeded, so no answer the scan gave changes. A
/// reference parameter is left alone (the must-oop mask, seeded by
/// `param_oop_mask`, describes it). A `J`/`D` is seeded only when its second
/// slot is `Unknown` or already `HighHalf`, which it then becomes, so the
/// cat-2 collapse stays aligned. Nothing is seeded when the tags' slot layout
/// is not the one the prologue writes (`param_jvm_slots`, or `0..num_params`
/// for the legacy layout that passes none — a cat-2 parameter there makes the
/// two disagree), and a slot whose machine home contradicts the kind
/// (`home_admits` false: a float parameter coloured into a GPR, whose bits
/// `Register` would read as an `int`) stays `Unknown`, i.e. `Undefined`, the
/// answer before the seed existed.
///
/// The driver applies it only while a debugger observes locals: there the
/// parameter is definitely assigned from bci 0 and kept in its home
/// (`regalloc::allocate_registers_keeping_assigned`), so the typed
/// description reads the value the caller passed.
pub(super) fn seed_unread_parameter_kinds(
    kinds: &mut [LocalKind],
    param_slot_tags: &[u8],
    param_jvm_slots: &[usize],
    num_params: usize,
    home_admits: impl Fn(usize, LocalKind) -> bool,
) {
    let tagged: Vec<usize> = (0..param_slot_tags.len())
        .filter(|&s| param_slot_tags[s] != 0)
        .collect();
    let layout_agrees = if param_jvm_slots.is_empty() {
        tagged.len() == num_params && tagged.iter().enumerate().all(|(i, &s)| i == s)
    } else {
        tagged.as_slice() == param_jvm_slots
    };
    if !layout_agrees {
        return;
    }
    for (slot, &tag) in param_slot_tags.iter().enumerate() {
        let kind = match tag {
            b'I' => LocalKind::Int,
            b'J' => LocalKind::Long,
            b'F' => LocalKind::Float,
            b'D' => LocalKind::Double,
            // A reference (the oop mask's), or the second slot of a cat-2.
            _ => continue,
        };
        if !matches!(kinds.get(slot), Some(LocalKind::Unknown)) {
            continue;
        }
        let wide = matches!(kind, LocalKind::Long | LocalKind::Double);
        if wide
            && !matches!(
                kinds.get(slot + 1),
                Some(LocalKind::Unknown | LocalKind::HighHalf)
            )
        {
            continue;
        }
        if !home_admits(slot, kind) {
            continue;
        }
        kinds[slot] = kind;
        if wide {
            kinds[slot + 1] = LocalKind::HighHalf;
        }
    }
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
    /// * **An exception range.** Handler entries are reached through
    ///   exception edges that carry the merge of each protected pc's in- AND
    ///   out-state. That is sound but WIDER than the verifier's in-state
    ///   merge (JVMS 4.10.1.6), so `Ambiguous` there still does not imply
    ///   `top` — a slot the verifier types precisely can read `Ambiguous`
    ///   here, and treating it as unreadable would drop a live value.
    /// * **`jsr`/`ret`.** [`crate::bytecode_analysis::lenient_successors`] gives `ret`
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
    /// the exception edges only widen — they can turn a settled kind into
    /// [`LocalKind::Ambiguous`], never manufacture one, so they cannot make a
    /// settled answer wrong.
    pub(super) has_jsr: bool,
}

impl AmbiguousLocalKinds {
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
/// in the retired `h2-jitban-longtail1` write-up. Uses the same
/// successor relation as the precise oop-mask pass so the two agree about
/// control flow, plus an exception edge from every pc in a protected range to
/// its handler carrying the merge of that pc's in- and out-state (a handler is
/// reachable from any point in its range, before or after that point's store).
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
    // slot -> column, built once: this used to be a linear `position` over
    // `slots` per lookup, twice per visit.
    let mut col_map: Vec<Option<usize>> = vec![None; kinds.len()];
    for (col, &slot) in slots.iter().enumerate() {
        if let Some(c) = col_map.get_mut(slot) {
            *c = Some(col);
        }
    }
    let col_of = |slot: usize| col_map.get(slot).copied().flatten();

    // `at` is the IN state; `reached` distinguishes "bottom" from "never seen"
    // so the first real predecessor seeds instead of merging with bottom.
    let mut at = vec![LocalKind::Unknown; code_len.saturating_mul(width)];
    let mut reached = vec![false; code_len];
    // A pc already waiting in `work` is not pushed again: it reads its IN
    // state when popped, so one queued visit sees every merge made meanwhile.
    let mut queued = vec![false; code_len];
    let mut work: Vec<usize> = Vec::new();

    reached[0] = true;
    queued[0] = true;
    work.push(0);
    // Exception handlers are NOT seeded: each protected pc reaches its handler
    // through an explicit edge in the loop below, carrying the merge of that
    // pc's in- and out-state. A handler no reached pc protects stays unreached.

    // Per-visit scratch, reused rather than allocated three times per visit
    // (round 11 wave 2, lane x64loop).
    let mut in_state: Vec<LocalKind> = vec![LocalKind::Unknown; width];
    let mut out: Vec<LocalKind> = vec![LocalKind::Unknown; width];
    let mut exc: Vec<LocalKind> = vec![LocalKind::Unknown; width];
    let mut edges: Vec<(usize, bool)> = Vec::new();
    // The handler table sorted by range start (stable, so equal starts keep
    // table order), so a visit stops at the first range that starts after it
    // instead of testing every handler (round 11 wave 4). The dataflow is a
    // monotone join, so the order edges are pushed in cannot change the
    // fixpoint.
    let mut handlers_by_start: Vec<(usize, usize, usize)> = exception_ranges.to_vec();
    handlers_by_start.sort_by_key(|&(start, _, _)| start);

    // Bound iterations defensively against any decoding pathology, exactly as
    // `compute_local_oop_masks` does.
    let mut guard = code_len.saturating_mul(64).saturating_add(64);
    while let Some(pc) = work.pop() {
        if pc >= code_len {
            continue;
        }
        queued[pc] = false;
        guard = guard.saturating_sub(1);
        if guard == 0 {
            // Ran out of budget: report nothing rather than a partial fixpoint.
            return AmbiguousLocalKinds::default();
        }
        in_state.copy_from_slice(&at[pc * width..pc * width + width]);
        out.copy_from_slice(&in_state);
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
        // The state an exception raised AT `pc` hands its handler: before or
        // after the instruction's local store, so the merge of both.
        for col in 0..width {
            exc[col] = merge_local_kind(in_state[col], out[col]);
        }
        edges.clear();
        edges.extend(
            bytecode_analysis::lenient_successors(code, code_len, pc)
                .into_iter()
                .map(|s| (s, false)),
        );
        for &(start, end, handler_pc) in &handlers_by_start {
            if start > pc {
                break;
            }
            if pc < end {
                edges.push((handler_pc, true));
            }
        }
        for &(succ, via_handler) in &edges {
            if succ >= code_len {
                continue;
            }
            let state: &[LocalKind] = if via_handler { &exc } else { &out };
            let mut changed = false;
            for col in 0..width {
                let idx = succ * width + col;
                let merged = if reached[succ] {
                    merge_local_kind(at[idx], state[col])
                } else {
                    state[col]
                };
                if merged != at[idx] {
                    at[idx] = merged;
                    changed = true;
                }
            }
            if (!reached[succ] || changed) && !queued[succ] {
                queued[succ] = true;
                work.push(succ);
            }
            reached[succ] = true;
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

/// The opcodes that can push a `long`/`float`/`double` WITHOUT naming one:
/// `ldc`/`ldc_w` (a `CONSTANT_Float`), `getstatic`/`getfield` of a `J`/`D`/`F`
/// field, and every `invoke*` returning one. `putstatic`/`putfield` only pop.
/// `ldc2_w` is not here: it is already in [`opcode_touches_long_float_double`].
pub(super) fn opcode_may_push_unnamed_wide_value(op: u8) -> bool {
    matches!(op, 0x12 | 0x13 | 0xb2 | 0xb4 | 0xb6..=0xba)
}

/// The descriptor-aware complement of [`code_uses_long_float_double`]: true
/// iff some [`opcode_may_push_unnamed_wide_value`] site pushes a `J`/`D`/`F`
/// value, or one whose kind is not known.
///
/// `pushed_tag(pc, op)` answers the JVM descriptor byte of the value the site
/// at `pc` pushes (`b'V'` for a void call), or `None` when no resolved
/// metadata says. `None` counts as wide: the gate this feeds must be sound,
/// and "no answer" is exactly the case in which the typed operand stack has
/// no answer either. A walk that cannot advance also answers `true`.
///
/// Why a second function and not four more opcodes in
/// [`opcode_touches_long_float_double`]: nearly every method has a field
/// access or a call, so the opcode-only widening would flip nearly every
/// "int-only" method to the conservative side, turning `Int` stack entries
/// into `Unsupported` wherever the typed stack declines — and
/// `osr_exit_policy` is artifact-wide. This version flips only methods that
/// really can hold such a value (census in
/// `deopt-stack-width-gate-misses-field-invoke-and-ldc-values-20260918.md`).
pub(super) fn code_pushes_unnamed_wide_value(
    code: &[u8],
    code_len: usize,
    pushed_tag: &dyn Fn(usize, u8) -> Option<u8>,
) -> bool {
    let end = code_len.min(code.len());
    let mut pc = 0usize;
    while pc < end {
        let op = code[pc];
        if opcode_may_push_unnamed_wide_value(op)
            && matches!(pushed_tag(pc, op), None | Some(b'J' | b'D' | b'F'))
        {
            return true;
        }
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            return true;
        }
        pc += len;
    }
    false
}

/// Find the induction variable of the loop body `[header, back_edge_end)`.
///
/// Priority 1: a local advanced by exactly one `iinc local, 1` and written by
/// nothing else in the body. Priority 2: a local written by exactly one
/// `iadd; istore local` (the `j += i` shape) and advanced by no `iinc`. A
/// `wide`-prefixed store or `iinc` of a local refuses it under both.
///
/// Returns the local index if found.
pub(super) fn find_induction_variable(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
) -> Option<usize> {
    let mut iinc_locals: Vec<(usize, i8)> = Vec::new(); // (local, increment)
    let mut stored_locals: u64 = 0; // bitmask of locals written by xstore
                                    // Bitmask of locals written by ANY `wide`-prefixed store or `iinc`. Kept
                                    // apart from `stored_locals` because the priority-2 recogniser below does
                                    // not consult that mask (its own `istore` sets it), and it must still
                                    // refuse a local a `wide iinc` also advances.
    let mut wide_written: u64 = 0;
    // Track iadd+istore pattern: iload X; ...; iadd; istore X
    let mut iadd_store_locals: Vec<usize> = Vec::new();
    // Every `istore`/`wide istore` occurrence, keyed by target local, counted
    // regardless of whether it is preceded by `iadd`.
    //
    // Priority 2 used to trust `iadd_store_locals` alone: "this local has
    // exactly one `iadd; istore` and no `iinc`". That is not "this local is
    // written nowhere else" — `stored_locals` only ever answers "at least
    // once", so a local written by ONE `iadd; istore` AND by a second,
    // unrelated plain `istore` elsewhere in the body (`if (cond) j = 5; ...;
    // j = j + i;`) passed both checks and was still returned as the
    // induction variable, with the plain store's target simply never having
    // been asked about. Every production caller of this function currently
    // re-verifies the exact bytecode shape of the whole loop body (the SIMD
    // sum / element-wise / matrix-dot detectors all require the walk to land
    // precisely on `back_edge - 3`), so today the gap cannot reach a real
    // elision — but this function's own contract ("modified by exactly one
    // `iadd; istore local`") is what a future caller would read and rely on,
    // and a wrong induction variable is a wrongly-deleted bounds check.
    let mut istore_count: FxHashMap<usize, u32> = FxHashMap::default();

    // The pc of the instruction this walk visited last, or `None` at the
    // header.
    //
    // # Why the walk has to carry this
    //
    // The two `iadd; istore X` recognisers below asked `code[pc - 1] == 0x60`.
    // This walk lands only on real instruction starts, so `pc` is one — but
    // `pc - 1` is the previous instruction's *last byte*, which is its opcode
    // only when that instruction was one byte long. `sipush 0x??60` ends in a
    // `0x60` operand byte and reads as an `iadd` that is not there.
    //
    // That is the same backward-byte-read class as the `arraylength` receiver
    // defect (`docs/feature-designs/jit-ir-optimizations.md
    // backward-byte-read-FIXED-20260916.md`) and the two `SOUNDNESS FIX`
    // blocks in `x64/null_check_elim.rs`, and here it is worse than a missed
    // optimization: a spurious entry in `iadd_store_locals` can name a local
    // as the loop's induction variable when it is not one, and this function's
    // answer is what guarded bounds-check elision reasons about. A wrong
    // induction variable is a wrongly-deleted bounds check.
    //
    // The walk already knows the answer — it stepped onto `pc` from the
    // previous start — so the fix is to keep it rather than to re-derive it
    // from bytes. `prev_is_iadd` is then exact: the previous instruction's
    // OPCODE is `iadd`, and it ends exactly where this one begins.
    let mut prev_pc: Option<usize> = None;
    let mut pc = header;
    while pc < back_edge_end {
        let prev_is_iadd = matches!(prev_pc, Some(p) if p + 1 == pc && code[p] == 0x60);
        let here = pc;
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
                *istore_count.entry(local).or_insert(0) += 1;
                // Check for iadd; istore X pattern (the iadd is right before).
                if prev_is_iadd {
                    iadd_store_locals.push(local);
                }
                pc += 1;
            }
            // istore (wide)
            0x36 => {
                let local = code[pc + 1] as usize; // Widening: always safe
                stored_locals |= 1u64 << local.min(63);
                *istore_count.entry(local).or_insert(0) += 1;
                // Check for iadd; istore X pattern
                if prev_is_iadd {
                    iadd_store_locals.push(local);
                }
                pc += 2;            }
            // lstore_0..lstore_3 — a cat-2 store writes slot k AND k+1.
            0x3f..=0x42 => {
                let k = (code[pc] - 0x3f) as usize; // Widening: always safe
                stored_locals |= (1u64 << k) | (1u64 << (k + 1));
                *istore_count.entry(k).or_insert(0) += 1;
                *istore_count.entry(k + 1).or_insert(0) += 1;
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                let local = (code[pc] - 0x4b) as usize; // Widening: always safe
                stored_locals |= 1 << local;
                *istore_count.entry(local).or_insert(0) += 1;
                pc += 1;
            }
            // lstore/fstore/dstore/astore (one-byte index)
            0x37..=0x3a => {
                let k = code[pc + 1] as usize; // Widening: always safe
                stored_locals |= 1u64 << k.min(63);
                *istore_count.entry(k).or_insert(0) += 1;
                if matches!(code[pc], 0x37 | 0x39) {
                    stored_locals |= 1u64 << (k + 1).min(63);
                    *istore_count.entry(k + 1).or_insert(0) += 1;
                }
                pc += 2;
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                let local = (code[pc] - 0x43) as usize; // Widening: always safe
                stored_locals |= 1 << local;
                *istore_count.entry(local).or_insert(0) += 1;
                pc += 1;
            }
            // dstore_0..dstore_3. These used to share the `fstore` arm and
            // compute `code[pc] - 0x43`, which for `dstore_0` (0x47) is FOUR:
            // the store marked slot k+4 instead of k and k+1, so a `double`
            // local at slot 0 refused an unrelated induction variable at
            // slot 4 (a missed elision) while its own two slots read as
            // unwritten.
            0x47..=0x4a => {
                let k = (code[pc] - 0x47) as usize; // Widening: always safe
                stored_locals |= (1u64 << k) | (1u64 << (k + 1));
                *istore_count.entry(k).or_insert(0) += 1;
                *istore_count.entry(k + 1).or_insert(0) += 1;
                pc += 1;
            }
            // `wide` — the prefix this walk had no arm for. `wide istore` /
            // `wide iinc` re-encode the arms above with a u16 index (and, for
            // `iinc`, an i16 delta: javac's spelling of `i += 1000`). Falling
            // to the length-only default kept the walk aligned while hiding
            // the write, so a local advanced by BOTH `iinc i, 1` and a
            // `wide iinc i, 1000` read as a unit-stride induction variable.
            // Any wide write refuses the local as an IV under both
            // recognisers below: priority 1 through `stored_locals`, which
            // every wide write marks, and priority 2 through `wide_written`.
            0xc4 => {
                if let (Some(&real), Some(&hi), Some(&lo)) =
                    (code.get(pc + 1), code.get(pc + 2), code.get(pc + 3))
                {
                    let idx = u16::from_be_bytes([hi, lo]) as usize; // Widening: u16 -> usize
                    if matches!(real, 0x36..=0x3a | 0x84) {
                        stored_locals |= 1u64 << idx.min(63);
                        wide_written |= 1u64 << idx.min(63);
                        if matches!(real, 0x37 | 0x39) {
                            stored_locals |= 1u64 << (idx + 1).min(63);
                        }
                    }
                }
                pc += bytecode_analysis::step(code, pc).max(1);
            }
            _ => pc += bytecode_analysis::step(code, pc),
        }
        // Set AFTER the arms, because each of them advanced `pc`; `here` is
        // the start this iteration actually visited. A malformed arm that
        // failed to advance would loop forever with or without this, which the
        // `while pc < back_edge_end` bound and `step(..).max(1)` already
        // prevent upstream.
        prev_pc = Some(here);
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
            // and only modified by this one iadd+istore in the loop, and NOWHERE
            // else — `iadd_count == 1` alone only says one `iadd`-preceded
            // `istore` exists; it says nothing about a second, unrelated plain
            // store to the same slot (`if (cond) j = 5; ...; j = j + i;`), which
            // would let this recogniser call `j` the induction variable while it
            // can also be reset outside the recognised step. `total_stores`
            // counts every store of any type to this slot (see `istore_count`),
            // so it must equal exactly the one this pattern itself accounts for.
            let iadd_count = iadd_store_locals.iter().filter(|&&l| l == local).count();
            let iinc_count = iinc_locals.iter().filter(|&&(l, _)| l == local).count();
            let total_stores = istore_count.get(&local).copied().unwrap_or(0);
            if iadd_count == 1
                && iinc_count == 0
                && total_stores == 1
                && (wide_written & (1u64 << local)) == 0
            {
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
/// The third tuple element is the net `iinc` displacement applied to the index
/// local between the loop header and the access, which discharges producer
/// obligation 3 in [`crate::scev`] ("an index read after an in-body advance must
/// fold that advance into [`crate::scev::IndexExpr::offset`]"). It was
/// previously not computed and the consumer assumed zero, which silently
/// elided the check on `while (i < n) { i++; a[i] = i; }` — the guard proves a
/// bound on the value the exit test saw, and the access used that value plus
/// one.
///
/// `hoisted` (round 13 wave 3, lane loopcall) names the `aload R; getfield F`
/// sites, by `aload` pc, whose value a loop pre-header read into a slot: an
/// access whose array is such a site's `getfield` -- with the site's own
/// `aload` as its receiver -- reports the site's synthetic local as its array.
/// `|_| None` is the historical analysis exactly.
pub(super) fn analyze_array_access_operands(
    code: &[u8],
    header: usize,
    back_edge_end: usize,
    hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
) -> FxHashMap<usize, (usize, usize, i32)> {
    let mut out: FxHashMap<usize, (usize, usize, i32)> = FxHashMap::default();
    // `getfield` pc -> the producer pc of the receiver it consumed.
    let mut getfield_receiver: FxHashMap<usize, usize> = FxHashMap::default();
    // The array a producer pc pushes, as a local: a bare `aload`, or the
    // `getfield` of a hoisted array-field site read straight off its `aload`.
    let array_local_of = |array_pc: usize, getfield_receiver: &FxHashMap<usize, usize>| {
        if let Some(l) = extract_aload_local(code, array_pc) {
            return Some(l);
        }
        if code.get(array_pc) != Some(&0xb4) {
            return None;
        }
        let recv = *getfield_receiver.get(&array_pc)?;
        extract_aload_local(code, recv)?;
        let recv_len = if code.get(recv) == Some(&0x19) { 2 } else { 1 };
        if recv + recv_len != array_pc {
            return None;
        }
        match hoisted(recv) {
            Some(HoistedFieldSite::Array(s)) => Some(s),
            _ => None,
        }
    };
    let code_len = code.len();
    if header >= back_edge_end || back_edge_end > code_len {
        return out;
    }

    // Precompute conditional-branch targets that land inside the loop. These
    // are control-flow *join* points where the linearly-simulated producer
    // stack — and, since this walk also accumulates `iinc` displacements, the
    // per-local value it tracks — is no longer guaranteed to match every
    // predecessor, so the walk must stop before analysing them.
    //
    // BACKWARD in-loop targets are collected too, which forward-only
    // collection did not do. The original note argued they "re-enter at the
    // same stack height by the JVM's structural constraint and need no special
    // handling". That is true of stack *height* and false of local *values*:
    // an inner loop whose back edge is a conditional branch re-enters its own
    // header with a different value of any local its body advances, so a
    // displacement accumulated straight-line past that header is right only on
    // the first iteration. Stack height was the only thing this walk used to
    // carry, which is why forward-only was sufficient before and is not now.
    // `goto`/switch/return are not modelled and stop the walk anyway.
    let mut join_targets: FxHashSet<usize> = FxHashSet::default();
    // `goto` targets inside the loop (round 11, lane x64loop). The walk never
    // steps THROUGH a `goto`, but it does walk straight into a `goto`'s
    // target from above — javac's inner-loop header, re-entered by the inner
    // back edge. That is only harmless while nothing is on the tracked
    // operand stack there: javac and ecj leave it empty at every statement
    // boundary, and a producer carried across such a header would pair an
    // access with an array or index some later iteration replaced. Stopping
    // at every `goto` target would cost the outer loop its accesses inside
    // inner loops (`a[i]` under `for (j ..)`), so the walk stops only when a
    // producer is live.
    let mut goto_targets: FxHashSet<usize> = FxHashSet::default();
    {
        let mut pc = header;
        while pc < back_edge_end {
            let op = code[pc];
            if matches!(op, 0xa7 | 0xc8) {
                if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                    if t > header && t < back_edge_end {
                        goto_targets.insert(t);
                    }
                }
            }
            if matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) && pc + 2 < code_len {
                // Cast: value to i32 (encoding immediate/displacement)
                let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                // Cast: value to i32 (encoding immediate/displacement)
                let target = pc as i32 + off;
                // Cast: non-negative index/count to usize
                if target > header as i32 && (target as usize) < back_edge_end {
                    // Cast: non-negative index/count to usize
                    join_targets.insert(target as usize);
                }
            }
            pc += bytecode_analysis::step(code, pc);
        }
    }

    // Net `iinc` displacement per local since the header, for the straight-line
    // path this walk is following. Only locals actually `iinc`'d appear; a
    // missing entry is zero.
    let mut iinc_delta: FxHashMap<usize, i32> = FxHashMap::default();
    // Locals written by a `*store` (as opposed to advanced by `iinc`). Their
    // value is no longer "the header's value plus a constant", so no index
    // derived from one can be described by an `IndexExpr` at all.
    let mut clobbered: FxHashSet<usize> = FxHashSet::default();
    // Displacement in force at the moment a producer PC pushed its value. The
    // index an access consumes is the value `iload` read WHEN IT RAN, so a
    // later `iinc` before the access must not be folded in: in
    // `iload i; iinc i 1; iaload` the element read is `a[i]`, not `a[i + 1]`.
    // An entry exists only for a producer whose value is modellable as
    // `iv_local + delta`; its absence is a refusal.
    let mut push_delta: FxHashMap<usize, i32> = FxHashMap::default();
    let mut producers: Vec<usize> = Vec::new();
    let mut pc = header;
    while pc < back_edge_end {
        // A join: the operand stack here may differ per predecessor path.
        if pc != header && join_targets.contains(&pc) {
            break;
        }
        if pc != header && !producers.is_empty() && goto_targets.contains(&pc) {
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
                if let (Some(al), Some(il), Some(&d)) = (
                    array_local_of(array_pc, &getfield_receiver),
                    extract_iload_local(code, index_pc),
                    push_delta.get(&index_pc),
                ) {
                    out.insert(pc, (al, il, d));
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
                if let (Some(al), Some(il), Some(&d)) = (
                    array_local_of(array_pc, &getfield_receiver),
                    extract_iload_local(code, index_pc),
                    push_delta.get(&index_pc),
                ) {
                    out.insert(pc, (al, il, d));
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
                // An `iload` of a local this walk can still describe carries
                // the displacement in force right now; the access that
                // consumes it reads that value, not the one after any later
                // `iinc`. A `*store`-clobbered local gets no entry, which
                // refuses every access derived from it.
                if let Some(l) = extract_iload_local(code, pc) {
                    if !clobbered.contains(&l) {
                        push_delta.insert(pc, iinc_delta.get(&l).copied().unwrap_or(0));
                    }
                }
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
                if op == 0xb4 {
                    getfield_receiver.insert(pc, producers[n - 1]);
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
                // A store writes an arbitrary value, so the local stops being
                // "the header's value plus a constant" for the rest of the
                // walk. Marked permanently: a later `iinc` advances an already
                // unknown value and does not restore modellability.
                if let Some(l) = store_local(code, pc) {
                    clobbered.insert(l);
                    iinc_delta.remove(&l);
                }
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
            // `nop` — no stack effect and no value effect.
            0x00 => {}
            // `iinc idx, const` — no stack effect, but it ADVANCES a local,
            // which is the whole point of tracking it here. Treating it as a
            // pure no-op is what let the consumer describe `a[i]` after
            // `i++` as the bare induction variable and elide a check the
            // guard never justified.
            0x84 => {
                let (Some(&idx), Some(&inc)) = (code.get(pc + 1), code.get(pc + 2)) else {
                    break;
                };
                let l = idx as usize;
                if !clobbered.contains(&l) {
                    let cur = iinc_delta.get(&l).copied().unwrap_or(0);
                    // A displacement that leaves `i32` cannot be folded into
                    // an `IndexExpr`, and wrapping it would be a lie. Refuse
                    // the local rather than carry a wrong offset.
                    match cur.checked_add(i32::from(inc as i8)) {
                        Some(next) => {
                            iinc_delta.insert(l, next);
                        }
                        None => {
                            clobbered.insert(l);
                            iinc_delta.remove(&l);
                        }
                    }
                }
            }
            // Everything else (goto, switches, returns, athrow, invoke*,
            // dup2/swap/dup_x*, pop2, wide, multianewarray, jsr/ret, ...) is not
            // modelled: stop so no access is reported on a desynchronised stack.
            _ => break,
        }
        pc += bytecode_analysis::step(code, pc);
    }

    out
}

/// Extract the local variable index written by a `*store` instruction at `pc`.
///
/// Covers both the operand form (`istore`/`lstore`/`fstore`/`dstore`/`astore`,
/// `0x36..=0x3a`) and the four packed forms of each (`0x3b..=0x4e`). The type
/// does not matter to the caller: the question is only which local stopped
/// being describable as the induction variable plus a constant.
pub(super) fn store_local(code: &[u8], pc: usize) -> Option<usize> {
    match *code.get(pc)? {
        // istore/lstore/fstore/dstore/astore <idx>
        0x36..=0x3a => code.get(pc + 1).map(|&b| b as usize),
        // {i,l,f,d,a}store_0..3 — four consecutive opcodes per type, in that
        // order, starting at 0x3b.
        op @ 0x3b..=0x4e => Some(((op - 0x3b) % 4) as usize),
        _ => None,
    }
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
                if let Some(target) = bytecode_analysis::offset_branch_target(&code[..code_len], pc)
                    .filter(|&t| t < code_len)
                {
                    targets.insert(target);                }            }
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
/// 1. the method contains EXACTLY ONE store to `bound_local`, it is the
///    canonical `aload A ; arraylength ; istore bound` triple (javac's
///    `int n = a.length;`), and that store lies on every path to every read
///    of `bound_local` ([`store_dominates_every_read`]). This used to be
///    argued from definite assignment, which does not cover a PARAMETER: it
///    is assigned on entry, so `if (all) n = a.length;` leaves a path on
///    which `n` is still the caller's value;
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
/// (docs/feature-designs/jit-ir-optimizations.md).
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
    bound_arraylength_provenance_with(code, code_len, bound_local, None)
}

/// [`find_bound_arraylength_provenance`], told the method's exception table
/// when the caller has it. `None` is the conservative reading (see
/// [`store_dominates_every_read`]); `Some` lets a `catch` block that runs only
/// after the store keep the proof.
fn bound_arraylength_provenance_with(
    code: &[u8],
    code_len: usize,
    bound_local: usize,
    handlers: Option<&[(usize, usize, usize)]>,
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
                astored.insert((op - 0x4b) as usize);            }
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
    // Condition 1's "single store dominates every read" is NOT implied by
    // definite assignment when `bound_local` is a PARAMETER — a parameter is
    // assigned at entry and needs no store. `void fill(int[] a, int n,
    // boolean all) { if (all) n = a.length; for (i = 0; i < n; i++) a[i] = 0; }`
    // has exactly one store to `n`, and it is the canonical triple, but the
    // `all == false` path reaches the loop with the caller's `n`. Answering
    // `Some(a)` there rewrote the limit to `a.length`, discharged `a[i]`
    // STATICALLY, and `fill(new int[2], 100, false)` wrote 98 ints past the
    // array with no bounds check. Prove the dominance instead of assuming it.
    if !store_dominates_every_read(code, code_len, store_pc, handlers, |p| {
        reads_int_local(code, code_len, p, bound_local)
    }) {
        return None;
    }
    Some(array_local)
}

/// Does `pc` read int local `local` (`iload`, `iinc`, or their `wide` forms)?
fn reads_int_local(code: &[u8], code_len: usize, pc: usize, local: usize) -> bool {
    match code.get(pc).copied() {
        Some(0x1a..=0x1d) => code[pc] as usize - 0x1a == local, // Widening: opcode-relative index
        Some(0x15) | Some(0x84) => {
            pc + 1 < code_len && code.get(pc + 1).map(|&b| b as usize) == Some(local)
        }
        Some(0xc4) if pc + 3 < code_len => {
            matches!(code.get(pc + 1).copied(), Some(0x15) | Some(0x84))
                && code
                    .get(pc + 2..pc + 4)
                    .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) // Widening: u16 -> usize
                    == Some(local)
        }
        _ => false,
    }
}

/// Is the instruction at `store_pc` on EVERY path to every pc `is_read`
/// accepts?
///
/// The whole-method provenance proofs above ([`find_bound_arraylength_provenance`],
/// [`find_iv_nonneg_start`]) used to justify "the single store dominates every
/// read" by JVM definite assignment. That holds for a local the method
/// declares and fails for a PARAMETER, which is definitely assigned on entry
/// with no store at all — the hole `loop_analysis::constant_iv_init` already
/// closed for its own copy of the argument. This checks the claim on the
/// control-flow graph instead.
///
/// Reachability with `store_pc` removed, over the normal edges
/// (`bytecode_analysis::lenient_successors`), from method entry plus the
/// handler entries an exception can deliver control to WITHOUT the store
/// having run:
///
/// * `handlers == Some(table)`: exactly the handlers whose protected range
///   holds a pc already reachable around the store (iterated to a fixpoint,
///   since a handler can itself reach another handler's range). A handler
///   whose range lies wholly after the store is entered only after the store
///   ran, so its reads stay dominated.
/// * `handlers == None` (the caller does not know the table): every
///   instruction start no normal edge reaches, because any of them may be a
///   handler entry. That refuses a proof whose only undominated read sits in
///   code a `catch` block leads to — a static elision lost (the speculative
///   guard still applies), never a wrong one.
///
/// Any read reachable that way is a read the store does not dominate, and the
/// answer is `false`. So is any decode that does not advance.
fn store_dominates_every_read(
    code: &[u8],
    code_len: usize,
    store_pc: usize,
    handlers: Option<&[(usize, usize, usize)]>,
    is_read: impl Fn(usize) -> bool,
) -> bool {
    let code_len = code_len.min(code.len());
    if store_pc >= code_len {
        return false;
    }
    let mut is_start = vec![false; code_len];
    let mut pc = 0usize;
    while pc < code_len {
        is_start[pc] = true;
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            return false;
        }
        pc += len;
    }
    if !is_start[store_pc] {
        return false;
    }
    // Normal-edge reachability from `roots`, never entering `blocked`.
    let walk = |roots: &[usize], blocked: Option<usize>| -> Vec<bool> {
        let mut seen = vec![false; code_len];
        let mut work: Vec<usize> = Vec::new();
        for &r in roots {
            if r < code_len && is_start[r] && Some(r) != blocked && !seen[r] {
                seen[r] = true;
                work.push(r);
            }
        }
        while let Some(p) = work.pop() {
            for s in bytecode_analysis::lenient_successors(code, code_len, p) {
                if s < code_len && is_start[s] && Some(s) != blocked && !seen[s] {
                    seen[s] = true;
                    work.push(s);
                }
            }
        }
        seen
    };
    let around_store = match handlers {
        None => {
            let from_entry = walk(&[0], None);
            let mut roots: Vec<usize> = vec![0];
            roots.extend((0..code_len).filter(|&p| is_start[p] && !from_entry[p]));
            walk(&roots, Some(store_pc))
        }
        Some(table) => {
            // Incremental: reachability only grows as handlers are rooted, so
            // one `seen` set is extended per round instead of re-walking the
            // whole method from every root once per rooted handler (that was
            // O(H x N) walks per call; round 11 wave 2, lane x64loop). The
            // per-round range test is O(1) per handler off a prefix count.
            let mut seen = vec![false; code_len];
            let mut work: Vec<usize> = Vec::new();
            let seed = |r: usize, seen: &mut Vec<bool>, work: &mut Vec<usize>| {
                if r < code_len && is_start[r] && r != store_pc && !seen[r] {
                    seen[r] = true;
                    work.push(r);
                }
            };
            seed(0, &mut seen, &mut work);
            let mut rooted = vec![false; table.len()];
            let mut prefix = vec![0usize; code_len + 1];
            loop {
                while let Some(p) = work.pop() {
                    for s in bytecode_analysis::lenient_successors(code, code_len, p) {
                        seed(s, &mut seen, &mut work);
                    }
                }
                for p in 0..code_len {
                    prefix[p + 1] = prefix[p] + usize::from(seen[p]);
                }
                let mut grew = false;
                for (i, &(start, end, handler)) in table.iter().enumerate() {
                    if rooted[i] {
                        continue;
                    }
                    let end = end.min(code_len);
                    if start < end && prefix[end] > prefix[start] {
                        rooted[i] = true;
                        seed(handler, &mut seen, &mut work);
                        grew = true;
                    }
                }
                if !grew {
                    break seen;
                }
            }
        }
    };
    !(0..code_len).any(|p| around_store[p] && is_read(p))
}

/// Whole-method proof that the induction variable can never be negative: its
/// only `istore` is a single store of a non-negative constant (`iconst_0..5` /
/// non-negative `bipush`/`sipush`) that lies on every path to every read of the
/// IV ([`store_dominates_every_read`] — definite assignment alone does not give
/// that for a parameter), no branch lands on that `istore`, and every `iinc` on
/// it is non-negative
/// (`find_induction_variable` separately guarantees the in-loop step is
/// exactly +1). Required for the *static* elision path: an elided access
/// assumes `0 <= iv`, and a `for (i = start; ...)` loop with a negative
/// `start` would otherwise silently access below the array base. The
/// speculative path needs no such proof — its header guard tests the runtime
/// `iv >= 0` at loop entry instead.
///
/// No production caller since round 11 wave 2: the SIMD coverage gate
/// (`simd_analysis::simd_array_covered`) asks `loop_analysis::constant_iv_init`
/// for the loop's entry value instead, which does not refuse a slot javac
/// reuses for a sibling loop. Kept for the tests that pin its answers.
#[cfg_attr(not(test), allow(dead_code))]
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
    // "Single dominating (definite-assignment) store" is false for a
    // PARAMETER IV: `void f(int[] a, int i, boolean c) { if (c) i = 0;
    // for (; i < n; i++) a[i] = 0; }` has one store of `0`, and `c == false`
    // reaches the loop with the caller's (possibly negative) `i`. See
    // `store_dominates_every_read`.
    nonneg_const
        && !branch_targets.contains(&store_pc)
        && store_dominates_every_read(code, code_len, store_pc, None, |p| {
            reads_int_local(code, code_len, p, iv_local)
        })
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
/// `iload x; <limit>; if_icmp*` triple inside the loop whose branch has an
/// edge leaving it, and it negates a continue-branch into exit polarity. It
/// does not decide the loop FORM (whether that test dominates every array
/// access), and "first leaving test" is not "the loop's exit test" when the
/// body holds a `break`-shaped compare ahead of it. This struct is produced
/// only from a branch that provably is the loop's exit (Pattern A: the header
/// itself) or its back edge (Pattern B), which is what settles the form.
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
    /// `Some(key)` when `bound_local` is the synthetic local of an `int`
    /// field hoist ([`FieldHoistAlias`], round 13 wave 3): the emitter loads
    /// the limit from that hoist's slot.
    bound_field_hoist: Option<usize>,
}

/// The exit-when-true comparison of the negation of `cmp`.
///
/// Pattern B's branch is taken to *continue*, so the loop's exit condition is
/// the complement of the opcode's own test. The table lives on the type
/// ([`ExitCmp::negate`], which `loop_analysis` uses for the same shape); this
/// name is kept so the Pattern-B site reads as what it does.
fn negate_exit_cmp(cmp: ExitCmp) -> ExitCmp {
    cmp.negate()
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
                if let Some(target) = bytecode_analysis::offset_branch_target(&code[..code_len], pc)
                    .filter(|&t| t < code_len)
                {
                    edges.push((pc, target));                }            }
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    Some(edges)
}

/// Whole-method tables the per-loop recognition below reads.
///
/// Built once per [`analyze_counted_loop_bce`] call. Each loop used to rebuild
/// them: `branch_edges` in `pattern_b_loop_form`, a fresh decode from pc 0 in
/// `falls_through_into`, and a whole-method `branch_target_map` in
/// `iv_store_advance_is_entered`. That made recognition O(loops x method)
/// (round 11 wave 2, lane x64loop;
/// `r11-x64loop-bce-compile-time-whole-method-rescans-per-loop-20260923.md`).
struct BceMethodTables {
    /// [`branch_edges`]: `None` when the method holds control flow that scan
    /// does not model.
    edges: Option<Vec<(usize, usize)>>,
    /// `bytecode_analysis::branch_target_map`, sized to the clamped `code_len`.
    targets: Vec<bool>,
    /// `bytecode_analysis::instruction_starts`, sized to the clamped `code_len`.
    starts: Vec<bool>,
}

impl BceMethodTables {
    fn build(code: &[u8], code_len: usize) -> Self {
        let code_len = code_len.min(code.len());
        BceMethodTables {
            edges: branch_edges(code, code_len),
            targets: bytecode_analysis::branch_target_map(code, code_len),
            starts: bytecode_analysis::instruction_starts(code, code_len),
        }
    }

    /// Whether some branch or switch names `pc`. Out of range answers `true`,
    /// the refusing direction.
    fn is_target(&self, pc: usize) -> bool {
        self.targets.get(pc).copied().unwrap_or(true)
    }

    /// Whether some branch or switch anywhere in the method lands in
    /// `(lo, hi]` — the question `loop_analysis::branch_lands_in` asks of a
    /// textually-read `iload x; <limit>; if_icmp*` triple.
    fn lands_in(&self, lo: usize, hi: usize) -> bool {
        (lo.saturating_add(1)..=hi).any(|p| self.is_target(p))
    }
}

/// Whether control can fall through into `header` from the instruction that
/// linearly precedes it. Answers `true` — the pessimistic answer — for a
/// header at pc 0 and for a `header` that is not an instruction start.
fn falls_through_into(code: &[u8], starts: &[bool], header: usize) -> bool {
    if header == 0 || !starts.get(header).copied().unwrap_or(false) {
        return true; // no predecessor, or a desynchronised decode
    }
    let Some(prev) = (0..header)
        .rev()
        .find(|&p| starts.get(p).copied().unwrap_or(false))
    else {
        return true;
    };
    // goto / jsr / switches / *return / athrow / goto_w / jsr_w do not fall
    // through; everything else does.
    !matches!(
        code.get(prev).copied(),
        Some(0xa7 | 0xa8 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf | 0xc8 | 0xc9)
    )
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
    tables: &BceMethodTables,
    header: usize,
    back_edge_end: usize,
    cmp_start: usize,
) -> LoopForm {
    let Some(edges) = tables.edges.as_deref() else {
        return LoopForm::PostTested;
    };
    let mut entries: Vec<usize> = Vec::new();
    for &(src, target) in edges {
        let target_inside = target >= header && target < back_edge_end;
        let src_inside = src >= header && src < back_edge_end;
        if target_inside && !src_inside {
            entries.push(target);
        }
    }
    if falls_through_into(code, &tables.starts, header) {
        entries.push(header);
    }
    for t in entries {
        if t == cmp_start {
            continue; // enters AT the test: the test still runs first
        }
        if t > cmp_start {
            // Lands INSIDE `iload i; iload n; if_icmplt` (round 11, lane
            // x64loop). The branch then compares whatever the entry path left
            // on the stack, not `i` against `n`, and the body's first pass is
            // untested. javac and ecj never emit this; fail closed.
            return LoopForm::PostTested;
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
/// The limit is decoded by `crate::loop_analysis::decode_bound_expr`'s shapes
/// (through `decode_bound_expr_with_hoisted_fields`, round 13 wave 3), which
/// accepts a constant, a local, an inline `a.length`, a field, or a
/// `Math.min`/`Math.max`. The `Math` resolver answers `None` here: this module
/// carries no constant pool, so a min/max limit simply is not recognised
/// (losing the shape, never mis-decoding it).
///
/// Both patterns read the test off TEXTUALLY consecutive instructions, which is
/// the dataflow only when no branch lands after the `iload`. A rotated
/// `while ((c ? i : j) < a.length)` lays out `…iload i; goto M; L: iload j; M:
/// aload a; arraylength; if_icmplt BODY`, and the Pattern-B scan used to read
/// the `iload j` triple as the test although the `c` path tests `i` (round 11
/// wave 2, lane x64loop; `r11-iropt-bce-exit-test-scan-ignores-joins-FIXED-20260923.md`).
/// A candidate with any branch landing in `(iload, if_icmp]` is refused here,
/// the rule `loop_analysis::branch_lands_in` applies to the recogniser, so this
/// scan no longer depends on the recogniser's cross-check to stay sound.
fn locate_exit_test(
    code: &[u8],
    code_len: usize,
    tables: &BceMethodTables,
    header: usize,
    back_edge: usize,
    back_edge_end: usize,
    hoisted: &dyn Fn(usize) -> Option<HoistedFieldSite>,
) -> Option<LoopExitTest> {
    let no_math = |_: u16| -> Option<MinMax> { None };
    // Round 13 wave 3 (lane loopcall): a limit read off a hoisted field site
    // is its slot (`FieldHoistAlias`); `hoisted` is `|_| None` otherwise, which
    // decodes exactly as `decode_bound_expr`.
    if header >= code_len || back_edge >= code_len || back_edge_end > code_len {
        return None;
    }

    // JIT round 21 wave 4 (lane bce, BCE21-3, `CRATONVM_JIT_DOWN_COUNT_BCE`):
    // `iload iv; if<lt|ge|gt|le>` is the test `iv <cond> 0`, limit
    // `Const(0)`, read only where no limit expression decodes and only with
    // the `iload` immediately before the branch (`ExitCmp::from_zero_compare_opcode`).
    // `loop_analysis` reads the same shape under the same switch, so the
    // `counted.bound == test.bound` agreement in `recognise_loop` holds.
    let zero_compare = down_count_bce_enabled();
    let zero_cmp_at = |q: usize| -> Option<ExitCmp> {
        if zero_compare && q < code_len {
            ExitCmp::from_zero_compare_opcode(code[q])
        } else {
            None
        }
    };

    // ---- Pattern A: the test is the header ------------------------------
    if let Some(iv) = extract_iload_local(code, header) {
        let after = header + if code[header] == 0x15 { 2 } else { 1 };
        let decoded =
            decode_bound_expr_with_hoisted_fields(code, after, back_edge_end, &no_math, hoisted)
                .map(|(b, q)| (b, q, false))
                .or_else(|| {
                    zero_cmp_at(after).map(|_| (BoundSource::Const(0), after, true))
                });
        if let Some((bound, q, zero_arm)) = decoded {
            if q + 2 < code_len && !tables.lands_in(header, q) {
                let cmp = if zero_arm {
                    zero_cmp_at(q)
                } else {
                    ExitCmp::from_opcode(code[q])
                };
                if let Some(cmp) = cmp {
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
    let back_zero = zero_cmp_at(back_edge);
    let cmp = match back_zero {
        Some(c) => c,
        None => ExitCmp::from_opcode(code[back_edge])?,
    };
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
            // BCE21-3: a zero-compare back edge's operand is the `iload iv`
            // ending exactly at it.
            if back_zero.is_some() {
                if after == back_edge && !tables.lands_in(s, back_edge) {
                    found = Some((s, iv, BoundSource::Const(0)));
                }
                s += bytecode_analysis::step(code, s);
                continue;
            }
            if let Some((bound, q)) =
                decode_bound_expr_with_hoisted_fields(code, after, back_edge_end, &no_math, hoisted)
            {
                if q == back_edge && !tables.lands_in(s, q) {
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
        form: pattern_b_loop_form(code, tables, header, back_edge_end, cmp_start),
    })
}

/// Recognise `(header, back_edge)` as a [`CountedLoop`] the range analysis can
/// reason about, with a verified comparator and loop form.
///
/// This is the single call that replaces steps 1-3d of the old
/// `analyze_bounds_elimination` (induction variable, loop bound, modified
/// locals, arraylength provenance, non-negative start, step provenance).
/// Does a branch land inside an `<a>; <b>; istore iv` advance in
/// `[header, back_edge_end)` — on the `istore` itself or on either of the two
/// instructions before it?
///
/// Those three pcs are where `find_iv_stride_in` reads the step from
/// (`iload iv; <step>; iadd; istore iv`); a join among them means the value
/// stored is not `iv + step` on every path (`i = (c ? k : i) + 1`). The
/// `iload iv` in front may itself be a target (a `continue` lands on the
/// update), which changes nothing. `iinc` advances have no window and are
/// unaffected. Fail-closed: a malformed walk answers `true`.
#[cfg_attr(not(test), allow(dead_code))]
fn iv_store_advance_is_entered(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge_end: usize,
    iv: usize,
) -> bool {
    iv_store_advance_is_entered_in(
        code,
        &BceMethodTables::build(code, code_len),
        header,
        back_edge_end,
        iv,
    )
}

/// [`iv_store_advance_is_entered`] against the method's prebuilt tables.
fn iv_store_advance_is_entered_in(
    code: &[u8],
    tables: &BceMethodTables,
    header: usize,
    back_edge_end: usize,
    iv: usize,
) -> bool {
    let is_target = |pc: usize| tables.is_target(pc);
    let mut prev: [Option<usize>; 2] = [None, None];
    let mut pc = header;
    while pc < back_edge_end {
        let Some(&op) = code.get(pc) else {
            return true;
        };
        let stores_iv = match op {
            0x36 => code.get(pc + 1).map(|&b| usize::from(b)) == Some(iv),
            0x3b..=0x3e => usize::from(op - 0x3b) == iv,
            _ => false,
        };
        if stores_iv
            && (is_target(pc) || prev[0].is_some_and(&is_target) || prev[1].is_some_and(&is_target))
        {
            return true;
        }
        prev = [Some(pc), prev[0]];
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            return true;
        }
        pc += len;
    }
    false
}

fn recognise_loop(
    code: &[u8],
    code_len: usize,
    tables: &BceMethodTables,
    loop_tables: &LoopMethodTables<'_>,
    provenance: &mut FxHashMap<usize, Option<usize>>,
    header: usize,
    back_edge: usize,
    handlers: Option<&[(usize, usize, usize)]>,
    aliases: &[FieldHoistAlias],
) -> Option<RecognisedLoop> {
    let back_edge_end = back_edge + bytecode_analysis::step(code, back_edge);
    if back_edge_end > code_len {
        return None;
    }
    // The field hoists whose slot is live over this whole loop (round 13 wave
    // 3, lane loopcall). A hoist of an INNER loop is not: its slot is
    // re-written on every entry into the inner loop, so it names no single
    // value across this one.
    let hoisted = |pc: usize| -> Option<HoistedFieldSite> {
        aliases
            .iter()
            .filter(|a| a.covers_loop(header, back_edge_end))
            .find_map(|a| a.site_at(pc))
    };
    let test = locate_exit_test(
        code,
        code_len,
        tables,
        header,
        back_edge,
        back_edge_end,
        &hoisted,
    )?;
    // The exception table goes in too: the recognition's "every path" claims
    // (the IV's constant entry value, one advance per iteration) must hold
    // along handler entries as well, and a loop an exception can enter from
    // outside is refused. `None` is `loop_analysis`' conservative reading.
    // `loop_tables` was built from the same `code`, `code_len` and `handlers`.
    let mut counted = loop_tables.analyze_counted_loop_with_hoisted_fields(
        header,
        back_edge,
        test.form,
        &|_| None,
        &hoisted,
    )?;
    // `analyze_counted_loop` decodes the FIRST `iload x; <limit>; if_icmp*`
    // triple in the body whose branch can leave the loop, negating a
    // continue-branch into exit polarity (it has done both since round 9).
    // That first leaving test need not be the loop's exit test -- a `break`
    // compare ahead of it also leaves -- so its answer is accepted only when it
    // describes the same `(iv, limit)` the verified exit test does, and the
    // polarity and the loop form are taken from the verified test. For a
    // matching test the two polarities agree; overriding keeps this site
    // independent of that.
    if counted.iv.local != test.iv_local || counted.bound != test.bound {
        return None;
    }
    counted.cmp = test.cmp;
    counted.form = test.form;
    // `loop_analysis::find_iv_stride_in` reads an `istore iv` advance from
    // its three textual predecessors and never asks whether a branch lands
    // among them, so `i = (c ? k : i) + 1` is recorded as `i++` (round 11,
    // lane x64loop; `r11-x64loop-iv-stride-ignores-branch-into-update-FIXED-20260923.md`).
    // Until that is fixed at the source, refuse such a loop here.
    if iv_store_advance_is_entered_in(code, tables, header, back_edge_end, counted.iv.local) {
        return None;
    }

    let bound_local = match &counted.bound {
        BoundSource::Local(bl) => Some(*bl),
        _ => None,
    };
    // A limit read off a hoisted `int` field site is its synthetic local; the
    // pre-header loads it from the hoist's slot, and no bytecode stores it, so
    // the provenance proof below has nothing to find.
    let bound_field_hoist = bound_local.and_then(|bl| {
        aliases
            .iter()
            .find(|a| !a.is_array && a.synthetic == bl && a.covers_loop(header, back_edge_end))
            .map(|a| a.key)
    });
    // Whole-method arraylength provenance, expressed in the limit itself rather
    // than as a separate per-access side condition: when the limit local
    // provably holds `A.length`, `prove_index_in_bounds_of` discharges `A[iv]`
    // against the `length >= A.length` tautology and needs no guard at all.
    // Naming a DIFFERENT array changes nothing — which is exactly the
    // multi-array out-of-bounds store this provenance pass exists to prevent
    // (docs/feature-designs/jit-ir-optimizations.md).
    // The proof is whole-method and depends only on `bl` (the code and the
    // handler table are fixed for the call), so it is memoised per bound local
    // rather than re-run, with its dominance walk, for every loop sharing it.
    if let Some(bl) = bound_local.filter(|_| bound_field_hoist.is_none()) {
        let a = *provenance
            .entry(bl)
            .or_insert_with(|| bound_arraylength_provenance_with(code, code_len, bl, handlers));
        if let Some(a) = a {
            counted.bound = BoundSource::ArrayLength(a);
        }
    }
    Some(RecognisedLoop {
        counted,
        bound_local,
        bound_field_hoist,
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
    /// BCE21-1 (JIT round 21 wave 4, lane bce): the limit is the inline
    /// `bound_term` array length, which the pre-header loads itself
    /// ([`BceGuardLimit::ArrayLength`]); stands in for `bound_local` in the
    /// `LengthAtLeast` arm only.
    inline_length: bool,
    /// BCE21-3: the per-array compare is against the IV's ENTRY value
    /// ([`BceGuardLimit::IvEntry`], `JBE`), proving `length >= iv_entry + 1`.
    /// Then `LengthAtLeast` is covered for `IvEntry(iv) + k`, `k <= 1`, and for
    /// nothing else.
    down_entry: bool,
}

impl GuardShape {
    /// Whether the emitted pre-header proves `g`.
    fn covers(&self, g: &PreheaderGuard) -> bool {
        // BCE21-3: a down count's pre-header compares each length against
        // the IV's entry value, so a demand on the limit is covered by
        // nothing.
        if self.down_entry {
            if let PreheaderGuard::LengthAtLeast(t) = g {
                return t.base == BoundTerm::IvEntry(self.iv_local) && t.addend <= 1;
            }
        }
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
            //
            // What is emitted is decided by `inclusive: addend == 0` alone:
            // `JBE` proves `length >= bound + 1`, `JB` proves `length >=
            // bound`. The old test `t.addend <= self.addend + 1` agreed with
            // that for the two increasing addends but read a DECREASING
            // exclusive loop's `addend == 1` as proving `bound + 2`, where its
            // `JB` proves `bound` (round 11, lane x64loop; unreachable today
            // because `iv_span` does not name the bound for a decreasing loop,
            // so this only closes the door before something does).
            PreheaderGuard::LengthAtLeast(t) => {
                let proven_addend: i64 = if self.addend == 0 { 1 } else { 0 };
                (self.bound_local.is_some() || (self.inline_length && self.addend == -1))
                    && !self.down_entry
                    && t.base == self.bound_term
                    && (t.addend as i64) <= proven_addend
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
/// needs (`RangeEnv::with_loop_iv_placed`).
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
/// (docs/feature-designs/jit-ir-optimizations.md).
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
    let (safe_pcs, guards, _) =
        analyze_bounds_elimination_with_field_hoists(code, code_len, loops, handlers, &[]);
    (safe_pcs, guards)
}

/// [`analyze_bounds_elimination_with_handlers`], told the loop pre-headers'
/// field hoists (round 13 wave 3, lane loopcall; [`FieldHoistAlias`]), so a
/// `this.data[i]` whose array is an X2 hoist's slot, under a limit that is a
/// local, that hoist's `data.length`, or an `int` field hoist's slot, is
/// proved like an access to a local array.
///
/// The third result is every pc whose elision reads a field hoist (a static
/// one against a hoisted length, or one a field-reading guard justifies): a
/// caller that drops the hoists after this ran must put those checks back.
/// With no aliases this is [`analyze_bounds_elimination_with_handlers`]
/// exactly, and the third result is empty.
pub(super) fn analyze_bounds_elimination_with_field_hoists(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    handlers: Option<&[(usize, usize, usize)]>,
    aliases: &[FieldHoistAlias],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>, Vec<usize>) {
    let (mut safe_pcs, guards, field_dependent_pcs) =
        analyze_counted_loop_bce(code, code_len, loops, handlers, aliases);
    if let Some(h) = handlers {
        if range_bce_enabled() {
            for pc in range_safe_pcs(code, code_len, h) {
                safe_pcs.insert(pc);
            }
        }
    }
    (safe_pcs, guards, field_dependent_pcs)
}

/// The counted-loop reason, unchanged but for the field hoists (round 13
/// wave 3). See [`analyze_bounds_elimination`] and
/// [`analyze_bounds_elimination_with_field_hoists`] for the contract.
fn analyze_counted_loop_bce(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    handlers: Option<&[(usize, usize, usize)]>,
    aliases: &[FieldHoistAlias],
) -> (FxHashSet<usize>, Vec<SpeculativeBCEGuard>, Vec<usize>) {
    let mut safe_pcs = FxHashSet::default();
    let mut speculative_guards: Vec<SpeculativeBCEGuard> = Vec::new();
    let mut field_dependent_pcs: Vec<usize> = Vec::new();
    // DBG (env-gated): CRATONVM_JIT_NO_SPEC_BCE disables ONLY the speculative
    // (runtime-guarded) BCE, keeping the statically-proven elisions — to
    // isolate whether the speculative guard is the unsound corruptor.
    // Read once here, not per loop (and not latched per process).
    let no_spec_bce = jit_no_spec_bce();
    let inclusive_spec_bce = inclusive_spec_bce_enabled();
    // JIT round 21 wave 4 (lane bce): BCE21-1 / BCE21-3, both opt-in.
    let inline_length_bce = inline_length_bce_enabled();
    let down_count_bce = down_count_bce_enabled();

    // Recognise every loop up front so an inner loop can be proved in an
    // environment that already knows its enclosing loop's IV range.
    let tables = BceMethodTables::build(code, code_len);
    // One set of loop-recognition tables for every loop pair: the entry-value
    // proof's whole-method decode is paid once, not once per loop.
    let loop_tables = LoopMethodTables::new(code, code_len, handlers);
    let mut provenance: FxHashMap<usize, Option<usize>> = FxHashMap::default();
    let recognised: Vec<Option<RecognisedLoop>> = loops
        .iter()
        .map(|&(header, back_edge)| {
            recognise_loop(
                code,
                code_len,
                &tables,
                &loop_tables,
                &mut provenance,
                header,
                back_edge,
                handlers,
                aliases,
            )
        })
        .collect();
    let enclosing = innermost_enclosing(code, loops);
    // Every pc an OSR entry can land on (`licm::osr_entry_pcs`: the targets
    // of backward branches, where the interpreter offers OSR).
    let osr_entry_targets: Vec<usize> = osr_entry_pcs(code, code_len);

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
        // BCE21-3: a down count (`i >= 0` is inclusive) is let through under
        // `CRATONVM_JIT_DOWN_COUNT_BCE`; its guard is the entry-value compare,
        // not the increasing loop's `JBE` + `bound != MAX` pair.
        let down_mode =
            down_count_bce && matches!(loop_.iv.stride, Stride::Const(s) if s < 0);
        if loop_.is_inclusive() && !inclusive_spec_bce && !down_mode {
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
        // A guarded elision rests on a pre-header that ran on THIS entry into
        // the loop. Every normal entry runs it (bypassable headers are dropped
        // by the driver), and so does an OSR entry at this header
        // (`osr_entry_native[header]` points before the pre-header). An OSR
        // entry at a backward-branch target strictly INSIDE the body -- an
        // inner loop's header -- does not: it enters the body directly, and
        // every later iteration comes round the back edge, which targets the
        // pre-header's far side. The guard then never runs for that
        // activation, and an elided access indexes past the array: measured on
        // the w7b binary with `review8a/src/OsrInnerGuard.java` (a hot inner
        // loop under `for (i < n) { a[i] = ...; for (j < m) ... }`, called once
        // with `n > a.length`): HotSpot throws AIOOBE, CratonVM wrote 56 ints
        // past the array. The pc-level OSR veto the hoists use
        // (`inside_*_hoisted` in the walk) would cost the inner loop its OSR;
        // refusing the GUARDED proof costs only the accesses the outer loop's
        // straight-line prefix makes (the walk in
        // `analyze_array_access_operands` stops at the inner header), so that is
        // the side taken. A STATIC proof needs no pre-header and is kept.
        let body_has_inner_osr_entry =
            span_holds_inner_osr_entry(&osr_entry_targets, header, back_edge_end);

        // `with_loop_iv` binds the outer IV to the values its EXIT TEST saw,
        // which is what the inner loop sees only while the outer advance comes
        // after it; an advance earlier in the outer body (`for (i = 0; i < K;)
        // { i++; for (j = c; j < i; j++) a[j] = ..; }`) shifts the value by
        // one stride, and a shifted span under-states the inner indices. The
        // placed form binds the outer IV only when the outer body's prefix in
        // front of this loop does not write it, and checks that itself
        // (`r11-x64loop-scev-with-loop-iv-binds-exit-test-range`, round 11 wave
        // 4: it also refuses rather than saturates a slot of 64 or more).
        let env = match enclosing[li].and_then(|pi| recognised[pi].as_ref()) {
            Some(outer) => RangeEnv::new().with_loop_iv_placed(&outer.counted, code, header),
            None => RangeEnv::new(),
        };

        // Soundly identify the (array_local, index_local) consumed by each
        // analysable array access via operand-stack producer tracking. NOT
        // subsumed by the range analysis: this is the *producer* of the index
        // expression, and its STOP-on-anything-unmodelled discipline stays
        // load-bearing.
        //
        // Round 13 wave 3 (lane loopcall): an access whose array is a hoisted
        // field site of a hoist live over this whole loop reports that hoist's
        // synthetic local ([`FieldHoistAlias`]), which the proof below treats
        // like any unmodified array local.
        let live_aliases: Vec<&FieldHoistAlias> = aliases
            .iter()
            .filter(|a| a.covers_loop(header, back_edge_end))
            .collect();
        let hoisted = |pc: usize| -> Option<HoistedFieldSite> {
            live_aliases.iter().find_map(|a| a.site_at(pc))
        };
        let array_hoist_key = |arr_local: usize| -> Option<usize> {
            live_aliases
                .iter()
                .find(|a| a.is_array && a.synthetic == arr_local)
                .map(|a| a.key)
        };
        let bound_field_hoist = rl.bound_field_hoist;
        let operands = analyze_array_access_operands(code, header, back_edge_end, &hoisted);
        // `operands` is a hash map, so its iteration order is nondeterministic;
        // sort so guard emission order (and thus codegen) is reproducible.
        let mut accesses: Vec<(usize, usize, usize, i32)> = operands
            .iter()
            .map(|(&pc, &(arr, idx, delta))| (pc, arr, idx, delta))
            .collect();
        accesses.sort_unstable();

        let addend = loop_.bound_addend();
        let step_local = match loop_.iv.stride {
            Stride::Variable(s) => Some(s),
            Stride::Const(_) => None,
        };
        // BCE21-1 (JIT round 21 wave 4, lane bce, `CRATONVM_JIT_INLINE_LENGTH_BCE`):
        // an inline `aload a; arraylength` limit has no local home, but the
        // pre-header can read it the way the exit test does -- `a` is unmodified
        // in the body (so every exit test reads this same length), not a field
        // hoist's synthetic local (whose value lives in a slot), exclusive (`JB`
        // only) and with a constant stride (no `MAX - bound` headroom test).
        let bound_array = match &loop_.bound {
            BoundSource::ArrayLength(a)
                if inline_length_bce
                    && !down_mode
                    && bound_local.is_none()
                    && rl.bound_local.is_none()
                    && addend == -1
                    && step_local.is_none()
                    && *a < 64
                    && (loop_.modified_locals & (1u64 << *a)) == 0
                    && !aliases.iter().any(|al| al.synthetic == *a) =>
            {
                Some(*a)            }
            _ => None,
        };
        // What the per-array compare reads; `None` refuses every guarded
        // elision of this loop.
        let limit: Option<BceGuardLimit> = if down_mode {
            // The floor the pre-tested loop runs down to: exit `iv < c` runs
            // while `iv >= c`; exit `iv <= c` while `iv >= c + 1`.
            let zero_trip_floor = match (&loop_.bound, loop_.cmp, loop_.form) {
                (BoundSource::Const(c), ExitCmp::Lt, LoopForm::PreTested) => Some(*c),
                (BoundSource::Const(c), ExitCmp::Le, LoopForm::PreTested) => c.checked_add(1),
                _ => None,
            };
            Some(BceGuardLimit::IvEntry { zero_trip_floor })
        } else if bound_local.is_some() {
            Some(BceGuardLimit::Bound)
        } else {
            bound_array.map(BceGuardLimit::ArrayLength)
        };
        let shape = GuardShape {
            iv_local: loop_.iv.local,
            bound_local,
            bound_term: BoundTerm::Bound(loop_.bound.clone()),
            addend,
            step_local,
            inline_length: bound_array.is_some(),
            down_entry: down_mode,
        };

        // One guard per DISTINCT array local, recording which access PCs its
        // pass justifies (`covered_pcs`) so a later de-spec restores exactly
        // those checks.
        let mut guard_arrays: Vec<(usize, Vec<usize>)> = Vec::new();
        for (pc, arr_local, idx_local, idx_delta) in accesses {
            // The index must be derived from this loop's induction variable.
            // `scale` still has no producer — a scaled index keeps its
            // per-element check — but `offset` now does: `idx_delta` is the
            // net `iinc` this access's `iload` had already seen, which is
            // exactly what producer obligation 3 in `crate::scev` requires be
            // folded in.
            if idx_local != loop_.iv.local {
                continue;
            }
            // Array-local invariance. `< 64` is load-bearing, not just a
            // bitmask bound: a local the `u64` cannot represent is refused
            // rather than assumed unmodified.
            if arr_local >= 64 || (loop_.modified_locals & (1u64 << arr_local)) != 0 {
                continue;
            }
            let idx = IndexExpr::shifted(loop_.iv.local, idx_delta);
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
            // Whether this elision reads a field hoist: its array is a
            // hoist's slot, or its limit (static tautology or guard) is.
            let reads_field_hoist =
                array_hoist_key(arr_local).is_some() || bound_field_hoist.is_some();
            match proof {
                BoundsProof::Static => {
                    safe_pcs.insert(pc);
                    if reads_field_hoist {
                        field_dependent_pcs.push(pc);
                    }
                }
                BoundsProof::Guarded(guards) => {
                    if no_spec_bce || limit.is_none() || body_has_inner_osr_entry {
                        continue;
                    }
                    // ALL or NOTHING. One guard this emitter cannot discharge
                    // makes the whole proof worthless.
                    if !guards.iter().all(|g| shape.covers(g)) {
                        continue;
                    }
                    safe_pcs.insert(pc);
                    if reads_field_hoist {
                        field_dependent_pcs.push(pc);
                    }
                    match guard_arrays.iter_mut().find(|(a, _)| *a == arr_local) {
                        Some((_, pcs)) => pcs.push(pc),
                        None => guard_arrays.push((arr_local, vec![pc])),
                    }
                }
                BoundsProof::Refused(_) => {}
            }
        }

        if let Some(limit) = limit {
            // Only the historical form reads `bound_local` (or its field
            // hoist); BCE21-1 / BCE21-3 name no limit local (`usize::MAX`,
            // which matches no real local in the SIMD coverage test either).
            let (guard_bound_local, guard_bound_hoist) = match (limit, bound_local) {
                (BceGuardLimit::Bound, Some(bl)) => (bl, bound_field_hoist),
                _ => (usize::MAX, None),
            };
            for (arr_local, covered_pcs) in guard_arrays {
                speculative_guards.push(SpeculativeBCEGuard {
                    loop_header: header,
                    array_local: arr_local,
                    bound_local: guard_bound_local,
                    iv_local: loop_.iv.local,
                    covered_pcs,
                    inclusive: limit == BceGuardLimit::Bound && addend == 0,
                    step_local: if limit == BceGuardLimit::Bound {
                        step_local
                    } else {
                        None
                    },
                    array_field_hoist: array_hoist_key(arr_local),
                    bound_field_hoist: guard_bound_hoist,
                    limit,
                });
            }
        }
    }

    // BCE21-4, single-pass arm (t1 wave 8, lane BCE,
    // `CRATONVM_JIT_LONG_INDUCTION_BCE`, default ON since j27): `long`-indexed accesses,
    // proved statically and independently of the `int` recognition above (a
    // separate fact about other accesses, so the two only add up).
    if long_induction_bce_enabled() {
        for &(header, back_edge) in loops {
            for pc in long_induction_static_pcs(
                code,
                code_len,
                &tables,
                &loop_tables,
                &mut provenance,
                header,
                back_edge,
                handlers,
            ) {
                safe_pcs.insert(pc);
            }
        }
    }

    (safe_pcs, speculative_guards, field_dependent_pcs)
}

// ===========================================================================
// BCE21-4, single-pass arm: a `long` induction variable (t1 wave 8, lane BCE)
//
// javac compiles `for (long i = 0; i < a.length; i++) s += a[(int) i];` to
//
//     lconst_0; lstore i
//  H: lload i; aload a; arraylength; i2l; lcmp; ifge EXIT
//     ... aload a; lload i; l2i; iaload ...
//     lload i; lconst_1; ladd; lstore i
//     goto H
//
// (ecj / kotlinc rotate it: `goto T; H: body; advance; T: lload i; ...; lcmp;
// iflt H`). The `int` machinery above tracks `int` locals only, so every such
// access kept its check in the single-pass tier.
//
// # Why the elision is sound
//
// [`long_induction_static_pcs`] admits an access `a[(int) i]` only when:
//
// 1. the exit test is `lload i; <a.length as long>; lcmp; if<cond>` and the
//    loop stays exactly while `i < a.length` (the limit is `aload a;
//    arraylength; i2l`, or `iload n; i2l` with `n` proven to hold `a.length`
//    at every read, `bound_arraylength_provenance_with`), entered only at the
//    test (Pattern A: the header IS the test; Pattern B: the test is the back
//    edge to the header, and every entry lands on its `lload`);
// 2. [`crate::loop_analysis::LongUnitIv`]: `i`'s only write in the body is
//    `i = i + 1`, once per iteration, the test runs on every iteration, and
//    `i` enters as `0` or `1`;
// 3. `a` is not written in the body (and is the access's array);
// 4. the access reads `lload i; l2i` on the straight-line walk from the header
//    with no write of `i` before it (so it reads the header's value).
//
// Then at every header visit `0 <= i < a.length`: on entry `i = init >= 0`;
// Pattern A tests it at the header, Pattern B tested it on the edge into the
// header; after a passing test the body changes `i` only by the single `+1`,
// which from `i < a.length <= i32::MAX` cannot wrap a `long` and keeps `i >=
// 0`, and the next header visit is tested again. By 4 the access reads that
// value, so `(int) i == i` exactly and `0 <= (int) i < a.length`. `a` is
// non-null there: the test (or the store of `n`) read `a.length` first.
//
// A STATIC fact about every execution, so it needs no pre-header and an OSR
// entry (which resumes a real interpreter state) does not weaken it.
// ===========================================================================

/// The exit test of a `long`-indexed loop: [`long_induction_static_pcs`] 1.
struct LongExitTest {
    /// The `long` local the test compares.
    iv: usize,
    /// The array whose length the test reads, as a local.
    limit_array: usize,
    /// The `if<cond>` that branches.
    test_pc: usize,
}

/// `<limit>` of `lload i; <limit>; lcmp; if<cond>` starting at `pc`, and the
/// pc of the `lcmp`: `aload a; arraylength; i2l` is `Ok(a)` (the array),
/// `iload n; i2l` is `Err(n)` (an `int` local, resolved by provenance).
fn decode_long_limit(code: &[u8], pc: usize, end: usize) -> Option<(Result<usize, usize>, usize)> {
    let (limit, after) = if let Some(a) = extract_aload_local(code, pc) {
        let after = pc + if code[pc] == 0x19 { 2 } else { 1 };
        if code.get(after) != Some(&0xbe) {
            return None;
        }
        (Ok(a), after + 1)
    } else if let Some(n) = extract_iload_local(code, pc) {
        (Err(n), pc + if code[pc] == 0x15 { 2 } else { 1 })
    } else {
        return None;
    };
    // i2l; lcmp
    if code.get(after) != Some(&0x85) || code.get(after + 1) != Some(&0x94) || after + 1 >= end {
        return None;
    }
    Some((limit, after + 1))
}

/// Whether every edge from outside `[header, end)` into it lands on `at`
/// (fall-through into the header counting as an edge onto it). Fail-closed:
/// a method whose branches `branch_edges` cannot list answers `false`.
fn every_loop_entry_lands_at(
    code: &[u8],
    tables: &BceMethodTables,
    header: usize,
    end: usize,
    at: usize,
) -> bool {
    let Some(edges) = tables.edges.as_deref() else {
        return false;
    };
    let inside = |p: usize| p >= header && p < end;
    if edges
        .iter()
        .any(|&(src, t)| inside(t) && !inside(src) && t != at)
    {
        return false;
    }
    !(header != at && falls_through_into(code, &tables.starts, header))
}

/// [`long_induction_static_pcs`] 1: the loop's exit test, Pattern A or B.
fn locate_long_exit_test(
    code: &[u8],
    code_len: usize,
    tables: &BceMethodTables,
    header: usize,
    back_edge: usize,
    back_edge_end: usize,
) -> Option<(usize, Result<usize, usize>, usize)> {
    // The loop's back edge goes to its header.
    if bytecode_analysis::offset_branch_target(&code[..code_len], back_edge) != Some(header) {
        return None;
    }
    let target_of =
        |q: usize| bytecode_analysis::offset_branch_target(&code[..code_len], q);
    let outside = |t: usize| t < header || t >= back_edge_end;
    // Pattern A: the header is `lload iv`, and the test's taken edge exits:
    // it exits when `i >= limit` (`ifge`).
    if let Some((iv, len)) = crate::loop_analysis::lload_local(code, header) {
        if let Some((limit, lcmp)) = decode_long_limit(code, header + len, back_edge_end) {
            let q = lcmp + 1;
            if q < back_edge_end
                && code[q] == 0x9c
                && target_of(q).is_some_and(outside)
                && !tables.lands_in(header, q)
                && every_loop_entry_lands_at(code, tables, header, back_edge_end, header)
            {
                return Some((iv, limit, q));
            }
        }
    }
    // Pattern B: the back edge is the test, continuing while `i < limit`
    // (`iflt header`), entered only at its `lload`.
    if code[back_edge] != 0x9b {
        return None;
    }
    let mut s = header;
    while s < back_edge {
        if let Some((iv, len)) = crate::loop_analysis::lload_local(code, s) {
            if let Some((limit, lcmp)) = decode_long_limit(code, s + len, back_edge_end) {
                if lcmp + 1 == back_edge
                    && !tables.lands_in(s, back_edge)
                    && every_loop_entry_lands_at(code, tables, header, back_edge_end, s)
                {
                    return Some((iv, limit, back_edge));
                }
            }
        }
        let len = bytecode_analysis::step(code, s);
        if len == 0 {
            return None;
        }
        s += len;
    }
    None
}

/// Every array-access pc in `[header, end)` whose index is `lload iv; l2i`
/// read before any write of `iv` on the straight-line walk from the header,
/// with its array local -- [`long_induction_static_pcs`] 4. The walk is
/// [`analyze_array_access_operands`]' producer simulation (the same opcode
/// table, the same STOP at a join and at a `goto` target with a live operand);
/// anything it cannot model stops it, and stopping reports nothing more.
fn long_index_accesses(
    code: &[u8],
    header: usize,
    end: usize,
    iv: usize,
    handler_pcs: &[usize],
) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    if header >= end || end > code.len() {
        return out;
    }
    let mut join_targets: FxHashSet<usize> = FxHashSet::default();
    let mut goto_targets: FxHashSet<usize> = FxHashSet::default();
    {
        let mut pc = header;
        while pc < end {
            let op = code[pc];
            if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                if t > header && t < end {
                    if matches!(op, 0xa7 | 0xc8) {
                        goto_targets.insert(t);
                    } else if matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) {
                        join_targets.insert(t);
                    }
                }
            }
            let len = bytecode_analysis::step(code, pc);
            if len == 0 {
                return out;
            }
            pc += len;
        }
    }
    // Set once a store touching `iv` or `iv + 1` has run on the walk.
    let mut iv_written = false;
    // Producers that push the header's `i` (an `lload iv`), and those that
    // push `(int)` of one (its `l2i`).
    let mut tagged_long: FxHashSet<usize> = FxHashSet::default();
    let mut tagged_index: FxHashSet<usize> = FxHashSet::default();
    let mut producers: Vec<usize> = Vec::new();
    let mut pc = header;
    while pc < end {
        if pc != header && join_targets.contains(&pc) {
            break;
        }
        if pc != header && !producers.is_empty() && goto_targets.contains(&pc) {
            break;
        }
        // An exception can land here after the advance ran.
        if handler_pcs.contains(&pc) {
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
                if tagged_index.contains(&producers[n - 1]) {
                    if let Some(a) = extract_aload_local(code, producers[n - 2]) {
                        out.push((pc, a));
                    }
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
                if tagged_index.contains(&producers[n - 2]) {
                    if let Some(a) = extract_aload_local(code, producers[n - 3]) {
                        out.push((pc, a));
                    }
                }
                producers.truncate(n - 3);
            }
            0x01..=0x2d | 0xb2 | 0xbb => {
                let reads_iv =
                    crate::loop_analysis::lload_local(code, pc).map(|(l, _)| l) == Some(iv);
                if !iv_written && reads_iv {
                    tagged_long.insert(pc);
                }
                producers.push(pc);
            }
            0x74..=0x77 | 0x85..=0x93 | 0xb4 | 0xbc | 0xbd | 0xbe | 0xc0 | 0xc1 => {
                let n = producers.len();
                if n < 1 {
                    break;
                }
                // `l2i` of the header's `i`.
                if op == 0x88 && tagged_long.contains(&producers[n - 1]) {
                    tagged_index.insert(pc);
                }
                producers.truncate(n - 1);
                producers.push(pc);
            }
            0x60..=0x73 | 0x78..=0x83 | 0x94..=0x98 => {
                let n = producers.len();
                if n < 2 {
                    break;
                }
                producers.truncate(n - 2);
                producers.push(pc);
            }
            0x36..=0x4e | 0x57 | 0xb3 | 0xc2 | 0xc3 | 0x99..=0x9e | 0xc6 | 0xc7 => {
                if let Some(slot) = store_local(code, pc) {
                    let cat2 = matches!(op, 0x37 | 0x39 | 0x3f..=0x42 | 0x47..=0x4a);
                    if slot == iv || slot == iv + 1 || (cat2 && slot + 1 == iv) {
                        iv_written = true;
                    }
                }
                let n = producers.len();
                if n < 1 {
                    break;
                }
                producers.truncate(n - 1);
            }
            0xb5 | 0x9f..=0xa6 => {
                let n = producers.len();
                if n < 2 {
                    break;
                }
                producers.truncate(n - 2);
            }
            0x59 => match producers.last().copied() {
                Some(t) => producers.push(t),
                None => break,
            },
            0x00 => {}
            // `iinc` on `iv`'s slots would be a verifier error for a `long`;
            // treat it as a write anyway.
            0x84 => {
                let Some(&idx) = code.get(pc + 1) else {
                    break;
                };
                let l = idx as usize;
                if l == iv || l == iv + 1 {
                    iv_written = true;                }            }
            _ => break,
        }
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            break;
        }
        pc += len;
    }
    out
}

/// BCE21-4's single-pass arm: the access pcs of loop `(header, back_edge)`
/// whose `a[(int) i]` is in bounds on every execution (the section notes
/// above give the four conditions and the argument). Empty when any condition
/// fails.
#[allow(clippy::too_many_arguments)]
fn long_induction_static_pcs(
    code: &[u8],
    code_len: usize,
    tables: &BceMethodTables,
    loop_tables: &LoopMethodTables<'_>,
    provenance: &mut FxHashMap<usize, Option<usize>>,
    header: usize,
    back_edge: usize,
    handlers: Option<&[(usize, usize, usize)]>,
) -> Vec<usize> {
    let code_len = code_len.min(code.len());
    if header >= code_len || back_edge >= code_len {
        return Vec::new();
    }
    let back_edge_end = back_edge + bytecode_analysis::step(code, back_edge);
    if back_edge_end > code_len || header >= back_edge_end {
        return Vec::new();
    }
    let Some((iv, limit, test_pc)) =
        locate_long_exit_test(code, code_len, tables, header, back_edge, back_edge_end)
    else {
        return Vec::new();
    };
    let test = LongExitTest {
        iv,
        limit_array: match limit {
            Ok(a) => a,
            Err(n) => {
                match *provenance
                    .entry(n)
                    .or_insert_with(|| bound_arraylength_provenance_with(code, code_len, n, handlers))
                {
                    Some(a) => a,
                    None => return Vec::new(),
                }
            }
        },
        test_pc,
    };
    let Some(unit) = loop_tables.long_unit_stride_iv(header, back_edge, test.iv, test.test_pc)
    else {
        return Vec::new();
    };
    let Some(modified) = crate::loop_analysis::modified_locals_strict(code, header, back_edge_end)
    else {
        return Vec::new();
    };
    let a = test.limit_array;
    if a >= 64 || modified & (1u64 << a) != 0 {
        return Vec::new();
    }
    long_index_accesses(code, header, back_edge_end, test.iv, &unit.handler_pcs)
        .into_iter()
        .filter(|&(_, arr)| arr == a)
        .map(|(pc, _)| pc)
        .collect()
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
pub(crate) fn range_bce_enabled() -> bool {
    if let Some(v) = RANGE_BCE_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    // Read once per bounds analysis, no `static` (j27 LATCHES).
    false
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
/// The `c - 1` / `c + 1` endpoints are computed in `i64`, so `c == i32::MIN`
/// (`x < Integer.MIN_VALUE`, impossible) collapses to bottom rather than
/// wrapping to a range that admits everything: [`Range::exact`] raises `lo`
/// only to the width's MINIMUM and never lowers it to the maximum, so
/// `[MIN, MIN-1]` and `[MAX+1, MAX]` both arrive with `lo > hi` and normalise
/// to bottom.
///
/// # Why the endpoints are checked anyway
///
/// `c` reaches here from [`range_const_push`], which stops at `sipush` and so
/// caps `|c|` at `i16::MAX`. That is the one place its exclusion of `ldc`
/// ("this module carries no constant pool") is load-bearing rather than a
/// limitation, and at that cap an `i64` endpoint cannot overflow.
///
/// But that is a premise about a DIFFERENT function, and it is the kind that
/// stops holding the day someone widens the operand repertoire — which that
/// function's own comment names as the obvious next extension. With a 64-bit
/// `c`, `c + 1` at `i64::MAX` wraps to `i64::MIN`, and `Range::exact(w,
/// i64::MIN, max)` is TOP: an interval admitting every value, handed out as
/// the consequence of a comparison that admits none. `checked_add` /
/// `checked_sub` answer [`Range::empty`] instead, which costs an elision
/// rather than permitting an unchecked index.
fn narrow_by(r: Range, rel: Rel, c: i64) -> Range {
    let w = IntWidth::W32;
    // Widening: the i32 endpoints into the i64 interval domain.
    let (min, max) = (i32::MIN as i64, i32::MAX as i64);
    let bound = match rel {
        Rel::Eq => return r.meet(Range::constant(w, c)),
        // An interval lattice cannot express "everything but c".
        Rel::Ne => return r,
        Rel::Lt => match c.checked_sub(1) {
            Some(hi) => Range::exact(w, min, hi),
            None => Range::empty(w),
        },
        Rel::Le => Range::exact(w, min, c),
        Rel::Gt => match c.checked_add(1) {
            Some(lo) => Range::exact(w, lo, max),
            None => Range::empty(w),
        },
        Rel::Ge => Range::exact(w, c, max),    };
    r.meet(bound)}

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
            *t.prev.get(value_pc)?        }
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
                // `definitely_non_negative`, which is `!is_empty() &&
                // is_non_negative()` written once and named. This site used to
                // spell the conjunction out, and spelling it out is what made
                // `Range::is_non_negative` survive as a predicate whose vacuous
                // answer at bottom (every claim holds of a value that does not
                // exist) reads as permission to delete a bounds check. There is
                // no vacuous form left to call.
                if idx_range.definitely_non_negative() && st.proves_lt_len(index_local, array_local)
                {
                    safe.insert(pc);
                }
            }
        }
        pc += 1;
    }
    safe
}

#[cfg(test)]
mod induction_variable_backward_read_tests;

/// The whole-method provenance proofs must not read "one store" as "one
/// dominating store" when the local is a PARAMETER.
#[cfg(test)]
mod parameter_bypass_provenance_tests;

#[cfg(test)]
mod ambiguous_local_cfg_exactness_tests;

#[cfg(test)]
mod range_analysis_bce_tests;

#[cfg(test)]
mod range_bce_tests;

#[cfg(test)]
mod iinc_index_displacement_tests;

#[cfg(test)]
mod iinc_before_access_is_not_elided_tests;

#[cfg(test)]
mod narrow_by_endpoint_tests;

#[cfg(test)]
mod unnamed_wide_value_gate_tests;

#[cfg(test)]
mod r11_x64loop_outer_advance_before_inner_tests;

#[cfg(test)]
mod r11_x64loop_guard_cover_tests;

#[cfg(test)]
mod r11_x64loop_ternary_iv_update_tests;

#[cfg(test)]
mod r11w2_x64loop_exit_test_join_tests;

#[cfg(test)]
mod unread_parameter_kind_tests;

// ── Round 13 wave 3, lane loopcall: field-array bounds-check elimination ─────
//
// `FieldHoistAlias`: an X2 array-field hoist (and an `int` field hoist) makes
// every `aload R; getfield F` site of its loop a slot load, named here by a
// synthetic local at or above `max_locals`. These pin what the analysis may
// and may not conclude from one.
#[cfg(test)]
mod r13_loopcall_field_bce_tests;



// BCE21-4's single-pass arm (t1 wave 8, lane BCE,
// `CRATONVM_JIT_LONG_INDUCTION_BCE`): `a[(int) i]` under a `long` counted
// loop, proved statically.

