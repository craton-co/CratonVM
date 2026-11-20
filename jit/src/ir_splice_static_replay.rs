// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 7, lane T1STATICFWD (STATICFWD21-1, primitive half;
//! `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY`, **opt-in**): may a spliced
//! body that may loop carry a plain primitive `putstatic` WITHOUT raising the
//! builder's replay fence?
//!
//! # Why the fence exists
//!
//! Every trap inside a splice (a null check, a bounds check, a division by
//! zero, a deopt at a poll) resumes at the CALLER's `invoke` and re-runs the
//! whole call. FG16-3 therefore COMMITS a spliced `putstatic`
//! (`IrBuilder::splice_committed_bases`), after which only instructions that
//! cannot trap may follow; a backward branch never may, so the planner refuses
//! every body that may loop (`ir-splice-putstatic-loop` in `lib.rs`'s
//! `append_ir_inline_site`), and every alloc / sum kernel ending in a static
//! write keeps its call.
//!
//! # When a replay is harmless
//!
//! A replay re-runs the body from its first instruction with the same
//! arguments. It computes and stores the same values when nothing the body
//! READS can have been changed by what the body WROTE before the trap:
//!
//! * the body's only memory writes are plain non-volatile PRIMITIVE
//!   `putstatic`s of a class the resolver found initialised (so the write runs
//!   no `<clinit>`): no `putfield`, array store, allocation, call,
//!   `invokedynamic`, monitor or `athrow`;
//! * it never reads a static it writes (`S = S + 1` is not idempotent), and
//!   reads other statics only of a class it also writes (initialised, so the
//!   read runs no `<clinit>` that could change state the body read earlier);
//! * no access is volatile (no acquire / release to repeat), and the callee is
//!   not `synchronized` (the planner checks `callee_is_synchronized`, the
//!   builder the frame's monitor window).
//!
//! Reference stores are NOT idempotent (a replay stores a different
//! allocation, which a racing reader can tell apart), so they stay committed;
//! their route is the chain-snapshot half of STATICFWD21-1
//! (`crate::ir_splice_static_chain`, j27 wave 8, opt-in).
//!
//! What a replay CAN show is a racing reader seeing a cell go back to an
//! earlier value of the same run (`S = 5`, trap, replay `S = 0`). JLS 17.4
//! gives plain (non-volatile) accesses no per-variable coherence, so that is
//! an allowed outcome; it is also the reason this stays opt-in until a host
//! A/B and the JMM probe rows say otherwise.
//!
//! **Racy inputs** (j27 wave 9, lane W9RPL): a replay RE-READS heap memory
//! another thread may have changed since the first run (`S = a[i]` in a loop:
//! the first run published `a0[0]`, the replay publishes `a1[0]`; no single
//! run wrote both). So a body that reads a field, an array element or a
//! static must not be able to trap once one of its stores has run
//! ([`no_trap_follows_a_store`]): then a replay only discards a run that
//! published nothing. `sumInto` (every read before its one store) keeps the
//! route; `fillFrom` (store, back edge, read) refuses and keeps its call.
//!
//! # Depth
//!
//! Only the OUTERMOST splice: a body nested in another may be re-run by the
//! ENCLOSING body's trap after the enclosing body read what this one wrote.
//! The builder asks for depth 1 itself; the planner admits only a body whose
//! caller pc lies in the compiling method's own code
//! ([`caller_pc_is_top_level`]), so it never admits a body the builder will
//! then commit (a whole-method refusal and a W5-2 rebuild).
//!
//! One decision, two askers: the planner (`append_ir_inline_site`) and the
//! builder (`IrBuilder::splice_static_store_replays_idempotently`) both call
//! [`body_replays_idempotently`] over the same bytes and rows.

use std::collections::HashMap;

use crate::bytecode_analysis;
use crate::ir::IrInlineSite;

/// The opt-in switch (token `ir-splice-putstatic-loop-replay`).
pub(crate) const SWITCH: &str = "CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY";

/// `(class id, field index, type tag, volatile)` of one static row, the shape
/// both `IrInlineTables::static_field_info` and the builder's table carry.
pub(crate) type StaticRow = (u32, usize, u8, bool);

/// Is STATICFWD21-1's primitive half on? Needs FG16-3
/// ([`crate::ir::ir_splice_putstatic_enabled`]), whose rows it reads. Read per
/// use; no static.
pub(crate) fn enabled() -> bool {
    crate::ir::ir_splice_putstatic_enabled() && cratonvm_types::flags::runtime_flag_on(SWITCH)
}

