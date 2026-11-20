    use super::predicate_trap_charge;
    use cratonvm_jit::deopt::{
        predicate_speculation_id, speculation_id, DeoptCause, DeoptReason, PredicateKind,
        ReconstructedFrame,
    };

fn frame(bci: u32) -> ReconstructedFrame {
        ReconstructedFrame {
            class_id: 0,
            method_key: "p/C.m:()V".into(),
            bci,
            ..Default::default()        }    }