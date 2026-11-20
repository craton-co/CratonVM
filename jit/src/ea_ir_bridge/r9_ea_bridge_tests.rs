use super::*;
use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

/// `entry == 0`, `exit == NO_NODE`, as the struct literal this replaced.
fn empty_graph() -> Graph {
    Graph::empty(0)
}

/// `Holder h = new Holder(); h.f = new Inner(); Inner x = h.f; foo();`
/// — the builder threads the `getfield` onto the memory chain, so the
/// call's TOKEN is the field load. That ordering edge used to reach the
/// `Call` escape rule as if it were an argument, and through the load's
/// field edge it escaped `Inner`.
#[test]
fn a_field_load_used_only_as_a_memory_token_does_not_escape_what_it_reads() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let inner = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let holder = g.add(
        Op::New {
            class_id: 8,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![ctrl, mem, holder, off0, inner],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, store, holder, off0],
        None,
    );
    // No reference ARGUMENT: the load is only the call's memory token.
    // `info_ptr: 0` keeps it a plain `escape_analysis::Op::Call`.
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, load],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl], None);
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    assert!(
        !ea.nodes[id_map[call as usize]]
            .inputs
            .contains(&id_map[load as usize]),
        "the memory token is not an EA operand of the call"
    );
    let result = escape_analysis::analyze_escapes(&ea);
    assert_eq!(
        result.escape_states.get(&id_map[inner as usize]).copied(),
        Some(escape_analysis::EscapeState::NoEscape),
        "nothing publishes Inner: it is only READ, and the read is ordered before a call"
    );
}

/// The same shape with the load as a real ARGUMENT still escapes: only the
/// token slot is dropped.
#[test]
fn a_field_load_passed_as_an_argument_still_escapes_what_it_reads() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let inner = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let holder = g.add(
        Op::New {
            class_id: 8,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Ref),
        IrType::Memory,
        vec![ctrl, mem, holder, off0, inner],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, store, holder, off0],
        None,
    );
    let _call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, load, load],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl], None);
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        result
            .escape_states
            .get(&id_map[inner as usize])
            .copied()
            .is_some_and(|s| s.may_escape()),
        "an argument is an argument"
    );
}

/// `System.identityHashCode` is the one identity-hash shape; an
/// `invokespecial java/lang/Object.hashCode()I` can select an override
/// (JVMS §6.5, `ACC_SUPER`) and must stay an ordinary escaping call.
#[test]
fn only_system_identity_hash_code_is_an_identity_observation() {
    fn info(class: &'static str, name: &'static str, desc: &'static str, kind: u8) -> usize {
        // LEAK(intentional): test-only; the bridge reads it through a raw pointer.
        Box::leak(Box::new(JitInvokeInfo {
            class_name: class,
            method_name: name,
            descriptor: desc,
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        })) as *const JitInvokeInfo as usize
    }
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let sys = info(
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        3,
    );
    let sup = info("java/lang/Object", "hashCode", "()I", 1);
    let c_sys = g.add(
        Op::Call { info_ptr: sys },
        IrType::Int,
        vec![ctrl, mem, obj],
        None,
    );
    let c_sup = g.add(
        Op::Call { info_ptr: sup },
        IrType::Int,
        vec![ctrl, mem, obj],
        None,
    );
    assert!(ir_call_is_identity_hash(&g.nodes[c_sys as usize], sys));
    assert!(
        !ir_call_is_identity_hash(&g.nodes[c_sup as usize], sup),
        "invokespecial Object.hashCode may run an override"
    );
}

/// `p.x = 7; b.v = p.x;` with BOTH objects replaced in one round: `b`'s
/// deopt recipe must name the value `p.x` reads (7), not the load the
/// victim splice is about to kill.
#[test]
fn a_recipe_field_names_the_forwarded_value_not_the_retired_load() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let b = g.add(
        Op::New {
            class_id: 8,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let st_p = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, p, off0, seven],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, st_p, p, off0],
        None,
    );
    let _st_b = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, load, b, off0, load],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, seven], None);
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert_eq!(
        result.scalar_replaceable.len(),
        2,
        "both objects are replaceable"
    );
    let map =
        build_scalar_replacement_map(&g, &id_map, &result, &std::collections::HashSet::new());
    let recipe = map
        .objects
        .get(&b)
        .expect("b is elided, so it has a recipe");
    assert_eq!(
        recipe.field_values.first().copied().flatten(),
        Some(seven),
        "the recipe names what p.x read, not the retired load {load}"
    );
}

// ── Box/unbox fold (round 9 wave 10, collect10) ──────────────────

