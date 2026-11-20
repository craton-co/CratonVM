use super::osr_predicate_exit_charge;
use cratonvm_jit::deopt::{
    predicate_speculation_id, speculation_id, DeoptAction, DeoptCause, DeoptReason,
    DeoptimizationPoint, FrameState, PredicateKind, ResumeSemantics,
};

fn point(bci: u32, reason: DeoptReason) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: DeoptAction::Reinterpret,
        semantics: ResumeSemantics::REEXECUTE,
        speculation_id: speculation_id(bci, reason),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    }
}

fn cause(reason: DeoptReason, speculation_id: u32) -> Option<DeoptCause> {
    Some(DeoptCause {
        reason,
        speculation_id,
    })
}

/// A planter-named exit is charged at any header; an unnamed
/// `UncommonTrap` only at an opaque entry header; nothing else, and
/// never without one of the artifact's own `UncommonTrap` points at
/// the bci.
#[test]
fn r23_w3_only_loop_entry_predicate_exits_are_charged() {
    let ut = DeoptReason::UncommonTrap;
    let pts = [point(10, ut), point(30, DeoptReason::BoundsCheck)];
    let named = predicate_speculation_id(10, PredicateKind::NullBase);
    let plain = speculation_id(10, ut);
    let opaque: &[u32] = &[10, 40];
    let ask = |bci, chain, c, opaque: &[u32]| {
        osr_predicate_exit_charge(pts.iter(), opaque, bci, chain, c)
    };
    // Named by its planter: charged, opaque header or not.
    assert!(ask(10, false, cause(ut, named), &[]));
    assert!(ask(10, false, cause(ut, named), opaque));
    // Unnamed: only at an opaque entry header.
    assert!(ask(10, false, cause(ut, plain), opaque));
    assert!(!ask(10, false, cause(ut, plain), &[]));
    // A chain, an unknown bci, no cause, another reason: never.
    assert!(!ask(10, true, cause(ut, named), opaque));
    assert!(!ask(u32::MAX, false, cause(ut, named), opaque));
    assert!(!ask(10, false, None, opaque));
    assert!(!ask(10, false, cause(DeoptReason::NullCheck, named), opaque));
    // No `UncommonTrap` point of this artifact at the bci: never (40 is
    // an opaque header with no point; 30 holds a `BoundsCheck` only).
    assert!(!ask(40, false, cause(ut, speculation_id(40, ut)), opaque));
    assert!(!ask(30, false, cause(ut, predicate_speculation_id(30, PredicateKind::Range)), &[30]));
}
