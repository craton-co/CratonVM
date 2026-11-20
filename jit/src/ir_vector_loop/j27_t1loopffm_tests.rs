use super::*;
use crate::ir::UnboxOp as U;

/// `for (k = 0; k < n; k++) h += s.getAtIndex(LE_INT, k)` as the FFM
/// expansion leaves it once its guards are out of the loop:
/// `h + ScopedGet(seg, scope, base, start + i2l(k) * 4, le)`, plus (when
/// `plain_read`) `+ a[k]` of another array, or (when `wide`) a `long`
/// accumulator over `(long) get`. Returns the graph and `[get, start,
/// base, acc, seg]`.
fn scoped_get_sum_graph(
    le: i64,
    proven: bool,
    plain_read: bool,
    wide: bool,
) -> (Graph, [NodeId; 5]) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let seg = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let arr = g.add(Op::Param(2), IrType::Ref, vec![start], None);
    let field = |g: &mut Graph, op: U, ty: IrType| {
        g.add(
            Op::Unbox { op, class_id: 804 },
            ty,
            vec![ctrl, mem, seg],
            Some(2),
        )
    };
    let _length = field(&mut g, U::FieldLong(0), IrType::Long);
    let start_off = field(&mut g, U::FieldLong(3), IrType::Long);
    let base = field(&mut g, U::FieldRef(2), IrType::Ref);
    let scope = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let merge = g.add(Op::Merge, IrType::Control, vec![ctrl, ctrl], Some(4));
    let mphi = g.add(Op::Phi, IrType::Memory, vec![merge, mem, mem], Some(4));
    let iv = g.add(Op::Phi, IrType::Int, vec![merge, zero, zero], Some(4));
    let (acc_ty, acc0) = if wide {
        (IrType::Long, g.add(Op::Const(0), IrType::Long, vec![], None))
    } else {
        (IrType::Int, zero)
    };
    let acc = g.add(Op::Phi, acc_ty, vec![merge, acc0, acc0], Some(4));
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, n], Some(6));
    let iff = g.add(Op::If, IrType::Control, vec![merge, cmp], Some(6));
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(6));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(6));
    let i2l = g.add(Op::I2L, IrType::Long, vec![iv], Some(12));
    let four = g.add(Op::Const(4), IrType::Long, vec![], None);
    let mul = g.add(Op::Mul, IrType::Long, vec![i2l, four], Some(16));
    let addr = g.add(Op::Add, IrType::Long, vec![start_off, mul], Some(16));
    let order = g.add(Op::Const(le), IrType::Int, vec![], None);
    let get = g.add(
        Op::Unbox {
            op: U::ScopedGet {
                width: 4,
                signed: true,
                ordered: true,
            },
            class_id: 0,
        },
        IrType::Int,
        vec![body, mphi, seg, scope, base, addr, order],
        Some(16),
    );
    let addend = if plain_read {
        let elem = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![body, get, arr, iv],
            Some(17),
        );
        g.add(Op::Add, IrType::Int, vec![get, elem], Some(17))
    } else if wide {
        g.add(Op::I2L, IrType::Long, vec![get], Some(17))
    } else {
        get
    };
    let sum = g.add(Op::Add, acc_ty, vec![acc, addend], Some(18));
    let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(21));
    g.nodes[merge as usize].inputs[1] = body;
    g.nodes[mphi as usize].inputs[2] = get;
    g.nodes[iv as usize].inputs[2] = next;
    g.nodes[acc as usize].inputs[2] = sum;
    g.exit = g.add(Op::Return, IrType::Void, vec![exit], Some(27));
    if proven {
        g.ffm_proven_heap_accesses.insert(get);
    }
    g.ffm_scoped_length_fields
        .insert(get, (804, U::FieldLong(0)));
    (g, [get, start_off, base, acc, seg])
}

