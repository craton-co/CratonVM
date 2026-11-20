use super::{
    door_rerun_verdict, replay_from_entry_is_observably_equivalent,
    replay_from_entry_is_observably_equivalent_after,
    replay_from_entry_is_observably_equivalent_for_stash, DoorRerunCause,
};
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ReconstructedFrame,
    ResumeSemantics,
};
use cratonvm_jit::JitLocalHandlerSite;

/// A handler (pc 3) placed BEFORE the range it protects (the call at 15),
/// falling back into the guard at 10 with a forward `goto`: the exc4
/// shape, not javac output.
///
/// ```text
///  0: goto 10          3: astore_1        4: goto 10       7-9: nop
/// 10: iconst_1        11: nop            12: putstatic #1
/// 15: invokestatic #2 18: return
/// ```
const HANDLER_BEFORE_RANGE: [u8; 19] = [
    0xa7, 0x00, 0x0a, 0x4c, 0xa7, 0x00, 0x06, 0x00, 0x00, 0x00, 0x04, 0x00, 0xb3, 0x00, 0x01,
    0xb8, 0x00, 0x02, 0xb1,
];

/// A body with a point at the guard (bci 10), and, when `catches`, the
/// local-handler site the single-pass emitter would give the call at 15.
fn body(catches: bool) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.deopt_points.push(DeoptimizationPoint {
        native_offset: 0,
        bci: 10,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        semantics: ResumeSemantics::REEXECUTE,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: "C.m:()V".to_string(),
            bci: 10,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    });
    if catches {
        cm._jit_local_handler_sites
            .push(Box::new(JitLocalHandlerSite {
                candidates: vec![("", 3)],
                declaring_class_id: 0,
                throw_bci: 15,
                cache: std::sync::atomic::AtomicU64::new(JitLocalHandlerSite::CACHE_EMPTY),
                implicit_cache: JitLocalHandlerSite::empty_implicit_cache(),
                no_collect: false,
            }));
    }
    cm
}

fn frame_at_the_guard() -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "C.m:()V".to_string(),
        bci: 10,
        semantics: ResumeSemantics::REEXECUTE,
        ..Default::default()
    }
}

/// Pass 1 stores at 12, the call throws at 15, the local handler comes
/// back to the guard, and the guard traps: a re-run would store again.
#[test]
fn a_body_whose_handler_comes_back_to_the_trap_refuses_the_rerun() {
    let code = HANDLER_BEFORE_RANGE;
    let ran = body(true);
    let own = ran.deopt_points.as_ptr() as usize;
    let rframe = frame_at_the_guard();
    // The table-less form (no body in hand) sees a pure prefix.
    assert!(replay_from_entry_is_observably_equivalent(&code, true, 10));
    assert!(!replay_from_entry_is_observably_equivalent_after(
        &code,
        Some(&ran),
        10
    ));
    assert!(!replay_from_entry_is_observably_equivalent_for_stash(
        &code, &ran, &rframe, own
    ));
    // A plain restash (no point address) is judged with the same edges.
    assert!(!replay_from_entry_is_observably_equivalent_for_stash(
        &code, &ran, &rframe, 0
    ));
    assert_eq!(
        door_rerun_verdict(&code, &ran, "C", "m", "()V", Some((&rframe, own))),
        (DoorRerunCause::OwnStash, false)
    );
}

/// A body without local handlers (every optimizing-tier body) never took
/// the edge: the answer is the one before this wave.
#[test]
fn a_body_without_local_handlers_keeps_the_branch_only_extent() {
    let code = HANDLER_BEFORE_RANGE;
    let ran = body(false);
    let own = ran.deopt_points.as_ptr() as usize;
    let rframe = frame_at_the_guard();
    assert!(replay_from_entry_is_observably_equivalent_after(
        &code,
        Some(&ran),
        10
    ));
    assert!(replay_from_entry_is_observably_equivalent_for_stash(
        &code, &ran, &rframe, own
    ));
    assert_eq!(
        door_rerun_verdict(&code, &ran, "C", "m", "()V", Some((&rframe, own))),
        (DoorRerunCause::OwnStash, true)
    );
    // No body at all: the spliced half is unknown, so only a pure body
    // re-runs exactly.
    assert!(!replay_from_entry_is_observably_equivalent_after(
        &code, None, 10
    ));
}
