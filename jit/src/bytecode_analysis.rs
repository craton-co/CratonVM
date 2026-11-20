// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! One bytecode decoder for both JIT tiers.
//!
//! Every analysis in this crate that walks raw JVM bytecode needs the same
//! facts: how long an instruction is, where it can jump, whether it falls
//! through, which pcs are reachable and which are loop headers. Each pass used
//! to derive them with a private copy. The copies drifted: one length table
//! lacked `ldc`, another lacked `jsr`/`ret`, a successor function dropped
//! switch edges, and a loop finder saw only 16-bit branches. Every drift was a
//! latent miscompile (see `two-optimizing-front-ends-duplicate-bytecode-analyses-FIXED-20260912.md`).
//!
//! This module is now the only place those facts are decoded:
//!
//! * [`insn_len`] / [`step`] — the instruction-length table (`wide`,
//!   `tableswitch`/`lookupswitch` padding, the 5-byte invokes and wide branches).
//! * [`offset_branch_target`], [`switch_table`], [`switch_targets_lenient`],
//!   [`explicit_targets`], [`falls_through`] — control transfer.
//! * [`instruction_starts`], [`branch_target_map`], [`reachable_pcs`],
//!   [`back_edges`] — whole-method maps.
//! * [`InsnCfg`] — an instruction-granularity CFG with exception edges,
//!   reverse post-order, dominators, loop headers and natural loop bodies.
//!
//! Two policies coexist on purpose, and each function says which it follows.
//! A *strict* answer (`Option`/`bool` refusal) is for transforms, which must
//! not guess. A *lenient* answer (skip what cannot be decoded) is for
//! analyses where a superset of edges is the conservative direction, such as
//! liveness. Both read the same decoded bytes.
//!
//! Bytecode liveness stays in `regalloc.rs` (`live_locals_per_pc_all` and
//! friends): it is the crate's only liveness over bytecode, and its CFG is
//! built from the decoders here.

use crate::x64::{checked_lookupswitch_npairs, checked_tableswitch_count};

/// One exception-table entry as `(start_pc, end_pc, handler_pc)`, with
/// `end_pc` exclusive — the spelling every caller in the crate already uses.
pub(crate) type HandlerRange = (usize, usize, usize);

/// Sentinel for "no such node" / "unreachable" in [`InsnCfg`].
const NONE: usize = usize::MAX;

#[inline]
fn read_i32(code: &[u8], at: usize) -> Option<i32> {
    let b = code.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Offset of a switch's first operand byte (its `default` offset): the byte
/// after the opcode rounded up to a multiple of four, counted from the start of
/// the code array (JVMS §6.5 `tableswitch`).
///
/// # The padding is relative to the BUFFER, so a relocated body cannot use this
///
/// The rounding is "up to the next multiple of four *from index zero of this
/// slice*". That is exactly right for a method's own `Code` array, which is
/// what JVMS specifies. It is **wrong** for a callee body that the IR tier has
/// relocated into a combined buffer at some base `b`: the padding bytes in that
/// body were emitted by javac relative to the callee's own array, i.e. at
/// `(pc_rel + 1 + 3) & !3`, and `(b + pc_rel + 1 + 3) & !3` equals
/// `b + ((pc_rel + 1 + 3) & !3)` only when `b` is itself a multiple of four.
/// For any other base the decode lands one to three bytes off and reads
/// whatever is there as the `default` offset.
///
/// [`switch_table`] is therefore bounded by the caller's `code_len` on purpose,
/// and answers `None` for a pc inside a splice — which makes
/// `IrBuilder::build`'s switch arm refuse the method rather than decode it.
/// A "fix" that widened that bound to `code.len()` without also rebasing
/// (`switch_operand_base(pc - b) + b`) would turn a refusal into a miscompile.
#[inline]
pub(crate) fn switch_operand_base(pc: usize) -> usize {
    (pc + 1 + 3) & !3
}

/// Byte length of the instruction at `pc`.
///
/// `None` when `pc` is outside `code`, or a switch's header does not fit in
/// `code` or declares a count [`checked_tableswitch_count`] /
/// [`checked_lookupswitch_npairs`] reject. The length does NOT require the
/// whole instruction to fit: a caller that needs that checks
/// `pc + len <= code_len` (as [`decode_at`] does).
///
/// A truncated `wide` with no modified opcode reads as the 4-byte form.
pub fn insn_len(code: &[u8], pc: usize) -> Option<usize> {
    let op = *code.get(pc)?;
    Some(match op {
        // bipush, ldc, [ilfda]load, [ilfda]store, ret, newarray
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        // sipush, ldc_w, ldc2_w, iinc, if*, goto, jsr, field and 3-byte
        // invokes, new, anewarray, checkcast, instanceof, ifnull, ifnonnull
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa8
        | 0xb2..=0xb8
        | 0xbb
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        // multianewarray
        0xc5 => 4,
        // invokeinterface, invokedynamic, goto_w, jsr_w
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        // wide: `wide iinc` is six bytes, every other widened form four.
        0xc4 => {
            if code.get(pc + 1) == Some(&0x84) {
                6
            } else {
                4
            }
        }
        0xaa => {
            let base = switch_operand_base(pc);
            let low = read_i32(code, base + 4)?;
            let high = read_i32(code, base + 8)?;
            let count = checked_tableswitch_count(low, high)?;
            count.checked_mul(4)?.checked_add(base + 12)? - pc
        }
        0xab => {
            let base = switch_operand_base(pc);
            let npairs = checked_lookupswitch_npairs(read_i32(code, base + 4)?)?;
            npairs.checked_mul(8)?.checked_add(base + 8)? - pc
        }
        _ => 1,
    })
}

/// [`insn_len`] for a linear walk: an undecodable instruction advances one
/// byte, so the walk always terminates. The method is rejected elsewhere
/// (`jit_scan`, the emitter's own switch guards); a walker only has to stay in
/// bounds.
#[inline]
pub(crate) fn step(code: &[u8], pc: usize) -> usize {
    insn_len(code, pc).unwrap_or(1)
}

/// One decoded instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Insn {
    /// Offset of the opcode byte.
    pub(crate) pc: usize,
    /// Byte length, prefix and padding included.
    pub(crate) len: usize,
    /// The byte at `pc` (`0xc4` for a widened instruction).
    pub(crate) opcode: u8,
    /// The opcode that executes: the byte after the prefix when `wide`.
    pub(crate) op: u8,
    /// `true` for a `wide`-prefixed instruction.
    pub(crate) wide: bool,
}

