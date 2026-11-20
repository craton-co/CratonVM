// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A type for every operand-stack entry, at every bci.
//!
//! # Why this exists
//!
//! A deopt snapshot has to describe the operand stack well enough for the
//! interpreter to rebuild it. Locals have a width source (`local_kinds` plus
//! the per-bci reaching-kind refinement); the stack had none. So
//! `build_and_record_deopt_point` fell back to a per-METHOD gate: if the method
//! touches a `long`/`float`/`double` anywhere (`uses_long_float_double`), every
//! non-oop stack entry is recorded `FrameValue::Unsupported`, because an 8-byte
//! frame slot holding a cat-1 `int` and one holding a cat-2 `long` are
//! indistinguishable and guessing wrong truncates a `long` on resume.
//!
//! That is sound and very coarse, and it had a consequence out of all
//! proportion to its cost: `CompiledMethod::osr_exit_policy` refuses OSR entry
//! for the WHOLE artifact when any one deopt point is unresumable, so a single
//! `Unsupported` stack entry — at a call-site guard anywhere in the method, even
//! outside the loop being entered — disabled OSR for every loop in it. See
//! `osr-entry-unresumable-exit-FIXED-20260803.md`.
//!
//! # What it is
//!
//! A forward abstract interpretation over the bytecode that tracks the KIND of
//! each entry of the same compact operand stack the backend simulates — one
//! entry per value, cat-2 included, which is what makes the result
//! index-alignable with `Compiler::stack`.
//!
//! # The safety argument, in four parts
//!
//! A wrong kind here is a truncated `long` on resume — silent wrong values, the
//! exact failure the coarse fallback existed to prevent. So:
//!
//! 1. **It may only ever upgrade.** The consult site asks this analysis only
//!    where it would otherwise emit `Unsupported`. Every encoding the old code
//!    produced, it still produces.
//! 2. **`Unknown` is not a guess.** An opcode whose stack DEPTH effect is known
//!    but whose result type is not (`ldc` of an int-or-float constant,
//!    `ldc2_w` of a long-or-double) pushes `Unknown`, which the consult site
//!    treats exactly as today: `Unsupported`. Only depth ambiguity poisons.
//! 3. **Poison is total.** An unmodelled opcode, a `jsr`/`ret`, a
//!    category-dependent `pop2`/`dup2` over an `Unknown` top, or a merge of two
//!    different depths abandons the state: no answer for that pc, nor for
//!    anything reached only through it, rather than a plausible one. (A pc
//!    downstream that ANOTHER path had already typed keeps that path's answer;
//!    the poisoned pc never re-publishes. For verified bytecode the paths agree
//!    there, since JVMS 4.10.1 merges agree on depth and on every primitive
//!    kind, and part 4's depth + ref-ness check is the backstop otherwise.)
//! 4. **The consumer re-checks.** `build_and_record_deopt_point` uses the
//!    vector only when its length equals the emitter's live stack depth AND
//!    every entry's ref-ness agrees with the emitter's own oop mark — an
//!    independent per-entry opinion maintained for the GC. A modelling error
//!    that shifts the stack cannot survive both.
//!
//! Depth agreement is the load-bearing check, and it is genuinely independent:
//! this analysis derives depth from the JVMS stack effects, the emitter derives
//! it from executing its own opcode handlers.

use super::*;

/// The kind of one compact operand-stack entry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum StackKind {
    /// Depth is known, type is not — indistinguishable from today's fallback.
    Unknown,
    Int,
    Long,
    Float,
    Double,
    Ref,
}

impl StackKind {
    /// The kind a value of JVM descriptor byte `tag` has on the stack.
    /// `V` and anything unrecognised answer `Unknown`.
    fn from_descriptor_byte(tag: u8) -> StackKind {
        match tag {
            b'I' | b'B' | b'C' | b'S' | b'Z' => StackKind::Int,
            b'J' => StackKind::Long,
            b'F' => StackKind::Float,
            b'D' => StackKind::Double,
            b'L' | b'[' => StackKind::Ref,
            _ => StackKind::Unknown,
        }
    }

