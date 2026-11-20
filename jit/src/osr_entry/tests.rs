use super::*;

const HEADER: usize = 8;
const TAGS: [u8; 3] = [
    cratonvm_types::VTAG_OBJECT,
    cratonvm_types::VTAG_INT,
    cratonvm_types::VTAG_INT,
];

/// One OSR entry at [`HEADER`], three memory-homed locals, no deopt
/// metadata: the production artifact shape (`jit/src/tests.rs`'s
/// `osr_t_artifact`).
fn artifact() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET: metadata-only fixture, never executed.
    let mut cm = CompiledMethod::new(buf);
    cm.method_label = "craton/probe/OsrEntry.sum([I)I".to_string();
    let mut table = vec![-1i32; HEADER + 2];
    table[HEADER] = 0;
    cm.osr_pc_to_native = Some(table);
    cm.osr_num_locals = 3;
    cm.osr_local_assignments = Some(vec![None; 3]);
    cm.osr_xmm_assignments = Some(vec![None; 3]);
    cm
}

/// Step 2 of `r11w5-tier-frameless-trap-osr-refusal-patch`: an artifact
/// that put down an uncommon-trap stub with no reconstructed frame is
/// refused at admission as an unresumable exit, permanently and with the
/// install-epoch stamp; the same artifact without the bit is admitted.
#[test]
fn a_frameless_trap_stub_refuses_osr_entry_as_an_unresumable_exit() {
    let locals = [0x1234_5678i64, 200, 4950];
    let state = OsrEntryState::at(HEADER, &locals, &TAGS);

    let clean = artifact();
    assert!(
        clean.validate_osr_entry(&state).is_ok(),
        "positive control: the fixture without the bit is admitted"
    );

    let mut frameless = artifact();
    frameless.has_frameless_trap_stub = true;
    let err = frameless
        .validate_osr_entry(&state)
        .expect_err("a frameless trap stub must refuse the entry");
    match &err.reason {
        bailout::BailoutReason::UnsupportedShape(tag) => {
            assert_eq!(*tag, OSR_REFUSE_UNRESUMABLE_EXIT);
        }
        other => panic!("expected an OSR refusal, got {other:?}"),
    }
    assert!(osr_refusal_is_permanent(&err));
    assert!(osr_refusal_depends_on_compile_state(&err));
}

/// Interpreter round i2 wave 62, lane L2: a splice post-call exit's
/// `OsrExit` point names the spliced CALLEE's bci under a caller chain. A
/// coincidence with an entry pc must not make it the entry contract
/// (whose caller scope would refuse the entry as `OSR_REFUSE_INLINED_SCOPE`):
/// with only such a point at [`HEADER`] the contract falls back to the
/// register homes and the entry is admitted; beside the root's own map
/// the root's map is the contract, whichever comes first.
#[test]
fn a_chain_exit_point_at_an_entry_pc_is_not_the_entry_contract() {
    use crate::deopt::{
        DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, ResumeSemantics,
    };
    let point = |method_key: &str, caller: Option<Box<FrameState>>| DeoptimizationPoint {
        native_offset: 0,
        bci: HEADER as u32,
        reason: DeoptReason::OsrExit,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        semantics: ResumeSemantics::REEXECUTE,
        frame_state: FrameState {
            class_id: 0,
            method_key: method_key.to_string(),
            bci: HEADER as u32,
            locals: vec![FrameValue::StackSlot(8)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller,
        },
    };
    let root = || FrameState {
        class_id: 0,
        method_key: "craton/probe/OsrEntry.sum([I)I".to_string(),
        bci: 3,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let chain = point("craton/probe/Callee.step()I", Some(Box::new(root())));

    let mut only_chain = artifact();
    only_chain.deopt_points = vec![chain.clone()];
    assert!(only_chain.osr_entry_frame_state(HEADER).is_none());
    let locals = [0x1234_5678i64, 200, 4950];
    let state = OsrEntryState::at(HEADER, &locals, &TAGS);
    if let Err(err) = only_chain.validate_osr_entry(&state) {
        assert!(
            !format!("{err:?}").contains(OSR_REFUSE_INLINED_SCOPE),
            "the chain point must not refuse the entry as an inlined scope: {err:?}"
        );
    }

    let mut beside = artifact();
    beside.deopt_points = vec![chain, point("craton/probe/OsrEntry.sum([I)I", None)];
    let contract = beside
        .osr_entry_frame_state(HEADER)
        .expect("the root's own map");
    assert!(contract.caller.is_none());
}