impl Insn {
    /// Offset of the textually next instruction.
    #[inline]
    pub(crate) fn next_pc(&self) -> usize {
        self.pc + self.len
    }
}

/// Strictly decode the instruction at `pc`: `None` unless it lies wholly
/// inside `code[..code_len]` and, for `wide`, modifies an opcode JVMS §6.5
/// allows (`[ilfda]load`, `[ilfda]store`, `ret`, `iinc`).
pub(crate) fn decode_at(code: &[u8], code_len: usize, pc: usize) -> Option<Insn> {
    let code_len = code_len.min(code.len());
    if pc >= code_len {
        return None;
    }
    let len = insn_len(&code[..code_len], pc)?;
    if pc.checked_add(len)? > code_len {
        return None;
    }
    let opcode = code[pc];
    let (op, wide) = if opcode == 0xc4 {
        let op = code[pc + 1];
        if !matches!(op, 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0x84) {
            return None;
        }
        (op, true)
    } else {
        (opcode, false)
    };
    Some(Insn {
        pc,
        len,
        opcode,
        op,
        wide,
    })
}

/// Every instruction of the method in pc order, or `None` when any one fails
/// [`decode_at`] (so the walk would not land exactly on `code_len`).
pub(crate) fn decode_method(code: &[u8], code_len: usize) -> Option<Vec<Insn>> {
    let code_len = code_len.min(code.len());
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        let insn = decode_at(code, code_len, pc)?;
        pc = insn.next_pc();
        out.push(insn);
    }
    Some(out)
}

/// `true` for the opcodes whose one explicit target is a signed offset: the
/// `if*` family, `goto`, `jsr`, `ifnull`/`ifnonnull` (16-bit) and
/// `goto_w`/`jsr_w` (32-bit).
#[inline]
pub(crate) fn is_offset_branch(op: u8) -> bool {
    matches!(op, 0x99..=0xa8 | 0xc6..=0xc9)
}

/// `true` for the subroutine opcodes (`jsr`, `ret`, `jsr_w`), whose successor
/// sets are not statically known.
#[inline]
pub(crate) fn is_subroutine_op(op: u8) -> bool {
    matches!(op, 0xa8 | 0xa9 | 0xc9)
}

/// `true` when the instruction at `pc` is `wide ret` (`0xc4 0xa9`): a `ret`
/// with a two-byte local index.
///
/// Every opcode-keyed predicate in this module sees `0xc4` for it, and `0xc4`
/// is neither a subroutine op nor a non-falling-through one — so without this
/// check the strict successor functions read `wide ret` as an ordinary
/// instruction that falls through to the next pc, which is exactly the
/// "successor set is not statically known" shape they exist to refuse.
#[inline]
pub(crate) fn is_wide_ret(code: &[u8], pc: usize) -> bool {
    code.get(pc) == Some(&0xc4) && code.get(pc + 1) == Some(&0xa9)
}

/// `true` when control can reach the textually next instruction: false for
/// `goto`/`goto_w`, both switches, every `*return`, `athrow` and `ret`.
/// `jsr`/`jsr_w` do fall through — the subroutine returns there.
#[inline]
pub(crate) fn falls_through(op: u8) -> bool {
    !matches!(op, 0xa7 | 0xa9 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf | 0xc8)
}

/// `true` for the method-exit opcodes: every `*return` and `athrow`.
#[inline]
pub(crate) fn is_exit(op: u8) -> bool {
    matches!(op, 0xac..=0xb1 | 0xbf)
}

/// Absolute target of the offset branch at `pc` ([`is_offset_branch`]).
///
/// `None` for any other opcode, when the offset bytes are missing, or when the
/// target is negative. NOT checked against the method end: a caller that needs
/// an in-range target filters on `t < code_len`.
pub(crate) fn offset_branch_target(code: &[u8], pc: usize) -> Option<usize> {
    let off = match *code.get(pc)? {
        0x99..=0xa8 | 0xc6 | 0xc7 => {
            i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]) as isize
        }
        0xc8 | 0xc9 => read_i32(code, pc + 1)? as isize,
        _ => return None,
    };
    pc.checked_add_signed(off)
}