fn plan(g: &Graph, scoped_get: bool) -> Result<VecLoopPlan, &'static str> {
    let schedule = crate::ir_schedule::schedule(g);
    let opts = VecOpts {
        bitops: true,
        reduce: true,
        widen: true,
        scoped_store: true,
        scoped_get,
        ..VecOpts::default()
    };
    let (plans, refusals) = plan_vector_loops_with(
        g,
        &schedule,
        &|c| crate::ir_lower::ctrl_block_of(g, &schedule, c),
        opts,
    );
    match (plans.into_iter().next(), refusals.first()) {
        (Some(p), _) => Ok(p),
        (None, Some(&(_, why))) => Err(why),
        (None, None) => Err("no loop"),
    }
}

/// VEC27-1: the heap segment sum loop is planned under the switch as a
/// reduction over ONE base (the array) with the segment's start, length
/// and receiver on file, exactly as B1's put loop -- and refused without
/// the switch, big-endian, unproven, or with a plain element read beside
/// the get (which the rebased base register would misaddress).
#[test]
fn j27_vec27_1_a_proven_heap_scoped_get_sum_is_planned() {
    let (g, [_get, start, base, acc, seg]) = scoped_get_sum_graph(0, true, false, false);
    let p = plan(&g, true).expect("planned");
    assert_eq!(p.kind, VecKind::Int);
    assert_eq!(p.bases, vec![base]);
    assert_eq!(p.value, VExpr::Load(0));
    assert_eq!(p.reduce, Some(VecReduce { acc, op: RedOp::Add }));
    assert_eq!(
        p.scoped,
        Some(ScopedStore {
            start,
            recv: seg,
            length_field: (804, U::FieldLong(0)),
            native: false,
            session: NO_NODE,
        })
    );
    assert!(plan(&g, false).is_err(), "switch off");
    let (be, _) = scoped_get_sum_graph(1, true, false, false);
    assert_eq!(plan(&be, true).err(), Some("a big-endian scoped store"));
    let (unproven, _) = scoped_get_sum_graph(0, false, false, false);
    assert_eq!(
        plan(&unproven, true).err(),
        Some("a scoped store not proven in bounds")
    );
    let (plain, _) = scoped_get_sum_graph(0, true, true, false);
    assert_eq!(
        plan(&plain, true).err(),
        Some("a scoped get reduction that reads another array")
    );
}

/// VEC27-1 residual (`R26SegVecPut native-get-sum`): `h += s.getAtIndex(INT, k)`
/// into a `long` accumulator is `h + i2l(get)`; the planner took the `i2l`
/// for a conversion of a non-element and refused the loop. It is the widening
/// load of VEC21-2 over the get: `long` lanes over `int` elements of the one
/// base.
#[test]
fn j27_vec27_1_a_widened_scoped_get_sum_is_planned_in_long_lanes() {
    let (g, [_get, _start, base, acc, _seg]) = scoped_get_sum_graph(0, true, false, true);
    let p = plan(&g, true).expect("planned");
    assert_eq!(p.kind, VecKind::Long, "long lanes");
    assert_eq!(p.bases, vec![base]);
    assert_eq!(p.base_kinds, vec![MemKind::Int], "over int elements");
    assert_eq!(p.value, VExpr::Load(0));
    assert_eq!(p.reduce, Some(VecReduce { acc, op: RedOp::Add }));
    assert!(p.scoped.is_some(), "the segment's start and length");
    assert!(plan(&g, false).is_err(), "switch off");
}

/// VEC27-1: the NATIVE segment sum (literal-null base, the session the
/// loop-entry pre-check) is planned with the pre-check named, and refused
/// when the session is a plain read.
#[test]
fn j27_vec27_1_a_native_scoped_get_sum_is_planned_with_its_precheck() {
    let (mut g, [get, start, _, _, seg]) = scoped_get_sum_graph(0, false, false, false);
    let (ctrl, mem) = (1, 2);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let scope_read = g.add(
        Op::Unbox {
            op: U::FieldRef(1),
            class_id: 804,
        },
        IrType::Ref,
        vec![ctrl, mem, seg],
        Some(2),
    );
    let precheck = g.add(
        Op::Unbox {
            op: U::SessionPrecheck,
            class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, scope_read],
        Some(2),
    );
    g.nodes[get as usize].inputs[4] = null;
    g.nodes[get as usize].inputs[3] = precheck;
    let p = plan(&g, true).expect("planned");
    assert_eq!(p.bases, vec![null]);
    assert_eq!(
        p.scoped,
        Some(ScopedStore {
            start,
            recv: seg,
            length_field: (804, U::FieldLong(0)),
            native: true,
            session: precheck,
        })
    );
    g.nodes[get as usize].inputs[3] = scope_read;
    assert_eq!(
        plan(&g, true).err(),
        Some("a native scoped store whose session is not pre-checked")
    );
}

