use super::*;

const M: &str = "T.m:(I)I";

fn limits() -> MethodFrameLimits {
    MethodFrameLimits::new(M, 32, 3, 2)
}

/// A point with no reference slots and no oop-map dependency, so each test
/// below introduces exactly one defect.
fn plain_point() -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x40,
        bci: 12,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: M.to_string(),
            bci: 12,
            locals: vec![FrameValue::Int(7)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    }
}

fn scoped() -> DeoptVerifier {
    DeoptVerifier::new().with_method(limits())
}

fn rendered(errors: &[DeoptMetadataError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

// ── frame-slot addressing ────────────────────────────────────────

/// Every producer encodes a frame slot as the negation of a strictly
/// positive `[rbp - spill]` offset, so a non-negative offset addresses the
/// saved frame pointer (`0`) or the caller's frame. Reading a local from
/// there reconstructs a value that looks entirely plausible and is not the
/// program's.
#[test]
fn a_frame_slot_at_or_above_rbp_is_rejected() {
    for bad in [
        FrameValue::StackSlot(0),
        FrameValue::StackSlot(16),
        FrameValue::StackSlotLong(8),
        FrameValue::StackSlotFloat(0),
        FrameValue::StackSlotDouble(24),
        FrameValue::StackSlotRef(0),
    ] {
        let mut p = plain_point();
        p.frame_state.locals[0] = bad.clone();
        let errs = scoped().violations(&[p]);
        assert!(
            errs.iter()
                .any(|e| matches!(e, DeoptMetadataError::FrameSlotOutsideFrame { .. })),
            "{bad:?} must be refused: {}",
            rendered(&errs)
        );
    }
}

#[test]
fn an_ordinary_negative_frame_slot_is_accepted() {
    let mut p = plain_point();
    p.frame_state.locals[0] = FrameValue::StackSlot(-40);
    assert!(scoped().violations(&[p]).is_empty());
}

/// The message names the word, not just "some slot is wrong".
#[test]
fn the_outside_frame_message_names_the_slot_and_the_offset() {
    let mut p = plain_point();
    p.frame_state.stack = vec![FrameValue::StackSlotRef(8)];
    let msg = rendered(&scoped().violations(&[p]));
    assert!(msg.contains("stack[0]"), "{msg}");
    assert!(msg.contains("rbp+8"), "{msg}");
}

// ── one word, one type ───────────────────────────────────────────

/// A machine word holds one value at one program point. Describing it as a
/// reference in one slot and an `int` in another is an internal
/// contradiction, and it is exactly the shape that reconstructs an `int`
/// into a slot the interpreter reads as a reference.
#[test]
fn one_word_described_as_two_types_is_rejected() {
    let mut p = plain_point();
    p.frame_state.locals = vec![FrameValue::StackSlotRef(-40)];
    p.frame_state.stack = vec![FrameValue::StackSlot(-40)];
    let errs = DeoptVerifier::new().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::SlotTypeConflict {
                offset: -40,
                first_kind: "ref",
                second_kind: "int",
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
    let msg = rendered(&errs);
    assert!(msg.contains("local[0]"), "must name the first slot: {msg}");
    assert!(msg.contains("stack[0]"), "must name the second slot: {msg}");
}

/// Sharing a word is legal — `aload_0` leaves local 0 and stack[0] naming
/// the same spill — as long as the two agree on its type.
#[test]
fn the_same_word_named_twice_with_the_same_type_is_fine() {
    let mut p = plain_point();
    p.frame_state.locals = vec![FrameValue::StackSlotRef(-40)];
    p.frame_state.stack = vec![FrameValue::StackSlotRef(-40)];
    assert!(DeoptVerifier::new().violations(&[p]).is_empty());
}

/// Cat-1 and cat-2 are different types even though both are "not a
/// reference": a `long` read as an `int` loses the upper 32 bits and the
/// second local slot.
#[test]
fn an_int_and_a_long_at_one_word_are_a_conflict() {
    let mut p = plain_point();
    p.frame_state.locals = vec![FrameValue::StackSlot(-40)];
    p.frame_state.stack = vec![FrameValue::StackSlotLong(-40)];
    let errs = DeoptVerifier::new().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::SlotTypeConflict {
                first_kind: "int",
                second_kind: "long",
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
}

// ── the converse oop-map direction ───────────────────────────────

/// The forward rule (a reference the deopt map names must be one the GC
/// updates) had a hole in the other direction: the deopt map could read a
/// word as an `int` that the oop map lists as a live reference. One of the
/// two is wrong and both readings are unsound.
#[test]
fn a_primitive_frame_slot_the_oop_map_calls_a_reference_is_rejected() {
    let mut p = plain_point();
    p.frame_state.locals[0] = FrameValue::StackSlot(-40);
    let v = scoped().with_oop_map(0x40, OopCoverage::complete([40]));
    let errs = v.violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::PrimitiveSlotCoveredByOopMap {
                bank: "frame",
                location: 40,
                kind: "int",
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("disagree about the type of one word"));
}

#[test]
fn a_primitive_register_the_oop_map_calls_a_reference_is_rejected() {
    let mut p = plain_point();
    p.frame_state.locals[0] = FrameValue::Register(3);
    let v = scoped().with_oop_map(
        0x40,
        OopCoverage {
            frame_slot_offsets: Vec::new(),
            registers: vec![3],
            moving_young_coverage_complete: true,
        },
    );
    let errs = v.violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::PrimitiveSlotCoveredByOopMap {
                bank: "gpr",
                location: 3,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
}

/// Same exemption as the forward lane: a map that does not claim
/// moving-young completeness is a conservative over-approximation the
/// collector already refuses to relocate against, so a disagreement there
/// is not evidence of a defect.
#[test]
fn an_incomplete_oop_map_does_not_flag_a_primitive_slot() {
    let mut p = plain_point();
    p.frame_state.locals[0] = FrameValue::StackSlot(-40);
    let v = scoped().with_oop_map(
        0x40,
        OopCoverage {
            frame_slot_offsets: vec![40],
            registers: Vec::new(),
            moving_young_coverage_complete: false,
        },
    );
    assert!(v.violations(&[p]).is_empty());
}

/// The direction that is *not* an error, restated as a test so it stays
/// that way: the GC may track a spilled temporary the interpreter frame
/// does not resume from.
#[test]
fn an_oop_map_slot_the_deopt_map_never_names_is_not_an_error() {
    let p = plain_point();
    let v = scoped().with_oop_map(0x40, OopCoverage::complete([40, 48, 56]));
    assert!(v.violations(&[p]).is_empty());
}

// ── monitors ─────────────────────────────────────────────────────

/// A monitor entry replays as `monitorenter` and is balanced by one
/// `monitorexit` at method exit. An entry naming a primitive descriptor
/// leaves the real lock held: a hang in an unrelated thread, arbitrarily
/// later, with nothing pointing back at the deopt.
#[test]
fn a_monitor_on_a_primitive_is_rejected() {
    for bad in [
        FrameValue::Int(5),
        FrameValue::Long(5),
        FrameValue::StackSlot(-40),
        FrameValue::StackSlotLong(-40),
        FrameValue::Register(2),
        FrameValue::XmmDouble(1),
    ] {
        let mut p = plain_point();
        p.frame_state.monitors = vec![MonitorInfo {
            object: bad.clone(),
            lock_depth: 1,
            relock: true,
        }];
        let errs = scoped().violations(&[p]);
        assert!(
            errs.iter()
                .any(|e| matches!(e, DeoptMetadataError::MonitorObjectNotAReference { .. })),
            "monitor on {bad:?} must be refused: {}",
            rendered(&errs)
        );
    }
}

/// `monitorenter` on null throws rather than locking, so a null monitor
/// object means the emitter lost the object — not that the program locked
/// nothing.
#[test]
fn a_monitor_on_null_is_rejected() {
    let mut p = plain_point();
    p.frame_state.monitors = vec![MonitorInfo {
        object: FrameValue::Object(0),
        lock_depth: 1,
        relock: true,
    }];
    let errs = scoped().violations(&[p]);
    assert!(
        errs.iter()
            .any(|e| matches!(e, DeoptMetadataError::MonitorObjectNotAReference { .. })),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("null"));
}

/// The reference-shaped forms stay accepted, including a monitor on a
/// scalar-replaced object that will be materialized before the re-lock.
#[test]
fn reference_shaped_monitor_objects_are_accepted() {
    for good in [
        FrameValue::StackSlotRef(-40),
        FrameValue::RegisterRef(4),
        FrameValue::VirtualObjectRef(9),
    ] {
        let mut p = plain_point();
        // A defining occurrence for the `VirtualObjectRef` case.
        p.frame_state.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 9,
            class_id: 3,
            num_fields: 0,
            field_values: Vec::new(),
        });
        p.frame_state.monitors = vec![MonitorInfo {
            object: good.clone(),
            lock_depth: 2,
            relock: true,
        }];
        let errs = scoped().violations(&[p]);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, DeoptMetadataError::MonitorObjectNotAReference { .. })),
            "monitor on {good:?} must be accepted: {}",
            rendered(&errs)
        );
    }
}

