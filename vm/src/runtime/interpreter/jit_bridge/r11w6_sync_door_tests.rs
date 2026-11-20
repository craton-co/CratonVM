use super::wrapped_body_attempt_never_reruns;
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ResumeSemantics,
};

/// Metadata only; the `ret` body never runs.
fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    cratonvm_jit::CompiledMethod::new(buf)
}

fn trap_point(bci: u32) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason: DeoptReason::UnreachedCode,
        action: DeoptAction::Reinterpret,
        semantics: ResumeSemantics::REEXECUTE,
        speculation_id: cratonvm_jit::deopt::speculation_id(bci, DeoptReason::UnreachedCode),
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

/// The `static synchronized` door enters a compiled body only when no exit
/// of it can make the door re-run a partly executed body; everything else
/// keeps the interpreted template, which never re-runs anything.
#[test]
fn the_synchronized_door_enters_only_bodies_that_never_rerun() {
    assert!(
        wrapped_body_attempt_never_reruns(&body()),
        "no call, no trap: every exit is a return or a routed exception"
    );

    let mut trapping = body();
    trapping.deopt_points.push(trap_point(7));
    assert!(
        !wrapped_body_attempt_never_reruns(&trapping),
        "a trap with no precise resume re-runs the body from entry"
    );
    trapping.can_deopt_resume = true;
    assert_eq!(
        wrapped_body_attempt_never_reruns(&trapping),
        cratonvm_jit::deopt_real_enabled(),
        "a precise resume hands the monitor to the resumed frame"
    );

    let mut frameless = body();
    frameless.has_frameless_trap_stub = true;
    assert!(!wrapped_body_attempt_never_reruns(&frameless));

    let mut indy = body();
    indy.has_indy_trap = true;
    assert!(!wrapped_body_attempt_never_reruns(&indy));

    let mut calls = body();
    calls._direct_callee_entries.push(0x1000);
    assert!(
        !wrapped_body_attempt_never_reruns(&calls),
        "a callee's declined trap arrives as a foreign stash"
    );
}

/// JIT round 16 wave 8 (lane sync), `CRATONVM_JIT_DOOR_SYNC_CALLING_BODY`:
/// the calling-body arm lifts the "no Java call" term and nothing else.
/// Off, the door's answer is exactly `wrapped_body_attempt_never_reruns`.
#[test]
fn the_calling_body_arm_admits_calls_and_nothing_else() {
    use super::door_wrapped_body_attempt_admitted as admitted;

    let mut calls = body();
    calls._direct_callee_entries.push(0x1000);
    assert!(
        !admitted(&calls, false),
        "off: a calling body keeps the template"
    );
    assert!(
        admitted(&calls, true),
        "on: a calling body with closed own exits"
    );
    assert!(admitted(&body(), false) && admitted(&body(), true));

    // Its own exits still decide, whatever it calls.
    let mut trapping = body();
    trapping._direct_callee_entries.push(0x1000);
    trapping.deopt_points.push(trap_point(7));
    assert!(
        !admitted(&trapping, true),
        "an own trap with no precise resume"
    );
    trapping.can_deopt_resume = true;
    assert_eq!(
        admitted(&trapping, true),
        cratonvm_jit::deopt_real_enabled(),
        "a precise resume hands the monitor over, as for a call-free body"
    );
    for (frameless, indy) in [(true, false), (false, true)] {
        let mut sentinel = body();
        sentinel._direct_callee_entries.push(0x1000);
        sentinel.has_frameless_trap_stub = frameless;
        sentinel.has_indy_trap = indy;
        assert!(
            !admitted(&sentinel, true),
            "bare-sentinel exits are never admitted"
        );
    }
}

