// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Block-monitor balance of a method's bytecode (JIT round 21 wave 4, lane
//! monitors; page
//! `docs/internal/fixed-bugs/r21-single-pass-compiles-unbalanced-monitor-bytecode-FIXED-20261018.md`).
//!
//! HotSpot's compilers refuse a method whose `monitorenter` / `monitorexit`
//! bytecode is not balanced (`ciMethod::has_balanced_monitors`, computed by
//! `GenerateOopMap`'s monitor-safety pass), so such a method always runs in
//! the interpreter and its structured-locking checks (JVMS 2.11.10) decide
//! what Java sees. Two readers here:
//!
//! * [`unbalanced_monitor_site`]: the single-pass tier's admission gate
//!   (`x64::deopt_stubs::monitor_balance_gate_enabled`), which refuses to
//!   compile such a method;
//! * [`block_monitor_depth_at`]: the interpreter's `monitorexit` of a monitor
//!   its frame's record does not name (`held_monitors::monitorexit_unrecorded`),
//!   which asks how many block monitors the frame's OWN code can hold at that
//!   pc, so that a hold a compiled caller took is never released by an
//!   interpreted callee.
//!
//! Only the method's own block monitors are counted: an `ACC_SYNCHRONIZED`
//! method's monitor is taken by the invoke, not by bytecode.
//!
//! The ARM64 backend is out of scope (round 21); both readers are
//! architecture-neutral.

use crate::bytecode_analysis;

/// Which of HotSpot's monitor-safety rules [`unbalanced_monitor_site`]
/// applies beyond the depth rules (exit with none held, return or uncaught
/// `athrow` with one held, depths that differ at a join), which always apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BalanceRules {
    /// Every instruction that can raise ([`op_can_trap`]) with a block
    /// monitor held must be covered by a catch-all handler: its exception
    /// would otherwise leave the frame holding the lock.
    pub implicit_traps: bool,
    /// A `monitorexit` is refused unless its operand is provably the
    /// innermost open monitor's object: a local that every path ties to the
    /// object the enter locked, through copies made before or after the
    /// enter (JIT round 21 wave 7; before it, an exit whose operand was not
    /// a local load was admitted and a copy made before the enter was not
    /// followed).
    pub operands: bool,
}

impl BalanceRules {
    /// Every rule.
    pub const ALL: BalanceRules = BalanceRules {
        implicit_traps: true,
        operands: true,
    };
    /// The depth rules only (the round-21 first cut).
    pub const DEPTH_ONLY: BalanceRules = BalanceRules {
        implicit_traps: false,
        operands: false,
    };
}

/// Whether `op` may raise at run time: the bytecodes HotSpot's
/// `Bytecodes::can_trap` names (constant loads, array accesses, integer
/// division, field and invoke opcodes, allocation, casts, `athrow`, the
/// monitor ops and the returns), less `aload_0`, which
/// `GenerateOopMap::do_exception_edge` exempts too.
pub fn op_can_trap(op: u8) -> bool {
    matches!(
        op,
        0x12..=0x14 // ldc, ldc_w, ldc2_w
            | 0x2e..=0x35 // array loads
            | 0x4f..=0x56 // array stores
            | 0x6c | 0x6d | 0x70 | 0x71 // idiv, ldiv, irem, lrem
            | 0xac..=0xb1 // returns
            | 0xb2..=0xba // field accesses and invokes
            | 0xbb..=0xc3 // new .. instanceof, monitorenter, monitorexit
            | 0xc5 // multianewarray
    )
}

/// Whether the walks below analyse this method at all: it has monitor
/// bytecode, no `jsr` / `ret`, and catch-all bits that describe its table.
fn analysable(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
    exception_catch_all: &[bool],
) -> bool {
    if code_len == 0 || exception_catch_all.len() != exception_ranges.len() {
        return false;
    }
    let mut has_monitor_op = false;
    let mut pc = 0usize;
    while pc < code_len {
        match code[pc] {
            0xc2 | 0xc3 => has_monitor_op = true,
            0xa8 | 0xa9 | 0xc9 => return false,
            0xc4 if code.get(pc + 1) == Some(&0xa9) => return false,
            _ => {}
        }
        pc += bytecode_analysis::step(code, pc);
    }
    has_monitor_op
}

/// The local a `*load` of a reference at `pc` reads (`load`) or a reference
/// `*store` writes (`!load`).
fn reference_local(code: &[u8], pc: usize, load: bool) -> Option<usize> {
    let (short, first_n) = if load { (0x19u8, 0x2au8) } else { (0x3au8, 0x4bu8) };
    let op = *code.get(pc)?;
    if op == short {
        return code.get(pc + 1).map(|&b| usize::from(b));
    }
    if (first_n..first_n + 4).contains(&op) {
        return Some(usize::from(op - first_n));
    }
    if op == 0xc4 && code.get(pc + 1) == Some(&short) {
        let hi = usize::from(*code.get(pc + 2)?);
        let lo = usize::from(*code.get(pc + 3)?);
        return Some((hi << 8) | lo);
    }
    None
}

/// The locals any store (or `iinc`) at `pc` overwrites: one slot, two for a
/// `long` / `double` store.
fn written_locals(code: &[u8], pc: usize) -> Option<(usize, usize)> {
    let op = *code.get(pc)?;
    let (real, slot) = if op == 0xc4 {
        let real = *code.get(pc + 1)?;
        let hi = usize::from(*code.get(pc + 2)?);
        let lo = usize::from(*code.get(pc + 3)?);
        (real, (hi << 8) | lo)
    } else {
        match op {
            0x36..=0x3a | 0x84 => (op, usize::from(*code.get(pc + 1)?)),
            0x3b..=0x4e => (op, usize::from((op - 0x3b) % 4)),
            _ => return None,
        }
    };
    if !matches!(real, 0x36..=0x4e | 0x84) {
        return None;
    }
    // lstore, dstore, lstore_n, dstore_n.
    let wide = matches!(real, 0x37 | 0x39 | 0x3f..=0x42 | 0x47..=0x4a);
    Some((slot, if wide { 2 } else { 1 }))
}