fn box_info(class: &'static str, desc: &'static str) -> usize {
    // LEAK(intentional): test-only; the bridge reads it through a raw pointer.
    Box::leak(Box::new(JitInvokeInfo {
        class_name: class,
        method_name: "valueOf",
        descriptor: desc,
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as *const JitInvokeInfo as usize
}

fn snapshot(
    bci: usize,
    locals: Vec<ir::NodeId>,
    stack: Vec<ir::NodeId>,
) -> ir::SafepointSnapshot {
    ir::SafepointSnapshot::new(bci, locals, stack, Vec::new())
}

/// `static int f(int v) { Integer a = v; g(); return a.intValue(); }`,
/// flattened: `valueOf` at bci 3, the box on the stack at bci 5, the
/// `intValue` at bci 7 (box in local 1 and on the stack), a token-only call
/// at bci 9, the return at 11. With `deopt_at_5`, a deoptable node sits at
/// bci 5, where the box is live.
struct BoxFixture {
    g: Graph,
    mem: ir::NodeId,
    v: ir::NodeId,
    call: ir::NodeId,
    unbox: ir::NodeId,
    after: ir::NodeId,
    ret: ir::NodeId,
}

fn box_fixture(deopt_at_5: bool) -> BoxFixture {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let call = g.add(
        Op::Call { info_ptr: info },
        IrType::Ref,
        vec![ctrl, mem, v],
        Some(3),
    );
    let mut chain = call;
    if deopt_at_5 {
        chain = g.add(
            Op::Call { info_ptr: 0 },
            IrType::Void,
            vec![ctrl, call],
            Some(5),
        );
    }
    let unbox = g.add(
        Op::Unbox {
            op: ir::UnboxOp::IntValue,
            class_id: 7,
        },
        IrType::Int,
        vec![ctrl, chain, call],
        Some(7),
    );
    let after = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, unbox],
        Some(9),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, unbox], Some(11));
    g.entry = start;
    g.exit = ret;
    g.push_safepoint(snapshot(3, vec![v, NO_NODE], vec![v]));
    g.push_safepoint(snapshot(5, vec![v, NO_NODE], vec![call]));
    g.push_safepoint(snapshot(7, vec![v, call], vec![call]));
    g.push_safepoint(snapshot(9, vec![v, NO_NODE], vec![unbox]));
    g.push_safepoint(snapshot(11, vec![v, NO_NODE], vec![unbox]));
    BoxFixture {
        g,
        mem,
        v,
        call,
        unbox,
        after,
        ret,
    }
}

/// The fold: the unbox's readers (value edge AND snapshot slot) now name
/// the primitive, the memory chain closes over both retired nodes, and no
/// snapshot slot names the dead box.
#[test]
fn integer_value_of_read_back_only_by_int_value_folds_to_its_argument() {
    let mut f = box_fixture(false);
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut f.g), 1);
    assert_eq!(
        f.g.nodes[f.call as usize].op,
        Op::Dead,
        "the valueOf call is gone"
    );
    assert_eq!(
        f.g.nodes[f.unbox as usize].op,
        Op::Dead,
        "the unbox is gone"
    );
    assert_eq!(
        f.g.nodes[f.ret as usize].inputs[1], f.v,
        "intValue() of the box is the int that was boxed"
    );
    assert_eq!(
        f.g.nodes[f.after as usize].inputs[1], f.mem,
        "the token chain closes over both retired nodes onto the entry memory"
    );
    let at = |bci: usize| {
        f.g.safepoints
            .iter()
            .find(|sp| sp.bci == bci)
            .expect("snapshot kept")
    };
    assert_eq!(
        at(9).stack[0],
        f.v,
        "a snapshot naming the unbox follows the value"
    );
    assert_eq!(at(5).stack[0], NO_NODE, "no snapshot names the dead box");
    assert_eq!(at(7).locals[1], NO_NODE, "no snapshot names the dead box");
}

/// A deopt that can resume where the box is live (a deoptable node at bci
/// 5, whose snapshot holds it) needs the object: refused, graph untouched.
#[test]
fn a_box_live_at_a_consultable_snapshot_is_not_folded() {
    let mut f = box_fixture(true);
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut f.g), 0);
    assert!(matches!(f.g.nodes[f.call as usize].op, Op::Call { .. }));
    assert!(matches!(f.g.nodes[f.unbox as usize].op, Op::Unbox { .. }));
    assert_eq!(f.g.nodes[f.ret as usize].inputs[1], f.unbox);
}

