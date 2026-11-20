use super::*;

/// A register number past the 16-entry files the deopt stub spills cannot
/// be read at exit (`try_resolve_value` reports it and the value resumes as
/// `Unsupported`), so resumability refuses it up front (review #88).
#[test]
fn a_register_past_the_spilled_files_blocks_the_resume() {
    assert_eq!(
        SavedRegisters::default().gpr.len(),
        SPILLED_REGISTER_FILE_LEN
    );
    assert_eq!(
        SavedRegisters::default().xmm.len(),
        SPILLED_REGISTER_FILE_LEN
    );
    let fs = |v: FrameValue| FrameState {
        class_id: 0,
        method_key: String::from("T.m()V"),
        bci: 0,
        locals: vec![FrameValue::Int(1), v],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    for ok in [
        FrameValue::Register(15),
        FrameValue::RegisterLong(0),
        FrameValue::RegisterRef(3),
        FrameValue::XmmFloat(15),
        FrameValue::XmmDouble(7),
    ] {
        assert_eq!(first_unresumable_slot(&fs(ok.clone())), None, "{ok:?}");
    }
    for bad in [
        FrameValue::Register(16),
        FrameValue::RegisterLong(17),
        FrameValue::RegisterRef(255),
        FrameValue::XmmFloat(16),
        FrameValue::XmmDouble(32),
    ] {
        let msg = first_unresumable_slot(&fs(bad.clone()))
            .expect("an unspilled register blocks the resume");
        assert!(msg.contains("local 1"), "{bad:?}: {msg}");
    }
}

/// The unresumable-frame refusal has to name the slot: an `Unsupported`
/// STACK entry means the operand stack had no width source at that bci, an
/// `Unsupported` LOCAL means the local's kind or liveness was unknown, and
/// those are different defects. Reading "reconstructs an unresumable frame"
/// alone, the first cost a full instrumented rebuild to tell apart.
#[test]
fn first_unresumable_slot_names_the_offender() {
    let fs = FrameState {
        class_id: 0,
        method_key: String::from("T.m()V"),
        bci: 0,
        locals: vec![FrameValue::Int(1), FrameValue::Int(2)],
        stack: vec![FrameValue::Int(3)],
        monitors: Vec::new(),
        caller: None,
    };
    assert_eq!(first_unresumable_slot(&fs), None);
    assert!(frame_state_is_resumable(&fs));

    let mut with_stack = fs.clone();
    with_stack.stack = vec![FrameValue::Int(3), FrameValue::Unsupported];
    let msg = first_unresumable_slot(&with_stack).expect("stack 1 blocks the resume");
    assert!(msg.starts_with("stack 1 "), "{msg}");
    assert!(
        msg.ends_with(" of 2"),
        "depth belongs in the message: {msg}"
    );

    let mut with_local = fs.clone();
    with_local.locals = vec![FrameValue::Int(1), FrameValue::Unsupported];
    let msg = first_unresumable_slot(&with_local).expect("local 1 blocks the resume");
    assert!(msg.starts_with("local 1 "), "{msg}");

    // Locals are scanned before the stack, so a frame with both names the
    // local — the same order `frame_state_is_resumable` walks.
    let mut both = fs;
    both.locals = vec![FrameValue::Unsupported];
    both.stack = vec![FrameValue::Unsupported];
    assert!(first_unresumable_slot(&both)
        .expect("blocked")
        .starts_with("local 0 "));
}

/// A RETHROW point's operand stack is never reconstructed, so it must not
/// veto — and its LOCALS must still veto, because the handler reads them.
///
/// Both halves are asserted here, and both matter. Dropping the first
/// costs every method whose `catch` block contains a call its OSR entry
/// (the veto is artifact-wide). Dropping the second enters a handler on
/// zeroed non-parameter locals, which is the silent miscompile
/// `route_jit_signal_exception` fails closed against.
#[test]
fn a_rethrow_frame_is_judged_on_its_locals_only() {
    let base = FrameState {
        class_id: 0,
        method_key: String::from("T.m()V"),
        bci: 105,
        locals: vec![FrameValue::Int(1)],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };

    // The shape this exists for: `catch (E e) { g(-1, x); }` in a method
    // that also touches a `long`, so the `-1` under the call's argument
    // has no width source and comes out `Unsupported`.
    let mut untypeable_stack = base.clone();
    untypeable_stack.stack = vec![FrameValue::Unsupported, FrameValue::Object(0)];
    assert!(
        first_unresumable_slot(&untypeable_stack).is_some(),
        "the general rule still sees it — this is the one a RESUME point is judged by"
    );
    assert_eq!(
        first_unresumable_local(&untypeable_stack),
        None,
        "a RETHROW frame's stack is replaced by [exception]; it cannot block anything"
    );

    let mut untypeable_local = base;
    untypeable_local.locals = vec![FrameValue::Int(1), FrameValue::Unsupported];
    let msg = first_unresumable_local(&untypeable_local).expect("a handler reads its locals");
    assert!(msg.starts_with("local 1 "), "{msg}");
}

/// The narrow rule walks the WHOLE caller chain, exactly as the general one
/// does — an inlined caller's local is as much a handler input as the
/// innermost scope's.
#[test]
fn the_rethrow_rule_walks_caller_scopes_too() {
    let caller = FrameState {
        class_id: 0,
        method_key: String::from("T.outer()V"),
        bci: 4,
        locals: vec![FrameValue::Unsupported],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let fs = FrameState {
        class_id: 0,
        method_key: String::from("T.inner()V"),
        bci: 0,
        locals: vec![FrameValue::Int(1)],
        stack: vec![FrameValue::Unsupported],
        monitors: Vec::new(),
        caller: Some(Box::new(caller)),
    };
    let msg = first_unresumable_local(&fs).expect("the caller's local blocks it");
    assert!(msg.starts_with("caller-scope-1 local 0 "), "{msg}");
}
