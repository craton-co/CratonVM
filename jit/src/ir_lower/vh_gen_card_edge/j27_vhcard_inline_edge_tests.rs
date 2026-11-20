    use super::super::{
        plan_slots, vh_const_core_bytes_gen_edge, Graph, IrType, Op, Schedule, SlotPlan,
        VhRefGates, VhRefValueCheck,
    };
    use crate::ir_schedule;
    use super::*;
    use crate::direct_helpers::UnsafeRmwOp;

    fn gated(op: VhConstOp) -> VhConstCore {
        VhConstCore {
            op,
            kind: b'L',
            disp: 24,
            class_id: 5,
            receiver_display: None,
            cid_off: 0,
            expected_check: VhRefValueCheck::Admitted,
            value_check: VhRefValueCheck::Admitted,
            gates: Some(VhRefGates {
                pre: 0x1000,
                post: 0x2000,
                floor: 0,
                mask: Some(0x40),
            }),
            g1: None,
            result_check: VhRefValueCheck::Admitted,
        }
    }

    const OPS: [VhConstOp; 5] = [
        VhConstOp::Set { volatile: false },
        VhConstOp::Set { volatile: true },
        VhConstOp::Rmw(UnsafeRmwOp::Cas),
        VhConstOp::Rmw(UnsafeRmwOp::Cae),
        VhConstOp::Rmw(UnsafeRmwOp::GetAndSet),
    ];

/// The old -> old edge jumps to `at + core.len() - access.len()`: the
    /// screen's own access must be the core bytes' tail, for every write mode.
    #[test]
    fn the_screens_access_is_the_tail_of_the_core_bytes() {
        for op in OPS {
            let core = gated(op);
            let (bytes, _, edge) = vh_const_core_bytes_gen_edge(&core);
            let (access, _) = vh_const_access_bytes(&core);
            assert!(edge.is_some(), "{op:?}: a card edge");
            assert!(bytes.ends_with(&access), "{op:?}: the access ends the core");
        }
    }

    // The edge's bytes: the inline form has the PRE and the POST card-byte
    // compare and keeps both card-only calls (on their slow edges); `=0`, or
    // no inline card view, is the two-call edge with no card compare.