/// The pc-shape facts both walks read: the textually previous instruction of
/// every instruction start, and whether a pc can be entered other than by
/// falling in from it (a branch target or a handler entry).
struct Shape {
    prev: Vec<Option<usize>>,
    joined: Vec<bool>,
}

impl Shape {
    fn new(code: &[u8], code_len: usize, exception_ranges: &[(usize, usize, usize)]) -> Shape {
        let starts = bytecode_analysis::instruction_starts(code, code_len);
        let mut prev: Vec<Option<usize>> = vec![None; code_len];
        let mut last: Option<usize> = None;
        for pc in 0..code_len {
            if starts.get(pc).copied().unwrap_or(false) {
                prev[pc] = last;
                last = Some(pc);
            }
        }
        let mut joined = bytecode_analysis::branch_target_map(code, code_len);
        for &(_, _, handler) in exception_ranges {
            if let Some(j) = joined.get_mut(handler) {
                *j = true;
            }
        }
        Shape { prev, joined }
    }

    fn joined_at(&self, pc: usize) -> bool {
        self.joined.get(pc).copied().unwrap_or(true)
    }

    fn prev_of(&self, pc: usize) -> Option<usize> {
        self.prev.get(pc).copied().flatten()
    }

    /// The local holding, on entry to `pc`, the object a `monitorenter` /
    /// `monitorexit` at `pc` pops: `aload k; op` gives `k`, javac's
    /// `dup; astore t; op` gives `t` (the store already made); anything else,
    /// or an instruction of the pattern entered by a jump, gives none.
    fn operand_local(&self, code: &[u8], pc: usize) -> Option<usize> {
        if self.joined_at(pc) {
            return None;
        }
        let before = self.prev_of(pc)?;
        if let Some(k) = reference_local(code, before, true) {
            return Some(k);
        }
        let t = reference_local(code, before, false)?;
        let dup = self.prev_of(before)?;
        if self.joined_at(before) || code.get(dup) != Some(&0x59) {
            return None;
        }
        Some(t)
    }

    /// The local whose object a reference `*store` at `pc` stores: the `a`
    /// of `aload a; astore` or `aload a; dup; astore`, none of them entered
    /// by a jump; none otherwise (the value is then a new one).
    fn stored_local(&self, code: &[u8], pc: usize) -> Option<usize> {
        if self.joined_at(pc) {
            return None;
        }
        let before = self.prev_of(pc)?;
        if let Some(a) = reference_local(code, before, true) {
            return Some(a);
        }
        if code.get(before) != Some(&0x59) || self.joined_at(before) {
            return None;
        }
        reference_local(code, self.prev_of(before)?, true)
    }
}

/// The locals the operand rule tracks, sorted: the only slots whose object
/// identity the walk can ever read (JIT round 21 wave 10; before it the rule
/// tracked a dense prefix of the locals capped at 1024, so javac output whose
/// lock temporary sat above slot 1023 was refused).
///
/// An identity is read only at a `monitorenter` / `monitorexit` operand
/// ([`Shape::operand_local`]) and at a reference store that copies a local
/// ([`Shape::stored_local`]) into a slot whose identity is itself read. So
/// the set is the monitor operand slots, closed under "the source local of a
/// copy into a tracked slot". Any other slot's label is never read: a store
/// to it is ignored, and the method's slot count does not matter.
struct Tracked {
    slots: Vec<usize>,
}

impl Tracked {
    /// The tracked slots and the number of `monitorenter` instructions (a
    /// bound on the depth).
    fn new(code: &[u8], code_len: usize, shape: &Shape) -> (Tracked, usize) {
        let mut enters = 0usize;
        let mut seeds: Vec<usize> = Vec::new();
        // (target, source) of every reference copy `aload a; [dup;] astore t`.
        let mut copies: Vec<(usize, usize)> = Vec::new();
        let mut pc = 0usize;
        while pc < code_len {
            match code[pc] {
                0xc2 | 0xc3 => {
                    if code[pc] == 0xc2 {
                        enters += 1;
                    }
                    if let Some(k) = shape.operand_local(code, pc) {
                        seeds.push(k);
                    }
                }
                _ => {
                    if let Some(t) = reference_local(code, pc, false) {
                        if let Some(a) = shape.stored_local(code, pc) {
                            copies.push((t, a));
                        }
                    }
                }
            }
            pc += bytecode_analysis::step(code, pc);
        }
        copies.sort_unstable();
        copies.dedup();
        let mut set: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut work: Vec<usize> = Vec::new();
        for k in seeds {
            if set.insert(k) {
                work.push(k);
            }
        }
        while let Some(t) = work.pop() {
            let from = copies.partition_point(|&(target, _)| target < t);
            for &(target, a) in &copies[from..] {
                if target != t {
                    break;
                }
                if set.insert(a) {
                    work.push(a);
                }
            }
        }
        let mut slots: Vec<usize> = set.into_iter().collect();
        slots.sort_unstable();
        (Tracked { slots }, enters)
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    /// The cell of slot `k`, when it is tracked.
    fn cell(&self, k: usize) -> Option<usize> {
        self.slots.binary_search(&k).ok()
    }
}

/// The operand rule's must-alias state at a pc: a class label per tracked
/// local (indexed by [`Tracked::cell`]) and per open block monitor
/// (outermost first). Two cells share a label exactly when every path to the
/// pc proves they hold the same object (HotSpot's `GenerateOopMap` follows
/// the lock reference the same way: `replace_all_CTS_matches` at the enter, a
/// mismatch at the exit clears `_monitor_safe`). A cell nothing is known
/// about has a label of its own.
///
/// A STORED state (one per join pc) is canonical: labels numbered by first
/// occurrence over the locals then the monitors, `next` their count. Equal
/// partitions therefore compare equal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Aliases {
    locals: Vec<u32>,
    monitors: Vec<u32>,
    /// A label no cell uses.
    next: u32,
}

