use super::*;
use std::collections::HashMap;

/// Publish, on this thread, the splice resume map of a build whose caller
/// is 25 bytes long with one body at `[25, 35)` spliced for the invoke at
/// bci 20 -- the way a real build publishes it (`IrBuilder::build` does
/// so before it reads a byte). The caller holds the returned scope, whose
/// drop clears the map again.
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
        sites: HashMap::from([(20usize, site)]),
        ..crate::ir::IrInlineTables::default()
    });
    // Every byte a `return`: the walk ends at pc 0, and whether the build
    // succeeds is irrelevant -- the map is published before it starts.
    let code = vec![0xb1u8; 40];
    let _ = b.build(&code, 25);
    assert_eq!(
        crate::ir::splice_resume_map_this_build().and_then(|m| m.resume_bci(30)),
        Some(20),
        "the fixture needs pc 30 to resume at the invoke at 20"
    );
    scope
}

/// `for (i = 0; i < box.size(); i++) {}` after the getter was spliced: the
/// header merge sits at the invoke's bci 20, the getter's `arraylength`
/// carries the RELOCATED pc 30 and deopts at bci 20 with the snapshot
/// there, which names the induction φ. Returns
/// `(graph, arraylength, preheader, arr, zero, iv)`.
fn relocated_arraylength_in_the_header() -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(20));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![region, m0], None);
    g.nodes[mem_phi as usize].inputs.push(mem_phi);
    let len = g.add(
        Op::ArrayLength,
        IrType::Int,
        vec![region, mem_phi, arr],
        Some(30),
    );
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, len], None);
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(23));
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], None);
    g.nodes[iv as usize].inputs.push(next);
    g.nodes[region as usize].inputs.push(back);
    let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(20, vec![arr, iv], vec![arr], vec![]));
    (g, len, c0, arr, zero, iv)
}

/// The last residue of `r11-iropt-licm-preheader-deopt-reads-unwritten-phi-slots`:
/// a relocated-pc read whose resume-bci snapshot is SHARED (the header
/// merge carries bci 20 too) was refused in wave 5, because
/// `Graph::set_node_frame_snapshot` would not name a frame at a bci other
/// than the node's own pc. With the splice map read there, it hoists, and
/// names a corrected copy at bci 20 with the φ at its entry value.
#[test]
fn a_relocated_read_with_a_shared_resume_frame_hoists_with_a_corrected_copy() {
    let _scope = publish_one_splice_at_20();
    let (mut g, len, pre, arr, zero, iv) = relocated_arraylength_in_the_header();
    assert!(
        !snapshot_serves_only(
            &g,
            len,
            0,
            20,
            crate::ir::splice_resume_map_this_build().as_ref()
        ),
        "the header merge shares bci 20, so the original must stay"
    );
    assert!(licm(&mut g));
    assert_eq!(g.nodes[len as usize].inputs[0], pre, "hoisted");
    assert_eq!(g.safepoints.len(), 2);
    assert_eq!(g.safepoints[0].locals, vec![arr, iv], "original untouched");
    let si = g.nodes[len as usize]
        .frame_snapshot
        .expect("names its corrected copy");
    let sp = &g.safepoints[si as usize];
    assert_eq!(sp.bci, 20, "the RESUME bci, not the relocated pc");
    assert_eq!(sp.locals, vec![arr, zero], "not the phi {iv}");
}
