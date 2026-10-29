//! JIT round 21, lane tcfuse (TCPERF21-3,
//! `CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD`): consecutive type checks of one
//! value share one header read.
//!
//! `o instanceof A1; o instanceof Base` (or the six interface checks of
//! `R21TcperfRows.instanceofWide`) re-reads `o`'s class id behind its own null
//! and kind screens at every check; HotSpot's C2 loads the klass once (a GVN'd
//! `LoadNKlass`) and every check starts from it. This pass gives each value
//! that two or more `Op::InstanceOf` / `Op::CheckCast` test ONE
//! `Op::ClassWordOf` of it, appended as the checks' fourth input. The lowering
//! (`Lowerer::emit_typecheck_row_first_arm_from_word`) starts the status-first
//! row arm from that word -- `TEST ECX, ECX ; JZ <the old screens>` and the
//! row compare -- instead of `TEST RAX ; JZ ; TEST BYTE [RAX+kind] ; JNE ;
//! MOV ECX, [RAX]`.
//!
//! Why it is sound: `Op::ClassWordOf` is pure and total (the class id of a
//! non-null plain object, `0` for a null or an array, no load for a null), and
//! an object's class id and kind byte never change, so the word read at its
//! scheduled position is the word every check would read. A non-zero word is
//! exactly the row arm's precondition (a non-null plain object); a zero word
//! sends the check to its unchanged screens. The word is an `int`: no frame
//! state names it, the collector never reads it, and a check whose fourth
//! input is not a `ClassWordOf` of its own operand (a later pass rewired one of
//! them) ignores it.
//!
//! A value tested once gets no NEW word (it would only add a node in front of
//! the same single header read), but takes one the graph already has: the head
//! of a guarded splice's class-word chain (`CRATONVM_JIT_IR_CLASS_WORD_CHAIN`,
//! `..._SINGLE_TEST`) on the same value is the same pure word, so the check
//! shares that read for free.

use std::collections::BTreeMap;

use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};

/// Give every type check that does not carry one yet the shared
/// `Op::ClassWordOf` of its value: an existing one, or a new one when two or
/// more checks test the value. Returns how many values got a word.
pub(crate) fn share_typecheck_class_words(graph: &mut Graph) -> usize {
    if !crate::typecheck_rows::typecheck_shared_class_word_enabled() {
        return 0;
    }
    let mut by_obj: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        if !matches!(n.op, Op::InstanceOf { .. } | Op::CheckCast { .. }) || n.inputs.len() != 3 {
            continue;
        }
        let obj = n.inputs[2];
        if obj == NO_NODE
            || !matches!(graph.nodes.get(obj as usize), Some(o) if o.ty == IrType::Ref)
        {
            continue;
        }
        // Cast: a node index back to its id.
        by_obj.entry(obj).or_default().push(i as NodeId);
    }
    // The class words already in the graph, by the value they read.
    let mut existing: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        if matches!(n.op, Op::ClassWordOf)
            && n.inputs.len() == 1
            && by_obj.contains_key(&n.inputs[0])
        {
            // Cast: a node index back to its id.
            existing.entry(n.inputs[0]).or_insert(i as NodeId);
        }
    }
    let mut planted = 0usize;
    for (obj, checks) in by_obj {
        let word = match existing.get(&obj) {
            Some(&w) => w,
            None if checks.len() >= 2 => {
                let pc = graph.nodes[checks[0] as usize].bytecode_pc;
                graph.add(Op::ClassWordOf, IrType::Int, vec![obj], pc)
            }
            None => continue,
        };
        for check in checks {
            graph.push_input(check, word);
        }
        planted += 1;
    }
    if planted > 0 && crate::x64::dbg_jitc_enabled() {
        eprintln!("[cratonvm-jitc] ir typecheck shared class word: {planted} value(s)");
    }
    planted
}

#[cfg(test)]
mod jr21_tcfuse_class_word_tests {
    use super::*;

    fn checks_graph() -> (Graph, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(16);
        let ctrl = g.add(Op::Start, IrType::Control, vec![], None);
        let mem = g.add(Op::Param(0), IrType::Memory, vec![], None);
        let o = g.add(Op::Param(1), IrType::Ref, vec![], None);
        let p = g.add(Op::Param(2), IrType::Ref, vec![], None);
        let a = g.add(
            Op::InstanceOf {
                name_ptr: 0,
                name_len: 0,
                seen_cell: 0,
                header_only: false,
            },
            IrType::Int,
            vec![ctrl, mem, o],
            Some(1),
        );
        let b = g.add(
            Op::CheckCast {
                name_ptr: 0,
                name_len: 0,
            },
            IrType::Ref,
            vec![ctrl, a, o],
            Some(4),
        );
        let c = g.add(
            Op::InstanceOf {
                name_ptr: 0,
                name_len: 0,
                seen_cell: 0,
                header_only: false,
            },
            IrType::Int,
            vec![ctrl, b, p],
            Some(7),
        );
        (g, o, a, b, c)
    }

    /// The pass with its (opt-in) switch on.
    fn share(g: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD", Some("1"))],
            || share_typecheck_class_words(g),
        )
    }

    #[test]
    fn the_pass_is_opt_in() {
        let (mut g, ..) = checks_graph();
        let n = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD", None)],
            || share_typecheck_class_words(&mut g),
        );
        assert_eq!(n, 0, "unset plants no word");
    }

    #[test]
    fn two_checks_of_one_value_share_one_word_and_a_single_check_gets_none() {
        let (mut g, o, a, b, c) = checks_graph();
        assert_eq!(share(&mut g), 1);
        let word = g.nodes[a as usize].inputs[3];
        assert_eq!(g.nodes[b as usize].inputs.len(), 4);
        assert_eq!(g.nodes[b as usize].inputs[3], word);
        assert_eq!(g.nodes[word as usize].op, Op::ClassWordOf);
        assert_eq!(g.nodes[word as usize].inputs[0], o);
        assert_eq!(
            g.nodes[c as usize].inputs.len(),
            3,
            "a value tested once gets no word"
        );
        // Idempotent: checks that carry a word are not planted again.
        assert_eq!(share(&mut g), 0);
    }

    #[test]
    fn a_single_check_takes_a_class_word_the_graph_already_has() {
        let (mut g, _o, _a, _b, c) = checks_graph();
        let p = g.nodes[c as usize].inputs[2];
        let chain_head = g.add(Op::ClassWordOf, IrType::Int, vec![p], Some(9));
        assert_eq!(share(&mut g), 2);
        assert_eq!(g.nodes[c as usize].inputs.len(), 4);
        assert_eq!(
            g.nodes[c as usize].inputs[3], chain_head,
            "no second word of p"
        );
    }
}
