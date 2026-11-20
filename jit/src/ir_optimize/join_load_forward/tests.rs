    use super::*;
    use crate::ir::CmpOp;

    /// `x = p.f; if (c) {} else {}; y = p.f` with the join's memory φ, and,
    /// with `store_between`, `p.g = 1` (another field) before the join or,
    /// with `store_same`, `p.f = v` on the true arm.
    struct Diamond {
        g: Graph,
        first: NodeId,
        second: NodeId,
        stored: NodeId,
    }

    fn diamond(store_other: bool, store_same: bool, volatile_second: bool) -> Diamond {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let c = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let v = g.add(Op::Param(2), IrType::Int, vec![start], None);
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let first = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![entry, mem0, p, f0],
            Some(1),
        );
        g.field_decl_class.insert(first, 40);
        let mut mem = first;
        if store_other {
            let s = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![entry, mem, p, f1, one],
                Some(2),
            );
            g.field_decl_class.insert(s, 40);
            mem = s;
        }
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![c, zero], None);
        let iff = g.add(Op::If, IrType::Control, vec![entry, cmp], Some(3));
        let t = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let mut tmem = mem;
        if store_same {
            let s = g.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![t, mem, p, f0, v],
                Some(4),
            );
            g.field_decl_class.insert(s, 40);
            tmem = s;
        }
        let join = g.add(Op::Merge, IrType::Control, vec![t, f], Some(6));
        let jmem = g.add(Op::Phi, IrType::Memory, vec![join, tmem, mem], None);
        let second = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![join, jmem, p, f0],
            Some(7),
        );
        g.field_decl_class.insert(second, 40);
        if volatile_second {
            g.nodes[second as usize].volatile_access = true;
        }
        let sum = g.add(Op::Add, IrType::Int, vec![first, second], None);
        g.exit = g.add(Op::Return, IrType::Int, vec![join, sum], Some(8));
        Diamond {
            g,
            first,
            second,
            stored: v,
        }
    }

    fn run(g: &mut Graph) -> JoinForwardCensus {
        cratonvm_types::flags::with_thread_overrides(
            &[],
            || forward_join_loads(g),
        )
    }

    #[test]
    fn nio21w16_a_read_after_a_join_takes_the_read_before_it() {
        let mut d = diamond(false, false, false);
        let c = run(&mut d.g);
        assert_eq!(c.removed, 1);
        assert_eq!(c.phis, 0, "both paths answer the same read");
        assert_eq!(d.g.nodes[d.second as usize].op, Op::Dead);
        assert!(d
            .g
            .nodes
            .iter()
            .any(|n| n.op == Op::Add && n.inputs.as_slice() == [d.first, d.first]));
    }

    #[test]
    fn nio21w16_a_store_to_another_field_is_stepped_over() {
        let mut d = diamond(true, false, false);
        assert_eq!(run(&mut d.g).removed, 1);
        assert_eq!(d.g.nodes[d.second as usize].op, Op::Dead);
    }

    #[test]
    fn nio21w16_a_store_on_one_arm_makes_a_value_phi() {
        let mut d = diamond(false, true, false);
        let c = run(&mut d.g);
        assert_eq!((c.removed, c.phis, c.from_stores), (1, 1, 1));
        let phi =
            d.g.nodes
                .iter()
                .find(|n| n.op == Op::Phi && n.ty == IrType::Int)
                .expect("a value phi at the join");
        assert_eq!(&phi.inputs.as_slice()[1..], &[d.stored, d.first]);
    }

#[test]
    fn nio21w16_a_narrow_store_is_not_forwarded() {
        let mut d = diamond(false, true, false);
        for n in d.g.nodes.iter_mut() {
            if let Op::Load(k) | Op::Store(k) = &mut n.op {
                *k = MemKind::Byte;
            }
        }
        assert_eq!(run(&mut d.g).removed, 0);
    }

    /// A loop header's memory φ answers nothing: no value crosses a back edge.
    #[test]
    fn nio21w16_a_loop_header_stops_the_walk() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let n = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let pre = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![entry, mem0, p, f0],
            Some(1),
        );
        g.field_decl_class.insert(pre, 40);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(5));
        let i = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let mem = g.add(Op::Phi, IrType::Memory, vec![region, pre], None);
        let ge = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![i, n], None);
        let head = g.add(Op::If, IrType::Control, vec![region, ge], Some(8));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![head], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![head], None);
        let ld = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![stay, mem, p, f0],
            Some(10),
        );
        g.field_decl_class.insert(ld, 40);
        let i1 = g.add(Op::Add, IrType::Int, vec![i, ld], None);
        let i2 = g.add(Op::Add, IrType::Int, vec![i1, one], None);
        g.nodes[region as usize].inputs.push(stay);
        g.nodes[i as usize].inputs.push(i2);
        g.nodes[mem as usize].inputs.push(ld);
        g.exit = g.add(Op::Return, IrType::Int, vec![exit, i], Some(20));
        assert_eq!(run(&mut g).removed, 0);
        assert_ne!(g.nodes[ld as usize].op, Op::Dead);
    }
