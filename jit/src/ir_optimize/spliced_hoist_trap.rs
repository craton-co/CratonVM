//! JIT round 23 wave 8 (lane W8-L7; `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST`,
//! perf, **default ON**, `=0` kill): the loop-entry predicates of a loop
//! SPLICED from a callee step over the pre-header copy LICM made of that same
//! loop's array length or element read.
//!
//! # What it fixes
//!
//! `W2cTypecheckLoops`' timed rows run through lambdas (`() -> nocastRow(subs,
//! holder)`). The body that runs is the lambda's method-entry compile with the
//! row spliced into it (`lambda$main$2`, `full/ir`), not the row's own OSR body.
//! In that body LICM hoists the header's `a.length` into the pre-header, where
//! its null check is a trapping node whose deopt resumes at the lambda's
//! `invoke` (a flat frame of the compiling method, before the header's combined
//! pc). [`super::preheader_traps_block_entry_predicate`] accepted only a trap
//! whose own frame is a flat frame AT OR PAST the header, so the null-base
//! predicate answered `header .. pre-header may trap` and `h.k`, `h.tab` and
//! `h.tab[5]` stayed in the loop: three field rows with their epoch compares,
//! compact selects and helper arms, plus the element's null and bounds checks,
//! on every element (n23-w6 `lambda$main$2` disassembly, 0x13f-0x399). The
//! checkcast predicate (the `cast` row) asks the same question.
//!
//! # Why stepping over it is sound
//!
//! The node is a copy LICM moved out of THIS loop: an `ArrayLength` or
//! `ArrayLoad` whose pc is at or past the header's, in the same spliced body
//! (a pre-header node built there has a pc BEFORE the header; nothing after the
//! loop is anchored in its pre-header). Its deopt resumes at the outermost
//! `invoke` with the caller's frame (LICM's `install_preheader_deopt_frame`;
//! no chain snapshot at its pc could re-route it to its in-loop position), so
//! the interpreter re-runs the callee from its first bytecode. The predicate's
//! guard resumes at the callee's loop header with the loop-entry values.
//! Both points are at or before every bytecode of the loop, nothing between
//! them and the pre-header is skipped, so whichever of the two fails first,
//! the interpreter raises what bytecode order raises first. That is the
//! argument the existing flat-frame exemption makes, with "at or before the
//! header" instead of "at the header".
//!
//! Any other trapping pre-header node still refuses, as does a hoisted node
//! whose deopt names a chain frame -- except the element copy at the header's
//! own pc that names the loop-entry chain (see
//! [`licm_array_load_spliced_chain_enabled`]), which resumes at the header.

use crate::ir::{Graph, IrSpliceResumeMap, Node, NodeId, Op};

/// `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST` -- **default ON**,
/// `=0` kill. Read per compile; no `static`.
pub(super) fn spliced_hoist_trap_exempt_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST",
    )
}

/// `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_SPLICED_CHAIN` (JIT round 23 wave 8, lane
/// W8-L7; perf, **default ON** since the w2c close (2026-10-24), `=0` kill:
/// with the late unroll round it is what takes `h.tab[5]` out of the timed
/// loop, which the main/post copies then no longer repeat; opt-in until then,
/// NEUTRAL alone on the n23-w8g A/B, W2c nocast
/// 2.11/1.21/3.88 ns/elem default vs 1.99/1.34/2.04 at `=0`, cast 3.05/1.94/3.18
/// vs 2.69/2.10/2.79): the array-load hoist's second round
/// (`CRATONVM_JIT_IR_LICM_ARRAY_LOAD_AFTER_PREDICATES`) also serves a loop
/// SPLICED from a callee, naming the loop-entry chain frame the loop's
/// null-base predicate guard already names ([`chain_predicate_entry_frame`]).
///
/// `h.tab[5]` in `W2cTypecheckLoops`' lambda-driven rows: after the null-base
/// predicate moved `h.tab` to the pre-header the element read was invariant,
/// but the planner wanted the header's FLAT snapshot, which a spliced header
/// never has, so its null test, bounds check and load stayed in the loop
/// (n23-w8c `lambda$main$2`, 0x342-0x360).
///
/// Sound as the flat hoist is: the copy sits in the pre-header after the
/// guard (a higher node id), reads the loop's entry memory in a body that
/// writes nothing it may alias, and a failing null or bounds check deopts with
/// the callee at its loop header and the entry values, every caller parked at
/// its invoke -- the interpreter runs the loop from its first test and raises
/// whatever the bytecode raises, where it raises it. Such a deopt is not a
/// listed chain predicate, so the VM charges it at the call site under the
/// wildcard, after which `chain_predicate_withdrawn` keeps the next compile's
/// null-base predicate (and so this hoist) out of that splice. Read per
/// compile; no `static`.
pub(super) fn licm_array_load_spliced_chain_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LICM_ARRAY_LOAD_SPLICED_CHAIN")
}