/// Any value use other than an unbox of the boxed width keeps the call.
#[test]
fn a_box_with_any_other_reader_is_not_folded() {
    // The box is returned: its identity is observable.
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let call = g.add(
        Op::Call { info_ptr: info },
        IrType::Ref,
        vec![ctrl, mem, v],
        None,
    );
    let _unbox = g.add(
        Op::Unbox {
            op: ir::UnboxOp::IntValue,
            class_id: 7,
        },
        IrType::Int,
        vec![ctrl, call, call],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, call], None);
    g.entry = start;
    g.exit = ret;
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 0);
    assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));

    // Width mismatch: an `Integer` box read by a `longValue` unbox.
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let call = g.add(
        Op::Call { info_ptr: info },
        IrType::Ref,
        vec![ctrl, mem, v],
        None,
    );
    let wide = g.add(
        Op::Unbox {
            op: ir::UnboxOp::LongValue,
            class_id: 7,
        },
        IrType::Long,
        vec![ctrl, call, call],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, wide], None);
    g.entry = start;
    g.exit = ret;
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 0);
    assert!(matches!(g.nodes[call as usize].op, Op::Call { .. }));
}

/// The `Long` twin folds; and a nested
/// `Integer.valueOf(Integer.valueOf(v).intValue()).intValue()` folds both
/// levels down to `v` (the outer plan's value is the inner plan's retired
/// unbox).
#[test]
fn long_value_of_and_nested_boxes_fold() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let j = g.add(Op::Param(0), IrType::Long, vec![], None);
    let linfo = box_info("java/lang/Long", "(J)Ljava/lang/Long;");
    let lcall = g.add(
        Op::Call { info_ptr: linfo },
        IrType::Ref,
        vec![ctrl, mem, j],
        None,
    );
    let lun = g.add(
        Op::Unbox {
            op: ir::UnboxOp::LongValue,
            class_id: 9,
        },
        IrType::Long,
        vec![ctrl, lcall, lcall],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, lun], None);
    g.entry = start;
    g.exit = ret;
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 1);
    assert_eq!(g.nodes[ret as usize].inputs[1], j);

    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let inner = g.add(
        Op::Call { info_ptr: info },
        IrType::Ref,
        vec![ctrl, mem, v],
        None,
    );
    let inner_un = g.add(
        Op::Unbox {
            op: ir::UnboxOp::IntValue,
            class_id: 7,
        },
        IrType::Int,
        vec![ctrl, inner, inner],
        None,
    );
    let outer = g.add(
        Op::Call { info_ptr: info },
        IrType::Ref,
        vec![ctrl, inner_un, inner_un],
        None,
    );
    let outer_un = g.add(
        Op::Unbox {
            op: ir::UnboxOp::IntValue,
            class_id: 7,
        },
        IrType::Int,
        vec![ctrl, outer, outer],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, outer_un], None);
    g.entry = start;
    g.exit = ret;
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 2);
    assert_eq!(g.nodes[ret as usize].inputs[1], v, "both levels fold to v");
    for dead in [inner, inner_un, outer, outer_un] {
        assert_eq!(g.nodes[dead as usize].op, Op::Dead);
    }
}

/// A box nothing reads back but a call-site exception flag (`Op::CallThrew`,
/// an ordering edge) still folds: the call is retired and its flag becomes
/// the constant "never threw" its `If` reads.
#[test]
fn a_box_read_only_by_its_exception_flag_folds_and_the_flag_is_never() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let call = g.add(Op::Call { info_ptr: info }, IrType::Ref, vec![ctrl, mem, v], Some(3));
    let threw = g.add(Op::CallThrew { site: 0, index: 0 }, IrType::Int, vec![ctrl, call], Some(3));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, threw], Some(3));
    let ret = g.add(Op::Return, IrType::Void, vec![iff, v], Some(6));
    g.entry = start;
    g.exit = ret;
    assert_eq!(ir_fold_unbox_of_fresh_box(&mut g), 1);
    assert_eq!(g.nodes[call as usize].op, Op::Dead);
    assert_eq!(g.nodes[threw as usize].op, Op::Dead);
    assert_eq!(g.nodes[g.nodes[iff as usize].inputs[1] as usize].op, Op::Const(0));
}