/// A fully decoded `tableswitch` / `lookupswitch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SwitchTable {
    /// Byte length of the whole instruction.
    pub(crate) len: usize,
    /// Absolute default target.
    pub(crate) default: usize,
    /// `(match value, absolute target)` per case, in table order.
    pub(crate) cases: Vec<(i32, usize)>,
}

impl SwitchTable {
    /// The default target followed by every case target, in table order.
    pub(crate) fn targets(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.default).chain(self.cases.iter().map(|&(_, t)| t))
    }
}

/// STRICT switch decode. `None` unless the instruction at `pc` is a switch
/// whose table lies wholly inside `code[..code_len]`, whose count passes the
/// dense/sparse caps, and whose every target is in `[0, code_len)`.
pub(crate) fn switch_table(code: &[u8], code_len: usize, pc: usize) -> Option<SwitchTable> {
    let code_len = code_len.min(code.len());
    if pc >= code_len {
        return None;
    }
    let base = switch_operand_base(pc);
    let target_of = |off: i32| -> Option<usize> {
        pc.checked_add_signed(off as isize)
            .filter(|&t| t < code_len)
    };
    match code[pc] {
        0xaa => {
            if base + 12 > code_len {
                return None;
            }
            let default = read_i32(code, base)?;
            let low = read_i32(code, base + 4)?;
            let high = read_i32(code, base + 8)?;
            let count = checked_tableswitch_count(low, high)?;
            let end = count.checked_mul(4)?.checked_add(base + 12)?;
            if end > code_len {
                return None;
            }
            let mut cases = Vec::with_capacity(count);
            for i in 0..count {
                let off = read_i32(code, base + 12 + i * 4)?;
                // `low + i <= high`, so the match value cannot overflow.
                cases.push((low + i as i32, target_of(off)?)); // Cast: i < count <= 2^24
            }
            Some(SwitchTable {
                len: end - pc,
                default: target_of(default)?,
                cases,
            })
        }
        0xab => {
            if base + 8 > code_len {
                return None;
            }
            let default = read_i32(code, base)?;
            let npairs = checked_lookupswitch_npairs(read_i32(code, base + 4)?)?;
            let end = npairs.checked_mul(8)?.checked_add(base + 8)?;
            if end > code_len {
                return None;
            }
            let mut cases = Vec::with_capacity(npairs);
            for i in 0..npairs {
                let pair = base + 8 + i * 8;
                cases.push((read_i32(code, pair)?, target_of(read_i32(code, pair + 4)?)?));
            }
            Some(SwitchTable {
                len: end - pc,
                default: target_of(default)?,
                cases,
            })
        }
        _ => None,
    }
}

/// LENIENT switch targets: the default and every case target that decodes
/// and lies in `[0, code_len)`, stopping at the first entry that does not fit.
/// An over-cap count yields the default alone. Empty for a non-switch.
///
/// For analyses where more edges is the safe direction. A transform wants
/// [`switch_table`].
pub(crate) fn for_each_switch_target_lenient(
    code: &[u8],
    code_len: usize,
    pc: usize,
    mut f: impl FnMut(usize),
) {
    let code_len = code_len.min(code.len());
    if pc >= code_len || !matches!(code[pc], 0xaa | 0xab) {
        return;
    }
    let base = switch_operand_base(pc);
    let fits = |at: usize, n: usize| at.checked_add(n).is_some_and(|e| e <= code_len);
    let mut push = |off: i32| {
        if let Some(t) = pc
            .checked_add_signed(off as isize)
            .filter(|&t| t < code_len)
        {
            f(t);
        }
    };
    if !fits(base, 4) {
        return;
    }
    push(read_i32(code, base).unwrap_or(0));
    if code[pc] == 0xaa {
        if !fits(base, 12) {
            return;
        }
        let (Some(low), Some(high)) = (read_i32(code, base + 4), read_i32(code, base + 8)) else {
            return;
        };
        let Some(count) = checked_tableswitch_count(low, high) else {
            return;
        };
        for i in 0..count {
            let at = base + 12 + i * 4;
            if !fits(at, 4) {
                break;
            }
            push(read_i32(code, at).unwrap_or(0));
        }
    } else {
        if !fits(base, 8) {
            return;
        }
        let Some(npairs) = read_i32(code, base + 4).and_then(checked_lookupswitch_npairs) else {
            return;
        };
        for i in 0..npairs {
            let at = base + 8 + i * 8;
            if !fits(at, 8) {
                break;
            }
            push(read_i32(code, at + 4).unwrap_or(0));
        }
    }
}

pub(crate) fn switch_targets_lenient(code: &[u8], code_len: usize, pc: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for_each_switch_target_lenient(code, code_len, pc, |t| out.push(t));
    out
}

