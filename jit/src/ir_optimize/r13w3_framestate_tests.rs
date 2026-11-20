use super::*;
use crate::ir::{IrSpliceResumeMap, SpliceScopeChain, SpliceScopeFrame};

fn chain_snapshot(bci: usize, locals: Vec<NodeId>, stack: Vec<NodeId>) -> SafepointSnapshot {
    let callee_stack = stack.len() as u32 - 1;
    let mut sp = SafepointSnapshot::new(bci, locals, stack, Vec::new());
    sp.splice_scope = Some(std::sync::Arc::new(SpliceScopeChain {
        scopes: vec![
            SpliceScopeFrame {
                class_id: 0,
                method_key: "P.f:(I)I".to_string(),
                bci: 2,
                code_len: 5,
                max_locals: 1,
                locals_len: 0,
                stack_len: callee_stack,
            },
            SpliceScopeFrame {
                class_id: 0,
                method_key: String::new(),
                bci: 1,
                code_len: 0,
                max_locals: 0,
                locals_len: 1,
                stack_len: 0,
            },
        ],
    }));
    sp
}

/// A load at combined pc 12 of a body spliced for the `invoke` at bci 1.
/// The flat frame at 1 (the fall-back) and the chain frame at 12 (the
/// preferred one) are both consultable; a chain frame at a pc no trapping
/// node carries is not.
#[test]
fn a_spliced_trap_roots_its_chain_frame_and_the_flat_one() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, zero],
        Some(12),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], Some(4));
    g.push_safepoint(SafepointSnapshot::new(1, vec![p], vec![p], Vec::new()));
    g.push_safepoint(chain_snapshot(12, vec![p], vec![p, p]));
    g.push_safepoint(chain_snapshot(13, vec![p], vec![p, p]));
    let map = IrSpliceResumeMap {
        code_len: 5,
        bodies: vec![(10, 5, 1)],
    };
    let mask = consultable_snapshots_with(&g, Some(&map)).expect("attributable");
    assert_eq!(mask, vec![true, true, false]);
}

/// The unroller's appended per-iteration frame keeps the original's chain
/// and nested-lock marks; without the chain it would read as a flat frame
/// of the compiling method at a combined-buffer pc.
#[test]
fn a_per_copy_frame_keeps_its_chain_and_its_lock_marks() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let y = g.add(Op::Const(7), IrType::Int, vec![], None);
    let clone = g.add(Op::Add, IrType::Int, vec![y, y], Some(12));
    let mut orig = chain_snapshot(12, vec![x], vec![x, x]);
    orig.monitor_elided_mask = 0b10;
    g.push_safepoint(orig.clone());
    let subst: FxHashMap<NodeId, NodeId> = [(x, y)].into_iter().collect();
    install_copy_frames(&mut g, &[(0, orig.clone())], &subst, &[clone], false);
    assert_eq!(g.safepoints.len(), 2);
    let copy = &g.safepoints[1];
    assert_eq!(copy.locals, vec![y]);
    assert_eq!(copy.stack, vec![y, y]);
    assert_eq!(copy.splice_scope, orig.splice_scope);
    assert_eq!(copy.monitor_elided_mask, 0b10);
    assert_eq!(g.nodes[clone as usize].frame_snapshot, Some(1));
}

/// Publish the splice resume map of a build whose caller is 25 bytes long
/// with one body at `[25, 35)` spliced for the invoke at bci 20 (the same
/// fixture as the LICM tests' helper). The returned scope clears it.
fn publish_one_splice_at_20() -> crate::ir::IrBuildResultsScope {
    let scope = crate::ir::IrBuildResultsScope::enter();
    let site = crate::ir::IrInlineSite {
        base: 25,
        code_len: 10,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: true,
        method_key: "P.size:()I".to_string(),
        class_id: 0,
    };
    let mut b = crate::ir::IrBuilder::new(1, 1);
    b.apply_inline_tables(crate::ir::IrInlineTables {
        sites: std::collections::HashMap::from([(20usize, site)]),
        ..crate::ir::IrInlineTables::default()
    });
    let code = vec![0xb1u8; 40];
    let _ = b.build(&code, 25);
    assert_eq!(
        crate::ir::splice_resume_map_this_build().and_then(|m| m.resume_bci(30)),
        Some(20)
    );
    scope
}

/// Per-copy frames (`CRATONVM_JIT_IR_PER_COPY_FRAMES`) for a loop whose
/// body holds a SPLICED trapping load with no chain frame: the load resumes
/// at its invoke (bci 20), so that flat frame is a program point of every
/// copy. It used to be left unplanned -- copy k then resumed with copy 0's
/// values, and with `NO_NODE` for every body value the teardown killed.
#[test]
fn a_spliced_clone_without_a_chain_frame_gets_a_per_copy_flat_frame() {
    let _scope = publish_one_splice_at_20();
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    // A body value the invoke's frame names (its argument), and the
    // spliced load at combined pc 30.
    let x = g.add(Op::Add, IrType::Int, vec![one, one], Some(18));
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, one],
        Some(30),
    );
    g.push_safepoint(SafepointSnapshot::new(20, vec![p], vec![x], Vec::new()));
    let to_clone: FxHashSet<NodeId> = [x, load].into_iter().collect();
    let plan = plan_copy_frames(&g, &to_clone).expect("plannable");
    assert_eq!(plan.len(), 1, "the invoke's frame is planned");
    assert_eq!(plan[0].1.bci, 20);
    let y = g.add(Op::Const(9), IrType::Int, vec![], None);
    let load2 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, p, one],
        Some(30),
    );
    let subst: FxHashMap<NodeId, NodeId> = [(x, y), (load, load2)].into_iter().collect();
    install_copy_frames(&mut g, &plan, &subst, &[load2], false);
    assert_eq!(g.safepoints.len(), 2);
    assert_eq!(
        g.safepoints[1].stack,
        vec![y],
        "copy 1's frame names copy 1's value"
    );
    assert_eq!(
        g.nodes[load2 as usize].frame_snapshot,
        Some(1),
        "the spliced clone resumes into its own copy's frame"
    );
}

// JIT round 19 wave 12, lane tail12
// (`r19-unrolled-copies-hand-guards-the-post-call-chain-snapshot-20261007`):
// at a spliced call's successor the builder records the guard chain
// snapshot, then a post-call-only one, at one pc. Copy 0 (rewritten in
// place) and copy 1 (appended) both bind the clone at that pc to the
// GUARD snapshot, and copy 1's post-call snapshot stays post-call-only.
// Under the kill word the old last-write-wins binding returns.