/// A box read by its own `!= null` test (a spliced `requireNonNull`) and an
/// unbox: the null compare folds first, then the box folds whole.
#[test]
fn a_box_tested_for_null_and_unboxed_folds_whole() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let call = g.add(Op::Call { info_ptr: info }, IrType::Ref, vec![ctrl, mem, v], Some(3));
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let ne = g.add(Op::Cmp(ir::CmpOp::Ne), IrType::Int, vec![call, null], Some(4));
    let guard = g.add(Op::Guard { bci: 4 }, IrType::Void, vec![ctrl, ne], Some(4));
    let unbox = g.add(
        Op::Unbox { op: ir::UnboxOp::IntValue, class_id: 7 },
        IrType::Int,
        vec![ctrl, call, call],
        Some(5),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, unbox], Some(6));
    g.entry = start;
    g.exit = ret;
    let _ = guard;
    assert_eq!(ir_fold_null_checks_on_fresh_allocations(&mut g), 1);
    assert_eq!(g.nodes[call as usize].op, Op::Dead, "the box is gone");
    assert_eq!(g.nodes[ret as usize].inputs[1], v, "the return reads x");
}

/// A lone `Integer.valueOf(v)` that only a snapshot (and its call-site
/// exception flag) still names folds as a one-member box web: the call is
/// retired, the flag answers "never threw", and the snapshot slot names an
/// auto-box recipe of `v` (rebuilt with the JDK's own `valueOf` on deopt).
#[test]
fn a_lone_box_named_only_by_a_snapshot_folds_to_an_auto_box_recipe() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let call = g.add(Op::Call { info_ptr: info }, IrType::Ref, vec![ctrl, mem, v], Some(3));
    let threw = g.add(Op::CallThrew { site: 0, index: 0 }, IrType::Int, vec![ctrl, call], Some(3));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, threw], Some(3));
    let ret = g.add(Op::Return, IrType::Void, vec![iff, v], Some(6));
    g.entry = start;
    g.exit = ret;
    g.push_safepoint(snapshot(5, vec![v], vec![call]));
    let mut sr_map = None;
    let mut pinned = std::collections::HashSet::new();
    assert_eq!(ir_fold_box_phi_webs(&mut g, &mut sr_map, &mut pinned), 1);
    assert_eq!(g.nodes[call as usize].op, Op::Dead, "the box call is gone");
    assert_eq!(g.nodes[threw as usize].op, Op::Dead);
    let key = g.safepoints[0].stack[0];
    let recipe = &sr_map.expect("a recipe").objects[&key];
    assert_eq!(recipe.array_element_type, Some(crate::deopt::AUTO_BOX_INTEGER_ATYPE));
    assert_eq!(recipe.field_values, vec![Some(v)]);
}

/// A box only an unbox reads, held by a removed varargs array's recipe (no
/// snapshot names it directly): the fold retires it and the array's recipe
/// names the box's auto-box recipe instead.
#[test]
fn a_box_held_by_a_removed_arrays_recipe_is_re_pointed_to_its_auto_box() {
    let mut g = empty_graph();
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let v = g.add(Op::Param(0), IrType::Int, vec![], None);
    let info = box_info("java/lang/Integer", "(I)Ljava/lang/Integer;");
    let call = g.add(Op::Call { info_ptr: info }, IrType::Ref, vec![ctrl, mem, v], Some(3));
    let unbox = g.add(
        Op::Unbox { op: ir::UnboxOp::IntValue, class_id: 7 },
        IrType::Int,
        vec![ctrl, call, call],
        Some(5),
    );
    let array_key = g.add(Op::Dead, IrType::Void, vec![], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, unbox], Some(6));
    g.entry = start;
    g.exit = ret;
    g.push_safepoint(snapshot(4, vec![v], vec![array_key]));
    let mut sr = ir_lower::ScalarReplacementMap::default();
    sr.objects.insert(
        array_key,
        ir_lower::VirtualObjectInfo {
            class_id: 1,
            array_element_type: Some(0),
            num_fields: 1,
            field_values: vec![Some(call)],
            new_ctrl: ctrl,
            store_ctrls: Vec::new(),
            store_nodes: Vec::new(),
            store_fields: Vec::new(),
            loop_phis: None,
            non_null_cond: None,
        },
    );
    let mut sr_map = Some(sr);
    let mut pinned = std::collections::HashSet::new();
    assert_eq!(ir_fold_box_phi_webs(&mut g, &mut sr_map, &mut pinned), 1);
    assert_eq!(g.nodes[call as usize].op, Op::Dead);
    assert_eq!(g.nodes[ret as usize].inputs[1], v, "the unbox reads v");
    let sr = sr_map.unwrap();
    let key = sr.objects[&array_key].field_values[0].expect("a recipe");
    assert_ne!(key, call);
    assert_eq!(sr.objects[&key].array_element_type, Some(crate::deopt::AUTO_BOX_INTEGER_ATYPE));
}