/// STRICT explicit targets of the instruction at `pc`, appended to `out`
/// (the fall-through successor is NOT included).
///
/// Returns `false` — *refuse* — when the successor set is not statically known
/// (`jsr`/`jsr_w`/`ret`/`wide ret`), the encoding is malformed, or a target
/// falls outside `[0, code_len)`. Nothing is appended on refusal. Callers must
/// treat `false` as opaque, not as "no targets".
pub(crate) fn explicit_targets(
    code: &[u8],
    code_len: usize,
    pc: usize,
    out: &mut Vec<usize>,
) -> bool {
    if code_len > code.len() || pc >= code_len {
        return false;
    }
    let op = code[pc];
    match op {
        0xa8 | 0xa9 | 0xc9 => false,
        // `wide ret` is a `ret`: refuse it the same way, rather than letting
        // the `_` arm below call it a target-less instruction that falls
        // through (see `is_wide_ret`).
        0xc4 if is_wide_ret(code, pc) => false,
        0x99..=0xa7 | 0xc6 | 0xc7 | 0xc8 => {
            let last_operand = if op == 0xc8 { pc + 4 } else { pc + 2 };
            if last_operand >= code_len {
                return false;
            }
            match offset_branch_target(code, pc).filter(|&t| t < code_len) {
                Some(t) => {
                    out.push(t);
                    true
                }
                None => false,
            }
        }
        0xaa | 0xab => match switch_table(code, code_len, pc) {
            Some(table) => {
                out.extend(table.targets());
                true
            }
            None => false,
        },
        _ => true,
    }
}

/// STRICT normal successors of the instruction at `pc`: [`explicit_targets`]
/// plus the fall-through when [`falls_through`] and it lies before `code_len`.
/// Returns `false` (and appends nothing) on refusal.
pub(crate) fn normal_successors(
    code: &[u8],
    code_len: usize,
    pc: usize,
    out: &mut Vec<usize>,
) -> bool {
    if !explicit_targets(code, code_len, pc, out) {
        return false;
    }
    if falls_through(code[pc]) {
        let next = pc + step(code, pc);
        if next < code_len {
            out.push(next);
        }
    }
    true
}

/// `starts[k]` is `true` iff `k` is the first byte of an instruction reached by
/// the linear walk from pc 0. Sized `code_len`.
///
/// The only reliable way to know whether an offset is an instruction boundary:
/// a backward scan (`code[pc - 1]`) can mistake an operand byte for an opcode.
pub(crate) fn instruction_starts(code: &[u8], code_len: usize) -> Vec<bool> {
    let mut starts = vec![false; code_len];
    let mut pc = 0usize;
    while pc < code_len {
        starts[pc] = true;
        pc += step(code, pc);
    }
    starts
}

/// LENIENT branch-target map: `targets[t]` is `true` when some instruction on
/// the linear walk names `t` as an explicit target (conditional branches,
/// `goto`/`goto_w`, `jsr`/`jsr_w` and every decodable switch entry). Sized
/// `code_len`.
pub(crate) fn branch_target_map(code: &[u8], code_len: usize) -> Vec<bool> {
    let code_len_c = code_len.min(code.len());
    let mut targets = vec![false; code_len];
    let mut pc = 0usize;
    while pc < code_len_c {
        let op = code[pc];
        if is_offset_branch(op) {
            if let Some(t) = offset_branch_target(&code[..code_len_c], pc) {
                if t < code_len_c {
                    targets[t] = true;
                }
            }
        } else if matches!(op, 0xaa | 0xab) {
            for t in switch_targets_lenient(code, code_len_c, pc) {
                targets[t] = true;
            }
        }
        pc += step(code, pc);
    }
    targets
}

/// Every backward explicit edge on the linear walk as `(target, source)` —
/// target `<=` source — in source order. LENIENT decoding (as
/// [`branch_target_map`]); a switch contributes each distinct backward target
/// once.
///
/// This is the *textual* back-edge set. For rotated (bottom-tested) loops it
/// names the body start, not the dominating test; use
/// [`InsnCfg::loop_headers`] for dominance-based headers.
pub(crate) fn back_edges(code: &[u8], code_len: usize) -> Vec<(usize, usize)> {
    let code_len = code_len.min(code.len());
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        if is_offset_branch(op) {
            if let Some(t) = offset_branch_target(&code[..code_len], pc) {
                if t < code_len && t <= pc {
                    out.push((t, pc));
                }
            }
        } else if matches!(op, 0xaa | 0xab) {
            let mut ts: Vec<usize> = switch_targets_lenient(code, code_len, pc)
                .into_iter()
                .filter(|&t| t <= pc)
                .collect();
            ts.sort_unstable();
            ts.dedup();
            out.extend(ts.into_iter().map(|t| (t, pc)));
        }
        pc += step(code, pc);
    }
    out
}

/// [`reachable_pcs`] from pc 0 alone with the normal-flow edges `removed`
/// (`(from, to)`) taken out: what stays reachable once those branches' taken
/// edges are pruned (JIT round 23 wave 4, CF23-2). Same refusals.
pub(crate) fn reachable_pcs_without(
    code: &[u8],
    code_len: usize,
    removed: &[(usize, usize)],
) -> Option<Vec<bool>> {
    if code_len > code.len() {
        return None;
    }
    let mut reachable = vec![false; code_len + 1];
    if code_len == 0 {
        return Some(reachable);
    }
    reachable[0] = true;
    let mut work = vec![0usize];
    let mut succs: Vec<usize> = Vec::new();
    while let Some(pc) = work.pop() {
        succs.clear();
        if !normal_successors(code, code_len, pc, &mut succs) {
            return None;
        }
        for &s in &succs {
            if !reachable[s] && !removed.contains(&(pc, s)) {
                reachable[s] = true;
                work.push(s);
            }
        }
    }
    Some(reachable)
}

