    use super::*;
    use crate::ir::IrType;



    #[test]
    fn r25_w13_4_the_walk_description_names_the_chain() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
        let iff = g.add(Op::If, IrType::Control, vec![start, cond], None);
        let proj = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let s = describe_bound_walk(&g, start, proj, cond);
        assert!(s.starts_with(&format!(
            "idx n{cond} Some(Const(1)); walk -> n{proj} Proj(0)"
        )));
        assert!(s.contains(&format!("n{iff} If [cond n{cond}")));
        assert!(s.ends_with("(header)"));
    }