/// Round 11 wave 16 (lane `tier`): an optimizing body publishes a plain
/// resume point at every snapshot even when nothing in it can deopt, so
/// its "no trap" fact is the empty box list, not the empty point list.
/// Before this, every optimizing body was refused by both the door and the
/// caller-held direct CALL (`R11X64genSyncCost` `sync-method-1t` ~800 ms
/// once `incSync`'s optimizing body superseded its single-pass one).
#[test]
fn an_optimizing_body_is_judged_by_its_deopt_boxes_not_its_resume_points() {
    use super::caller_held_body_is_closed;

    // `incSync`'s shape: resume points everywhere, no guard, two
    // call-exception exits (the static-field helpers).
    let mut ir = body();
    ir.used_ir_backend = true;
    ir.deopt_points.push(trap_point(0));
    ir.deopt_points.push(trap_point(9));
    ir.ir_osr_exception_exit_bcis = Some(vec![3u32, 9].into_boxed_slice());
    assert!(wrapped_body_attempt_never_reruns(&ir));
    assert!(caller_held_body_is_closed(&ir));

    // A guard pad: a real deopt exit.
    let mut guarded = body();
    guarded.used_ir_backend = true;
    guarded.deopt_points.push(trap_point(4));
    guarded.ir_osr_exception_exit_bcis = Some(Vec::new().into_boxed_slice());
    guarded._deopt_point_boxes.push(Box::new(trap_point(4)));
    assert!(!caller_held_body_is_closed(&guarded));
    assert_eq!(
        wrapped_body_attempt_never_reruns(&guarded),
        guarded.can_deopt_resume && cratonvm_jit::deopt_real_enabled()
    );

    // The emission did not confirm "no guard pad" (and the body is not
    // sentinel-free): refused even with no box, fail closed.
    let mut unconfirmed = body();
    unconfirmed.used_ir_backend = true;
    unconfirmed.deopt_points.push(trap_point(2));
    assert!(unconfirmed.ir_osr_exception_exit_bcis.is_none());
    assert!(!unconfirmed.ir_osr_sentinel_free);
    assert!(!caller_held_body_is_closed(&unconfirmed));
    assert!(!wrapped_body_attempt_never_reruns(&unconfirmed));

    // A sentinel-free optimizing body has no exit at all.
    let mut free = body();
    free.used_ir_backend = true;
    free.deopt_points.push(trap_point(1));
    free.ir_osr_sentinel_free = true;
    assert!(caller_held_body_is_closed(&free));

    // OSR-exit maps are deopt exits on either backend.
    let mut osr_exit = body();
    osr_exit.used_ir_backend = true;
    osr_exit.ir_osr_sentinel_free = true;
    osr_exit.osr_exit_points.push(5);
    assert!(!caller_held_body_is_closed(&osr_exit));

    // The single-pass reading is unchanged: a published point is a trap.
    let mut sp = body();
    sp.deopt_points.push(trap_point(7));
    sp.ir_osr_exception_exit_bcis = Some(Vec::new().into_boxed_slice());
    assert!(!caller_held_body_is_closed(&sp));
}

/// Interpreter round i2 wave 53, lane L2 (the wave-25 design of the i22-L2
/// caller-held proposal): an optimizing body whose only boxes are poll
/// exits on `SAFEPOINT_VERDICT_NAMED_BODY` (`named_poll_exits_only`) is
/// bound like a body with no box; one guard box among them, a rethrowing
/// box, an unstamped body or an OSR-exit map still refuses.
#[test]
fn a_body_whose_only_exits_are_named_polls_is_closed_for_the_caller_held_site() {
    use super::{caller_held_body_exits_are_named_polls, caller_held_body_is_closed};
    let poll_exit = |bci: u32| {
        let mut p = trap_point(bci);
        p.reason = cratonvm_jit::deopt::DeoptReason::OsrExit;
        p.semantics = cratonvm_jit::deopt::ResumeSemantics::for_reason(p.reason);
        p
    };
    let named = || {
        let mut b = body();
        b.used_ir_backend = true;
        b.deopt_points.push(trap_point(0));
        b.ir_osr_exception_exit_bcis = Some(Vec::new().into_boxed_slice());
        b._deopt_point_boxes.push(Box::new(poll_exit(2)));
        b.named_poll_exits_only = true;
        // The IR door's vouch for a synchronized body with a box.
        b.can_deopt_resume = true;
        b
    };
    assert!(caller_held_body_exits_are_named_polls(&named()));
    assert!(caller_held_body_is_closed(&named()));

    let mut unstamped = named();
    unstamped.named_poll_exits_only = false;
    assert!(
        !caller_held_body_is_closed(&unstamped),
        "not a named-only body"
    );

    let mut guarded = named();
    guarded._deopt_point_boxes.push(Box::new(trap_point(4)));
    assert!(
        !caller_held_body_is_closed(&guarded),
        "a guard box is a trap"
    );

    let mut rethrows = named();
    rethrows._deopt_point_boxes[0].semantics.rethrow_exception = true;
    assert!(!caller_held_body_is_closed(&rethrows), "a rethrow pad");

    let mut unconfirmed = named();
    unconfirmed.ir_osr_exception_exit_bcis = None;
    assert!(
        !caller_held_body_is_closed(&unconfirmed),
        "the emission did not confirm \"no guard pad\""
    );

    let mut osr_exit = named();
    osr_exit.osr_exit_points.push(5);
    assert!(!caller_held_body_is_closed(&osr_exit));

    let mut single_pass = named();
    single_pass.used_ir_backend = false;
    assert!(!caller_held_body_exits_are_named_polls(&single_pass));

    // The mark the binding sets, read by the safepoint verdict.
    let bound = named();
    assert!(!bound.is_bound_by_a_caller_held_site());
    bound.mark_bound_by_a_caller_held_site();
    assert!(bound.is_bound_by_a_caller_held_site());
}