    /// JVM category: `true` for `long`/`double`. `None` when unknown — the
    /// caller must poison rather than guess, because the category decides how
    /// many entries `pop2`/`dup2` touch.
    pub(crate) fn is_category_2(self) -> Option<bool> {
        match self {
            StackKind::Long | StackKind::Double => Some(true),
            StackKind::Int | StackKind::Float | StackKind::Ref => Some(false),
            StackKind::Unknown => None,
        }
    }

    /// Whether this kind is an object reference. `Unknown` answers `false`,
    /// so callers that cross-check against the emitter's oop marks must gate
    /// on [`Self::is_category_2`] answering first — an `Unknown` entry has no
    /// opinion about ref-ness either.
    pub(crate) fn is_ref(self) -> bool {
        matches!(self, StackKind::Ref)
    }
}

/// Per-bci operand-stack kinds. Absent = no answer (unreached or poisoned).
#[derive(Default)]
pub(crate) struct StackKindMap {
    at: FxHashMap<usize, Vec<StackKind>>,
}

impl StackKindMap {
    pub(crate) fn get(&self, bci: usize) -> Option<&[StackKind]> {
        self.at.get(&bci).map(|v| v.as_slice())
    }

    /// How many pcs the analysis answered for. `0` means it declined entirely,
    /// which is the first thing to check when a snapshot that should have been
    /// typed stayed `Unsupported`.
    pub(crate) fn answered(&self) -> usize {
        self.at.len()
    }

    // The test-only constructor (`from_entries_for_test`) lives in the bottom
    // test module: a test gate this high up used to end the panic-free
    // ratchet's scan of this file here, leaving the analysis below unchecked.
}

/// Everything the analysis needs beyond the bytecode: the per-pc metadata the
/// compiler already resolved. Borrowed, so the analysis allocates nothing but
/// its own state.
pub(crate) struct StackKindInputs<'a> {
    /// `pc -> field type tag` for `getfield`/`putfield`.
    pub(crate) field_types: FxHashMap<usize, u8>,
    /// `pc -> field type tag` for `getstatic`/`putstatic`.
    pub(crate) static_types: FxHashMap<usize, u8>,
    /// `pc -> (arg slot count, return type tag)` for every call site, however
    /// it is dispatched. Receiver NOT included — the opcode says whether there
    /// is one.
    pub(crate) calls: FxHashMap<usize, (usize, u8)>,
    /// `pc`s whose `ldc` pushes a reference (String / Class).
    pub(crate) ldc_refs: &'a FxHashSet<usize>,
    /// `pc`s in the `ldc` family whose constant is floating-point: a
    /// `CONSTANT_Float` for `ldc`/`ldc_w`, a `CONSTANT_Double` for `ldc2_w`.
    ///
    /// The two opcode families have disjoint pcs and the opcode at the pc says
    /// which is which, so one set types both: an `ldc` pc present is `Float`
    /// and absent is `Int`; an `ldc2_w` pc present is `Double` and absent is
    /// `Long`. "Absent" is only allowed to mean "the other one" for a pc the
    /// resolver actually answered — see [`Self::ldc_resolved`].
    pub(crate) ldc_fp: &'a FxHashSet<usize>,
    /// `pc`s in the `ldc` family the constant-pool resolver reduced to an
    /// immediate. Without this, a pc the resolver never saw (no resolver wired
    /// at all, or a site it declined) would read as "absent from `ldc_fp`,
    /// therefore `Int`" — a guess, which rule 2 of this module's safety
    /// argument forbids. A pc that is not here stays `Unknown`.
    pub(crate) ldc_resolved: &'a FxHashSet<usize>,
    /// Exception-table `handler_pc`s to seed as extra ENTRY points, each with
    /// the JVMS §2.10 handler-entry stack: exactly one reference, the
    /// throwable.
    ///
    /// An exception edge is a predecessor no branch instruction names, so
    /// without this a handler body is unreached, the analysis has no state
    /// there, and every deopt point inside one falls back to `Unsupported` —
    /// which, because `osr_exit_policy` is artifact-wide, costs the WHOLE
    /// method its OSR entry. That is not hypothetical: it is what a
    /// `catch (E e) { g(-1, x); }` inside a method that also touches a `long`
    /// does, and `HttpHeaderValidationUtilTest`'s exhaustive loops are exactly
    /// that shape.
    ///
    /// Seeding is not a guess. JVMS fixes the handler-entry stack completely,
    /// and a handler also reachable by ordinary control flow at a different
    /// depth still poisons through the ordinary merge. Empty for every caller
    /// that emits no handler bodies, and byte-identical there.
    pub(crate) handler_pcs: &'a [usize],
}