/// The snapshot index of the loop-entry CHAIN frame a chain predicate guard
/// (`Graph::chain_predicate_guards`) of the loop at combined pc `header_bci`
/// names: a live `Guard { bci: header_bci }` anchored at `entry_pred` whose
/// frame is a guard-usable chain snapshot at that pc. `None` with the switch
/// off, or when there is none.
pub(super) fn chain_predicate_entry_frame(
    graph: &Graph,
    entry_pred: NodeId,
    header_bci: usize,
) -> Option<u32> {
    if !licm_array_load_spliced_chain_enabled() {
        return None;
    }
    graph.chain_predicate_guards.iter().find_map(|&(g, _)| {
        let n = graph.node_opt(g)?;
        if !matches!(n.op, Op::Guard { bci } if bci == header_bci)
            || n.inputs.first() != Some(&entry_pred)
        {
            return None;
        }
        let si = n.frame_snapshot?;
        let sp = graph.safepoints.get(si as usize)?;
        (sp.is_guard_splice_state() && sp.bci == header_bci).then_some(si)
    })
}

/// The spliced body `[base, base + len)` holding combined pc `pc`: its base.
fn body_base(map: &IrSpliceResumeMap, pc: usize) -> Option<usize> {
    if pc < map.code_len {
        return None;
    }
    map.bodies
        .iter()
        .find(|&&(base, len, _)| pc >= base && pc < base.saturating_add(len))
        .map(|&(base, _, _)| base)
}