/// The monitor object goes through the same oop-map agreement lane as a
/// local: a lock the collector cannot see is re-acquired on a stale address
/// after a relocating young collection.
#[test]
fn a_monitor_object_is_checked_against_the_oop_map() {
    let mut p = plain_point();
    p.frame_state.monitors = vec![MonitorInfo {
        object: FrameValue::StackSlotRef(-64),
        lock_depth: 1,
        relock: true,
    }];
    let v = scoped().with_oop_map(0x40, OopCoverage::complete([40]));
    let errs = v.violations(&[p]);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            DeoptMetadataError::ReferenceNotInOopMap {
                frame_offset: 64,
                ..
            }
        )),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("monitor[0]"));
}

// ── exception state ──────────────────────────────────────────────

/// `x64_deopt_entry` routes on `reason`; a resume sink reads `semantics`.
/// If the two disagree, one of them resumes a frame the other knows is
/// exceptional — the metadata restatement of "the `finally` block was not
/// run" and "the `athrow` bci leaked".
#[test]
fn a_pending_exception_point_without_rethrow_semantics_is_rejected() {
    let mut p = plain_point();
    p.reason = DeoptReason::PendingException;
    p.semantics = ResumeSemantics::REEXECUTE;
    let errs = scoped().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::ResumeSemanticsMismatch {
                reason: DeoptReason::PendingException,
                rethrow: false,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
}

#[test]
fn a_rethrow_point_whose_reason_routes_it_to_the_resume_stash_is_rejected() {
    let mut p = plain_point();
    p.reason = DeoptReason::NullCheck;
    p.semantics = ResumeSemantics::RETHROW;
    let errs = scoped().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::ResumeSemanticsMismatch { rethrow: true, .. })
        ),
        "{}",
        rendered(&errs)
    );
}

