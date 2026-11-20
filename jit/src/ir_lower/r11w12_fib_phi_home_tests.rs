use super::*;

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; nothing here emits a helper call.
    unsafe { std::mem::zeroed() }
}

/// `fib`'s multi-return splice merge in miniature:
/// `n > 1 ? n + 1 : n`, joined by an `Int` phi that no safepoint names.
fn merge_phi_lowerer() -> (Lowerer<'static>, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let cmp = g.add(Op::Cmp(CmpOp::Gt), IrType::Int, vec![n, one], Some(2));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cmp], Some(2));
    let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    let sum = g.add(Op::Add, IrType::Int, vec![n, one], Some(9));
    let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(20));
    let phi = g.add(Op::Phi, IrType::Int, vec![merge, sum, n], Some(20));
    g.exit = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(20));
    let schedule = crate::ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
    let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
    let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
        Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
    let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(no_helpers()));
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let lo = Lowerer::new(
        graph,
        schedule,
        buf,
        1,
        1,
        plan,
        helpers,
        empty,
        &[],
        None,
        no_direct,
        no_ic,
        &no_compact,
        no_frame_sites,
    );
    (lo, phi)
}

/// A phi the residency plan gave NO register keeps its home word.
///
/// Before the fix `phi_home_droppable`'s widening ("no reachable deopt
/// names it") dropped this home although nothing else could hold the
/// value, and `emit_phi_copies` then refused the method at the first
/// incoming edge -- every recursively spliced `CratonBench.fib` body
/// (`r11w11-orch-recursive-inline-bodies-refused-unallocated-phi`).
#[test]
fn r11w12_a_phi_without_a_register_keeps_its_home() {
    let _deopt_regs = DeoptRegsForce::on();
    let (mut lo, phi) = merge_phi_lowerer();
    lo.phi_copy_regs_on = true;
    let n = lo.graph.nodes.len();
    lo.set_residency(RegResidency {
        reg_of: vec![None; n],
        gp_reg_of: vec![None; n],
        ..Default::default()
    });
    assert!(
        !lo.deopt_named_reachable
            .get(phi as usize)
            .copied()
            .unwrap_or(true),
        "fixture: no reachable deopt names the phi, so only the register clause can refuse"
    );
    assert!(
        !lo.home_dropped.get(phi as usize).copied().unwrap_or(false),
        "a phi with no register must keep its home: it is the only location the value has"
    );
}

/// The widening itself stands: the same phi WITH a register still drops
/// its home when no reachable deopt names it.
#[test]
fn r11w12_a_registered_unnamed_phi_still_drops_its_home() {
    let _deopt_regs = DeoptRegsForce::on();
    let (mut lo, phi) = merge_phi_lowerer();
    lo.phi_copy_regs_on = true;
    let n = lo.graph.nodes.len();
    let reg = *IR_LOWER_LS_GPRS.first().expect("a GP residency file");
    let mut gp_reg_of = vec![None; n];
    gp_reg_of[phi as usize] = Some(reg);
    lo.set_residency(RegResidency {
        reg_of: vec![None; n],
        gp_reg_of,
        promoted: 1,
        ..Default::default()
    });
    assert!(
        lo.home_dropped.get(phi as usize).copied().unwrap_or(false),
        "a register-resident phi no reachable deopt names needs no home word"
    );
}

