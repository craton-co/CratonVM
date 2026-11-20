    use super::*;
    use crate::ir::IrType;

    const SWITCH: &str = "CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS";




    #[test]
    fn r25_w12_4_a_miss_arm_call_behind_a_guard_is_the_arms() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
        let iff = g.add(Op::If, IrType::Control, vec![start, cond], None);
        let hit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let miss = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let guard = g.add(
            Op::Guard { bci: 0 },
            IrType::Void,
            vec![miss, cond],
            Some(0),
        );
        let call = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![guard], Some(0));
        let direct = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![miss], Some(0));
        let on_hit = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![hit], Some(0));
        let plan_of = |t: NodeId| (t == iff).then_some(3usize);
        assert_eq!(miss_arm_plan(&g, call, plan_of), Some(3));
        assert_eq!(miss_arm_plan(&g, direct, plan_of), Some(3));
        // The hit arm is never the miss arm; an unnamed test is no plan.
        assert_eq!(miss_arm_plan(&g, on_hit, plan_of), None);
        assert_eq!(miss_arm_plan(&g, call, |_| None), None);
        // A merge on the way stops the walk.
        let merge = g.add(Op::Merge, IrType::Control, vec![miss, hit], None);
        let after = g.add(Op::Call { info_ptr: 0 }, IrType::Int, vec![merge], Some(0));
        assert_eq!(miss_arm_plan(&g, after, plan_of), None);
        assert!(describe_node(&g, call).starts_with(&format!("n{call} Call")));
    }