/// Pcs reachable from the method entry (and each of `extra_roots`) along
/// ORDINARY control flow — fall-through plus explicit branch and switch edges.
/// Sized `code_len + 1`.
///
/// Exception handlers are roots only when passed in `extra_roots`: a compiled
/// body enters its own handler only when it runs local handlers.
///
/// STRICT: `None` when any reached instruction's successors are not statically
/// known (`jsr`/`ret`/`jsr_w`) or its encoding is malformed.
pub(crate) fn reachable_pcs(
    code: &[u8],
    code_len: usize,
    extra_roots: &[usize],
) -> Option<Vec<bool>> {
    if code_len > code.len() {
        return None;
    }
    let mut reachable = vec![false; code_len + 1];
    if code_len == 0 {
        return Some(reachable);
    }
    reachable[0] = true;
    let mut work = vec![0usize];
    for &root in extra_roots {
        if root < code_len && !reachable[root] {
            reachable[root] = true;
            work.push(root);
        }
    }
    let mut succs: Vec<usize> = Vec::new();
    while let Some(pc) = work.pop() {
        // A branch into the middle of an instruction decodes garbage from
        // there on; the walk stays bounded by `code_len`, and such a method is
        // rejected where branch targets are resolved to native offsets.
        succs.clear();
        if !normal_successors(code, code_len, pc, &mut succs) {
            return None;
        }
        for &s in &succs {
            if !reachable[s] {
                reachable[s] = true;
                work.push(s);
            }
        }
    }
    Some(reachable)
}

/// Forward `goto`s that a single linear abstract interpretation may walk
/// THROUGH without treating the transfer as a merge.
///
/// Returned map is indexed by pc and sized `code_len`; `true` only for a plain
/// `goto` (`0xa7`) at that pc.
///
/// `goto_w` (`0xc8`) is deliberately excluded, and the reason is agreement
/// rather than difficulty. The two walks in `x64::escape_analysis` must model
/// the same bytecode identically -- their drift is what
/// `plan_scalar_replacement`'s four-paragraph `poisoned` comment is about --
/// and `goto_w` reaches the second walk's catch-all, which poisons everything
/// live. Admitting it here would make `analyze_escapes` hand over an object
/// the plan then demotes: safe, but a silent asymmetry between the pair for no
/// gain, since `goto_w` is only emitted for a jump beyond 32 KiB and the shape
/// this exists for (javac's skip over a `synchronized`/`finally` handler) is
/// always a few bytes.
///
/// # What this is for
///
/// `x64::escape_analysis`'s two walks treat every control transfer as a hard
/// barrier: they escape (or poison) every tracked object on the stack AND in
/// every local, then drop all provenance. The barrier is correct and the
/// miscompile that produced it is named in that file (bc
/// `org.bouncycastle.math.ec`, a SEGV in the young-gen scavenge copy loop) —
/// but it is blunt. It treats an unconditional forward `goto` with exactly one
/// successor and no merge at its target the same as a conditional branch into
/// a join.
///
/// The shape that costs is javac's, around every `synchronized` block and
/// every `try`/`finally`:
///
/// ```text
///   18: monitorexit
///   19: goto 27        <-- barrier fires: escapes the receiver out of its locals
///   22: astore_3       <-- reachable ONLY through the exception table
///   ...
///   26: athrow
///   27: return
/// ```
///
/// Nothing merges at 27 and nothing between 19 and 27 runs on this path, so
/// the state at 27 is exactly the state at 19 — but the barrier cannot see it.
///
/// # The three conditions, and why each is load-bearing
///
/// A `goto` at `p` targeting `t` qualifies when all of:
///
/// 1. **`t > p`.** A back edge is a real loop merge and the linear state at
///    the target is not the state on every incoming edge.
/// 2. **`t`'s only ORDINARY predecessor is `p`.** Anything else reaching `t`
///    is a join, which is what the barrier exists for. Counted over the whole
///    method, so a later back edge into `t` disqualifies it too.
/// 3. **No pc in `(p, t)` is ordinarily reachable.** The skipped region must be
///    exclusively handler code. Without this the walk would jump over
///    instructions that DO run on some ordinary path — including `astore`s,
///    which is precisely how a stale provenance would bind a later field op to
///    the wrong object.
///
/// And a fourth condition this function ENFORCES rather than assumes:
///
/// 4. **`t` is not an exception-handler entry.** A handler entry has an
///    extraordinary predecessor by construction, so the state on the edge that
///    reaches it is not `p`'s. `handler_pcs` is what makes condition 2's
///    "ordinary" honest.
///
/// # Why `handler_pcs` is not optional
///
/// The caller must pass its method's handler entry pcs. Passing an empty slice
/// where the method HAS handlers is a silent unsoundness of exactly the shape
/// this whole file's comment history is about, so callers that do not have the
/// exception table to hand must not call this at all — see
/// `x64::escape_analysis::analyze_escapes`, whose parameter is an `Option` for
/// that reason.
///
/// # Failure is total, not partial
///
/// `reachable_pcs` answers `None` for a method containing `jsr`/`ret` or a
/// malformed encoding. This then returns an all-`false` map: every `goto` stays
/// a barrier, which is today's behaviour.
pub(crate) fn straight_line_gotos(
    code: &[u8],
    code_len: usize,
    handler_pcs: &[usize],
) -> Vec<bool> {
    let mut out = vec![false; code_len];
    let code_len_c = code_len.min(code.len());
    if code_len_c == 0 {
        return out;
    }
    // Ordinary reachability with NO handler roots: an unreachable pc here is
    // exactly "reachable only through the exception table", which is
    // condition 3.
    let Some(reachable) = reachable_pcs(code, code_len_c, &[]) else {
        return out;
    };
    // Predecessors are counted over everything that can run, handler code
    // included (round 11). A handler body that FALLS THROUGH into `t` — `try
    // { A } catch (E e) { B } C`, where `B` ends by running into `C` — is an
    // ordinary edge out of code only an exception reaches, and the count
    // above missed it: `preds[C]` read 1, the `goto C` qualified, and the
    // linear state at `C` was taken to be the `goto`'s although the handler
    // path arrives with its own. Condition 2 is about every edge into `t`
    // that is not an exception edge itself, wherever it starts.
    let Some(runnable) = reachable_pcs(code, code_len_c, handler_pcs) else {
        return out;
    };

    // Ordinary predecessor counts, over the runnable instruction starts only —
    // an unreachable instruction's successors are not edges the walk can take.
    let mut preds: Vec<u32> = vec![0; code_len_c + 1];
    let mut succs: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len_c {
        if runnable.get(pc).copied().unwrap_or(false) {
            succs.clear();
            if !normal_successors(code, code_len_c, pc, &mut succs) {
                return out;
            }
            for &s in &succs {
                if s <= code_len_c {
                    preds[s] = preds[s].saturating_add(1);
                }
            }
        }
        pc += step(code, pc);
    }

    pc = 0;
    while pc < code_len_c {
        let op = code[pc];
        if op == 0xa7 && reachable.get(pc).copied().unwrap_or(false) {
            if let Some(t) = offset_branch_target(&code[..code_len_c], pc) {
                let len = step(code, pc);
                let gap_is_handler_only =
                    (pc + len..t).all(|g| !reachable.get(g).copied().unwrap_or(false));
                if t > pc
                    && t < code_len_c
                    && preds[t] == 1
                    && gap_is_handler_only
                    && !handler_pcs.contains(&t)
                {
                    out[pc] = true;
                }
            }
        }
        pc += step(code, pc);
    }
    out
}

