// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One bytecode decoder for both JIT tiers.
//!
//! Every analysis in this crate that walks raw JVM bytecode needs the same
//! facts: how long an instruction is, where it can jump, whether it falls
//! through, which pcs are reachable and which are loop headers. Each pass used
//! to derive them with a private copy. The copies drifted: one length table
//! lacked `ldc`, another lacked `jsr`/`ret`, a successor function dropped
//! switch edges, and a loop finder saw only 16-bit branches. Every drift was a
//! latent miscompile (see `two-optimizing-front-ends-duplicate-bytecode-analyses-FIXED.md`).
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
pub(crate) fn insn_len(code: &[u8], pc: usize) -> Option<usize> {
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
pub(crate) fn switch_targets_lenient(code: &[u8], code_len: usize, pc: usize) -> Vec<usize> {
    let code_len = code_len.min(code.len());
    let mut out = Vec::new();
    if pc >= code_len || !matches!(code[pc], 0xaa | 0xab) {
        return out;
    }
    let base = switch_operand_base(pc);
    let fits = |at: usize, n: usize| at.checked_add(n).is_some_and(|e| e <= code_len);
    let push = |off: i32, out: &mut Vec<usize>| {
        if let Some(t) = pc.checked_add_signed(off as isize).filter(|&t| t < code_len) {
            out.push(t);
        }
    };
    if !fits(base, 4) {
        return out;
    }
    push(read_i32(code, base).unwrap_or(0), &mut out);
    if code[pc] == 0xaa {
        if !fits(base, 12) {
            return out;
        }
        let (Some(low), Some(high)) = (read_i32(code, base + 4), read_i32(code, base + 8)) else {
            return out;
        };
        let Some(count) = checked_tableswitch_count(low, high) else {
            return out;
        };
        for i in 0..count {
            let at = base + 12 + i * 4;
            if !fits(at, 4) {
                break;
            }
            push(read_i32(code, at).unwrap_or(0), &mut out);
        }
    } else {
        if !fits(base, 8) {
            return out;
        }
        let Some(npairs) = read_i32(code, base + 4).and_then(checked_lookupswitch_npairs) else {
            return out;
        };
        for i in 0..npairs {
            let at = base + 8 + i * 8;
            if !fits(at, 8) {
                break;
            }
            push(read_i32(code, at + 4).unwrap_or(0), &mut out);
        }
    }
    out
}

/// STRICT explicit targets of the instruction at `pc`, appended to `out`
/// (the fall-through successor is NOT included).
///
/// Returns `false` — *refuse* — when the successor set is not statically known
/// (`jsr`/`jsr_w`/`ret`), the encoding is malformed, or a target falls outside
/// `[0, code_len)`. Nothing is appended on refusal. Callers must treat `false`
/// as opaque, not as "no targets".
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
/// their target and the fall-through; `ret` gives nothing.
pub(crate) fn lenient_successors(code: &[u8], code_len: usize, pc: usize) -> Vec<usize> {
    let code_len = code_len.min(code.len());
    let mut out = Vec::new();
    if pc >= code_len {
        return out;
    }
    let op = code[pc];
    if matches!(op, 0xaa | 0xab) {
        out = switch_targets_lenient(code, code_len, pc);
    } else if let Some(t) = offset_branch_target(&code[..code_len], pc).filter(|&t| t < code_len) {
        out.push(t);
    }
    if falls_through(op) {
        let next = pc + step(code, pc);
        if next < code_len {
            out.push(next);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_reader::instruction::Instruction;

    /// The class reader's decoder is the oracle: the verifier and the IR
    /// builder already trust it, so the JIT's table must agree byte for byte.
    fn reader_len(code: &[u8], pc: usize) -> Option<usize> {
        Instruction::decode(code, pc).ok().map(|(_, next)| next - pc)
    }

    /// Operand bytes that make each fixed-width opcode decodable.
    fn operands_for(op: u8) -> Vec<u8> {
        match op {
            0xba => vec![op, 0x00, 0x01, 0x00, 0x00], // invokedynamic: two zero bytes
            0xb9 => vec![op, 0x00, 0x01, 0x01, 0x00], // invokeinterface: count 1, zero
            0xbc => vec![op, 0x0a],                   // newarray T_INT
            0xc5 => vec![op, 0x00, 0x01, 0x01],       // multianewarray, 1 dimension
            _ => vec![op, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
        }
    }

    #[test]
    fn every_fixed_width_opcode_matches_the_class_reader() {
        let mut compared = 0;
        for op in 0x00u8..=0xc9 {
            if matches!(op, 0xaa | 0xab | 0xc4) {
                continue;
            }
            let code = operands_for(op);
            let Some(expected) = reader_len(&code, 0) else {
                continue;
            };
            assert_eq!(insn_len(&code, 0), Some(expected), "opcode {op:#04x}");
            compared += 1;
        }
        // 0x00..=0xc9 is 202 opcodes; three are variable-width.
        assert_eq!(compared, 199, "the reader must decode every other opcode");
    }

    #[test]
    fn jvms_widths_that_drifted_before_are_pinned() {
        // Each of these was missing from some private table at some point.
        let cases: &[(&[u8], usize)] = &[
            (&[0x12, 0xbb], 2),                   // ldc (CM-FASTMATH)
            (&[0x13, 0x00, 0xbb], 3),             // ldc_w
            (&[0x14, 0x00, 0xbb], 3),             // ldc2_w
            (&[0xa8, 0x00, 0x10], 3),             // jsr (loop_analysis lacked it)
            (&[0xa9, 0x01], 2),                   // ret (loop_analysis lacked it)
            (&[0xba, 0x00, 0x01, 0x00, 0x00], 5), // invokedynamic
            (&[0xc8, 0x00, 0x00, 0x00, 0x10], 5), // goto_w
            (&[0xc9, 0x00, 0x00, 0x00, 0x10], 5), // jsr_w
            (&[0xc5, 0x00, 0x01, 0x02], 4),       // multianewarray
        ];
        for &(code, len) in cases {
            assert_eq!(insn_len(code, 0), Some(len), "{code:02x?}");
        }
    }

    #[test]
    fn wide_forms_match_the_class_reader() {
        for op in [0x15u8, 0x16, 0x17, 0x18, 0x19, 0x36, 0x37, 0x38, 0x39, 0x3a, 0xa9] {
            let code = [0xc4, op, 0x01, 0x00];
            assert_eq!(insn_len(&code, 0), Some(4), "wide {op:#04x}");
            assert_eq!(reader_len(&code, 0), Some(4), "reader wide {op:#04x}");
            let insn = decode_at(&code, code.len(), 0).expect("decodes");
            assert!(insn.wide);
            assert_eq!((insn.opcode, insn.op), (0xc4, op));
        }
        let iinc = [0xc4, 0x84, 0x01, 0x00, 0x00, 0x05];
        assert_eq!(insn_len(&iinc, 0), Some(6));
        assert_eq!(reader_len(&iinc, 0), Some(6));
        // A truncated prefix reads as the 4-byte form, but does not decode.
        assert_eq!(insn_len(&[0xc4], 0), Some(4));
        assert_eq!(decode_at(&[0xc4], 1, 0), None);
        // `wide` may not modify an arbitrary opcode.
        assert_eq!(decode_at(&[0xc4, 0x60, 0x00, 0x00], 4, 0), None);
    }

    /// `nop * lead` then a switch whose targets are all `target_back` bytes
    /// before the switch opcode (0 = the switch itself).
    fn tableswitch_at(lead: usize, low: i32, high: i32) -> (Vec<u8>, usize) {
        let mut code = vec![0x00; lead];
        let pc = code.len();
        code.push(0xaa);
        while code.len() % 4 != 0 {
            code.push(0x00);
        }
        let off = -(pc as i32); // every target is pc 0
        code.extend_from_slice(&off.to_be_bytes());
        code.extend_from_slice(&low.to_be_bytes());
        code.extend_from_slice(&high.to_be_bytes());
        for _ in low..=high {
            code.extend_from_slice(&off.to_be_bytes());
        }
        (code, pc)
    }

    fn lookupswitch_at(lead: usize, keys: &[i32]) -> (Vec<u8>, usize) {
        let mut code = vec![0x00; lead];
        let pc = code.len();
        code.push(0xab);
        while code.len() % 4 != 0 {
            code.push(0x00);
        }
        let off = -(pc as i32);
        code.extend_from_slice(&off.to_be_bytes());
        code.extend_from_slice(&(keys.len() as i32).to_be_bytes());
        for &k in keys {
            code.extend_from_slice(&k.to_be_bytes());
            code.extend_from_slice(&off.to_be_bytes());
        }
        (code, pc)
    }

    #[test]
    fn switch_padding_is_right_at_every_alignment() {
        for lead in 0..8 {
            let (code, pc) = tableswitch_at(lead, 3, 5);
            let pad = (4 - (pc + 1) % 4) % 4;
            let expected = 1 + pad + 12 + 3 * 4;
            assert_eq!(code.len() - pc, expected, "fixture, lead {lead}");
            assert_eq!(insn_len(&code, pc), Some(expected), "tableswitch lead {lead}");
            assert_eq!(reader_len(&code, pc), Some(expected), "reader tableswitch lead {lead}");
            let table = switch_table(&code, code.len(), pc).expect("tableswitch decodes");
            assert_eq!(table.len, expected);
            assert_eq!(table.default, 0);
            assert_eq!(table.cases, vec![(3, 0), (4, 0), (5, 0)]);

            let (code, pc) = lookupswitch_at(lead, &[-7, 9]);
            let pad = (4 - (pc + 1) % 4) % 4;
            let expected = 1 + pad + 8 + 2 * 8;
            assert_eq!(insn_len(&code, pc), Some(expected), "lookupswitch lead {lead}");
            assert_eq!(reader_len(&code, pc), Some(expected), "reader lookupswitch lead {lead}");
            let table = switch_table(&code, code.len(), pc).expect("lookupswitch decodes");
            assert_eq!(table.cases, vec![(-7, 0), (9, 0)]);
            assert_eq!(table.targets().collect::<Vec<_>>(), vec![0, 0, 0]);
        }
    }

    #[test]
    fn malformed_switches_refuse_strictly_and_step_one_byte() {
        // No room for the header.
        let bare = [0xaa, 0x00, 0x00];
        assert_eq!(insn_len(&bare, 0), None);
        assert_eq!(step(&bare, 0), 1);
        assert_eq!(switch_table(&bare, bare.len(), 0), None);
        assert!(switch_targets_lenient(&bare, bare.len(), 0).is_empty());

        // high < low.
        let (mut code, pc) = tableswitch_at(0, 5, 5);
        code[pc + 8..pc + 12].copy_from_slice(&6i32.to_be_bytes()); // low = 6 > high
        assert_eq!(insn_len(&code, pc), None);
        assert_eq!(switch_table(&code, code.len(), pc), None);
        assert_eq!(switch_targets_lenient(&code, code.len(), pc), vec![0], "default only");

        // Negative npairs.
        let mut neg = vec![0xab, 0x00, 0x00, 0x00];
        neg.extend_from_slice(&0i32.to_be_bytes());
        neg.extend_from_slice(&(-5i32).to_be_bytes());
        assert_eq!(insn_len(&neg, 0), None);
        assert_eq!(step(&neg, 0), 1);

        // One case target out of range: strict refuses, lenient keeps the rest.
        let (mut code, pc) = tableswitch_at(1, 0, 1);
        let last = code.len() - 4;
        code[last..].copy_from_slice(&1000i32.to_be_bytes());
        assert_eq!(switch_table(&code, code.len(), pc), None);
        assert_eq!(switch_targets_lenient(&code, code.len(), pc), vec![0, 0]);
        let mut out = Vec::new();
        assert!(!explicit_targets(&code, code.len(), pc, &mut out));
        assert!(out.is_empty(), "nothing appended on refusal");

        // Table runs past `code_len`: strict refuses, lenient stops.
        let (code, pc) = tableswitch_at(0, 0, 2);
        let short = code.len() - 4;
        assert_eq!(switch_table(&code, short, pc), None);
        assert_eq!(switch_targets_lenient(&code, short, pc), vec![0, 0, 0]);
    }

    #[test]
    fn offset_branches_decode_both_widths() {
        // nop; nop; goto -2
        let code = [0x00, 0x00, 0xa7, 0xff, 0xfe];
        assert_eq!(offset_branch_target(&code, 2), Some(0));
        // goto_w +5 from pc 0 lands past the end: decoded but not in range.
        let w = [0xc8, 0x00, 0x00, 0x00, 0x05];
        assert_eq!(offset_branch_target(&w, 0), Some(5));
        let mut out = Vec::new();
        assert!(!explicit_targets(&w, w.len(), 0, &mut out));
        // A negative target is no target.
        assert_eq!(offset_branch_target(&[0xa7, 0xff, 0xf0], 0), None);
        // Not a branch.
        assert_eq!(offset_branch_target(&[0x00], 0), None);
        // Truncated offset.
        assert_eq!(offset_branch_target(&[0x99, 0x00], 0), None);
    }

    #[test]
    fn subroutines_are_opaque_and_exits_do_not_fall_through() {
        let mut out = Vec::new();
        for code in [&[0xa8u8, 0x00, 0x03][..], &[0xa9, 0x01][..], &[0xc9, 0, 0, 0, 5][..]] {
            assert!(!explicit_targets(code, code.len(), 0, &mut out), "{code:02x?}");
            assert!(is_subroutine_op(code[0]));
        }
        for op in [0xa7u8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xbf, 0xc8] {
            assert!(!falls_through(op), "{op:#04x}");
        }
        for op in [0x00u8, 0x99, 0xa6, 0xa8, 0xb6, 0xc6, 0xc7, 0xc9] {
            assert!(falls_through(op), "{op:#04x}");
        }
        assert!(is_exit(0xbf) && is_exit(0xb1) && !is_exit(0xa7));
    }

    /// `iconst_0; istore_1; L2: iload_1; bipush 10; if_icmpge L15;
    /// iinc 1,1; goto L2; L15: return`
    fn top_tested_loop() -> Vec<u8> {
        vec![
            0x03, 0x3c, // 0: iconst_0, 1: istore_1
            0x1b, // 2: iload_1
            0x10, 0x0a, // 3: bipush 10
            0xa2, 0x00, 0x0a, // 5: if_icmpge +10 -> 15
            0x84, 0x01, 0x01, // 8: iinc 1 1
            0xa7, 0xff, 0xf7, // 11: goto -9 -> 2
            0x00, // 14: nop (dead)
            0xb1, // 15: return
        ]
    }

    #[test]
    fn walks_maps_and_decoded_method_agree() {
        let code = top_tested_loop();
        let insns = decode_method(&code, code.len()).expect("decodes");
        let pcs: Vec<usize> = insns.iter().map(|i| i.pc).collect();
        assert_eq!(pcs, vec![0, 1, 2, 3, 5, 8, 11, 14, 15]);
        let starts = instruction_starts(&code, code.len());
        for (pc, &s) in starts.iter().enumerate() {
            assert_eq!(s, pcs.contains(&pc), "pc {pc}");
        }
        // The reader's canonical walk sees the same boundaries.
        let verified = cratonvm_reader::verified_code(&code).expect("verifies");
        for vi in verified.instructions() {
            let pc = vi.pc as usize;
            assert_eq!(insn_len(&code, pc), Some(vi.next_pc as usize - pc), "pc {pc}");
        }
        let targets = branch_target_map(&code, code.len());
        let marked: Vec<usize> = (0..code.len()).filter(|&p| targets[p]).collect();
        assert_eq!(marked, vec![2, 15]);
        assert_eq!(back_edges(&code, code.len()), vec![(2, 11)]);
        // Overrunning walk: truncate inside `iinc`.
        assert_eq!(decode_method(&code, 10), None);
    }

    #[test]
    fn reachability_skips_dead_code_and_honours_extra_roots() {
        let code = top_tested_loop();
        let r = reachable_pcs(&code, code.len(), &[]).expect("known control flow");
        assert!(r[0] && r[2] && r[8] && r[11] && r[15]);
        assert!(!r[14], "the nop after goto is dead");
        let r = reachable_pcs(&code, code.len(), &[14]).expect("known control flow");
        assert!(r[14]);
        // jsr makes it opaque.
        assert_eq!(reachable_pcs(&[0xa8, 0x00, 0x03, 0xb1], 4, &[]), None);
    }

    #[test]
    fn cfg_finds_the_header_and_body_of_a_top_tested_loop() {
        let code = top_tested_loop();
        let cfg = InsnCfg::build(&code, code.len()).expect("cfg builds");
        let n = |pc| cfg.node_of(pc).unwrap();
        assert_eq!(cfg.loop_headers(), vec![n(2)]);
        assert!(cfg.dominates(n(2), n(11)));
        assert!(cfg.dominates(n(5), n(15)));
        assert!(!cfg.is_reachable(n(14)));
        assert!(!cfg.dominates(n(14), n(15)), "unreachable dominates nothing");
        assert_eq!(cfg.idom(n(8)), Some(n(5)));
        assert_eq!(cfg.idom(n(0)), None);
        let body = cfg.natural_loop(n(2), n(11)).expect("natural loop");
        let body_pcs: Vec<usize> = cfg
            .nodes()
            .iter()
            .enumerate()
            .filter(|&(i, _)| body[i])
            .map(|(_, &pc)| pc)
            .collect();
        assert_eq!(body_pcs, vec![2, 3, 5, 8, 11]);
        assert_eq!(cfg.rpo()[0], 0);
        assert_eq!(cfg.preds(n(2)).len(), 2, "entry fall-through and the goto");
        assert_eq!(cfg.natural_loop(n(5), n(11)), None, "5 -> 11 is not a back edge");
    }

    #[test]
    fn a_rotated_loop_is_headed_by_its_test_not_by_the_backward_target() {
        // 0: goto L8; L3: iinc 1,1; L6: nop; L7: nop;
        // L8: iload_1; L9: bipush 10; L11: if_icmplt L3; L14: return
        let code = vec![
            0xa7, 0x00, 0x08, // 0: goto +8 -> 8
            0x84, 0x01, 0x01, // 3: iinc 1 1
            0x00, // 6: nop
            0x00, // 7: nop
            0x1b, // 8: iload_1
            0x10, 0x0a, // 9: bipush 10
            0xa1, 0xff, 0xf8, // 11: if_icmplt -8 -> 3
            0xb1, // 14: return
        ];
        // Textually the backward branch targets the body start...
        assert_eq!(back_edges(&code, code.len()), vec![(3, 11)]);
        // ...but the body start does not dominate the test (the entry `goto`
        // bypasses it); the test dominates the body.
        let cfg = InsnCfg::build(&code, code.len()).expect("cfg builds");
        let n = |pc| cfg.node_of(pc).unwrap();
        assert!(!cfg.dominates(n(3), n(11)));
        assert!(cfg.dominates(n(8), n(3)));
        assert_eq!(cfg.loop_headers(), vec![n(8)]);
        let body = cfg.natural_loop(n(8), n(7)).expect("7 -> 8 is the back edge");
        for pc in [3, 6, 7, 8, 9, 11] {
            assert!(body[n(pc)], "pc {pc} in the body");
        }
        assert!(!body[n(0)] && !body[n(14)]);
    }

    #[test]
    fn nested_loops_have_nested_headers() {
        // for (i = 0; i < 4; i++) { for (j = 0; j < 4; j++) {} }
        let code = vec![
            0x03, 0x3c, // 0: iconst_0, 1: istore_1
            0x1b, 0x10, 0x04, // 2: iload_1, 3: bipush 4
            0xa2, 0x00, 0x17, // 5: if_icmpge +23 -> 28
            0x03, 0x3d, // 8: iconst_0, 9: istore_2
            0x1c, 0x10, 0x04, // 10: iload_2, 11: bipush 4
            0xa2, 0x00, 0x09, // 13: if_icmpge +9 -> 22
            0x84, 0x02, 0x01, // 16: iinc 2 1
            0xa7, 0xff, 0xf7, // 19: goto -9 -> 10
            0x84, 0x01, 0x01, // 22: iinc 1 1
            0xa7, 0xff, 0xe9, // 25: goto -23 -> 2
            0xb1, // 28: return
        ];
        let cfg = InsnCfg::build(&code, code.len()).expect("cfg builds");
        let n = |pc| cfg.node_of(pc).unwrap();
        assert_eq!(cfg.loop_headers(), vec![n(2), n(10)]);
        assert!(cfg.dominates(n(2), n(10)));
        assert!(!cfg.dominates(n(10), n(2)));
        let outer = cfg.natural_loop(n(2), n(25)).expect("outer loop");
        let inner = cfg.natural_loop(n(10), n(19)).expect("inner loop");
        assert!(outer[n(10)] && outer[n(19)] && outer[n(22)]);
        assert!(inner[n(16)] && !inner[n(22)] && !inner[n(2)]);
        assert_eq!(back_edges(&code, code.len()), vec![(10, 19), (2, 25)]);
    }

    #[test]
    fn handler_edges_make_the_handler_reachable_and_dominated_by_the_try() {
        // 0: iconst_1; 1: istore_1; 2: iload_1; 3: ireturn;
        // 4: astore_2 (handler for [0, 4)); 5: iconst_m1; 6: ireturn
        let code = vec![0x04, 0x3c, 0x1b, 0xac, 0x4d, 0x02, 0xac];
        let plain = InsnCfg::build(&code, code.len()).expect("cfg builds");
        assert!(!plain.is_reachable(plain.node_of(4).unwrap()));
        let cfg = InsnCfg::build_with_handlers(&code, code.len(), &[(0, 4, 4)]).expect("cfg builds");
        let n = |pc| cfg.node_of(pc).unwrap();
        assert!(cfg.is_reachable(n(4)));
        assert_eq!(cfg.preds(n(4)).len(), 4, "every protected instruction");
        assert!(cfg.dominates(n(0), n(4)));
        assert_eq!(cfg.idom(n(4)), Some(n(0)));
        assert!(cfg.loop_headers().is_empty());
        // A handler pc inside an instruction refuses.
        let mid = [0x10, 0x05, 0xac];
        assert!(InsnCfg::build_with_handlers(&mid, 3, &[(0, 2, 1)]).is_none());
    }

    #[test]
    fn switch_back_edges_are_counted_once_per_switch() {
        // nop; tableswitch low 0 high 2, every target pc 0.
        let (code, pc) = tableswitch_at(1, 0, 2);
        assert_eq!(back_edges(&code, code.len()), vec![(0, pc)]);
        let map = branch_target_map(&code, code.len());
        assert!(map[0]);
        assert_eq!(map.iter().filter(|&&b| b).count(), 1);
    }
}
