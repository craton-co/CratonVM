use super::*;
use crate::ir::{CmpOp, MemKind, SafepointSnapshot};

/// A leaked `CharSequence.charAt(I)C` interface row.
fn charat_row() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "java/lang/CharSequence",
        method_name: "charAt",
        descriptor: "(I)C",
        num_jit_args: 2,
        return_type: b'C',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

/// `while (i < n) { s.charAt(src.f); i++; }` with a loop-entry
/// class guard on `src` (when `guarded`) and a flat snapshot at the call's
/// pc 12 (when `frame`). Returns `(graph, call, the field read)`.
fn charat_loop(guarded: bool, frame: bool) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let src = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let s = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(2), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    if guarded {
        let is = g.add(
            Op::ExactClassIs { class_id: 7 },
            IrType::Int,
            vec![src],
            None,
        );
        g.add(Op::Guard { bci: 5 }, IrType::Void, vec![entry, is], Some(5));
        // As `class_predicate` records a header it planted for.
        g.seq_charat_trap_headers.insert(5);
    }
    let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
    let i = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let mem = g.add(Op::Phi, IrType::Memory, vec![region, mem0], None);
    let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i, n], None);
    let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
    let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
    let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let ld = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![stay, mem, src, f0],
        Some(10),
    );
    g.field_decl_class.insert(ld, 3);
    let call = g.add(
        Op::Call {
            info_ptr: charat_row(),
        },
        IrType::Int,
        // `s.charAt(src.f)`: the read is the call's token and its index.
        vec![stay, ld, s, ld],
        Some(12),
    );
    let i1 = g.add(Op::Add, IrType::Int, vec![i, one], None);
    g.nodes[region as usize].inputs.push(stay);
    g.nodes[i as usize].inputs.push(i1);
    g.nodes[mem as usize].inputs.push(call);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(20));
    g.push_safepoint(SafepointSnapshot::new(
        5,
        vec![src, s, n, i],
        vec![],
        vec![],
    ));
    if frame {
        g.push_safepoint(SafepointSnapshot::new(
            12,
            vec![src, s, n, i],
            vec![s, i],
            vec![],
        ));
    }
    (g, call, ld)
}

/// The prefix kind the predicate keys on is resolvable in this build (a
/// run with the String prefix switched off has nothing to test).
fn prefix_names_charat() -> bool {
    !crate::x64::narrow_oops_block_inline_fields()
        && crate::ir_string_search_prefix_kind("java/lang/CharSequence", "charAt", "(I)C")
            == Some(crate::IrStringSearchKind::SeqCharAt)
}

/// nio21 wave 6 (L5W5-3): the withdrawal is per SITE -- the bci the VM
/// charges the trap at -- and wave 5's per-method rule is the kill
/// switch's answer.
/// The charge site of a pc inside a splice is the OUTERMOST invoke the
/// map names; an unattributable pc falls back to the per-method rule.
#[test]
fn nio21w6_a_spliced_trap_is_charged_at_its_outermost_invoke() {
    let (g, _, _) = charat_loop(false, true);
    // A body of 20 bytes at combined pc 100 replaced the invoke at 7; a
    // nested one at 200 replaced the invoke at combined pc 105.
    let map = crate::ir::IrSpliceResumeMap {
        code_len: 50,
        bodies: vec![(100, 20, 7), (200, 10, 105)],
    };
    assert_eq!(trap_charge_bci(&g, 12, Some(&map)), Some(12));
    assert_eq!(trap_charge_bci(&g, 110, Some(&map)), Some(7));
    assert_eq!(trap_charge_bci(&g, 203, Some(&map)), Some(7));
    assert_eq!(trap_charge_bci(&g, 400, Some(&map)), None);
    assert_eq!(trap_charge_bci(&g, 12, None), Some(12));
}

