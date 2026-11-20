// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

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
//! A value that is (or is a φ of) a fresh allocation gets none: escape
//! analysis runs after this pass and would read the word as an escape
//! (`reaches_allocation`).
//!
//! A value tested once gets no NEW word (it would only add a node in front of
//! the same single header read), but takes one the graph already has: the head
//! of a guarded splice's class-word chain (`CRATONVM_JIT_IR_CLASS_WORD_CHAIN`,
//! `..._SINGLE_TEST`) on the same value is the same pure word, so the check
//! shares that read for free.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};

/// The most nodes [`reaches_allocation`] visits before it declines.
const ALLOCATION_WALK_LIMIT: usize = 64;

/// Is `obj` a fresh allocation, or a reference φ one of whose inputs is (or
/// whose web reaches one)? Such a value is a candidate for escape analysis,
/// which runs AFTER this pass and reads every `Op::ClassWordOf` use of an
/// allocation as an escape (`EaOp::Other`; the same reason the fresh-class-test
/// fold exists, `ir_optimize::try_fold_fresh_class_test`), and which folds a
/// check of an allocation only while the check still has its three inputs
/// (`ea_ir_bridge`'s `inputs.len() != 3` screen; the fold itself is the
/// opt-in `CRATONVM_JIT_EA_FRESH_TYPECHECK_FOLD`, so this matters for the
/// pair of opt-in switches, not for a default compile). A word planted here
/// would keep a foldable `new` alive for the sake of a header read the
/// allocation's own class answers. A web larger than the walk limit also
/// declines: a missed sharing is a coverage loss, a planted word is a cost.
fn reaches_allocation(graph: &Graph, obj: NodeId) -> bool {
    let mut work = vec![obj];
    let mut seen: BTreeSet<NodeId> = BTreeSet::new();
    while let Some(n) = work.pop() {
        if !seen.insert(n) {
            continue;
        }
        if seen.len() > ALLOCATION_WALK_LIMIT {
            return true;
        }
        let Some(node) = graph.nodes.get(n as usize) else {
            continue;
        };
        match node.op {
            Op::New { .. } | Op::NewArray { .. } => return true,
            // Input 0 is the merge the φ joins at; the rest are the values.
            Op::Phi if node.ty == IrType::Ref => {
                work.extend(
                    node.inputs
                        .iter()
                        .skip(1)
                        .copied()
                        .filter(|&i| i != NO_NODE),
                );
            }
            _ => {}
        }
    }
    false
}

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
            || reaches_allocation(graph, obj)
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


