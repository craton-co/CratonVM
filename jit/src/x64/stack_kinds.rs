// SPDX-License-Identifier: Apache-2.0
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
//!    different depths abandons the state: no answer for that pc or anything
//!    downstream of it, rather than a plausible one.
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
        map.at.insert(pc, state.clone());

        let op = code[pc];
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            continue;
        }
        let next = pc + len;

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

        // Apply this opcode's typed stack effect. `None` = poison.
        let after = transfer(code, pc, op, &state, inputs);
        let Some(after) = after else {
            // Poison every successor: we no longer know the depth.
            for t in successors(code, pc, op, len, code_len) {
                if t < code_len {
                    poisoned[t] = true;
                    in_state[t] = None;
                }
            }
            continue;
        };

        match op {
            // Unconditional transfers and terminators: no fall-through.
            0xa7 => {
                // goto
                if let Some(t) = branch_target(code, pc, 2) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
            }
            0xc8 => {
                // goto_w
                if let Some(t) = branch_target_wide(code, pc) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
            }
            0xac..=0xb1 | 0xbf => {} // returns / athrow — path ends
            // Conditional branches: both edges.
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                if let Some(t) = branch_target(code, pc, 2) {
                    publish(t, &after, &mut in_state, &mut poisoned, &mut work);
                }
                publish(next, &after, &mut in_state, &mut poisoned, &mut work);
            }
            // Switches: every target plus default, no fall-through.
            0xaa | 0xab => {
                for t in switch_targets(code, pc, op, code_len) {
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

/// Successors used only for poison propagation (a superset is fine there).
fn successors(code: &[u8], pc: usize, op: u8, len: usize, code_len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    match op {
        0xa7 => out.extend(branch_target(code, pc, 2)),
        0xc8 => out.extend(branch_target_wide(code, pc)),
        0xac..=0xb1 | 0xbf => {}
        0x99..=0xa6 | 0xc6 | 0xc7 => {
            out.extend(branch_target(code, pc, 2));
            out.push(pc + len);
        }
        0xaa | 0xab => out.extend(switch_targets(code, pc, op, code_len)),
        _ => out.push(pc + len),
    }
    out
}

fn branch_target(code: &[u8], pc: usize, off_at: usize) -> Option<usize> {
    let hi = *code.get(pc + off_at - 1)?;
    let lo = *code.get(pc + off_at)?;
    let off = i16::from_be_bytes([hi, lo]) as i32; // Widening: always safe
    pc.checked_add_signed(off as isize) // Cast: branch offset for pc arithmetic
}

fn branch_target_wide(code: &[u8], pc: usize) -> Option<usize> {
    let b = code.get(pc + 1..pc + 5)?;
    let off = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    pc.checked_add_signed(off as isize) // Cast: branch offset for pc arithmetic
}

fn switch_targets(code: &[u8], pc: usize, op: u8, code_len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut p = pc + 1;
    while p % 4 != 0 {
        p += 1;
    }
    let read_i32 = |at: usize| -> Option<i32> {
        let b = code.get(at..at + 4)?;
        Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let Some(default) = read_i32(p) else {
        return out;
    };
    if let Some(t) = pc.checked_add_signed(default as isize) {
        // Cast: switch offset for pc arithmetic
        out.push(t);
    }
    if op == 0xaa {
        let (Some(low), Some(high)) = (read_i32(p + 4), read_i32(p + 8)) else {
            return out;
        };
        let n = (high as i64 - low as i64 + 1).max(0) as usize; // Widening then clamp
        for k in 0..n {
            let Some(off) = read_i32(p + 12 + k * 4) else {
                break;
            };
            if let Some(t) = pc.checked_add_signed(off as isize) {
                // Cast: switch offset for pc arithmetic
                out.push(t);
            }
        }
    } else {
        let Some(npairs) = read_i32(p + 4) else {
            return out;
        };
        for k in 0..npairs.max(0) as usize {
            let Some(off) = read_i32(p + 12 + k * 8) else {
                break;
            };
            if let Some(t) = pc.checked_add_signed(off as isize) {
                // Cast: switch offset for pc arithmetic
                out.push(t);
            }
        }
    }
    out.retain(|&t| t < code_len);
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
        0x59 => {
            // dup — cat-1 only per JVMS.
            let top = *s.last()?;
            if top.is_category_2()? {
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
            if top.is_category_2()? || below.is_category_2()? {
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
            if top.is_category_2()? || below.is_category_2()? {
                return None;
            }
            let n = s.len();
            s.swap(n - 1, n - 2);
        }
        0x5b => {
            // dup_x2 — FORM 1 `[v3, v2, v1] -> [v1, v3, v2, v1]` (all cat-1,
            // three entries) vs FORM 2 `[v2, v1] -> [v1, v2, v1]` (v2 cat-2,
            // two entries). v1 is cat-1 in both forms; the entry below decides
            // how deep the copy is inserted.
            if s.len() < 2 {
                return None;
            }
            let top = s[s.len() - 1];
            if top.is_category_2()? {
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
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_meta() -> (
        FxHashSet<usize>,
        FxHashMap<usize, u8>,
        FxHashMap<usize, (usize, u8)>,
    ) {
        (
            FxHashSet::default(),
            FxHashMap::default(),
            FxHashMap::default(),
        )
    }

    fn run(code: &[u8], calls: FxHashMap<usize, (usize, u8)>) -> StackKindMap {
        let (ldc_refs, types, _) = no_meta();
        // No resolver ran, so no `ldc` pc is resolved and every one of them
        // stays `Unknown` — the pre-fix behaviour these cases were written
        // against. `run_with_ldc` is the arm that supplies the tags.
        let inputs = StackKindInputs {
            field_types: types.clone(),
            static_types: types,
            calls,
            ldc_refs: &ldc_refs,
            ldc_fp: &FxHashSet::default(),
            ldc_resolved: &FxHashSet::default(),
            handler_pcs: &[],
        };
        analyze(code, code.len(), &inputs)
    }

    /// [`run`] with the `ldc`-family constant-pool tags the compiler resolves:
    /// `resolved` is every `ldc`/`ldc2_w` pc reduced to an immediate, `fp` the
    /// subset whose constant is a `CONSTANT_Float`/`CONSTANT_Double`.
    fn run_with_ldc(
        code: &[u8],
        calls: FxHashMap<usize, (usize, u8)>,
        resolved: &[usize],
        fp: &[usize],
    ) -> StackKindMap {
        let (ldc_refs, types, _) = no_meta();
        let resolved: FxHashSet<usize> = resolved.iter().copied().collect();
        let fp: FxHashSet<usize> = fp.iter().copied().collect();
        let inputs = StackKindInputs {
            field_types: types.clone(),
            static_types: types,
            calls,
            ldc_refs: &ldc_refs,
            ldc_fp: &fp,
            ldc_resolved: &resolved,
            handler_pcs: &[],
        };
        analyze(code, code.len(), &inputs)
    }

    /// [`run`] with exception-handler entry points seeded.
    fn run_with_handlers(
        code: &[u8],
        calls: FxHashMap<usize, (usize, u8)>,
        handler_pcs: &[usize],
    ) -> StackKindMap {
        let (ldc_refs, types, _) = no_meta();
        let inputs = StackKindInputs {
            field_types: types.clone(),
            static_types: types,
            calls,
            ldc_refs: &ldc_refs,
            ldc_fp: &FxHashSet::default(),
            ldc_resolved: &FxHashSet::default(),
            handler_pcs,
        };
        analyze(code, code.len(), &inputs)
    }

    /// A handler body is reached by an edge no branch instruction names, so
    /// without a seed the analysis has no state there and every deopt point in
    /// a `catch` block falls back to `Unsupported` — which vetoes the whole
    /// artifact's OSR entry, because `osr_exit_policy` is artifact-wide.
    ///
    /// The seed is not a guess: JVMS §2.10 fixes the handler-entry stack at
    /// exactly one reference.
    #[test]
    fn a_handler_entry_is_seeded_with_the_throwable() {
        // 0: return
        // 1: astore_0     <- handler_pc; entry stack is [Ref]
        // 2: iconst_m1
        // 3: return
        let code = [0xb1u8, 0x4b, 0x02, 0xb1];
        let without = run_with_handlers(&code, FxHashMap::default(), &[]);
        assert_eq!(
            without.get(1),
            None,
            "unseeded, a handler body is unreached and has no answer at all"
        );

        let with = run_with_handlers(&code, FxHashMap::default(), &[1]);
        assert_eq!(with.get(1), Some(&[StackKind::Ref][..]));
        assert_eq!(
            with.get(2),
            Some(&[][..]),
            "the `astore_0` consumed it, so the stack is empty at pc 2"
        );
        assert_eq!(
            with.get(3),
            Some(&[StackKind::Int][..]),
            "and the typing carries on through the handler body"
        );
    }

    /// A handler pc that ordinary control flow also reaches at a different
    /// depth still poisons — the seed goes through the same merge as any other
    /// incoming edge, so it cannot launder a disagreement into an answer.
    #[test]
    fn a_handler_entry_that_disagrees_with_a_branch_still_poisons() {
        // 0: iconst_0   1: goto +3 (-> 4)   4: pop   5: return
        // pc 4 is reached by the goto with depth 1 AND seeded with depth 1 but
        // a different KIND, which merges to Unknown rather than poisoning; pc 5
        // is where the depths would differ if the seed were wrong.
        let code = [0x03u8, 0xa7, 0x00, 0x03, 0x57, 0xb1];
        let merged = run_with_handlers(&code, FxHashMap::default(), &[4]);
        assert_eq!(
            merged.get(4),
            Some(&[StackKind::Unknown][..]),
            "one path says Int and the other says Ref: the merge must forget, not pick"
        );

        // 0: iconst_0   1: iconst_0   2: goto +3 (-> 5)   5: return
        // Here the branch arrives with depth 2 and the seed says depth 1.
        let code = [0x03u8, 0x03, 0xa7, 0x00, 0x03, 0xb1];
        let poisoned = run_with_handlers(&code, FxHashMap::default(), &[5]);
        assert_eq!(
            poisoned.get(5),
            None,
            "a depth disagreement is not representable and must not be answered"
        );
    }

    // -----------------------------------------------------------------------
    // The category-dependent dup family.
    //
    // Until 2026-08-18 all four of `dup_x2`/`dup2`/`dup2_x1`/`dup2_x2` poisoned
    // here — "not modelling them costs precision, guessing them costs
    // correctness". Modelling them costs neither, because every form is decided
    // by categories this analysis already tracks, and `Unknown` still poisons.
    //
    // The payoff is not only precision downstream of a dup: this is the
    // SECOND-ENTRY WIDTH ORACLE the single-pass `dup2_x2` arm needs, and whose
    // absence kept that opcode unlowered on x64. A form picked from a wrong
    // answer here duplicates an unrelated slot, so each case below pins the
    // exact resulting vector, not just that an answer exists.
    // -----------------------------------------------------------------------

    /// `dup2` FORM 2 — a single category-2 entry, duplicated like `dup`.
    #[test]
    fn dup2_over_a_category_2_top_duplicates_one_entry() {
        // 0: lconst_0  1: dup2  2: ladd  3: lreturn
        let code: Vec<u8> = vec![0x09, 0x5c, 0x61, 0xad];
        let m = run(&code, FxHashMap::default());
        assert_eq!(m.get(1), Some(&[StackKind::Long][..]));
        assert_eq!(m.get(2), Some(&[StackKind::Long, StackKind::Long][..]));
    }

    /// `dup2` FORM 1 — two category-1 entries.
    #[test]
    fn dup2_over_two_category_1_entries_duplicates_both() {
        // 0: iconst_0  1: fconst_0  2: dup2  3: return
        let code: Vec<u8> = vec![0x03, 0x0b, 0x5c, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(3),
            Some(
                &[
                    StackKind::Int,
                    StackKind::Float,
                    StackKind::Int,
                    StackKind::Float
                ][..]
            )
        );
    }

    /// `dup_x2` FORM 2 — the value below the category-1 top is a category-2,
    /// so the copy is inserted TWO entries down, not three.
    #[test]
    fn dup_x2_inserts_below_one_entry_when_that_entry_is_category_2() {
        // 0: dconst_0  1: iconst_0  2: dup_x2  3: return
        let code: Vec<u8> = vec![0x0e, 0x03, 0x5b, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(3),
            Some(&[StackKind::Int, StackKind::Double, StackKind::Int][..])
        );
    }

    /// `dup2_x1` FORM 2 — a category-2 top over one category-1.
    #[test]
    fn dup2_x1_form2_slides_a_category_2_under_one_entry() {
        // 0: iconst_0  1: dconst_0  2: dup2_x1  3: return
        let code: Vec<u8> = vec![0x03, 0x0e, 0x5d, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(3),
            Some(&[StackKind::Double, StackKind::Int, StackKind::Double][..])
        );
    }

    /// `dup2_x2` FORM 4 — both operands category-2, two entries.
    #[test]
    fn dup2_x2_form4_is_two_entries() {
        // 0: lconst_0  1: dconst_0  2: dup2_x2  3: return
        let code: Vec<u8> = vec![0x09, 0x0e, 0x5e, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(3),
            Some(&[StackKind::Double, StackKind::Long, StackKind::Double][..])
        );
    }

    /// `dup2_x2` FORM 2 — a category-2 top over two category-1 entries. This
    /// is the form javac emits (`longArr[i] = otherArr[j] = v`), and the one
    /// whose depth an unconditional four-pop gets wrong.
    #[test]
    fn dup2_x2_form2_slides_a_category_2_under_two_entries() {
        // 0: iconst_0  1: fconst_0  2: dconst_0  3: dup2_x2  4: return
        let code: Vec<u8> = vec![0x03, 0x0b, 0x0e, 0x5e, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(4),
            Some(
                &[
                    StackKind::Double,
                    StackKind::Int,
                    StackKind::Float,
                    StackKind::Double
                ][..]
            )
        );
    }

    /// `dup2_x2` FORM 3 — two category-1 entries duplicated over ONE
    /// category-2. Three entries deep, not four: the difference between this
    /// and FORM 1 is exactly what the top-entry oracle cannot see.
    #[test]
    fn dup2_x2_form3_duplicates_two_entries_over_a_category_2() {
        // 0: lconst_0  1: iconst_0  2: fconst_0  3: dup2_x2  4: return
        let code: Vec<u8> = vec![0x09, 0x03, 0x0b, 0x5e, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(4),
            Some(
                &[
                    StackKind::Int,
                    StackKind::Float,
                    StackKind::Long,
                    StackKind::Int,
                    StackKind::Float
                ][..]
            )
        );
    }

    /// `dup2_x2` FORM 1 — four category-1 entries, six after.
    #[test]
    fn dup2_x2_form1_duplicates_two_entries_over_two() {
        // 0: iconst_0 1: iconst_1 2: fconst_0 3: iconst_2 4: dup2_x2 5: return
        let code: Vec<u8> = vec![0x03, 0x04, 0x0b, 0x05, 0x5e, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(
            m.get(5),
            Some(
                &[
                    StackKind::Float,
                    StackKind::Int,
                    StackKind::Int,
                    StackKind::Int,
                    StackKind::Float,
                    StackKind::Int
                ][..]
            )
        );
    }

    /// Rule 2 of the module's safety argument still holds for the new arms: an
    /// `Unknown` in a position whose category decides the form poisons instead
    /// of guessing. An unresolved `ldc2_w` is `Unknown` but its CATEGORY is not
    /// in doubt, so use an unresolved `ldc` (int-or-float, both category-1 —
    /// still `Unknown` as a KIND) under a category-1 top, where the third
    /// entry's category is what picks FORM 1 from FORM 3.
    #[test]
    fn dup2_x2_over_an_unknown_third_entry_poisons() {
        // 0: ldc #0 (unresolved -> Unknown)  2: iconst_0  3: iconst_1
        // 4: dup2_x2  5: return
        let code: Vec<u8> = vec![0x12, 0x00, 0x03, 0x04, 0x5e, 0xb1];
        let m = run(&code, FxHashMap::default());
        // The dup's own bci still has its IN state (published pre-transfer)...
        assert_eq!(
            m.get(4),
            Some(&[StackKind::Unknown, StackKind::Int, StackKind::Int][..])
        );
        // ...but nothing downstream of it gets an answer.
        assert_eq!(m.get(5), None, "an unprovable form must poison, not guess");
    }

    /// The shape the whole thing exists for: a counted loop with a `long`
    /// accumulator. Mid-expression bcis have a typed stack, which is what the
    /// per-method `wide_fp` gate could never give.
    #[test]
    fn counted_long_loop_is_typed_at_every_bci() {
        // 0: lconst_0   1: lstore_1   2: iconst_0   3: istore_3
        // 4: iload_3     5: iload_0    6: if_icmpge 25
        // 9: lload_1    10: iload_3   11: i2l      12: ladd
        // 13: lstore_1  14: iinc 3,1  17: goto 4
        // 20..: return
        let code: Vec<u8> = vec![
            0x09, 0x40, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00, 0x13, 0x1f, 0x1d, 0x85, 0x61, 0x40,
            0x84, 0x03, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
        ];
        let m = run(&code, FxHashMap::default());

        // bci 1 (`lstore_1`): the long pushed by `lconst_0` is still live.
        assert_eq!(m.get(1), Some(&[StackKind::Long][..]));
        // bci 4, the loop header: empty stack.
        assert_eq!(m.get(4), Some(&[][..]));
        // bci 6 (`if_icmpge`): two ints.
        assert_eq!(m.get(6), Some(&[StackKind::Int, StackKind::Int][..]));
        // bci 12 (`ladd`): the accumulator and the widened counter.
        assert_eq!(m.get(12), Some(&[StackKind::Long, StackKind::Long][..]));
    }

    /// A call's arguments are typed from where they were PUSHED, which is why
    /// the analysis needs only arity and return type at the call itself.
    #[test]
    fn a_call_pops_its_arguments_and_pushes_its_return_kind() {
        // 0: dload_1   1: invokestatic (D)J   4: lstore_3   5: return
        let code: Vec<u8> = vec![0x28, 0xb8, 0x00, 0x01, 0x40, 0xb1];
        let mut calls = FxHashMap::default();
        calls.insert(1usize, (1usize, b'J'));
        let m = run(&code, calls);

        assert_eq!(m.get(1), Some(&[StackKind::Double][..]));
        assert_eq!(m.get(4), Some(&[StackKind::Long][..]));
    }

    /// An instance call also pops its receiver — the opcode supplies that, not
    /// the arity, which is why `calls` stores the descriptor's count alone.
    #[test]
    fn an_instance_call_also_pops_the_receiver() {
        // 0: aload_0  1: iload_1  2: invokevirtual (I)I  5: istore_2  6: return
        let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x01, 0x3d, 0xb1];
        let mut calls = FxHashMap::default();
        calls.insert(2usize, (1usize, b'I'));
        let m = run(&code, calls);

        assert_eq!(m.get(2), Some(&[StackKind::Ref, StackKind::Int][..]));
        assert_eq!(m.get(5), Some(&[StackKind::Int][..]));
    }

    /// An unmodelled opcode poisons its successors rather than guessing a
    /// depth. `jsr` (0xa8) is the canonical one — it pushes a return address
    /// this backend has no representation for.
    #[test]
    fn an_unmodelled_opcode_poisons_everything_after_it() {
        // 0: iconst_0  1: jsr +3  4: pop  5: return
        let code: Vec<u8> = vec![0x03, 0xa8, 0x00, 0x03, 0x57, 0xb1];
        let m = run(&code, FxHashMap::default());

        assert_eq!(m.get(0), Some(&[][..]));
        assert_eq!(m.get(1), Some(&[StackKind::Int][..]));
        assert_eq!(m.get(4), None, "the jsr's successor must have no answer");
    }

    /// The defect behind `osr-refused-for-a-loop-inline-in-main-FIXED-20260818`: a
    /// numeric `ldc` answered `Unknown`, the deopt snapshot recorded it
    /// `Unsupported`, and `osr_exit_policy`'s artifact-wide veto then refused
    /// OSR entry at every pc of the method — 180 ns/iter against 1 compiled.
    /// The constant-pool tag is known, so the entry must be typed.
    #[test]
    fn a_resolved_numeric_ldc_is_typed_from_its_constant_pool_tag() {
        // 0: ldc #1   2: ldc #2   4: return
        let code: Vec<u8> = vec![0x12, 0x01, 0x12, 0x02, 0xb1];

        // pc 0 is a CONSTANT_Integer, pc 2 a CONSTANT_Float.
        let m = run_with_ldc(&code, FxHashMap::default(), &[0, 2], &[2]);
        assert_eq!(m.get(2), Some(&[StackKind::Int][..]));
        assert_eq!(m.get(4), Some(&[StackKind::Int, StackKind::Float][..]));
    }

    /// `ldc2_w` is one compact entry either way, so only the KIND was ever in
    /// doubt — and the same tag answers it.
    #[test]
    fn a_resolved_ldc2w_is_typed_from_its_constant_pool_tag() {
        // 0: ldc2_w #1   3: ldc2_w #2   6: return
        let code: Vec<u8> = vec![0x14, 0x00, 0x01, 0x14, 0x00, 0x02, 0xb1];

        let m = run_with_ldc(&code, FxHashMap::default(), &[0, 3], &[3]);
        assert_eq!(m.get(3), Some(&[StackKind::Long][..]));
        assert_eq!(m.get(6), Some(&[StackKind::Long, StackKind::Double][..]));
    }

    /// Rule 2 of the module's safety argument: `Unknown` is not a guess. A pc
    /// the resolver never answered must NOT default to the non-floating-point
    /// member of its pair just because it is absent from the `fp` set.
    #[test]
    fn an_unresolved_ldc_stays_unknown_rather_than_defaulting_to_int() {
        // 0: ldc #1   2: ldc2_w #2   5: return
        let code: Vec<u8> = vec![0x12, 0x01, 0x14, 0x00, 0x02, 0xb1];

        let m = run_with_ldc(&code, FxHashMap::default(), &[], &[]);
        assert_eq!(m.get(2), Some(&[StackKind::Unknown][..]));
        assert_eq!(
            m.get(5),
            Some(&[StackKind::Unknown, StackKind::Unknown][..]),
            "no resolver answer means no kind, for both ldc families"
        );
    }

    /// The exact operand stack the refusal named: a `long` static under an
    /// `ldc` int argument, at the `invokestatic` that carries the deopt point.
    /// Both entries must be describable, or OSR is refused for the whole
    /// artifact.
    #[test]
    fn a_long_static_under_an_ldc_int_argument_is_fully_typed() {
        // 0: getstatic #1 (J)   3: ldc #2 (int)   5: invokestatic #3 (I)J
        let code: Vec<u8> = vec![0xb2, 0x00, 0x01, 0x12, 0x02, 0xb8, 0x00, 0x03, 0xb1];
        let mut statics: FxHashMap<usize, u8> = FxHashMap::default();
        statics.insert(0, b'J');
        let mut calls: FxHashMap<usize, (usize, u8)> = FxHashMap::default();
        calls.insert(5, (1, b'J'));

        let ldc_resolved: FxHashSet<usize> = [3usize].into_iter().collect();
        let inputs = StackKindInputs {
            field_types: FxHashMap::default(),
            static_types: statics,
            calls,
            ldc_refs: &FxHashSet::default(),
            ldc_fp: &FxHashSet::default(),
            ldc_resolved: &ldc_resolved,
            handler_pcs: &[],
        };
        let m = analyze(&code, code.len(), &inputs);

        assert_eq!(
            m.get(5),
            Some(&[StackKind::Long, StackKind::Int][..]),
            "the deopt point's operand stack must have no Unknown entry"
        );
    }

    /// Two paths that disagree about DEPTH cannot be merged, and the answer
    /// must be withdrawn rather than taken from whichever path ran first.
    #[test]
    fn a_depth_disagreement_withdraws_the_answer() {
        // Two paths reach bci 9 with different depths. Note the extra
        // `iconst_0`: a conditional branch CONSUMES its operand, so without a
        // spare value on the stack both arms would arrive at equal depth and
        // the test would prove nothing.
        //
        // 0: iconst_0      -> [I]
        // 1: iconst_0      -> [I, I]
        // 2: ifeq 9        -> pops the condition; target 9 sees [I]
        // 5: iconst_1      -> [I, I]
        // 6: goto 9        -> target 9 sees [I, I]
        // 9: pop   10: return
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x03, // 1: iconst_0
            0x99, 0x00, 0x07, // 2: ifeq -> 9
            0x04, // 5: iconst_1
            0xa7, 0x00, 0x03, // 6: goto -> 9
            0x57, // 9: pop
            0xb1, // 10: return
        ];
        let m = run(&code, FxHashMap::default());
        assert_eq!(m.get(9), None, "conflicting depths must not be answered");
        assert_eq!(m.get(10), None, "and the conflict propagates forward");
    }

    /// `pop2` is category-dependent: over a known cat-1 top it pops two
    /// entries, over a cat-2 top one. Over an `Unknown` top the depth is
    /// genuinely unknown, so it poisons instead of picking.
    #[test]
    fn pop2_over_an_unknown_top_poisons() {
        // 0: iconst_0  1: ldc2_w #1 (long-or-double, Unknown)  4: pop2  5: return
        let code: Vec<u8> = vec![0x03, 0x14, 0x00, 0x01, 0x58, 0xb1];
        let m = run(&code, FxHashMap::default());
        assert_eq!(m.get(4), Some(&[StackKind::Int, StackKind::Unknown][..]));
        assert_eq!(m.get(5), None);
    }

    /// `pop2` over a known cat-2 top pops one entry; over cat-1, two.
    #[test]
    fn pop2_respects_a_known_category() {
        // 0: lconst_0  1: pop2  2: return   -> one entry popped, stack empty
        let m = run(&[0x09, 0x58, 0xb1], FxHashMap::default());
        assert_eq!(m.get(2), Some(&[][..]));

        // 0: iconst_0  1: iconst_1  2: pop2  3: return -> two popped, empty
        let m = run(&[0x03, 0x04, 0x58, 0xb1], FxHashMap::default());
        assert_eq!(m.get(3), Some(&[][..]));
    }

    /// A conditional whose two arms push the same DEPTH but different KINDS
    /// merges to `Unknown` — describable depth, undescribable type, which the
    /// consult site then treats exactly as it did before this analysis existed.
    #[test]
    fn conflicting_kinds_at_the_same_depth_merge_to_unknown() {
        let code: Vec<u8> = vec![
            0x03, // 0: iconst_0
            0x99, 0x00, 0x06, // 1: ifeq -> 7
            0x0b, // 4: fconst_0
            0xa7, 0x00, 0x03, // 5: goto -> 8
            0x03, // 7: iconst_0
            0x57, // 8: pop
            0xb1, // 9: return
        ];
        let m = run(&code, FxHashMap::default());
        assert_eq!(m.get(8), Some(&[StackKind::Unknown][..]));
    }

    /// `checkcast` proves its operand is a reference; `instanceof` replaces it
    /// with an int. Both are everywhere in generic-heavy loop bodies.
    #[test]
    fn checkcast_keeps_a_ref_and_instanceof_yields_an_int() {
        // 0: aload_0  1: checkcast #1  4: astore_1  5: aload_0
        // 6: instanceof #1  9: istore_2  10: return
        let code: Vec<u8> = vec![
            0x2a, 0xc0, 0x00, 0x01, 0x4c, 0x2a, 0xc1, 0x00, 0x01, 0x3d, 0xb1,
        ];
        let m = run(&code, FxHashMap::default());
        assert_eq!(m.get(4), Some(&[StackKind::Ref][..]));
        assert_eq!(m.get(9), Some(&[StackKind::Int][..]));
    }
}
