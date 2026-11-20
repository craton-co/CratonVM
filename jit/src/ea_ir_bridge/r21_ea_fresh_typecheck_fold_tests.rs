    use super::*;
    use crate::ir::{Graph, IrType, Op};
    use cratonvm_types::flags::with_thread_overrides;

/// `o = new C(); b = o instanceof T; T t = (T) o; return t;` with `C` the
    /// allocated class id and `T` resolved to `target`.
    fn fixture(
        class: u32,
        target: u32,
        name: &str,
        final_target: bool,
    ) -> (Graph, [ir::NodeId; 4]) {
        let (ptr, len) =
            crate::intern_typecheck_target_with_finality(name, Some(target), final_target);
        let mut g = Graph::empty(16);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let obj = g.add(
            Op::New {
                class_id: class,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(0),
        );
        let io = g.add(
            Op::InstanceOf {
                name_ptr: ptr as usize,
                name_len: len,
                seen_cell: 0,
                header_only: false,
            },
            IrType::Int,
            vec![ctrl, mem, obj],
            Some(4),
        );
        let cc = g.add(
            Op::CheckCast {
                name_ptr: ptr as usize,
                name_len: len,
            },
            IrType::Ref,
            vec![ctrl, io, obj],
            Some(8),
        );
        // Pure readers (no memory slot), so the assertions read value edges.
        let k = g.add(
            Op::Cmp(ir::CmpOp::Eq),
            IrType::Int,
            vec![cc, obj],
            Some(9),
        );
        let b = g.add(Op::Add, IrType::Int, vec![io, io], Some(10));
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(11));
        g.entry = start;
        g.exit = ret;
        let _ = k;
        (g, [obj, io, cc, b])    }