/// The direct self-call's stack sample is `MOVZX EAX, SP; CMP EAX, imm32`
/// (was `MOV RAX, RSP; AND EAX, 0xFFFF; CMP ...`): one instruction fewer
/// per recursive call, same value.
#[test]
fn r11w12_the_self_call_stack_sample_is_one_movzx() {
    // static int f(int n) { return f(n); }
    // iload_0; invokestatic #2; ireturn
    let code = [0x1a, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00];
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/Fib",
        method_name: "f",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 4, // the direct self-recursive route
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(
        1usize,
        (info as *const JitInvokeInfo as usize, 1usize, b'I'),
    );
    builder.set_invoke_info(invoke_info);
    let graph = builder
        .build(&code, 5)
        .expect("IR build of a self-recursive call");
    let schedule = crate::ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.self_call_stack_guard = 0x7fff_0000_0000_4000;
    let empty_hints: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let cm = lower_inner(
        &graph,
        &schedule,
        1,
        1,
        &helpers,
        &empty_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
    .expect("the self-recursive body lowers");
    let bytes = cm.code_bytes();
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(
        has(&[0x0F, 0xB7, 0xC4, 0x3D]),
        "the stack sample is `MOVZX EAX, SP` followed by the span compare"
    );
    assert!(
        !has(&[0x48, 0x89, 0xE0, 0x25, 0xFF, 0xFF, 0x00, 0x00]),
        "the two-instruction `MOV RAX, RSP; AND EAX, 0xFFFF` sample is gone"
    );
}

/// `static int fib(int n) { if (n < 2) return n; return fib(n - 1) +
/// fib(n - 2); }`, both invokes bound to the direct self-recursive route.
const R11W13_FIB_CODE: [u8; 23] = [
    0x1a, 0x05, 0xa2, 0x00, 0x05, // iload_0; iconst_2; if_icmpge +5
    0x1a, 0xac, // iload_0; ireturn
    0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic
    0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic
    0x60, 0xac, // iadd; ireturn
    0x00, 0x00,
];

fn r11w13_fib_graph() -> Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/Fib",
        method_name: "fib",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 4, // the direct self-recursive route
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    let mut invoke_info = HashMap::new();
    for pc in [10usize, 16] {
        invoke_info.insert(pc, (info as *const JitInvokeInfo as usize, 1usize, b'I'));
    }
    builder.set_invoke_info(invoke_info);
    builder
        .build(&R11W13_FIB_CODE, 21)
        .expect("IR build of fib")
}

/// A `Lowerer` over the fib graph, with the two call ARGUMENTS sharing one
/// register and the two call RESULTS sharing another -- so neither is
/// `deopt_nameable` and a dropped home can only come from the
/// reachability clause. Returns the arguments and the calls.
fn r11w13_fib_lowerer(protected: bool) -> (Lowerer<'static>, Vec<NodeId>) {
    let graph: &'static Graph = Box::leak(Box::new(r11w13_fib_graph()));
    let schedule: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
    let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
    let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
        Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
    let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(no_helpers()));
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let mut lo = Lowerer::new(
        graph,
        schedule,
        buf,
        1,
        1,
        plan,
        helpers,
        empty,
        &[],
        None,
        no_direct,
        no_ic,
        &no_compact,
        no_frame_sites,
    );
    if protected {
        lo.protected_ranges = vec![(0, 21)];
    }
    let calls: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| Lowerer::is_direct_self_call(n))
        .map(|(i, _)| i as NodeId)
        .collect();
    assert_eq!(calls.len(), 2, "fixture: fib has two direct self-calls");
    let mut values: Vec<NodeId> = calls
        .iter()
        .filter_map(|&c| graph.nodes[c as usize].inputs.get(2).copied())
        .collect();
    assert_eq!(values.len(), 2, "fixture: each call has one argument");
    let n = graph.nodes.len();
    let arg_reg = IR_LOWER_LS_GPRS[0];
    let result_reg = IR_LOWER_LS_GPRS[1];
    let mut gp_reg_of = vec![None; n];
    for &a in &values {
        gp_reg_of[a as usize] = Some(arg_reg);
    }
    for &c in &calls {
        gp_reg_of[c as usize] = Some(result_reg);
    }
    lo.phi_copy_regs_on = true;
    lo.set_residency(RegResidency {
        reg_of: vec![None; n],
        gp_reg_of,
        promoted: 4,
        ..Default::default()
    });
    values.extend(calls);
    (lo, values)
}