/// Run the analysis. `None` results are normal: an unmodelled construct poisons
/// its successors rather than answering.
pub(crate) fn analyze(code: &[u8], code_len: usize, inputs: &StackKindInputs<'_>) -> StackKindMap {
    let mut map = StackKindMap::default();
    if code_len == 0 || code_len > code.len() {
        return map;
    }

    // `in_state[pc]`: Some(stack) once a predecessor has published one.
    // `poisoned[pc]`: this pc's state is unusable and must not be re-seeded —
    // once we have lost the depth we cannot re-derive it from a later edge.
    let mut in_state: Vec<Option<Vec<StackKind>>> = vec![None; code_len];
    let mut poisoned: Vec<bool> = vec![false; code_len];
    let mut work: Vec<usize> = vec![0];
    in_state[0] = Some(Vec::new());
    // Handler entries are entry points too — see `StackKindInputs::handler_pcs`.
    // Seeded before the walk so the JVMS state is what any later merge is
    // merged AGAINST, rather than something a fall-through path gets to define
    // first.
    for &handler_pc in inputs.handler_pcs {
        if handler_pc >= code_len || in_state[handler_pc].is_some() {
            continue;
        }
        in_state[handler_pc] = Some(vec![StackKind::Ref]);
        work.push(handler_pc);
    }

    // Bounded: each pc can be re-queued only when its state actually changed,
    // and the merge is monotone downward (equal -> keep, differ -> Unknown,
    // depth differs -> poison), so a pc can change at most `depth + 1` times.
    let mut steps = 0usize;
    let step_budget = code_len.saturating_mul(64).max(4096);

    while let Some(pc) = work.pop() {
        steps += 1;
        if steps > step_budget {
            // A malformed or pathological method. Abandon the whole result
            // rather than keep a partially-refined one: a pc whose merge had
            // not converged could still be holding a state from ONE path, and
            // "less conservative than the truth" is exactly the shape of
            // answer this analysis must never give.
            return StackKindMap::default();
        }
        if pc >= code_len || poisoned[pc] {
            continue;
        }
        let Some(state) = in_state[pc].clone() else {
            continue;
        };

        let op = code[pc];
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            map.at.insert(pc, state);
            continue;
        }
        let next = pc + len;

        // Apply this opcode's typed stack effect. `None` = poison. Computed
        // BEFORE the state is published so the map can take `state` by value:
        // this loop is on every compile's path, and the published copy was a
        // second clone of the same vector per visit.
        let after = transfer(code, pc, op, &state, inputs);
        map.at.insert(pc, state);

        // `publish(target, state)`: merge `state` into `target`'s IN state and
        // queue it if that changed anything.
        let mut publish = |target: usize,
                           s: &[StackKind],
                           in_state: &mut Vec<Option<Vec<StackKind>>>,
                           poisoned: &mut Vec<bool>,
                           work: &mut Vec<usize>| {
            if target >= code_len || poisoned[target] {
                return;
            }
            match &mut in_state[target] {
                None => {
                    in_state[target] = Some(s.to_vec());
                    work.push(target);
                }
                Some(existing) => {
                    if existing.len() != s.len() {
                        // Two paths disagree about DEPTH. Not a merge we can
                        // represent, and not one we may guess at.
                        poisoned[target] = true;
                        in_state[target] = None;
                        return;
                    }
                    let mut changed = false;
                    for (e, n) in existing.iter_mut().zip(s.iter()) {
                        if *e != *n && *e != StackKind::Unknown {
                            *e = StackKind::Unknown;
                            changed = true;
                        }
                    }
                    if changed {
                        work.push(target);
                    }
                }
            }
        };

        let Some(after) = after else {
            // Poison every successor: we no longer know the depth.
            for t in successors(code, pc, op, len, code_len) {
                if t < code_len {
                    poisoned[t] = true;
                    in_state[t] = None;                }            }
            continue;
        };

        match op {
            // Unconditional transfers and terminators: no fall-through.
            0xa7 => {
                // goto
                if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
            }
            0xc8 => {
                // goto_w
                if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
            }
            0xac..=0xb1 | 0xbf => {} // returns / athrow — path ends
            // Conditional branches: both edges.
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
                publish(next, &after, &mut in_state, &mut poisoned, &mut work);
            }
            // Switches: every target plus default, no fall-through.
            0xaa | 0xab => {
                for t in bytecode_analysis::switch_targets_lenient(code, code_len, pc) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
            }
            _ => publish(next, &after, &mut in_state, &mut poisoned, &mut work),
        }
    }

    // A pc poisoned AFTER it was first published still has that first answer
    // in the map. Drop those: poison means two paths disagree about the depth
    // there, and the published one is only one path's view.
    for (pc, dead) in poisoned.iter().enumerate() {
        if *dead {
            map.at.remove(&pc);
        }
    }
    map
}