/// Does re-running `body` (a callee's bytecode, body-relative pcs) from its
/// first instruction store the same values into the same statics? See the
/// module comment. `static_row(pc)` answers the row of the `getstatic` /
/// `putstatic` at `pc`; `getfield_is_volatile(pc)` whether the `getfield` at
/// `pc` reads a volatile field. `false` for a body with no `putstatic`, and
/// for anything it cannot decode.
pub(crate) fn body_replays_idempotently(
    body: &[u8],
    static_row: impl Fn(usize) -> Option<StaticRow>,
    getfield_is_volatile: impl Fn(usize) -> bool,
) -> bool {
    let Some(written) = written_statics(body, &static_row) else {
        return false;
    };
    !written.is_empty()
        && every_insn_replays(body, &written, &static_row, &getfield_is_volatile)
        && (!reads_shared_memory(body) || no_trap_follows_a_store(body))
}

/// j27 wave 9 (lane W9RPL; page
/// `j27-splice-putstatic-loop-replay-racy-inputs-FIXED-20261029.md`): does `body`
/// read memory another thread may write (a `getfield`, an array load, a
/// `getstatic`)? A replay re-reads it, so after a published store it could
/// publish a value computed from contents no single run read. Assumes
/// `body` decodes ([`written_statics`] checked it).
fn reads_shared_memory(body: &[u8]) -> bool {
    insn_pcs(body).any(|pc| matches!(body[pc], 0x2e..=0x35 | 0xb2 | 0xb4))
}

/// Can no trap (and so no replay) happen once a `putstatic` of `body` has
/// run? Every instruction reachable after one must be in
/// [`cannot_trap_after_store`]'s set and branch only forward (a backward edge
/// carries a poll). Then a replay only ever discards a run that published
/// nothing, and the racy reads it repeats are the only reads of the run.
fn no_trap_follows_a_store(body: &[u8]) -> bool {
    let len = body.len();
    let mut seen = vec![false; len];
    let mut work: Vec<usize> = insn_pcs(body)
        .filter(|&pc| body[pc] == 0xb3)
        .flat_map(|pc| bytecode_analysis::lenient_successors(body, len, pc))
        .collect();
    while let Some(pc) = work.pop() {
        if pc >= len || std::mem::replace(&mut seen[pc], true) {
            continue;
        }
        let succ = bytecode_analysis::lenient_successors(body, len, pc);
        if !cannot_trap_after_store(body, pc) || succ.iter().any(|&s| s <= pc) {
            return false;
        }
        work.extend(succ);
    }
    true
}

/// The instructions the builder builds without a trap inside an outermost
/// splice whose classes are initialised: the committed-splice fence's set
/// (`IrBuilder::admits_after_committed_splice_store`) minus everything this
/// route already refuses. No `ldc` (its slot may be unresolved), no div /
/// rem, no switch.
fn cannot_trap_after_store(body: &[u8], pc: usize) -> bool {
    match body[pc] {
        0x00..=0x11 | 0x15..=0x2d | 0x36..=0x4e | 0x57..=0x5f => true,
        0x60..=0x6b | 0x74..=0x98 => true,
        // if*, goto, ifnull, ifnonnull, goto_w (direction checked by the caller)
        0x99..=0xa7 | 0xc6..=0xc8 => true,
        // returns; get/putstatic of an initialised class (`written_statics`,
        // `getstatic_replays` vetted the rows)
        0xac..=0xb1 | 0xb2 | 0xb3 => true,
        0xc4 => body
            .get(pc + 1)
            .is_some_and(|&w| matches!(w, 0x15..=0x19 | 0x36..=0x3a | 0x84)),
        _ => false,
    }
}

/// The instruction start pcs of `body`, stopping at the first that does not
/// decode.
fn insn_pcs(body: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut pc = 0usize;
    std::iter::from_fn(move || {
        let here = pc;
        let len = bytecode_analysis::insn_len(body, here).filter(|&l| l > 0)?;
        pc = here + len;
        Some(here)
    })
    .take_while(move |&p| p < body.len())
}

/// The planner's question (`append_ir_inline_site`, before its
/// `ir-splice-putstatic-loop` refusal): is `site`, about to be appended for
/// the call at `caller_pc`, an outermost, non-`synchronized` body whose
/// replay is harmless? The builder asks the same of the same bytes
/// (`IrBuilder::splice_static_store_replays_idempotently`).
pub(crate) fn planner_admits_loop_site(
    site: &crate::InlineSite,
    sites: &HashMap<usize, IrInlineSite>,
    caller_pc: usize,
) -> bool {
    let len = site.callee_code_len.min(site.callee_code.len());
    enabled()
        && !site.callee_is_synchronized
        && caller_pc_is_top_level(sites, caller_pc)
        && body_replays_idempotently(
            &site.callee_code[..len],
            |pc| {
                site.static_field_info
                    .iter()
                    .find(|r| r.0 == pc)
                    .map(|&(_, class_id, field_index, tag, is_volatile)| {
                        (class_id, field_index, tag, is_volatile)
                    })
            },
            |pc| site.volatile_field_pcs.contains(&pc),
        )
}