/// Is `node`, anchored in the pre-header of the loop whose header sits at
/// combined pc `header_bci`, LICM's copy of that spliced loop's own length or
/// element read, whose deopt resumes at or before the loop's first bytecode?
/// See the module notes. `false` whenever unsure.
pub(super) fn hoisted_from_spliced_loop(
    graph: &Graph,
    node: &Node,
    header_bci: usize,
    map: &IrSpliceResumeMap,
) -> bool {
    if !matches!(node.op, Op::ArrayLength | Op::ArrayLoad(_)) {
        return false;
    }
    let Some(pc) = node.bytecode_pc else {
        return false;
    };
    if pc < header_bci {
        return false;
    }
    let Some(header_body) = body_base(map, header_bci) else {
        return false;
    };
    if body_base(map, pc) != Some(header_body) {
        return false;
    }
    let Some(outer) = map.resume_bci(header_bci) else {
        return false;
    };
    match node.frame_snapshot {
        // Resolved by pc at lowering: the outermost invoke, unless a chain
        // snapshot describes the pc (it would resume in the loop, uncorrected).
        None => !graph
            .safepoints
            .iter()
            .any(|s| s.is_guard_splice_state() && s.bci == pc),
        // A flat frame at the outermost invoke, or (the element copy
        // `chain_predicate_entry_frame` lets the array-load hoist make) the
        // loop-entry chain itself, which resumes at the header.
        Some(si) => graph.safepoints.get(si as usize).is_some_and(|s| {
            (!s.is_splice_state() && s.bci == outer)
                || (s.is_guard_splice_state() && s.bci == header_bci && pc == header_bci)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrType, MemKind, NodeId, SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame};

    const SW: &str = "CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_SPLICED_HOIST";

    /// The compiling method is 20 bytes; one callee spliced at 100..143 for the
    /// `invoke` at pc 6. The callee's loop header is at combined pc 105.
    fn map() -> IrSpliceResumeMap {
        IrSpliceResumeMap {
            code_len: 20,
            bodies: vec![(100, 43, 6), (200, 30, 120)],
        }
    }

    /// A pre-header `entry` with one node `op` at `pc` anchored on it, naming
    /// `frame` when given. Returns `(graph, entry, node)`.
    fn preheader_with(
        op: Op,
        pc: usize,
        frame: Option<SafepointSnapshot>,
    ) -> (Graph, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let inputs = match op {
            Op::ArrayLoad(_) => vec![entry, mem, a, zero],
            _ => vec![entry, mem, a],
        };
        let n = g.add(op, IrType::Int, inputs, Some(pc));
        if let Some(sp) = frame {
            g.push_safepoint(sp);
            let si = (g.safepoints.len() - 1) as u32;
            // Directly: `set_node_frame_snapshot` reads the build's splice
            // map (a thread-local no test build published) for a frame at
            // the outermost invoke.
            g.nodes[n as usize].frame_snapshot = Some(si);
        }
        (g, entry, n)
    }

    fn chain_at(pc: usize) -> SafepointSnapshot {
        let mut s = SafepointSnapshot::new(pc, vec![], vec![], vec![]);
        s.splice_scope = Some(Box::new(SpliceScopeChain {
            scopes: vec![
                SpliceScopeFrame {
                    method_key: "T.row:([I)J".to_string(),
                    bci: (pc - 100) as u32,
                    code_len: 43,
                    max_locals: 4,
                    locals_len: 0,
                    stack_len: 0,
                },
                SpliceScopeFrame {
                    method_key: String::new(),
                    bci: 6,
                    code_len: 0,
                    max_locals: 0,
                    locals_len: 0,
                    stack_len: 0,
                },
            ],
        }));
        s
    }

    fn exempt(g: &Graph, n: NodeId, header: usize) -> bool {
        hoisted_from_spliced_loop(g, &g.nodes[n as usize], header, &map())
    }

    #[test]
    fn r23_w8_l7_the_hoisted_length_of_a_spliced_loop_is_stepped_over() {
        // `a.length` at callee bci 8 (combined 108), header at 105, resolved
        // by pc (the lambda's invoke): exempt.
        let (g, entry, n) = preheader_with(Op::ArrayLength, 108, None);
        assert!(exempt(&g, n, 105));
        // The same with LICM's explicit flat frame at the outermost invoke.
        let (g2, _, n2) = preheader_with(
            Op::ArrayLength,
            108,
            Some(SafepointSnapshot::new(6, vec![], vec![], vec![])),
        );
        assert!(exempt(&g2, n2, 105));
        // The element read too.
        let (g3, _, n3) = preheader_with(Op::ArrayLoad(MemKind::Int), 110, None);
        assert!(exempt(&g3, n3, 105));
        // The array-load hoist's copy at the header pc naming the loop-entry
        // chain (`chain_predicate_entry_frame`).
        let (g4, _, n4) = preheader_with(Op::ArrayLoad(MemKind::Int), 105, Some(chain_at(105)));
        assert!(exempt(&g4, n4, 105));
        // Through the predicate's trap test: no map is the old answer
        // (refuses), the map steps over it; the kill switch restores the
        // refusal.
        assert!(super::super::preheader_traps_block_entry_predicate_with(&g, entry, 105, None));
        let m = map();
        assert!(!super::super::preheader_traps_block_entry_predicate_with(
            &g,
            entry,
            105,
            Some(&m)
        ));
        cratonvm_types::flags::with_thread_overrides(&[(SW, Some("0"))], || {
            assert!(!spliced_hoist_trap_exempt_enabled());
        });
        cratonvm_types::flags::with_thread_overrides(&[(SW, None)], || {
            assert!(spliced_hoist_trap_exempt_enabled(), "default ON");
        });
    }

    /// `for (i = 0; i < n; i++) s += tab[5];` spliced at combined pcs 100+
    /// (header 110), with a null-base-style chain predicate guard in the
    /// pre-header naming the loop-entry chain frame (listed or not).
    /// Returns `(graph, entry, the in-loop element read, the guard)`.
    fn spliced_element_loop(listed: bool) -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let tab = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let five = g.add(Op::Const(5), IrType::Int, vec![], None);
        // The predicate: a guard at the header's combined pc, anchored at the
        // pre-header, naming the chain frame.
        g.push_safepoint(chain_at(110));
        let si = (g.safepoints.len() - 1) as u32;
        let cond = g.add(Op::Cmp(crate::ir::CmpOp::Lt), IrType::Int, vec![zero, n], None);
        let guard = g.add(Op::Guard { bci: 110 }, IrType::Void, vec![entry, cond], Some(110));
        g.nodes[guard as usize].frame_snapshot = Some(si);
        if listed {
            g.chain_predicate_guards
                .push((guard, crate::deopt::PredicateKind::NullBase as u8));
        }
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(110));
        let i = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let s = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let lt = g.add(Op::Cmp(crate::ir::CmpOp::Lt), IrType::Int, vec![i, n], None);
        let head = g.add(Op::If, IrType::Control, vec![region, lt], Some(112));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let el = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![stay, mem0, tab, five],
            Some(120),
        );
        let s1 = g.add(Op::Add, IrType::Int, vec![s, el], None);
        let i1 = g.add(Op::Add, IrType::Int, vec![i, one], None);
        g.nodes[region as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(i1);
        g.nodes[s as usize].inputs.push(s1);
        g.exit = g.add(Op::Return, IrType::Void, vec![exit, s], Some(130));
        (g, entry, el, guard)
    }

    #[test]
    fn r23_w8_l7_a_spliced_element_read_hoists_under_the_predicates_chain_frame() {
        const HOIST: &str = "CRATONVM_JIT_IR_LICM_ARRAY_LOAD_SPLICED_CHAIN";
        // Listed predicate: its frame is found, and the element read moves to
        // the pre-header naming exactly that frame (no new snapshot).
        let (mut g, entry, el, guard) = spliced_element_loop(true);
        let si = g.nodes[guard as usize].frame_snapshot;
        let before = g.safepoints.len();
        let moved = cratonvm_types::flags::with_thread_overrides(&[(HOIST, Some("1"))], || {
            assert_eq!(chain_predicate_entry_frame(&g, entry, 110), si);
            super::super::hoist_invariant_array_loads(&mut g, None, "")
        });
        assert_eq!(moved, 1);
        assert_eq!(g.nodes[el as usize].op, Op::Dead, "the in-loop read is retired");
        assert_eq!(g.safepoints.len(), before, "the predicate's frame is reused");
        let copy = g
            .nodes
            .iter()
            .position(|n| {
                matches!(n.op, Op::ArrayLoad(_)) && n.inputs.first() == Some(&entry)
            })
            .expect("a pre-header copy");
        assert_eq!(g.nodes[copy].frame_snapshot, si);
        assert_eq!(g.nodes[copy].bytecode_pc, Some(110));
        assert!(copy > guard as usize, "after the guard in the pre-header");
        // Kill switch: nothing moves.
        let (mut g, entry, el, _) = spliced_element_loop(true);
        let moved = cratonvm_types::flags::with_thread_overrides(&[(HOIST, Some("0"))], || {
            assert_eq!(chain_predicate_entry_frame(&g, entry, 110), None);
            super::super::hoist_invariant_array_loads(&mut g, None, "")
        });
        assert_eq!(moved, 0);
        assert_ne!(g.nodes[el as usize].op, Op::Dead);
        // An unlisted guard is no predicate frame, and another pc finds none.
        cratonvm_types::flags::with_thread_overrides(&[(HOIST, Some("1"))], || {
            let (g, entry, _, _) = spliced_element_loop(false);
            assert_eq!(chain_predicate_entry_frame(&g, entry, 110), None);
            let (g, entry, _, _) = spliced_element_loop(true);
            assert_eq!(chain_predicate_entry_frame(&g, entry, 112), None);
        });
        cratonvm_types::flags::with_thread_overrides(&[(HOIST, None)], || {
            assert!(licm_array_load_spliced_chain_enabled(), "default ON (w2c close)");
        });
    }

    #[test]
    fn r23_w8_l7_every_other_pre_header_trap_still_refuses() {
        // A pc before the header: built in the pre-header, not hoisted.
        let (g, _, n) = preheader_with(Op::ArrayLength, 103, None);
        assert!(!exempt(&g, n, 105));
        // Another spliced body.
        let (g, _, n) = preheader_with(Op::ArrayLength, 210, None);
        assert!(!exempt(&g, n, 105));
        // The compiling method's own header: not a splice.
        let (g, _, n) = preheader_with(Op::ArrayLength, 12, None);
        assert!(!exempt(&g, n, 10));
        // A division hoisted from the loop is not LICM's length/element copy.
        let (g, _, n) = preheader_with(Op::Div, 108, None);
        assert!(!exempt(&g, n, 105));
        // A chain snapshot at its pc would resume it inside the loop.
        let (mut g, _, n) = preheader_with(Op::ArrayLength, 108, None);
        g.push_safepoint(chain_at(108));
        assert!(!exempt(&g, n, 105));
        // A frame naming a chain, or a flat frame elsewhere.
        let (g, _, n) = preheader_with(Op::ArrayLength, 108, Some(chain_at(105)));
        assert!(!exempt(&g, n, 105));
        let (g, _, n) = preheader_with(
            Op::ArrayLength,
            108,
            Some(SafepointSnapshot::new(3, vec![], vec![], vec![])),
        );
        assert!(!exempt(&g, n, 105));
    }
}