impl Aliases {
    /// Method entry: every tracked local its own object.
    fn entry(cells: usize) -> Aliases {
        let n = u32::try_from(cells).unwrap_or(u32::MAX);
        Aliases {
            locals: (0..n).collect(),
            monitors: Vec::new(),
            next: n,
        }
    }

    fn empty() -> Aliases {
        Aliases {
            locals: Vec::new(),
            monitors: Vec::new(),
            next: 0,
        }
    }

    /// `self = other`, reusing `self`'s buffers.
    fn assign(&mut self, other: &Aliases) {
        self.locals.clone_from(&other.locals);
        self.monitors.clone_from(&other.monitors);
        self.next = other.next;
    }

    fn fresh(&mut self) -> u32 {
        let label = self.next;
        self.next = self.next.saturating_add(1);
        label
    }

    fn local(&self, cell: usize) -> Option<u32> {
        self.locals.get(cell).copied()
    }

    fn set_local(&mut self, cell: usize, label: u32) {
        if let Some(c) = self.locals.get_mut(cell) {
            *c = label;
        }
    }

    fn cell_count(&self) -> usize {
        self.locals.len() + self.monitors.len()
    }

    fn cell_at(&self, i: usize) -> u32 {
        let n = self.locals.len();
        if i < n {
            self.locals[i]
        } else {
            self.monitors[i - n]
        }
    }
}

/// Buffers [`meet_into`] reuses across merges, so a merge allocates nothing
/// once they have grown to the method's cell count.
#[derive(Default)]
struct MeetScratch {
    /// `(label in a << 32 | label in b, cell)`, sorted to group equal pairs.
    keys: Vec<(u64, u32)>,
    /// The pair group of every cell.
    group: Vec<u32>,
    /// A group's canonical label, or `u32::MAX` before its first cell.
    renum: Vec<u32>,
    /// The meet's canonical labels, locals then monitors.
    out: Vec<u32>,
}

/// The meet of two states of one shape (same tracked locals, same depth):
/// two cells share a label iff they share one in both. Writes the canonical
/// labels (numbered by first occurrence) to `s.out` and answers their count.
/// `meet(a, a)` is `a`'s canonical form. O(cells log cells), no allocation
/// once the scratch is warm (before wave 10 a `HashMap` was built per merge).
fn meet_into(a: &Aliases, b: &Aliases, s: &mut MeetScratch) -> u32 {
    let n = a.cell_count();
    s.keys.clear();
    for i in 0..n {
        let key = (u64::from(a.cell_at(i)) << 32) | u64::from(b.cell_at(i));
        s.keys.push((key, u32::try_from(i).unwrap_or(u32::MAX)));
    }
    s.keys.sort_unstable();
    s.group.clear();
    s.group.resize(n, 0);
    let mut groups = 0u32;
    let mut last: Option<u64> = None;
    for &(key, cell) in &s.keys {
        if last != Some(key) {
            last = Some(key);
            groups += 1;
        }
        s.group[cell as usize] = groups - 1;
    }
    s.renum.clear();
    s.renum.resize(groups as usize, u32::MAX);
    s.out.clear();
    let mut next = 0u32;
    for &g in &s.group {
        let g = g as usize;
        if s.renum[g] == u32::MAX {
            s.renum[g] = next;
            next += 1;
        }
        s.out.push(s.renum[g]);
    }
    next
}

/// Merges `incoming` into the stored state of join pc `succ` and queues
/// `succ` when that state changed. No copy of `incoming` is made unless the
/// stored state changes (wave 10: every handler of a trapping instruction is
/// merged from the one walk state, before wave 10 each got a clone). Answers
/// the refusal when the depths differ.
fn merge_at(
    succ: usize,
    incoming: &Aliases,
    at: &mut [Option<Aliases>],
    queued: &mut [bool],
    work: &mut Vec<usize>,
    s: &mut MeetScratch,
) -> Option<(usize, &'static str)> {
    let slot = at.get_mut(succ)?;
    let changed = match slot {
        None => {
            let next = meet_into(incoming, incoming, s);
            let n = incoming.locals.len();
            *slot = Some(Aliases {
                locals: s.out[..n].to_vec(),
                monitors: s.out[n..].to_vec(),
                next,
            });
            true
        }
        Some(seen) => {
            if seen.monitors.len() != incoming.monitors.len() {
                return Some((succ, "monitor depth differs where paths join"));
            }
            let next = meet_into(seen, incoming, s);
            let n = seen.locals.len();
            if seen.next == next && seen.locals[..] == s.out[..n] && seen.monitors[..] == s.out[n..] {
                false
            } else {
                seen.locals.copy_from_slice(&s.out[..n]);
                seen.monitors.copy_from_slice(&s.out[n..]);
                seen.next = next;
                true
            }
        }
    };
    if changed && !queued[succ] {
        queued[succ] = true;
        work.push(succ);
    }
    None
}