/// Instruction-granularity control-flow graph of one method, with immediate
/// dominators.
///
/// Built to answer the questions a transform must not guess at: is a region
/// single-entry, is a loop reducible, is every instruction reachable, which
/// pcs head a loop. Nodes are instruction start pcs in ascending order; node
/// `0` is the method entry.
#[derive(Debug)]
pub(crate) struct InsnCfg {
    /// Instruction start pcs, ascending. Node `i` is `pcs[i]`.
    pcs: Vec<usize>,
    /// `pc` → node index, [`NONE`] when `pc` is not an instruction start.
    idx_of: Vec<usize>,
    /// Successor nodes (normal edges, then exception edges).
    succs: Vec<Vec<usize>>,
    /// Predecessor nodes.
    preds: Vec<Vec<usize>>,
    /// Reachable nodes in reverse post-order; `rpo[0] == 0`.
    rpo: Vec<usize>,
    /// Reverse-post-order number, [`NONE`] when unreachable from the entry.
    rpo_num: Vec<usize>,
    /// Immediate dominator, [`NONE`] when unknown or unreachable.
    idom: Vec<usize>,
}

/// Cooper/Harvey/Kennedy `intersect`: walk two dominator-tree paths up until
/// they meet. [`NONE`] when either chain is incomplete — the dominator then
/// stays unknown, which every query reads as "does not dominate".
fn dom_intersect(idom: &[usize], rpo_num: &[usize], a0: usize, b0: usize) -> usize {
    let (mut a, mut b) = (a0, b0);
    let limit = idom.len().saturating_mul(2).saturating_add(8);
    let mut steps = 0usize;
    while a != b {
        steps += 1;
        if steps > limit || a >= rpo_num.len() || b >= rpo_num.len() {
            return NONE;
        }
        let (ra, rb) = (rpo_num[a], rpo_num[b]);
        if ra == NONE || rb == NONE {
            return NONE;
        }
        // Reverse-post-order numbers are unique, so `a != b` implies
        // `ra != rb` and each step strictly decreases `max(ra, rb)`.
        if ra > rb {
            let na = idom[a];
            if na == NONE || na == a {
                return NONE;
            }
            a = na;
        } else {
            let nb = idom[b];
            if nb == NONE || nb == b {
                return NONE;
            }
            b = nb;
        }
    }
    a
}