/// What every producer stamps today (`ResumeSemantics::for_reason`) passes
/// for every reason, so the lane is a tripwire on a future producer rather
/// than a tax on the current ones.
#[test]
fn for_reason_agrees_with_the_lane_for_every_reason() {
    for reason in [
        DeoptReason::NullCheck,
        DeoptReason::ClassCheck,
        DeoptReason::BoundsCheck,
        DeoptReason::DivByZero,
        DeoptReason::ReceiverTypeChanged,
        DeoptReason::ClassLoading,
        DeoptReason::UninitializedAccess,
        DeoptReason::TransferToInterpreter,
        DeoptReason::UncommonTrap,
        DeoptReason::SpeculationFailed,
        DeoptReason::NotCompiled,
        DeoptReason::UnreachedCode,
        DeoptReason::OsrExit,
        DeoptReason::PendingException,
        DeoptReason::UnstableIf,
    ] {
        let mut p = plain_point();
        p.reason = reason;
        p.semantics = ResumeSemantics::for_reason(reason);
        let errs = scoped().violations(&[p]);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, DeoptMetadataError::ResumeSemanticsMismatch { .. })),
            "{reason:?}: {}",
            rendered(&errs)
        );
    }
}

/// A caller scope is `RESUME`, and that is the whole reason
/// `ResumeSemantics` is more than a rename of `DeoptReason`.
///
/// Re-homed here 2026-09-21: the assertion used to live in
/// `the_reexecute_flag_survives_interning`, which went with the interned
/// representation. It is worth keeping on its own because three files' prose
/// rests on it — `jit/src/x64.rs`, `jit/src/ir.rs` and
/// `vm/src/runtime/interpreter/deopt_resume.rs` all cite
/// "`for_caller_scope()` is `RESUME`" when explaining why a frame parked
/// mid-`invoke` must not re-execute the invoke (which would call the callee a
/// second time). Nothing else now checks the claim those three depend on.
#[test]
fn a_caller_scope_resumes_rather_than_re_executing_its_invoke() {
    assert_eq!(ResumeSemantics::for_caller_scope(), ResumeSemantics::RESUME);
    assert!(!ResumeSemantics::for_caller_scope().reexecute);
    assert!(!ResumeSemantics::for_caller_scope().rethrow_exception);
    // And it is genuinely distinct from what every producer stamps today, so
    // a producer of caller scopes has to say so explicitly.
    assert_ne!(
        ResumeSemantics::for_caller_scope(),
        ResumeSemantics::default()
    );
    assert_eq!(ResumeSemantics::default(), ResumeSemantics::REEXECUTE);
}

// ── caller-chain depth ───────────────────────────────────────────

fn owned_chain(depth: usize) -> FrameState {
    let mut fs = FrameState {
        class_id: 0,
        method_key: String::new(),
        bci: 0,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    for i in 0..depth {
        fs = FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: i as u32 + 1,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: Some(Box::new(fs)),
        };
    }
    fs
}

/// The verifier used to walk a caller chain to its end with no bound and
/// report nothing about the fact that it was absurdly deep. Now the cap is
/// a finding: stopping at it and saying "clean" would report a verdict for
/// scopes nobody looked at.
#[test]
fn an_over_deep_owned_chain_is_reported_not_silently_accepted() {
    let mut p = plain_point();
    p.frame_state = owned_chain(MAX_SCOPE_CHAIN + 2);
    p.frame_state.bci = p.bci;
    let errs = DeoptVerifier::new().violations(&[p]);
    assert!(
        errs.iter()
            .any(|e| matches!(e, DeoptMetadataError::ScopeChainTooDeep { .. })),
        "{}",
        rendered(&errs)
    );
}

#[test]
fn a_chain_within_the_cap_is_walked_without_complaint() {
    let mut p = plain_point();
    p.frame_state = owned_chain(4);
    p.frame_state.bci = p.bci;
    assert!(DeoptVerifier::new().violations(&[p]).is_empty());
}