/// May the call at `caller_pc` (a site-table key) be the OUTERMOST splice,
/// i.e. does it lie in no body already appended to `sites`? A nested body's
/// parent is inserted into the table before its nested rows are appended.
pub(crate) fn caller_pc_is_top_level(sites: &HashMap<usize, IrInlineSite>, caller_pc: usize) -> bool {
    let pc = crate::ir::inline_site_caller_pc(caller_pc);
    !sites
        .values()
        .any(|s| s.base <= pc && pc < s.base.saturating_add(s.code_len))
}

/// Primitive field types (JVMS 4.3.2 base types).
fn is_primitive_tag(tag: u8) -> bool {
    matches!(tag, b'I' | b'Z' | b'B' | b'C' | b'S' | b'J' | b'D' | b'F')
}

/// The `(class, field)` of every `putstatic` in `body`, or `None` when one is
/// not a plain primitive write of a resolved class, or the bytes do not
/// decode.
fn written_statics(
    body: &[u8],
    static_row: &impl Fn(usize) -> Option<StaticRow>,
) -> Option<Vec<(u32, usize)>> {
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < body.len() {
        let len = bytecode_analysis::insn_len(body, pc).filter(|&l| l > 0)?;
        if body.get(pc) == Some(&0xb3) {
            let (class_id, field_index, tag, is_volatile) = static_row(pc)?;
            if class_id == 0 || is_volatile || !is_primitive_tag(tag) {
                return None;
            }
            out.push((class_id, field_index));
        }
        pc += len;
    }
    Some(out)
}

/// Is every instruction of `body` one a replay re-runs harmlessly, given the
/// statics it writes (`written`)?
fn every_insn_replays(
    body: &[u8],
    written: &[(u32, usize)],
    static_row: &impl Fn(usize) -> Option<StaticRow>,
    getfield_is_volatile: &impl Fn(usize) -> bool,
) -> bool {
    let mut pc = 0usize;
    while pc < body.len() {
        let Some(len) = bytecode_analysis::insn_len(body, pc).filter(|&l| l > 0) else {
            return false;
        };
        let admitted = match body[pc] {
            0xb2 => getstatic_replays(static_row(pc), written),
            0xb4 => !getfield_is_volatile(pc),
            // wide: a local load / store or `iinc`, never `wide ret`.
            0xc4 => body
                .get(pc + 1)
                .is_some_and(|&w| matches!(w, 0x15..=0x19 | 0x36..=0x3a | 0x84)),
            op => opcode_replays(op),
        };
        if !admitted {
            return false;
        }
        pc += len;
    }
    true
}

/// A plain read of a static the body does not write, of a class it writes.
fn getstatic_replays(row: Option<StaticRow>, written: &[(u32, usize)]) -> bool {
    let Some((class_id, field_index, _, is_volatile)) = row else {
        return false;
    };
    !is_volatile
        && class_id != 0
        && written.iter().any(|&(c, _)| c == class_id)
        && !written.contains(&(class_id, field_index))
}

/// The opcodes that write no memory, take no lock and call nothing. A trap
/// some of them can raise (array loads, `idiv`, `arraylength`) is fine: the
/// replay re-runs it and raises it again.
fn opcode_replays(op: u8) -> bool {
    matches!(
        op,
        // nop, constants, bipush, sipush, ldc / ldc_w / ldc2_w (the resolver
        // admits only immediate, String and Class constants in a splice)
        0x00..=0x14
        // local loads, array loads
        | 0x15..=0x35
        // local stores
        | 0x36..=0x4e
        // pop .. swap
        | 0x57..=0x5f
        // arithmetic, iinc, conversions, compares
        | 0x60..=0x98
        // if*, if_icmp*, if_acmp*, goto (backward too: the point of this)
        | 0x99..=0xa7
        // tableswitch, lookupswitch
        | 0xaa
        | 0xab
        // returns
        | 0xac..=0xb1
        // putstatic: vetted by `written_statics`
        | 0xb3
        // arraylength
        | 0xbe
        // ifnull, ifnonnull, goto_w
        | 0xc6..=0xc8
    )
}

#[cfg(test)]
mod tests;