/// A pc the map places in a splice needs a chain snapshot: the flat one
/// alone no longer counts as a trap frame.
#[test]
fn nio21w6_a_relocated_pc_needs_a_chain_frame() {
    let (g, _, _) = charat_loop(false, true);
    assert!(trap_frame_exists(&g, 12, None));
    let map = crate::ir::IrSpliceResumeMap {
        code_len: 10,
        bodies: vec![(10, 20, 3)],
    };
    assert!(!trap_frame_exists(&g, 12, Some(&map)));
}

// ── nio21 wave 21 (lane L3): the hoisted String reads ──

fn with_layout(mut g: Graph) -> Graph {
    g.string_layout = Some(crate::StringFieldLayout::new(0, Some(1), 2, 7));
    g
}

fn count(g: &Graph, f: impl Fn(&Node) -> bool) -> usize {
    g.nodes.iter().filter(|n| f(n)).count()
}

// ── JIT rx27 wave 1 (lane scanloop): the proven String receiver ──

/// `charat_loop(true, true)` with the String layout, plus (when
/// `class` is `Some`) a second loop-entry guard at the header's bci,
/// `ExactClassIs(s, class)` on the CALL's receiver `s` (the loop's own
/// guard tests `src`). Returns `(graph, call, the added guard)`.
fn proven_loop(class: Option<u32>) -> (Graph, NodeId, NodeId) {
    let (g, call, _) = charat_loop(true, true);
    let mut g = with_layout(g);
    let (entry, s) = {
        let n = &g.nodes[call as usize];
        let src_guard = g
            .nodes
            .iter()
            .find(|x| x.op == (Op::Guard { bci: 5 }))
            .expect("the loop's own guard");
        (src_guard.inputs[0], n.inputs[2])
    };
    let mut guard = NO_NODE;
    if let Some(class_id) = class {
        let is = g.add(Op::ExactClassIs { class_id }, IrType::Int, vec![s], None);
        guard = g.add(Op::Guard { bci: 5 }, IrType::Void, vec![entry, is], Some(5));
    }
    (g, call, guard)
}

#[test]
fn hoist_invariant_arraylist_get_handles_prehoisted_ed_and_inbody_null_lic_guard() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let list = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let c_ed_off = g.add(Op::Const(16), IrType::Int, vec![], None);

    // Pre-hoisted `elementData` sitting on `entry`, licensed by an in-body null guard below!
    let pre_ed = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![entry, mem0, list, c_ed_off],
        Some(2),
    );

    let region = g.add(Op::Region, IrType::Control, vec![entry, NO_NODE], Some(4));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], Some(4));
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(5));
    let iff = g.add(Op::If, IrType::Control, vec![region, lt], Some(5));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let in_body_ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![list, null], None);
    let in_body_lic = g.add(
        Op::Guard { bci: 6 },
        IrType::Void,
        vec![body, in_body_ne],
        Some(6),
    );
    g.nodes[pre_ed as usize].inputs.push(in_body_lic);
    let ed_len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![body, mem0, pre_ed],
        Some(7),
    );
    let mask3 = g.add(Op::Const(3), IrType::Int, vec![], None);
    let masked_idx = g.add(Op::And, IrType::Int, vec![i_phi, mask3], Some(8));
    let aload = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![body, ed_len, pre_ed, masked_idx, in_body_lic],
        Some(9),
    );
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(10));
    g.set_input(region, 1, body);
    g.set_input(i_phi, 2, i_next);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, aload], Some(11));

    g.push_safepoint(SafepointSnapshot::new(4, vec![list, i_phi], vec![], vec![]));
    let mut left = Vec::new();
    let mut replaced = 0usize;
    let planted = hoist_invariant_arraylist_get(
        &mut g,
        region,
        entry,
        &[body],
        Some(mem0),
        4,
        0,
        false,
        &mut left,
        &mut replaced,
    );
    assert!(
        planted > 0,
        "pre-hoisted elementData + in-body null Guard must admit hoisting"
    );
    assert_ne!(
        g.nodes[pre_ed as usize].op,
        Op::Dead,
        "pre-hoisted elementData must stay live"
    );
}