/// The handlers an exception raised at `pc` may reach, in the JVM's search
/// order (JVMS 2.10: table order, ending at the first covering catch-all),
/// and whether a catch-all ended the search.
fn handlers_at(
    pc: usize,
    exception_ranges: &[(usize, usize, usize)],
    exception_catch_all: &[bool],
    out: &mut Vec<usize>,
) -> bool {
    out.clear();
    for (i, &(start, end, handler)) in exception_ranges.iter().enumerate() {
        if start <= pc && pc < end {
            out.push(handler);
            if exception_catch_all.get(i).copied().unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Whether an instruction at block-monitor depth `d` adds exception edges:
/// it can raise, except a `monitorexit` with a monitor held (HotSpot's
/// exemption: it is the instruction javac's handler range covers to release
/// the lock).
fn adds_exception_edges(op: u8, d: usize) -> bool {
    op_can_trap(op) && !(op == 0xc3 && d > 0)
}

/// The first pc at which this method's BLOCK monitors are unbalanced, and
/// why; `None` when they are balanced, when the method has no monitor
/// bytecode, or when it is not analysed (`jsr`/`ret`, catch-all bits that do
/// not describe the table).
///
/// HotSpot's monitor-safety rules (`GenerateOopMap`, `_monitor_safe`):
///
/// * a `monitorexit` reached with no block monitor held;
/// * a `*return` reached with one held;
/// * an instruction that can raise with one held and no covering catch-all
///   (`rules.implicit_traps`; without it only an `athrow` is refused there);
/// * two paths (normal or exceptional) reaching one pc at different depths;
/// * a `monitorexit` whose operand is not the innermost open monitor's
///   object (`rules.operands`). HotSpot tracks the lock reference through
///   every cell that holds it (`replace_all_CTS_matches`), copies made before
///   the enter included. This tracks it through the locals ([`Aliases`]: a
///   reference store keeps the identity of an `aload a` / `aload a; dup` it
///   stores, any other store gives the slot a new one, a join keeps only
///   what both paths prove). The operand of an enter or exit is known when it
///   is `aload k` or javac's `dup; astore t` just before it; an exit is
///   refused unless its operand is known and is the innermost open monitor's
///   object, so an exit through a field load, a call, a stack copy or a
///   local nothing ties to the lock is refused (HotSpot refuses those too,
///   unless the value happens to be the lock reference it tracks on the
///   stack, which this does not model).
///
/// Exception edges enter a handler at the state BEFORE the instruction (a
/// throwing enter took nothing), from every instruction that can raise.
///
/// The walk is bounded (each join pc's state only refines, at most once per
/// cell); were its work budget ever exhausted the method is refused, as a
/// correctness gate must.
///
/// javac, ecj and kotlinc output always passes.
pub fn unbalanced_monitor_site(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
    exception_catch_all: &[bool],
    rules: BalanceRules,
) -> Option<(usize, &'static str)> {
    walk_balance(code, code_len, exception_ranges, exception_catch_all, rules, None)
}

/// [`unbalanced_monitor_site`] with an optional work budget in place of the
/// proven bound (unit tests of the exhaustion answer).
fn walk_balance(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
    exception_catch_all: &[bool],
    rules: BalanceRules,
    budget_override: Option<usize>,
) -> Option<(usize, &'static str)> {
    let code_len = code_len.min(code.len());
    if !analysable(code, code_len, exception_ranges, exception_catch_all) {
        return None;
    }
    let shape = Shape::new(code, code_len, exception_ranges);
    // Only the slots that can carry a lock's identity are cells (wave 10):
    // any slot number works, and the states stay small.
    let (tracked, enters) = Tracked::new(code, code_len, &shape);
    let slots = tracked.len();
    // States are stored only where paths can join (and at entry): a pc that
    // is neither a jump target nor a handler is reached only by falling in
    // from the instruction before it, so a chain of those is walked inline.
    let mut at: Vec<Option<Aliases>> = vec![None; code_len];
    at[0] = Some(Aliases::entry(slots));
    let mut queued = vec![false; code_len];
    queued[0] = true;
    let mut work: Vec<usize> = vec![0];
    // A stored state only refines (its partition of the tracked locals and
    // open monitors splits at most once per cell), and the chains from
    // distinct join pcs are disjoint, so this many instruction visits always
    // suffice.
    let mut budget = budget_override.unwrap_or_else(|| {
        code_len
            .saturating_mul(slots.saturating_add(enters).saturating_add(2))
            .saturating_add(64)
    });
    let mut handlers: Vec<usize> = Vec::new();
    let mut scratch = MeetScratch::default();
    // The walk state, reused across chains (assigned, not cloned).
    let mut state = Aliases::empty();
    while let Some(start) = work.pop() {
        queued[start] = false;
        match &at[start] {
            Some(seen) => state.assign(seen),
            None => continue,
        }
        let mut pc = start;
        loop {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                return Some((pc, "the monitor-balance walk ran out of its work budget"));
            }
            let d = state.monitors.len();
            let op = code[pc];
            let traps = adds_exception_edges(op, d);
            if traps {
                let caught = handlers_at(pc, exception_ranges, exception_catch_all, &mut handlers);
                if d > 0 && !caught {
                    if op == 0xbf {
                        return Some((pc, "athrow can leave the frame with a block monitor held"));
                    }
                    // A return with a monitor held is refused below, whatever
                    // the table says.
                    if rules.implicit_traps && !matches!(op, 0xac..=0xb1) {
                        return Some((
                            pc,
                            "an instruction that can raise runs with a block monitor held and no catch-all",
                        ));
                    }
                }
            }
            // The instruction's own refusals, before any state changes (the
            // same answers, in the same order, as when the handler states
            // were cloned up front).
            match op {
                0xc3 => {
                    let Some(&top) = state.monitors.last() else {
                        return Some((pc, "monitorexit with no block monitor held"));
                    };
                    if rules.operands {
                        let operand = shape
                            .operand_local(code, pc)
                            .map(|k| tracked.cell(k).and_then(|c| state.local(c)));
                        match operand {
                            None => {
                                return Some((
                                    pc,
                                    "monitorexit whose operand is not provably the innermost monitorenter's object",
                                ));
                            }
                            Some(label) if label != Some(top) => {
                                return Some((
                                    pc,
                                    "monitorexit of another object than the innermost monitorenter's",
                                ));
                            }
                            Some(_) => {}
                        }
                    }
                }
                0xac..=0xb1 if d > 0 => {
                    return Some((pc, "return with a block monitor held"));
                }
                _ => {}
            }
            // Exception edges carry the state BEFORE the instruction; every
            // handler merges from the one walk state, nothing is cloned.
            if traps {
                for &h in &handlers {
                    if h >= code_len {
                        continue;
                    }
                    if let Some(refusal) =
                        merge_at(h, &state, &mut at, &mut queued, &mut work, &mut scratch)
                    {
                        return Some(refusal);
                    }
                }
            }
            match op {
                0xc2 => {
                    let known = shape
                        .operand_local(code, pc)
                        .and_then(|k| tracked.cell(k))
                        .and_then(|c| state.local(c));
                    let label = known.unwrap_or_else(|| state.fresh());
                    state.monitors.push(label);
                }
                0xc3 => {
                    state.monitors.pop();
                }
                _ => {
                    // A store to an untracked slot is ignored: nothing ever
                    // reads that slot's identity.
                    if let Some((slot, width)) = written_locals(code, pc) {
                        for (i, k) in (slot..slot + width).enumerate() {
                            let Some(cell) = tracked.cell(k) else {
                                continue;
                            };
                            let copied = if i == 0 && reference_local(code, pc, false).is_some() {
                                shape
                                    .stored_local(code, pc)
                                    .and_then(|a| tracked.cell(a))
                                    .and_then(|c| state.local(c))
                            } else {
                                None
                            };
                            let label = copied.unwrap_or_else(|| state.fresh());
                            state.set_local(cell, label);
                        }
                    }
                }
            }
            let next = pc + bytecode_analysis::step(code, pc);
            let mut fall_in = false;
            for succ in bytecode_analysis::lenient_successors(code, code_len, pc) {
                if succ == next && !shape.joined_at(succ) {
                    fall_in = true;
                } else if succ < code_len {
                    if let Some(refusal) =
                        merge_at(succ, &state, &mut at, &mut queued, &mut work, &mut scratch)
                    {
                        return Some(refusal);
                    }
                }
            }
            if !fall_in {
                break;
            }
            pc = next;
        }
    }
    None
}

/// How many BLOCK monitors this method's own code holds on entry to `pc`, on
/// every path that reaches it (`None`: unreached, not analysed, or paths
/// that disagree). Depth only, and lenient: a violation does not stop the
/// walk, it makes the pcs it reaches answer `None`. Same exception edges as
/// [`unbalanced_monitor_site`].
///
/// The interpreter asks this at a `monitorexit` whose frame record does not
/// name the object: a frame whose record already names this many monitors
/// cannot hold the object through its own code, so the hold the table shows
/// is another frame's (a compiled caller's) and the exit is HotSpot's
/// `IllegalMonitorStateException`.
pub fn block_monitor_depth_at(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
    exception_catch_all: &[bool],
    pc: usize,
) -> Option<u32> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum D {
        Unreached,
        Known(u32),
        Conflict,
    }
    let code_len = code_len.min(code.len());
    if pc >= code_len || !analysable(code, code_len, exception_ranges, exception_catch_all) {
        return None;
    }
    let mut at: Vec<D> = vec![D::Unreached; code_len];
    at[0] = D::Known(0);
    let mut work: Vec<usize> = vec![0];
    // Each pc moves Unreached -> Known -> Conflict at most once each.
    let mut budget = code_len.saturating_mul(4).saturating_add(64);
    let mut handlers: Vec<usize> = Vec::new();
    while let Some(here) = work.pop() {
        budget = budget.saturating_sub(1);
        if budget == 0 {
            return None;
        }
        let state = at[here];
        let op = code[here];
        let mut joins: Vec<(usize, D)> = Vec::new();
        let d = match state {
            D::Unreached => continue,
            D::Known(d) => Some(d),
            D::Conflict => None,
        };
        // A conflicting state takes every exception edge it could.
        if d.map_or(op_can_trap(op), |d| adds_exception_edges(op, d as usize)) {
            handlers_at(here, exception_ranges, exception_catch_all, &mut handlers);
            for &h in &handlers {
                joins.push((h, state));
            }
        }
        let out = match (op, d) {
            (0xac..=0xb1 | 0xbf, _) => None,
            (_, None) => Some(D::Conflict),
            (0xc2, Some(d)) => Some(D::Known(d.saturating_add(1))),
            (0xc3, Some(0)) => Some(D::Conflict),
            (0xc3, Some(d)) => Some(D::Known(d - 1)),
            (_, Some(d)) => Some(D::Known(d)),
        };
        if let Some(out) = out {
            for succ in bytecode_analysis::lenient_successors(code, code_len, here) {
                joins.push((succ, out));
            }
        }
        for (succ, s) in joins {
            if succ >= code_len {
                continue;
            }
            let merged = match (at[succ], s) {
                (D::Unreached, s) => s,
                (D::Known(a), D::Known(b)) if a == b => D::Known(a),
                _ => D::Conflict,
            };
            if merged != at[succ] {
                at[succ] = merged;
                work.push(succ);
            }
        }
    }
    match at[pc] {
        D::Known(d) => Some(d),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(code: &[u8], table: &[(usize, usize, usize)], all: &[bool]) -> Option<usize> {
        unbalanced_monitor_site(code, code.len(), table, all, BalanceRules::ALL).map(|(pc, _)| pc)
    }

    fn site_depth_only(code: &[u8]) -> Option<usize> {
        unbalanced_monitor_site(code, code.len(), &[], &[], BalanceRules::DEPTH_ONLY)
            .map(|(pc, _)| pc)
    }

    /// javac's `synchronized (o) { o.hashCode(); }` and two nested blocks.
    #[rustfmt::skip]
    const JAVAC_BLOCK: [u8; 20] = [
        0x2a, 0x59, 0x4c, 0xc2,       // 0: aload_0 dup astore_1 monitorenter
        0x2a, 0xb6, 0x00, 0x02, 0x57, // 4: aload_0 invokevirtual pop
        0x2b, 0xc3,                   // 9: aload_1 monitorexit
        0xa7, 0x00, 0x08,             // 11: goto 19
        0x4d, 0x2b, 0xc3, 0x2c, 0xbf, // 14: astore_2 aload_1 monitorexit aload_2 athrow
        0xb1,                         // 19: return
    ];
    const JAVAC_BLOCK_TABLE: [(usize, usize, usize); 2] = [(4, 11, 14), (14, 17, 14)];

    #[test]
    fn javac_shapes_are_balanced() {
        assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true]), None);
        #[rustfmt::skip]
        let nested = [
            0x2a, 0x59, 0x4c, 0xc2,       // 0: enter a (locals 1, 0)
            0x2b, 0x59, 0x4d, 0xc2,       // 4: enter b (locals 2, 1)
            0x2c, 0xc3,                   // 8: exit b
            0xa7, 0x00, 0x08,             // 10: goto 18
            0x4e, 0x2c, 0xc3, 0x2d, 0xbf, // 13: inner handler
            0x2b, 0xc3,                   // 18: exit a
            0xa7, 0x00, 0x0a,             // 20: goto 30
            0x3a, 0x04, 0x2b, 0xc3, 0x19, 0x04, 0xbf, // 23: outer handler
            0xb1,                         // 30: return
        ];
        let table = [(8, 10, 13), (13, 16, 13), (4, 20, 23), (23, 27, 23)];
        assert_eq!(site(&nested, &table, &[true, true, true, true]), None);
        // kotlinc's `synchronized(lock)`: `aload_1; astore_2; aload_2;
        // monitorenter`, released through local 2.
        #[rustfmt::skip]
        let kotlin = [
            0x2b, 0x4d, 0x2c, 0xc2, // 0: aload_1 astore_2 aload_2 monitorenter
            0x03, 0x3e,             // 4: iconst_0 istore_3
            0x2c, 0xc3,             // 6: aload_2 monitorexit
            0xb1,                   // 8: return
        ];
        assert_eq!(site(&kotlin, &[], &[]), None);
    }

    #[test]
    fn the_depth_rules_refuse_at_their_pc() {
        assert_eq!(site(&[0x2a, 0xc2, 0xb1], &[], &[]), Some(2));
        assert_eq!(site(&[0x2a, 0xc3, 0xb1], &[], &[]), Some(1));
        #[rustfmt::skip]
        let branch = [
            0x1b, 0x99, 0x00, 0x05, // 0: iload_1 ifeq 6
            0x2a, 0xc2,             // 4: aload_0 monitorenter
            0xb1,                   // 6: return
        ];
        assert_eq!(site(&branch, &[], &[]), Some(6));
        #[rustfmt::skip]
        let join = [
            0x1b, 0x99, 0x00, 0x06, // 0: iload_1 ifeq 7
            0x2a, 0xc2,             // 4: aload_0 monitorenter
            0x2a, 0xc3,             // 6: aload_0; 7: monitorexit
            0xb1,                   // 8: return
        ];
        assert_eq!(site(&join, &[], &[]), Some(7));
        assert_eq!(site(&[0x2a, 0xc2, 0x01, 0xbf], &[], &[]), Some(3));
    }

    /// Residual 1: an implicit trap under a monitor with no catch-all.
    #[test]
    fn an_uncovered_implicit_trap_is_refused_only_under_its_rule() {
        // static int m() { o(); monitorenter; o(); monitorexit; return 1; }
        #[rustfmt::skip]
        let fixture = [0xb8, 0x00, 0x01, 0xc2, 0xb8, 0x00, 0x01, 0xc3, 0x04, 0xac];
        assert_eq!(site(&fixture, &[], &[]), Some(4));
        assert_eq!(site_depth_only(&fixture), None);
        // The same call inside javac's catch-all is admitted.
        assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true]), None);
        // A typed handler alone does not end the search.
        assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[false, true]), Some(5));
    }

    /// Residual 2: `enter a; exit b` at the same depth.
    #[test]
    fn an_exit_of_another_local_is_refused_only_under_its_rule() {
        let wrong = [0x2a, 0xc2, 0x2b, 0xc3, 0xb1];
        assert_eq!(site(&wrong, &[], &[]), Some(3));
        assert_eq!(site_depth_only(&wrong), None);
        // A copy `aload_0; astore_2` made while the lock is held is the same
        // object.
        let copied = [0x2a, 0xc2, 0x2a, 0x4d, 0x2c, 0xc3, 0xb1];
        assert_eq!(site(&copied, &[], &[]), None);
        // A store over the only local holding the lock leaves nothing tied to
        // it: the exit through local 1 is not provably the lock's (refused
        // since wave 7; HotSpot refuses it too).
        let forgot = [0x2a, 0xc2, 0x01, 0x4b, 0x2b, 0xc3, 0xb1];
        assert_eq!(site(&forgot, &[], &[]), Some(5));
        assert_eq!(site_depth_only(&forgot), None);
        // Non-LIFO release of two locals: the first exit names the outer.
        // (Under every rule the inner enter, which can raise with the outer
        // held and no handler, is refused first, at pc 3.)
        let non_lifo = [0x2a, 0xc2, 0x2b, 0xc2, 0x2a, 0xc3, 0x2b, 0xc3, 0xb1];
        let operands_only = BalanceRules {
            implicit_traps: false,
            operands: true,
        };
        assert_eq!(
            unbalanced_monitor_site(&non_lifo, non_lifo.len(), &[], &[], operands_only)
                .map(|(pc, _)| pc),
            Some(5)
        );
        assert_eq!(site(&non_lifo, &[], &[]), Some(3));
        assert_eq!(
            site_depth_only(&[0x2a, 0xc2, 0x2b, 0xc2, 0x2a, 0xc3, 0x2b, 0xc3, 0xb1]),
            None
        );
    }

    const OPERANDS_ONLY: BalanceRules = BalanceRules {
        implicit_traps: false,
        operands: true,
    };

    fn site_operands_only(code: &[u8]) -> Option<usize> {
        unbalanced_monitor_site(code, code.len(), &[], &[], OPERANDS_ONLY).map(|(pc, _)| pc)
    }

    /// Wave 7 (review w45 item 2): the lock's identity follows copies made
    /// BEFORE the enter, as HotSpot's `replace_all_CTS_matches` does.
    #[test]
    fn a_copy_made_before_the_enter_is_the_lock() {
        // aload_0; astore_2; aload_0; monitorenter; aload_2; monitorexit; return
        let before = [0x2a, 0x4d, 0x2a, 0xc2, 0x2c, 0xc3, 0xb1];
        assert_eq!(site(&before, &[], &[]), None);
        // The same with a copy of local 1: another object.
        let other = [0x2b, 0x4d, 0x2a, 0xc2, 0x2c, 0xc3, 0xb1];
        assert_eq!(site(&other, &[], &[]), Some(5));
        assert_eq!(site_depth_only(&other), None);
    }

    /// Wave 7: an exit (or enter) operand nothing ties to a local is refused,
    /// not admitted.
    #[test]
    fn an_unknown_operand_is_refused() {
        // aload_0; monitorenter; aload_0; getfield #1; monitorexit; return
        let field = [0x2a, 0xc2, 0x2a, 0xb4, 0x00, 0x01, 0xc3, 0xb1];
        assert_eq!(site_operands_only(&field), Some(6));
        assert_eq!(site_depth_only(&field), None);
        // invokestatic #1; monitorenter; aload_0; monitorexit; return: the
        // locked value is in no local, so the exit cannot be proved to be it.
        let call = [0xb8, 0x00, 0x01, 0xc2, 0x2a, 0xc3, 0xb1];
        assert_eq!(site_operands_only(&call), Some(5));
        assert_eq!(site_depth_only(&call), None);
    }

    /// Wave 7: a join keeps only what both paths prove.
    #[test]
    fn a_join_keeps_an_alias_only_when_both_paths_prove_it() {
        #[rustfmt::skip]
        let mut code = [
            0x2a, 0xc2,             // 0: aload_0 monitorenter
            0x1d, 0x99, 0x00, 0x08, // 2: iload_3 ifeq 11
            0x2a, 0x4d,             // 6: aload_0 astore_2
            0xa7, 0x00, 0x05,       // 8: goto 13
            0x01, 0x4d,             // 11: aconst_null astore_2
            0x2c, 0xc3,             // 13: aload_2 monitorexit
            0xb1,                   // 15: return
        ];
        assert_eq!(site(&code, &[], &[]), Some(14));
        code[11] = 0x2a; // 11: aload_0 astore_2 on both paths
        assert_eq!(site(&code, &[], &[]), None);
    }

    /// Wave 7: `o = this; while (..) { synchronized (o) { if (c) {} } o = f(); }`
    /// -- the lock local changes between iterations; the alias inside the
    /// block survives the refinement of the loop header's state.
    #[test]
    fn a_block_in_a_loop_over_a_changing_local_is_balanced() {
        #[rustfmt::skip]
        let code = [
            0x2a, 0x4c,                   //  0: aload_0 astore_1
            0x2b, 0x59, 0x4d, 0xc2,       //  2: aload_1 dup astore_2 monitorenter
            0x1d, 0x99, 0x00, 0x04,       //  6: iload_3 ifeq 11
            0x00,                         // 10: nop
            0x2c, 0xc3,                   // 11: aload_2 monitorexit
            0xb8, 0x00, 0x01, 0x4c,       // 13: invokestatic astore_1
            0x1d, 0x9a, 0x00, 0x06,       // 17: iload_3 ifne 24
            0xa7, 0xff, 0xed,             // 21: goto 2
            0xb1,                         // 24: return
        ];
        assert_eq!(site(&code, &[], &[]), None);
    }

    /// Wave 7: an exhausted work budget refuses (the gate fails closed).
    #[test]
    fn an_exhausted_budget_refuses() {
        let tiny = walk_balance(
            &JAVAC_BLOCK,
            JAVAC_BLOCK.len(),
            &JAVAC_BLOCK_TABLE,
            &[true, true],
            BalanceRules::ALL,
            Some(1),
        );
        assert_eq!(tiny.map(|(pc, _)| pc), Some(0));
        assert_eq!(site(&JAVAC_BLOCK, &JAVAC_BLOCK_TABLE, &[true, true]), None);
    }

    /// Wave 10 (review w68 item 1): javac's block with its lock temporary in
    /// slot 2000 (`wide astore` / `wide aload`), in a method whose locals
    /// reach slot 4999, is balanced; the same with the exit through another
    /// high slot is refused.
    #[test]
    fn a_lock_local_above_slot_1023_is_tracked() {
        #[rustfmt::skip]
        let mut code = [
            0x2a, 0x59, 0xc4, 0x3a, 0x07, 0xd0, 0xc2, // 0: aload_0 dup wide astore 2000; 6: monitorenter
            0x2a, 0xb6, 0x00, 0x02, 0x57,             // 7: aload_0 invokevirtual pop
            0xc4, 0x19, 0x07, 0xd0, 0xc3,             // 12: wide aload 2000; 16: monitorexit
            0xa7, 0x00, 0x16,                         // 17: goto 39
            0xc4, 0x3a, 0x07, 0xd1,                   // 20: wide astore 2001
            0xc4, 0x19, 0x07, 0xd0, 0xc3,             // 24: wide aload 2000; 28: monitorexit
            0x03, 0xc4, 0x36, 0x13, 0x87,             // 29: iconst_0 wide istore 4999
            0xc4, 0x19, 0x07, 0xd1, 0xbf,             // 34: wide aload 2001; 38: athrow
            0xb1,                                     // 39: return
        ];
        let table = [(7, 17, 20), (20, 29, 20)];
        assert_eq!(site(&code, &table, &[true, true]), None);
        // The normal-path exit through slot 2002: another object.
        code[15] = 0xd2;
        assert_eq!(site(&code, &table, &[true, true]), Some(16));
        let rules = BalanceRules::DEPTH_ONLY;
        assert_eq!(
            unbalanced_monitor_site(&code, code.len(), &table, &[true, true], rules),
            None
        );
    }

    /// Wave 10: a method with 5000 locals whose lock reaches its enter
    /// through copies in slots 3000 and 4000 and is released through
    /// `aload_0`: the copy chain's source slots are tracked although no
    /// monitor op names them.
    #[test]
    fn a_copy_chain_through_untouched_high_slots_is_followed() {
        #[rustfmt::skip]
        let mut code = [
            0x2a, 0xc4, 0x3a, 0x0b, 0xb8, // 0: aload_0 wide astore 3000
            0xc4, 0x19, 0x0b, 0xb8,       // 5: wide aload 3000
            0xc4, 0x3a, 0x0f, 0xa0,       // 9: wide astore 4000
            0x03, 0xc4, 0x36, 0x13, 0x87, // 13: iconst_0 wide istore 4999
            0xc4, 0x19, 0x0f, 0xa0, 0xc2, // 18: wide aload 4000; 22: monitorenter
            0x2a, 0xc3,                   // 23: aload_0 monitorexit
            0xb1,                         // 25: return
        ];
        assert_eq!(site(&code, &[], &[]), None);
        // Only the slots that can carry the lock are cells.
        let shape = Shape::new(&code, code.len(), &[]);
        let (tracked, enters) = Tracked::new(&code, code.len(), &shape);
        assert_eq!(tracked.slots, vec![0, 3000, 4000]);
        assert_eq!(enters, 1);
        // The chain starts from local 1 instead: another object.
        code[0] = 0x2b;
        assert_eq!(site(&code, &[], &[]), Some(24));
    }

    /// Wave 10 (review w68 item 3): the allocation-free meet numbers its
    /// classes by first occurrence over the locals then the monitors.
    #[test]
    fn the_meet_is_canonical() {
        let mut s = MeetScratch::default();
        let a = Aliases {
            locals: vec![7, 7, 9],
            monitors: vec![7],
            next: 10,
        };
        let b = Aliases {
            locals: vec![4, 5, 5],
            monitors: vec![4],
            next: 6,
        };
        assert_eq!(meet_into(&a, &b, &mut s), 3);
        assert_eq!(s.out, vec![0, 1, 2, 0]);
        assert_eq!(meet_into(&a, &a, &mut s), 2);
        assert_eq!(s.out, vec![0, 0, 1, 0]);
        let empty = Aliases::empty();
        assert_eq!(meet_into(&empty, &empty, &mut s), 0);
        assert!(s.out.is_empty());
    }

    /// Wave 10: a 30 KiB block of calls under javac's catch-all (one handler
    /// merge per call) passes within the proven budget.
    #[test]
    fn a_huge_covered_block_passes_within_its_budget() {
        const CALLS: usize = 10_000;
        let mut code = vec![0x2a, 0x59, 0x4c, 0xc2]; // aload_0 dup astore_1 monitorenter
        for _ in 0..CALLS {
            code.extend_from_slice(&[0xb8, 0x00, 0x01]); // invokestatic #1
        }
        let exit = code.len();
        code.extend_from_slice(&[0x2b, 0xc3]); // aload_1 monitorexit
        let goto = code.len();
        code.extend_from_slice(&[0xa7, 0x00, 0x08]); // goto return
        let handler = code.len();
        code.extend_from_slice(&[0x4d, 0x2b, 0xc3, 0x2c, 0xbf]); // astore_2 aload_1 monitorexit aload_2 athrow
        code.push(0xb1);
        assert_eq!(code.len(), handler + 6);
        assert_eq!(exit + 2, goto);
        let table = [(4, goto, handler), (handler, handler + 3, handler)];
        assert_eq!(site(&code, &table, &[true, true]), None);
    }

    #[test]
    fn what_is_not_analysed_is_admitted() {
        assert_eq!(site(&[0x03, 0xac], &[], &[]), None);
        assert_eq!(site(&[0x2a, 0xc2, 0xb1], &[(0, 3, 3)], &[]), None);
    }

    #[test]
    fn the_depth_answer_is_per_pc_and_lenient() {
        let at = |code: &[u8], pc: usize| block_monitor_depth_at(code, code.len(), &[], &[], pc);
        // exitOnly: nothing of the frame's own is held at its exit.
        assert_eq!(at(&[0x2a, 0xc3, 0xb1], 1), Some(0));
        // exitWrong: one of its own is held at the exit of `b`.
        assert_eq!(at(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], 3), Some(1));
        // javac's block: one held at both releases, the handler's included.
        let t = |pc| {
            block_monitor_depth_at(&JAVAC_BLOCK, JAVAC_BLOCK.len(), &JAVAC_BLOCK_TABLE, &[true, true], pc)
        };
        assert_eq!(t(10), Some(1));
        assert_eq!(t(16), Some(1));
        assert_eq!(t(19), Some(0));
        // Paths that disagree answer nothing past the join.
        #[rustfmt::skip]
        let join = [0x1b, 0x99, 0x00, 0x06, 0x2a, 0xc2, 0x2a, 0xc3, 0xb1];
        assert_eq!(at(&join, 7), None);
        // Not analysed: no answer.
        assert_eq!(at(&[0x03, 0xac], 0), None);
    }
}
