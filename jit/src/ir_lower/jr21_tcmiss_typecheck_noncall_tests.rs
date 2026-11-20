use super::{
    ir_lower_machine_model, ir_poll_preserves_fp, lower, plan_register_residency, plan_slots,
    typecheck_noncall_positions, JitRuntimeHelpers, LsForce, IR_LOWER_LS_XMMS,
    IR_LOWER_SAVED_XMMS, TYPECHECK_FP_RELOAD_KEY,
};
use crate::ir::{Graph, IrType, MemKind, NodeId, Op};
use crate::ir_schedule;
use crate::regalloc::{
    allocate_linear_scan, build_live_model, MachineModel, PhysReg, RegClass, RegFile, RegSpec,
};

fn instance_of() -> Op {
    Op::InstanceOf {
        name_ptr: 0,
        name_len: 0,
        seen_cell: 0,
        header_only: false,
    }
}

/// `a` (a double) is stored to an array BEFORE the check, whose memory
/// input is that store, and read again AFTER it: live across the check by
/// construction, whatever the intra-block order of the pure nodes.
fn double_across_a_check() -> (Graph, NodeId, NodeId) {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Double, vec![start], Some(0));
    let arr = graph.add(Op::Param(1), IrType::Ref, vec![start], Some(0));
    let o = graph.add(Op::Param(2), IrType::Ref, vec![start], Some(0));
    let idx = graph.add(Op::Const(0), IrType::Int, vec![], Some(1));
    let st = graph.add(
        Op::ArrayStore(MemKind::Double),
        IrType::Memory,
        vec![ctrl, mem, arr, idx, a],
        Some(2),
    );
    let inst = graph.add(instance_of(), IrType::Int, vec![ctrl, st, o], Some(3));
    let r = graph.add(Op::I2D, IrType::Double, vec![inst], Some(6));
    let s = graph.add(Op::Add, IrType::Double, vec![r, a], Some(7));
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, s], Some(8));
    (graph, inst, a)
}

/// The allocator test: with every XMM caller-saved (System V's file), the
/// double keeps ONE register across the check under the new model, and
/// cannot under `MachineModel::for_graph`'s (the positive control: the
/// check still reads as a call there).
#[test]
fn a_double_keeps_a_caller_saved_xmm_across_an_instanceof() {
    let (graph, inst, a) = double_across_a_check();
    let schedule = ir_schedule::schedule(&graph);
    let live = build_live_model(&graph, &schedule);
    assert!(live.converged);
    let pos = live.pos_of[inst as usize].expect("the check is scheduled");
    let a_range = live.range[a as usize].expect("a is live");
    assert!(
        a_range.lo < pos && pos < a_range.hi,
        "a is live across the check by construction"
    );
    assert_eq!(
        typecheck_noncall_positions(&graph, &live),
        vec![(pos, inst)]
    );

    let file = || {
        RegFile::from_specs(IR_LOWER_LS_XMMS.iter().map(|&n| RegSpec {
            reg: PhysReg::xmm(n),
            caller_saved: true,
        }))
    };
    let bare = MachineModel::for_graph(&graph, &schedule, &live, file());
    let ours = ir_lower_machine_model(&graph, &schedule, &live, file(), None);
    let xmm_clobbered = |m: &MachineModel| {
        m.clobbers
            .iter()
            .any(|(p, regs)| *p == pos && regs.iter().any(|r| r.class == RegClass::Xmm))
    };
    let in_one_register = |m: &MachineModel| match allocate_linear_scan(&graph, &live, m) {
        Ok(alloc) => {
            matches!(alloc.segments[a as usize].as_slice(), [seg] if seg.reg.is_some())
        }
        Err(_) => false,
    };
    assert!(
        xmm_clobbered(&bare),
        "MachineModel::for_graph still models the check as a call"
    );
    assert!(
        !in_one_register(&bare),
        "positive control: a clobbered XMM bank cannot hold the double across the check"
    );
    if !(crate::typecheck_rows::typecheck_noncall_enabled() && ir_poll_preserves_fp()) {
        assert!(xmm_clobbered(&ours), "switch off: the round head's model");
        return;
    }
    assert!(
        !xmm_clobbered(&ours),
        "the check's position destroys no XMM register"
    );
    assert!(
        in_one_register(&ours),
        "no caller-saved spill around the check: one register for the whole range"
    );
    assert!(
        ours.clobbers.windows(2).all(|w| w[0].0 < w[1].0),
        "the clobber list stays sorted with one entry per position"
    );
    // The residency plan, when it takes the double into a caller-saved
    // XMM, names it in the check's reload set.
    let plan = plan_slots(&graph, &schedule, None);
    if let Ok(Some(res)) = plan_register_residency(&graph, &schedule, &plan) {
        if let Some(reg) = res.reg_of[a as usize] {
            if !IR_LOWER_SAVED_XMMS.contains(&reg) {
                assert!(
                    res.poll_fp_reloads
                        .get(&(TYPECHECK_FP_RELOAD_KEY | inst as usize))
                        .is_some_and(|ids| ids.contains(&a)),
                    "xmm{reg} is caller-saved: the check's slow path reloads it"
                );
            }
        }
    }
}

/// Executed: a helper that zeroes every caller-saved XMM register (what
/// any `extern "C"` callee may do) answers the check; the double read
/// after it is still the parameter.
#[test]
fn a_double_resident_across_an_instanceof_survives_a_helper_that_clobbers_the_xmm_file() {
    let mut stub = crate::ExecutableBuffer::new(256).expect("executable buffer");
    // XMM6/XMM7 are callee-saved on Win64; every other is volatile.
    #[cfg(target_os = "windows")]
    let volatile = 0u8..6;
    #[cfg(not(target_os = "windows"))]
    let volatile = 0u8..8;
    for x in volatile {
        stub.emit(&[0x0F, 0x57, 0xC0 | (x << 3) | x]); // XORPS xmmX, xmmX
    }
    stub.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    assert!(!stub.overflowed());
    stub.finalize();

    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let a = graph.add(Op::Param(0), IrType::Double, vec![start], Some(0));
    let o = graph.add(Op::Param(1), IrType::Ref, vec![start], Some(0));
    let sq = graph.add(Op::Mul, IrType::Double, vec![a, a], Some(1));
    let inst = graph.add(instance_of(), IrType::Int, vec![ctrl, mem, o], Some(2));
    let r = graph.add(Op::I2D, IrType::Double, vec![inst], Some(5));
    let s1 = graph.add(Op::Add, IrType::Double, vec![sq, r], Some(6));
    let s2 = graph.add(Op::Add, IrType::Double, vec![s1, a], Some(7));
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, s2], Some(8));
    let schedule = ir_schedule::schedule(&graph);

    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; only the check's helper is
    // reached.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.instanceof_check = stub.as_ptr() as usize;
    let _ls = LsForce::on();
    let compiled = lower(&graph, &schedule, 2, 2, &helpers).expect("the body lowers");
    let obj = Box::new(0u64);
    let obj_addr = &*obj as *const u64 as i64; // Cast: a dummy receiver's address
    for x in [3.5f64, -0.25, 1e300, 0.0] {
        // SAFETY: two arguments as raw bits in integer slots (the all-GPR
        // entry ABI); the only call is the stub, which reads nothing.
        let got = unsafe { compiled.try_call_with_context(7, &[x.to_bits() as i64, obj_addr]) }
            .expect("runs");
        assert_eq!(f64::from_bits(got as u64), x * x + 1.0 + x, "x = {x}");
    }
    drop(obj);
}