/// Interpreter round i1 wave 25, lane L2: the door reads the exceptional
/// stash's depth only for a body that owns a point a pad could bake — a
/// box on the optimizing tier (its `deopt_points` are resume points no
/// stub names), any point on the single-pass tier.
#[test]
fn only_a_body_with_a_bakeable_point_may_publish_a_pad_frame() {
    use super::body_may_publish_a_pad_frame;
    assert!(
        !body_may_publish_a_pad_frame(&body()),
        "a leaf with no point"
    );
    let mut ir_resume_points_only = body();
    ir_resume_points_only.used_ir_backend = true;
    ir_resume_points_only.deopt_points.push(trap_point(3));
    assert!(!body_may_publish_a_pad_frame(&ir_resume_points_only));
    let mut ir_pad = body();
    ir_pad.used_ir_backend = true;
    ir_pad._deopt_point_boxes.push(Box::new(trap_point(3)));
    assert!(body_may_publish_a_pad_frame(&ir_pad));
    let mut sp = body();
    sp.deopt_points.push(trap_point(7));
    assert!(body_may_publish_a_pad_frame(&sp));
}

/// The door enters an optimizing body only when each of its
/// call-exception exits (which publish no precise frame) lies outside the
/// method's exception table, because the door's fallback handler frame
/// holds only the parameters.
#[test]
fn the_door_enters_an_optimizing_body_only_when_its_exits_are_unprotected() {
    use super::optimizing_exits_route_exactly;
    use std::sync::Arc;
    let template = |rows: Vec<cratonvm_reader::attribute::ExceptionTableEntry>| {
        cratonvm_jit_api::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cratonvm_types::ClassId::new(79),
                class_name: Arc::from("r11w16/Door"),
                method_name: Arc::from("incSync"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: Arc::from(vec![0xB1u8, 0, 0].as_slice()),
                exception_table: Arc::from(rows.as_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: true,
                is_static: true,
            },
        )
    };
    let row = |start_pc: u16, end_pc: u16| cratonvm_reader::attribute::ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc: 20,
        catch_type: 0,
    };
    let mut ir = body();
    ir.used_ir_backend = true;
    ir.ir_osr_exception_exit_bcis = Some(vec![3u32, 9].into_boxed_slice());

    assert!(optimizing_exits_route_exactly(&template(Vec::new()), &ir));
    assert!(optimizing_exits_route_exactly(
        &template(vec![row(10, 15)]),
        &ir
    ));
    assert!(!optimizing_exits_route_exactly(
        &template(vec![row(8, 12)]),
        &ir
    ));

    // No confirmed exit list: refused unless the body has no exit at all,
    // or the method has no handler an exit could reach (round 12 wave 8).
    ir.ir_osr_exception_exit_bcis = None;
    assert!(optimizing_exits_route_exactly(&template(Vec::new()), &ir));
    assert!(!optimizing_exits_route_exactly(
        &template(vec![row(10, 15)]),
        &ir
    ));
    ir.ir_osr_sentinel_free = true;
    assert!(optimizing_exits_route_exactly(
        &template(vec![row(0, 3)]),
        &ir
    ));

    // Single-pass bodies are not asked.
    assert!(optimizing_exits_route_exactly(
        &template(vec![row(0, 3)]),
        &body()
    ));
}