fn reduce_iota_plan(op: RedOp) -> VecLoopPlan {
    VecLoopPlan {
        header: 0,
        entry_pred: 0,
        header_bci: None,
        loop_blocks: Vec::new(),
        iv: 0,
        bound: 0,
        bound_length_of: None,
        kind: VecKind::Int,
        bases: vec![10],
        base_kinds: vec![MemKind::Int],
        invariants: Vec::new(),
        shift_counts: Vec::new(),
        store_base: 0,
        value: VExpr::Bin(VOp::Add, Box::new(VExpr::Load(0)), Box::new(VExpr::Iota)),
        reduce: Some(VecReduce { acc: 30, op }),
        scan: None,
        widen: None,
        iota: true,
        scoped: None,
    }
}

/// A reduction that reads the induction (`s += a[k] + k`, which the
/// planner admits since round 24 B1) advances its `j + i` lanes by `W`
/// after every step, as a store loop does. Before the j27 fix the
/// reduction step returned first, so every unrolled step and trip re-added
/// the FIRST step's `j + i` -- a wrong sum.
#[test]
fn j27_a_reduction_step_advances_its_iota_lanes() {
    let inv = [4u8, 5];
    let regs = StepRegs {
        temps: &[0, 1],
        inv_regs: &inv,
        acc: Some(3),
        nan: None,
    };
    let mut advance = Vec::new();
    emit_binop(&mut advance, VecKind::Int, VOp::Add, 4, 4, Src::Reg(5));
    for op in [RedOp::Add, RedOp::Sub, RedOp::Max] {
        let p = reduce_iota_plan(op);
        assert_eq!(iota_regs(&p, &inv), Some((4, 5)));
        let code = step_code(&p, &regs, 0x10).expect("two temporaries");
        assert!(code.ends_with(&advance), "{op:?}: no iota advance");
        let count = code
            .windows(advance.len())
            .filter(|w| *w == advance.as_slice())
            .count();
        assert_eq!(count, 1, "{op:?}: one advance per step");
    }
    // The sibling sweep (orchestrator, j27 wave 6): every non-scan step
    // kind -- a store, and each reduction op -- over `int` and `long`
    // lanes ends in exactly one advance of its own lane width.
    let temps = [0u8, 1, 6, 7];
    let wide = StepRegs {
        temps: &temps,
        inv_regs: &inv,
        acc: Some(3),
        nan: None,
    };
    for kind in [VecKind::Int, VecKind::Long] {
        let mut adv = Vec::new();
        emit_binop(&mut adv, kind, VOp::Add, 4, 4, Src::Reg(5));
        let ops = [
            None,
            Some(RedOp::Add),
            Some(RedOp::Sub),
            Some(RedOp::Min),
            Some(RedOp::Max),
        ];
        for op in ops {
            let mut p = reduce_iota_plan(op.unwrap_or(RedOp::Add));
            p.kind = kind;
            p.base_kinds = vec![if kind == VecKind::Long {
                MemKind::Long
            } else {
                MemKind::Int
            }];
            p.reduce = op.map(|op| VecReduce { acc: 30, op });
            let code = step_code(&p, &wide, 0x10).expect("enough temporaries");
            assert!(code.ends_with(&adv), "{kind:?} {op:?}: no iota advance");
            let n = code
                .windows(adv.len())
                .filter(|w| *w == adv.as_slice())
                .count();
            assert_eq!(n, 1, "{kind:?} {op:?}: one advance per step");
        }
    }
    // Without the induction, nothing is advanced.
    let mut p = reduce_iota_plan(RedOp::Add);
    p.iota = false;
    p.value = VExpr::Load(0);
    let code = step_code(&p, &regs, 0x10).expect("no temporary");
    assert!(!code.ends_with(&advance));
}