impl InsnCfg {
    /// Build the CFG over normal control flow, or `None` when the bytecode
    /// cannot be walked exactly, a branch target is not an instruction
    /// boundary, control flow is opaque (`jsr`/`ret`), or the dominator
    /// fixpoint did not settle. Every `None` is a refusal.
    pub(crate) fn build(code: &[u8], code_len: usize) -> Option<InsnCfg> {
        Self::build_with_handlers(code, code_len, &[])
    }

    /// [`InsnCfg::build`] with EXCEPTION EDGES: every instruction inside
    /// `[start_pc, end_pc)` gains an edge to `handler_pc`. `None` also when a
    /// handler pc is not an instruction start.
    pub(crate) fn build_with_handlers(
        code: &[u8],
        code_len: usize,
        handlers: &[HandlerRange],
    ) -> Option<InsnCfg> {
        if code_len == 0 || code_len > code.len() {
            return None;
        }
        let mut pcs: Vec<usize> = Vec::new();
        let mut idx_of: Vec<usize> = vec![NONE; code_len + 1];
        let mut pc = 0usize;
        while pc < code_len {
            idx_of[pc] = pcs.len();
            pcs.push(pc);
            pc += insn_len(code, pc)?;
        }
        if pc != code_len {
            // The last instruction runs past the end: the length table and
            // this code disagree, so nothing below can be trusted.
            return None;
        }

        let n = pcs.len();
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut targets: Vec<usize> = Vec::new();
        for (i, &at) in pcs.iter().enumerate() {
            targets.clear();
            if !normal_successors(code, code_len, at, &mut targets) {
                return None;
            }
            for &t in &targets {
                let ti = idx_of[t];
                if ti == NONE {
                    return None; // target lands mid-instruction
                }
                if !succs[i].contains(&ti) {
                    succs[i].push(ti);
                }
            }
        }
        for &(start, end, handler) in handlers {
            let hi = *idx_of.get(handler)?;
            if hi == NONE {
                return None;
            }
            let from = pcs.partition_point(|&p| p < start);
            for (i, &p) in pcs.iter().enumerate().skip(from) {
                if p >= end {
                    break;
                }
                if !succs[i].contains(&hi) {
                    succs[i].push(hi);
                }
            }
        }
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, ss) in succs.iter().enumerate() {
            for &s in ss {
                preds[s].push(i);
            }
        }