/// Successors used only for poison propagation (a superset is fine there):
/// [`bytecode_analysis::lenient_successors`], plus the textual fall-through of
/// `ret` and `wide ret`, which this walk has always poisoned (the lenient view
/// gives both no successors at all).
fn successors(code: &[u8], pc: usize, op: u8, len: usize, code_len: usize) -> Vec<usize> {
    let mut out = bytecode_analysis::lenient_successors(code, code_len, pc);
    if op == 0xa9 || bytecode_analysis::is_wide_ret(code, pc) {
        out.push(pc + len);
    }
    out
}

/// The typed stack effect of one opcode. `None` = poison (depth unknown).
///
/// Every arm is a direct transcription of the JVMS "Operand Stack" clause for
/// that instruction, in COMPACT slots: a cat-2 value is one entry.
fn transfer(
    code: &[u8],
    pc: usize,
    op: u8,
    state: &[StackKind],
    inputs: &StackKindInputs<'_>,
) -> Option<Vec<StackKind>> {
    let mut s = state.to_vec();
    // Pop `n`, failing (poison) on underflow — an underflow means this
    // analysis and the bytecode disagree, so it must not answer.
    macro_rules! pop {
        ($n:expr) => {{
            let n = $n;
            if s.len() < n {
                return None;
            }
            s.truncate(s.len() - n);
        }};
    }
    macro_rules! push {
        ($k:expr) => {
            s.push($k)
        };
    }
    macro_rules! replace {
        ($n:expr, $k:expr) => {{
            pop!($n);
            push!($k);
        }};
    }

    match op {
        0x00 => {}                               // nop
        0x01 => push!(StackKind::Ref),           // aconst_null
        0x02..=0x08 => push!(StackKind::Int),    // iconst_m1..iconst_5
        0x09 | 0x0a => push!(StackKind::Long),   // lconst_0/1
        0x0b..=0x0d => push!(StackKind::Float),  // fconst_0..2
        0x0e | 0x0f => push!(StackKind::Double), // dconst_0/1
        0x10 | 0x11 => push!(StackKind::Int),    // bipush / sipush
        0x12 | 0x13 => {
            // ldc / ldc_w: a String or Class literal is a ref; a numeric
            // literal is `CONSTANT_Integer` or `CONSTANT_Float`, and the
            // constant-pool tag the resolver already read says which.
            push!(if inputs.ldc_refs.contains(&pc) {
                StackKind::Ref
            } else if !inputs.ldc_resolved.contains(&pc) {
                StackKind::Unknown
            } else if inputs.ldc_fp.contains(&pc) {
                StackKind::Float
            } else {
                StackKind::Int
            });
        }
        0x14 => {
            // ldc2_w — `CONSTANT_Long` or `CONSTANT_Double`, one compact entry
            // either way, so only the KIND was ever in doubt.
            push!(if !inputs.ldc_resolved.contains(&pc) {
                StackKind::Unknown
            } else if inputs.ldc_fp.contains(&pc) {
                StackKind::Double
            } else {
                StackKind::Long
            });
        }
        0x15 => push!(StackKind::Int),              // iload
        0x16 => push!(StackKind::Long),             // lload
        0x17 => push!(StackKind::Float),            // fload
        0x18 => push!(StackKind::Double),           // dload
        0x19 => push!(StackKind::Ref),              // aload
        0x1a..=0x1d => push!(StackKind::Int),       // iload_0..3
        0x1e..=0x21 => push!(StackKind::Long),      // lload_0..3
        0x22..=0x25 => push!(StackKind::Float),     // fload_0..3
        0x26..=0x29 => push!(StackKind::Double),    // dload_0..3
        0x2a..=0x2d => push!(StackKind::Ref),       // aload_0..3
        0x2e => replace!(2, StackKind::Int),        // iaload
        0x2f => replace!(2, StackKind::Long),       // laload
        0x30 => replace!(2, StackKind::Float),      // faload
        0x31 => replace!(2, StackKind::Double),     // daload
        0x32 => replace!(2, StackKind::Ref),        // aaload
        0x33..=0x35 => replace!(2, StackKind::Int), // baload / caload / saload
        0x36..=0x3a => pop!(1),                     // istore/lstore/fstore/dstore/astore
        0x3b..=0x4e => pop!(1),                     // *store_0..3
        0x4f..=0x56 => pop!(3),                     // *astore
        0x57 => pop!(1),                            // pop
        0x58 => {
            // pop2: one cat-2 entry, or two cat-1 entries.
            let top = *s.last()?;
            match top.is_category_2()? {
                true => pop!(1),
                false => pop!(2),
            }
        }
        // `dup`, `dup_x1` and `swap` have ONE form each: JVMS admits only
        // category-1 operands, so their compact stack effect does not depend
        // on an operand's category. An `Unknown` operand (an unresolved `ldc`)
        // therefore leaves the DEPTH perfectly known and must not poison —
        // rule 3 of the module doc poisons on depth ambiguity only, and
        // poisoning here used to cost every pc downstream of the shuffle its
        // answer (and with it the `dup2`/`pop2` width oracle). A KNOWN
        // category-2 operand is malformed bytecode and still poisons.
        0x59 => {
            // dup — cat-1 only per JVMS.
            let top = *s.last()?;
            if top.is_category_2() == Some(true) {
                return None;
            }
            push!(top);
        }
        0x5a => {
            // dup_x1 — both entries cat-1.
            if s.len() < 2 {
                return None;
            }
            let top = s[s.len() - 1];
            let below = s[s.len() - 2];
            if top.is_category_2() == Some(true) || below.is_category_2() == Some(true) {
                return None;
            }
            s.insert(s.len() - 2, top);
        }
        0x5f => {
            // swap — both cat-1.
            if s.len() < 2 {
                return None;
            }
            let top = s[s.len() - 1];
            let below = s[s.len() - 2];
            if top.is_category_2() == Some(true) || below.is_category_2() == Some(true) {
                return None;
            }
            let n = s.len();
            s.swap(n - 1, n - 2);
        }
        0x5b => {
            // dup_x2 — FORM 1 `[v3, v2, v1] -> [v1, v3, v2, v1]` (all cat-1,
            // three entries) vs FORM 2 `[v2, v1] -> [v1, v2, v1]` (v2 cat-2,
            // two entries). v1 is cat-1 in both forms, so only a KNOWN
            // category-2 top poisons (see `dup` above); the entry below
            // decides how deep the copy is inserted and must be known.
            if s.len() < 2 {
                return None;
            }
            let top = s[s.len() - 1];
            if top.is_category_2() == Some(true) {
                return None; // not a legal dup_x2 shape
            }
            let below = s[s.len() - 2];
            let depth = if below.is_category_2()? { 2 } else { 3 };
            if s.len() < depth {
                return None;
            }
            let at = s.len() - depth;
            s.insert(at, top);
        }
        0x5c => {
            // dup2 — FORM 1 `[v2, v1] -> [v2, v1, v2, v1]` (both cat-1) vs
            // FORM 2 `[v] -> [v, v]` (v cat-2, one entry — structurally `dup`).
            let top = *s.last()?;
            if top.is_category_2()? {
                push!(top);
            } else {
                if s.len() < 2 {
                    return None;
                }
                let below = s[s.len() - 2];
                if below.is_category_2()? {
                    return None; // no legal dup2 form has cat-1 over cat-2
                }
                push!(below);
                push!(top);
            }
        }
        0x5d => {
            // dup2_x1 — FORM 1 `[v3, v2, v1] -> [v2, v1, v3, v2, v1]` (all
            // cat-1) vs FORM 2 `[v2, v1] -> [v1, v2, v1]` (v1 cat-2, v2 cat-1).
            let top = *s.last()?;
            if s.len() < 2 {
                return None;
            }
            if top.is_category_2()? {
                // FORM 2: two entries; JVMS requires v2 cat-1.
                if s[s.len() - 2].is_category_2()? {
                    return None;
                }
                let at = s.len() - 2;
                s.insert(at, top);
            } else {
                // FORM 1: three cat-1 entries.
                if s.len() < 3 {
                    return None;
                }
                let v2 = s[s.len() - 2];
                let v3 = s[s.len() - 3];
                if v2.is_category_2()? || v3.is_category_2()? {
                    return None;
                }
                let at = s.len() - 3;
                s.insert(at, top);
                s.insert(at, v2);
            }
        }
        0x5e => {
            // dup2_x2 — the four JVMS forms, in COMPACT entries. The top's
            // category says how many entries are duplicated (1 for a cat-2
            // top, 2 for a cat-1 pair); the next entry down says how deep the
            // copy is inserted, because four JVM *slots* is either one cat-2
            // entry or two cat-1 entries.
            //
            //   FORM 4  v1,v2 cat-2      [v2, v1]         -> [v1, v2, v1]
            //   FORM 2  v1 cat-2         [v3, v2, v1]     -> [v1, v3, v2, v1]
            //   FORM 3  v3 cat-2         [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
            //   FORM 1  all cat-1        [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
            if s.len() < 2 {
                return None;
            }
            let v1 = s[s.len() - 1];
            let v2 = s[s.len() - 2];
            if v1.is_category_2()? {
                // FORM 4 (v2 cat-2, two entries) or FORM 2 (v2/v3 cat-1,
                // three entries).
                let depth = if v2.is_category_2()? { 2 } else { 3 };
                if s.len() < depth {
                    return None;
                }
                if depth == 3 && s[s.len() - 3].is_category_2()? {
                    return None; // FORM 2 requires v3 cat-1
                }
                let at = s.len() - depth;
                s.insert(at, v1);
            } else {
                // FORM 1 or FORM 3 — two cat-1 entries duplicated. v2 is cat-1
                // in both.
                if v2.is_category_2()? {
                    return None;
                }
                if s.len() < 3 {
                    return None;
                }
                let depth = if s[s.len() - 3].is_category_2()? {
                    3
                } else {
                    4
                };
                if s.len() < depth {
                    return None;
                }
                if depth == 4 && s[s.len() - 4].is_category_2()? {
                    return None; // FORM 1 requires v4 cat-1
                }
                let at = s.len() - depth;
                s.insert(at, v1);
                s.insert(at, v2);
            }
        }
        // Arithmetic. Operand counts are JVMS; result kinds are the opcode's.
        0x60 | 0x64 | 0x68 | 0x6c | 0x70 => replace!(2, StackKind::Int), // i add/sub/mul/div/rem
        0x61 | 0x65 | 0x69 | 0x6d | 0x71 => replace!(2, StackKind::Long), // l add/sub/mul/div/rem
        0x62 | 0x66 | 0x6a | 0x6e | 0x72 => replace!(2, StackKind::Float), // f
        0x63 | 0x67 | 0x6b | 0x6f | 0x73 => replace!(2, StackKind::Double), // d
        0x74 => replace!(1, StackKind::Int),                             // ineg
        0x75 => replace!(1, StackKind::Long),                            // lneg
        0x76 => replace!(1, StackKind::Float),                           // fneg
        0x77 => replace!(1, StackKind::Double),                          // dneg
        0x78 | 0x7a | 0x7c => replace!(2, StackKind::Int),               // ishl/ishr/iushr
        0x79 | 0x7b | 0x7d => replace!(2, StackKind::Long),              // lshl/lshr/lushr
        0x7e | 0x80 | 0x82 => replace!(2, StackKind::Int),               // iand/ior/ixor
        0x7f | 0x81 | 0x83 => replace!(2, StackKind::Long),              // land/lor/lxor
        0x84 => {}                                                       // iinc
        0x85 => replace!(1, StackKind::Long),                            // i2l
        0x86 => replace!(1, StackKind::Float),                           // i2f
        0x87 => replace!(1, StackKind::Double),                          // i2d
        0x88 => replace!(1, StackKind::Int),                             // l2i
        0x89 => replace!(1, StackKind::Float),                           // l2f
        0x8a => replace!(1, StackKind::Double),                          // l2d
        0x8b => replace!(1, StackKind::Int),                             // f2i
        0x8c => replace!(1, StackKind::Long),                            // f2l
        0x8d => replace!(1, StackKind::Double),                          // f2d
        0x8e => replace!(1, StackKind::Int),                             // d2i
        0x8f => replace!(1, StackKind::Long),                            // d2l
        0x90 => replace!(1, StackKind::Float),                           // d2f
        0x91..=0x93 => replace!(1, StackKind::Int),                      // i2b/i2c/i2s
        0x94..=0x98 => replace!(2, StackKind::Int),                      // lcmp / f/d cmp
        0x99..=0x9e => pop!(1),                                          // if<cond>
        0x9f..=0xa4 => pop!(2),                                          // if_icmp<cond>
        0xa5 | 0xa6 => pop!(2),                                          // if_acmp<cond>
        0xa7 | 0xc8 => {}                                                // goto / goto_w
        0xa8 | 0xa9 | 0xc9 => return None,                               // jsr / ret / jsr_w
        0xaa | 0xab => pop!(1),                                          // switches
        0xac | 0xae | 0xb0 => pop!(1),                                   // ireturn/freturn/areturn
        0xad | 0xaf => pop!(1),                                          // lreturn/dreturn
        0xb1 => {}                                                       // return
        0xb2 => push!(StackKind::from_descriptor_byte(
            *inputs.static_types.get(&pc)?
        )), // getstatic
        0xb3 => pop!(1),                                                 // putstatic
        0xb4 => replace!(
            1,
            StackKind::from_descriptor_byte(*inputs.field_types.get(&pc)?)
        ), // getfield
        0xb5 => pop!(2),                                                 // putfield
        0xb6..=0xba => {
            let (args, ret) = *inputs.calls.get(&pc)?;
            let receiver = usize::from(op != 0xb8 && op != 0xba);
            pop!(args + receiver);
            if ret != b'V' {
                push!(StackKind::from_descriptor_byte(ret));
            }
        }
        0xbb => push!(StackKind::Ref),              // new
        0xbc | 0xbd => replace!(1, StackKind::Ref), // newarray / anewarray
        0xbe => replace!(1, StackKind::Int),        // arraylength
        0xbf => pop!(1),                            // athrow
        0xc0 => {
            // checkcast — leaves the ref in place (and proves it is one).
            if s.is_empty() {
                return None;
            }
            let n = s.len();
            s[n - 1] = StackKind::Ref;
        }
        0xc1 => replace!(1, StackKind::Int), // instanceof
        0xc2 | 0xc3 => pop!(1),              // monitorenter / monitorexit
        0xc4 => {
            // wide: the modified opcode decides.
            let modified = *code.get(pc + 1)?;
            match modified {
                0x15 => push!(StackKind::Int),    // wide iload
                0x16 => push!(StackKind::Long),   // wide lload
                0x17 => push!(StackKind::Float),  // wide fload
                0x18 => push!(StackKind::Double), // wide dload
                0x19 => push!(StackKind::Ref),    // wide aload
                0x36..=0x3a => pop!(1),           // wide *store
                0x84 => {}                        // wide iinc
                _ => return None,                 // wide ret, anything else
            }
        }
        0xc5 => {
            // multianewarray: pops `dimensions` counts, pushes the array ref.
            let dims = usize::from(*code.get(pc + 3)?);
            pop!(dims);
            push!(StackKind::Ref);
        }
        0xc6 | 0xc7 => pop!(1), // ifnull / ifnonnull
        _ => return None,       // reserved / unknown    
}
    Some(s)}

#[cfg(test)]
mod tests;