/// ... and inside a protected range the call's reason-9 pad DOES read the
/// frame, so every value the frame states at the two invokes name keeps
/// its home. (An argument the invoke's snapshot no longer names -- it was
/// popped before the snapshot -- is read by no exit, so it may still drop.)
#[test]
fn r11w13_a_protected_self_call_still_pins_its_frame_state() {
    let _deopt_regs = DeoptRegsForce::on();
    let (lo, values) = r11w13_fib_lowerer(true);
    let named_at_invokes: Vec<NodeId> = values
        .iter()
        .copied()
        .filter(|&v| {
            lo.graph.safepoints.iter().any(|sp| {
                matches!(sp.bci, 10 | 16)
                    && sp
                        .locals
                        .iter()
                        .chain(sp.stack.iter())
                        .chain(sp.monitors.iter())
                        .any(|&x| x == v)
            })
        })
        .collect();
    assert!(
        !named_at_invokes.is_empty(),
        "fixture: the first call's result is live across the second call"
    );
    for &v in &named_at_invokes {
        assert!(
            lo.deopt_named_reachable
                .get(v as usize)
                .copied()
                .unwrap_or(false),
            "n{v} is named at a protected call, whose rethrow pad reads it"
        );
        assert!(
            !lo.home_dropped.get(v as usize).copied().unwrap_or(true),
            "n{v} shares its register, so its home is its only description"
        );
    }
}

/// The fib body predicts no root publication, so its prologue emits no
/// thread fetch and its exits no shadow-`top` restore.
#[test]
fn r11w13_a_reference_free_body_publishes_no_roots() {
    let (lo, _) = r11w13_fib_lowerer(false);
    assert!(lo.body_publishes_no_roots());
}

/// End to end: fib with both calls on the direct self-recursive route
/// lowers (the dropped homes named at the calls do not trip
/// `build_deopt_points`' latch, and the home census identity holds).
#[test]
fn r11w13_fib_with_direct_self_calls_lowers() {
    let graph = r11w13_fib_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.self_call_stack_guard = 0x7fff_0000_0000_4000;
    let empty_hints: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let cm = lower_inner(
        &graph,
        &schedule,
        1,
        1,
        &helpers,
        &empty_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
    .expect("fib with two direct self-calls lowers");
    assert!(!cm.code_bytes().is_empty());
}


/// Round 11 wave 14 (lane fib, `r11w14-rt-ir-lower-oop-free-frame-patch`):
/// the reference-free fib body is published as such, and each direct
/// self-call has a return-site row naming its own invoke, so a frame
/// suspended in either call is placed on the right line.
#[test]
fn r11w14_fib_publishes_the_reference_free_frame_contract() {
    let graph = r11w13_fib_graph();
    let schedule = crate::ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.self_call_stack_guard = 0x7fff_0000_0000_4000;
    let empty_hints: HashMap<usize, bool> = HashMap::new();
    let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let cm = lower_inner(
        &graph,
        &schedule,
        1,
        1,
        &helpers,
        &empty_hints,
        &[],
        None,
        &no_direct,
        &no_ic,
        &no_compact,
        &crate::ir::IrInlineFrameSites::default(),
    )
    .expect("fib lowers");
    assert!(cm.frame_holds_no_references, "fib keeps no reference");
    let table = &cm.return_site_bci_table;
    for bci in [10u32, 16] {
        assert!(
            table.iter().any(|&(_, b)| b == bci),
            "the self-call at bci {bci} has a return-site row: {table:?}"
        );
    }
    assert!(
        table.windows(2).all(|w| w[0].0 < w[1].0),
        "rows are strictly ascending by offset: {table:?}"
    );
    let len = u32::try_from(cm.code_bytes().len()).unwrap_or(u32::MAX);
    assert!(
        table.iter().all(|&(off, _)| off > 0 && off <= len),
        "every row is a return address inside the body: {table:?}"
    );
}


// Round 11 wave 16 (lane irback; step 4 of
// `r11w14-rt-ir-lower-oop-free-frame-patch`): a reference-free body's
// prologue drops the three GC-only reserved-tail stores (watermark, saved
// base, phi scratch) and keeps the thread-slot and safepoint-id zeroing;
// the kill switch restores all of them. The publication is unchanged.

