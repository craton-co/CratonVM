use super::{door_rerun_verdict, DoorRerunCause};
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ReconstructedFrame,
    ResumeSemantics,
};

/// `putstatic` at bci 1, then a loop storing at bci 13 (the fixture of
/// `cratonvm_jit::deopt`'s `r12w4_replay2_tests`).
const STORE_THEN_LOOP: [u8; 23] = [
    0x04, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0x05, 0xb3, 0x00,
    0x01, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
];

fn body_with_a_point() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.deopt_points.push(DeoptimizationPoint {
        native_offset: 0,
        bci: 4,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        semantics: ResumeSemantics::REEXECUTE,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "C.m:()V".to_string(),
            bci: 4,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    });
    cm
}

fn frame(key: &str, bci: u32) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        semantics: ResumeSemantics::REEXECUTE,
        ..Default::default()
    }
}

#[test]
fn the_verdict_reads_the_bci_only_of_this_bodys_own_frame() {
    let code = STORE_THEN_LOOP;
    let ran = body_with_a_point();
    let own = ran.deopt_points.as_ptr() as usize;
    let verdict = |stash: Option<(&ReconstructedFrame, usize)>| {
        door_rerun_verdict(&code, &ran, "C", "m", "()V", stash)
    };
    // Before the first store: the re-run is the same execution.
    assert_eq!(
        verdict(Some((&frame("C.m:()V", 0), own))),
        (DoorRerunCause::OwnStash, true)
    );
    // After it: the re-run commits the putstatic again.
    assert_eq!(
        verdict(Some((&frame("C.m:()V", 4), own))),
        (DoorRerunCause::OwnStash, false)
    );
    // Another method's frame, or a point this body does not own: its bci
    // says nothing about this attempt, so the whole body answers.
    assert_eq!(
        verdict(Some((&frame("D.n:()V", 0), own))),
        (DoorRerunCause::ForeignStash, false)
    );
    assert_eq!(
        verdict(Some((&frame("C.m:()V", 0), 1))),
        (DoorRerunCause::ForeignStash, false)
    );
    assert_eq!(verdict(None), (DoorRerunCause::Frameless, false));
    // Interpreter round i1 wave 28, lane L2: an inlined chain whose
    // OUTERMOST scope is this method is this body's own, and its bci (the
    // spliced callee's 0, which would read as "before the first store" in
    // `code`) is not read: the whole body answers.
    let mut chain = frame("D.n:()V", 0);
    chain.caller_frames = vec![frame("C.m:()V", 4)];
    assert_eq!(
        verdict(Some((&chain, own))),
        (DoorRerunCause::OwnStash, false)
    );
    let mut other_chain = frame("C.m:()V", 0);
    other_chain.caller_frames = vec![frame("D.n:()V", 4)];
    assert_eq!(
        verdict(Some((&other_chain, own))),
        (DoorRerunCause::ForeignStash, false),
        "a chain of another method stays foreign"
    );
    assert!(
        !super::replay_from_entry_is_observably_equivalent_for_stash(&code, &ran, &chain, own)
    );
    // A body that commits nothing re-runs exactly whatever stopped it.
    let pure = [0x1a, 0x1b, 0x60, 0xac]; // iload_0 iload_1 iadd ireturn
    assert_eq!(
        door_rerun_verdict(&pure, &ran, "C", "m", "()V", None),
        (DoorRerunCause::Frameless, true)
    );
}