        // Reverse post-order from the entry, iteratively: a deep method must
        // not blow the compiler thread's stack.
        let mut visited = vec![false; n];
        let mut post: Vec<usize> = Vec::with_capacity(n);
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        visited[0] = true;
        while let Some((node, ci)) = stack.pop() {
            if ci < succs[node].len() {
                stack.push((node, ci + 1));
                let s = succs[node][ci];
                if !visited[s] {
                    visited[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(node);
            }
        }
        let mut rpo_num = vec![NONE; n];
        let rpo: Vec<usize> = post.into_iter().rev().collect();
        for (k, &node) in rpo.iter().enumerate() {
            rpo_num[node] = k;
        }
        if rpo.first().copied() != Some(0) {
            return None;
        }

        // Cooper/Harvey/Kennedy iterative dominators, capped so a malformed
        // graph refuses instead of spinning on the JIT thread.
        let mut idom = vec![NONE; n];
        idom[0] = 0;
        let mut settled = false;
        for _ in 0..(n + 2) {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new_idom = NONE;
                for &p in &preds[b] {
                    if rpo_num[p] == NONE || idom[p] == NONE {
                        continue; // unreachable or not yet processed
                    }
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        dom_intersect(&idom, &rpo_num, p, new_idom)
                    };
                    if new_idom == NONE {
                        break;
                    }
                }
                if new_idom != NONE && idom[b] != new_idom {
                    idom[b] = new_idom;
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return None;
        }

        Some(InsnCfg {
            pcs,
            idx_of,
            succs,
            preds,
            rpo,
            rpo_num,
            idom,
        })
    }

    /// Instruction start pcs, ascending.
    pub(crate) fn nodes(&self) -> &[usize] {
        &self.pcs
    }

    /// Node index for an instruction start pc.
    pub(crate) fn node_of(&self, pc: usize) -> Option<usize> {
        match self.idx_of.get(pc).copied() {
            Some(i) if i != NONE => Some(i),
            _ => None,
        }
    }

    /// Successor nodes of `node`.
    pub(crate) fn succs(&self, node: usize) -> &[usize] {
        self.succs.get(node).map_or(&[], Vec::as_slice)
    }

    /// Predecessor nodes of `node`.
    pub(crate) fn preds(&self, node: usize) -> &[usize] {
        self.preds.get(node).map_or(&[], Vec::as_slice)
    }

    /// Reachable nodes in reverse post-order (the entry first).
    pub(crate) fn rpo(&self) -> &[usize] {
        &self.rpo
    }

    /// `true` when `node` is reachable from the method entry.
    pub(crate) fn is_reachable(&self, node: usize) -> bool {
        self.rpo_num.get(node).copied().unwrap_or(NONE) != NONE
    }

    /// Immediate dominator of `node`; `None` for the entry, an unreachable
    /// node, or an unknown answer.
    pub(crate) fn idom(&self, node: usize) -> Option<usize> {
        match self.idom.get(node).copied() {
            Some(d) if d != NONE && d != node => Some(d),
            _ => None,
        }
    }

    /// `true` when `a` dominates `b` (every path from the entry to `b` passes
    /// through `a`). Unknown or unreachable answers `false`, so a caller that
    /// requires domination refuses rather than assuming it.
    pub(crate) fn dominates(&self, a: usize, b: usize) -> bool {
        if a >= self.idom.len() || b >= self.idom.len() {
            return false;
        }
        if !self.is_reachable(a) || !self.is_reachable(b) {
            return false;
        }
        let mut cur = b;
        let mut steps = 0usize;
        let limit = self.idom.len() + 8;
        loop {
            if cur == a {
                return true;
            }
            steps += 1;
            if steps > limit {
                return false;
            }
            let nxt = self.idom[cur];
            if nxt == NONE || nxt == cur {
                return false; // reached the entry without meeting `a`
            }
            cur = nxt;
        }
    }

    /// Pcs that may repeat on a control-flow cycle, including irreducible
    /// cycles and the exception edges supplied to `build_with_handlers`.
    /// Unreachable instructions remain conservatively repeatable. Iterative
    /// Kosaraju walk: reverse post-order of this graph, DFS of its reverse.
    pub(crate) fn cyclic_pcs(&self) -> std::collections::HashSet<usize> {
        let mut repeated: std::collections::HashSet<usize> = self.pcs.iter().copied().collect();
        let mut component = vec![NONE; self.pcs.len()];
        for &root in &self.rpo {
            if component[root] != NONE {
                continue;
            }
            let mut nodes = Vec::new();
            let mut stack = vec![root];
            while let Some(node) = stack.pop() {
                if component[node] != NONE {
                    continue;
                }
                component[node] = root;
                nodes.push(node);
                for &pred in &self.preds[node] {
                    if self.is_reachable(pred) && component[pred] == NONE {
                        stack.push(pred);
                    }
                }
            }
            let cyclic = nodes.len() > 1 || self.succs[root].contains(&root);
            if !cyclic {
                repeated.remove(&self.pcs[root]);
            }
        }
        repeated
    }

    /// Natural-loop headers: the target of every reachable edge whose target
    /// dominates its source, ascending by node. For a rotated loop
    /// (`goto test; body: …; test: if<cond> body`) this is the test, not the
    /// textually-backward branch's target — see [`back_edges`] for that set.
    pub(crate) fn loop_headers(&self) -> Vec<usize> {
        let n = self.pcs.len();
        let mut is_header = vec![false; n];
        for src in 0..n {
            if !self.is_reachable(src) {
                continue;
            }
            for &t in &self.succs[src] {
                if self.dominates(t, src) {
                    is_header[t] = true;
                }
            }
        }
        (0..n).filter(|&i| is_header[i]).collect()
    }

    /// Body of the natural loop of back edge `latch → header`: `body[node]` is
    /// `true` for the header and every reachable node that reaches `latch`
    /// without passing through the header. `None` unless `header` dominates
    /// `latch` and `latch → header` is an edge.
    pub(crate) fn natural_loop(&self, header: usize, latch: usize) -> Option<Vec<bool>> {
        if !self.succs(latch).contains(&header) || !self.dominates(header, latch) {
            return None;
        }
        let mut body = vec![false; self.pcs.len()];
        body[header] = true;
        let mut stack = vec![latch];
        while let Some(v) = stack.pop() {
            if body[v] {
                continue;
            }
            body[v] = true;
            for &p in &self.preds[v] {
                if !body[p] && self.is_reachable(p) {
                    stack.push(p);
                }
            }
        }
        Some(body)
    }
}

/// LENIENT normal successors of the instruction at `pc`, for dataflow
/// analyses where a superset of edges is the conservative direction:
/// the branch target when it lies in `[0, code_len)`, every decodable switch
/// target, and the fall-through when [`falls_through`]. `jsr`/`jsr_w` give
/// their target and the fall-through; `ret` and `wide ret` give nothing.
pub(crate) fn for_each_lenient_successor(
    code: &[u8],
    code_len: usize,
    pc: usize,
    mut f: impl FnMut(usize),
) {
    let code_len = code_len.min(code.len());
    if pc >= code_len {
        return;
    }
    let op = code[pc];
    if matches!(op, 0xaa | 0xab) {
        for_each_switch_target_lenient(code, code_len, pc, &mut f);
    } else if is_offset_branch(op) {
        if let Some(t) = offset_branch_target(&code[..code_len], pc).filter(|&t| t < code_len) {
            f(t);
        }
    }
    if falls_through(op) && !is_wide_ret(&code[..code_len], pc) {
        let next = pc + step(code, pc);
        if next < code_len {
            f(next);
        }
    }
}

pub(crate) fn lenient_successors(code: &[u8], code_len: usize, pc: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for_each_lenient_successor(code, code_len, pc, |t| out.push(t));
    out
}

#[cfg(test)]
mod tests;

// ── Round 11 (lane iropt) ────────────────────────────────────────────

#[cfg(test)]
mod r11_iropt_tests;
