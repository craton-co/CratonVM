use super::*;

const M: &str = "T.m:(I)I";

fn limits() -> MethodFrameLimits {
    // 32 bytes of code, 3 locals, 2 stack.
    MethodFrameLimits::new(M, 32, 3, 2)
}

/// A well-formed point: bci in range, counts within limits, the one
/// reference local ([rbp-40]) covered by the oop map.
fn good_point() -> DeoptimizationPoint {
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
            locals: vec![FrameValue::StackSlotRef(-40), FrameValue::Int(7)],
            stack: vec![FrameValue::StackSlot(-48)],
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    }
}

fn verifier() -> DeoptVerifier {
    DeoptVerifier::new()
        .with_method(limits())
        .with_oop_map(0x40, OopCoverage::complete([40]))
}

fn rendered(errors: &[DeoptMetadataError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn well_formed_metadata_passes() {
    let v = verifier();
    let points = [good_point()];
    assert!(
        v.violations(&points).is_empty(),
        "{}",
        rendered(&v.violations(&points))
    );
    assert!(v.verify(&points).is_ok());
}

#[test]
fn bci_past_the_end_of_the_method_is_rejected() {
    let mut p = good_point();
    p.bci = 99;
    p.frame_state.bci = 99;
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::BciOutOfRange {
                bci: 99,
                code_len: 32,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
    let msg = rendered(&errs);
    assert!(msg.contains("resume bci 99"), "{msg}");
    assert!(msg.contains("32-byte code"), "{msg}");
}

#[test]
fn more_locals_than_max_locals_is_rejected() {
    let mut p = good_point();
    p.frame_state.locals = vec![FrameValue::Int(0); 4]; // max_locals is 3
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::LocalCountMismatch {
                found: 4,
                max_locals: 3,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("max_locals is 3"));
}

#[test]
fn deeper_stack_than_max_stack_is_rejected() {
    let mut p = good_point();
    p.frame_state.stack = vec![FrameValue::Int(0); 3]; // max_stack is 2
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::StackCountMismatch {
                found: 3,
                max_stack: 2,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
}

/// The moving-GC agreement rule: a reference the deopt map will hand the
/// interpreter must be a reference the collector knows to update. A
/// `StackSlotRef` whose word the oop map does not name is a stale-pointer
/// bug waiting for the first relocating young collection.
#[test]
fn reference_slot_absent_from_the_oop_map_is_rejected() {
    let mut p = good_point();
    // The oop map covers [rbp-40]; move the reference to [rbp-56].
    p.frame_state.locals[0] = FrameValue::StackSlotRef(-56);
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::ReferenceNotInOopMap {
                frame_offset: 56,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
    let msg = rendered(&errs);
    assert!(msg.contains("[rbp-56]"), "{msg}");
    assert!(msg.contains("local[0]"), "must name the slot: {msg}");
    assert!(msg.contains("stale address"), "{msg}");
}

/// A register-resident reference is checked against the same map.
#[test]
fn reference_register_absent_from_the_oop_map_is_rejected() {
    let mut p = good_point();
    p.frame_state.locals[0] = FrameValue::RegisterRef(9);
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::ReferenceRegisterNotInOopMap { reg: 9, .. })
        ),
        "{}",
        rendered(&errs)
    );
}

/// An oop map that does NOT claim moving-young completeness forces a
/// non-moving cycle for this frame, so an uncovered reference cannot go
/// stale — the lane must not report it (that would be a false positive on
/// every conservatively-handled frame).
#[test]
fn incomplete_coverage_does_not_flag_uncovered_references() {
    let mut p = good_point();
    p.frame_state.locals[0] = FrameValue::StackSlotRef(-56);
    let v = DeoptVerifier::new().with_method(limits()).with_oop_map(
        0x40,
        OopCoverage {
            frame_slot_offsets: vec![40],
            registers: Vec::new(),
            moving_young_coverage_complete: false,
        },
    );
    assert!(v.violations(&[p]).is_empty());
}

#[test]
fn reference_slots_without_any_oop_map_are_rejected_when_required() {
    let p = good_point();
    let v = DeoptVerifier::new()
        .with_method(limits())
        .requiring_oop_map(true);
    let errs = v.violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::MissingOopMap {
                native_offset: 0x40,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );
}

/// A slot that names an IR node the optimizer removed, without that node
/// being registered as materializable, rebuilds the slot from nothing.
#[test]
fn slot_naming_a_removed_node_is_rejected() {
    let mut p = good_point();
    p.frame_state.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 17, // IR node 17
        class_id: 5,
        num_fields: 1,
        field_values: vec![FrameValue::Int(3)],
    });
    let v = verifier().with_removed_nodes([17u32]);
    let errs = v.violations(&[p.clone()]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::SlotNamesRemovedNode { node: 17, .. })
        ),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("rebuild this slot from nothing"));

    // The same node, registered as materializable (it has a recipe), is fine.
    let ok = verifier()
        .with_removed_nodes([17u32])
        .with_materializable_nodes([17u32]);
    assert!(ok.violations(&[p]).is_empty());
}

#[test]
fn unbalanced_lock_state_is_rejected() {
    // depth 0 — a lock nobody holds.
    let mut p = good_point();
    p.frame_state.monitors = vec![MonitorInfo {
        object: FrameValue::StackSlotRef(-40),
        lock_depth: 0,
        relock: true,
    }];
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::UnbalancedMonitor { .. })
        ),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("lock_depth is 0"));

    // The same object recorded twice instead of once at depth 2.
    let mut p2 = good_point();
    p2.frame_state.monitors = vec![
        MonitorInfo {
            object: FrameValue::StackSlotRef(-40),
            lock_depth: 1,
            relock: true,
        },
        MonitorInfo {
            object: FrameValue::StackSlotRef(-40),
            lock_depth: 1,
            relock: true,
        },
    ];
    let errs2 = verifier().violations(&[p2]);
    assert!(
        rendered(&errs2).contains("already held by an earlier entry"),
        "{}",
        rendered(&errs2)
    );

    // Round 11 wave 15 (lane `ir`): the one admitted split -- the levels
    // the compiled code holds, then the levels a nested-lock elision
    // removed (`ir_lower::Lowerer::resolve_monitors`). Every other
    // two-or-more-entry shape on one object is still refused.
    let entry = |relock| MonitorInfo {
        object: FrameValue::StackSlotRef(-40),
        lock_depth: 1,
        relock,
    };
    let mut split = good_point();
    split.frame_state.monitors = vec![entry(false), entry(true)];
    let split_errs = verifier().violations(&[split]);
    assert!(
        split_errs.is_empty(),
        "held, then re-take: admitted: {}",
        rendered(&split_errs)
    );
    for bad in [
        vec![entry(true), entry(false)],
        vec![entry(false), entry(false)],
        vec![entry(false), entry(true), entry(true)],
    ] {
        let mut p = good_point();
        p.frame_state.monitors = bad;
        let errs = verifier().violations(&[p]);
        assert!(
            rendered(&errs).contains("already held by an earlier entry"),
            "{}",
            rendered(&errs)
        );
    }

    // A monitor whose object cannot be reconstructed cannot be unlocked.
    let mut p3 = good_point();
    p3.frame_state.monitors = vec![MonitorInfo {
        object: FrameValue::MaterializationRequired(EliminatedValue::new(
            4,
            EliminationCause::ElidedLock,
        )),
        lock_depth: 1,
        relock: true,
    }];
    let errs3 = verifier().violations(&[p3]);
    assert!(
        rendered(&errs3).contains("cannot be reconstructed"),
        "{}",
        rendered(&errs3)
    );
}

#[test]
fn virtual_object_graph_integrity_is_checked() {
    // Field count disagreement.
    let mut p = good_point();
    p.frame_state.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 3,
        class_id: 1,
        num_fields: 2,
        field_values: vec![FrameValue::Int(1)],
    });
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::VirtualObjectFieldCountMismatch {
                declared: 2,
                found: 1,
                ..
            })
        ),
        "{}",
        rendered(&errs)
    );

    // A dangling reference edge.
    let mut p2 = good_point();
    p2.frame_state.locals[0] = FrameValue::VirtualObjectRef(9);
    let errs2 = verifier().violations(&[p2]);
    assert!(
        matches!(
            errs2.first(),
            Some(DeoptMetadataError::UndefinedVirtualObjectRef { id: 9, .. })
        ),
        "{}",
        rendered(&errs2)
    );

    // A forward reference (ref in a local, definition later on the stack)
    // is legal — the materializer allocates every shell before wiring.
    let mut p3 = good_point();
    p3.frame_state.locals[0] = FrameValue::VirtualObjectRef(4);
    p3.frame_state.stack[0] = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 4,
        class_id: 1,
        num_fields: 0,
        field_values: Vec::new(),
    });
    assert!(verifier().violations(&[p3]).is_empty());

    // Defining the same object twice in one scope is not.
    let mut p4 = good_point();
    let vo = FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id: 4,
        class_id: 1,
        num_fields: 0,
        field_values: Vec::new(),
    });
    p4.frame_state.locals[0] = vo.clone();
    p4.frame_state.stack[0] = vo;
    let errs4 = verifier().violations(&[p4]);
    assert!(
        matches!(
            errs4.first(),
            Some(DeoptMetadataError::DuplicateVirtualObjectDefinition { id: 4, .. })
        ),
        "{}",
        rendered(&errs4)
    );
}

#[test]
fn inlined_caller_scopes_are_checked_too() {
    let mut p = good_point();
    p.frame_state.caller = Some(Box::new(FrameState {
        class_id: 0,
        method_key: "T.outer:()V".to_string(),
        bci: 500, // past the caller's code length
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    }));
    let v = verifier().with_method(MethodFrameLimits::new("T.outer:()V", 20, 1, 1));
    let errs = v.violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::BciOutOfRange { scope_depth: 1, .. })
        ),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("T.outer:()V"));
}

#[test]
fn a_scope_naming_an_unregistered_method_is_rejected() {
    let mut p = good_point();
    p.frame_state.method_key = "Other.x:()V".to_string();
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(errs.first(), Some(DeoptMetadataError::UnknownMethod { .. })),
        "{}",
        rendered(&errs)
    );
    assert!(rendered(&errs).contains("unverifiable"));
}

#[test]
fn out_of_range_register_descriptors_are_rejected() {
    let mut p = good_point();
    p.frame_state.locals[1] = FrameValue::Register(31);
    p.frame_state.stack[0] = FrameValue::XmmDouble(20);
    let errs = verifier().violations(&[p]);
    assert_eq!(errs.len(), 2, "{}", rendered(&errs));
    assert!(rendered(&errs).contains("gpr[31]"));
    assert!(rendered(&errs).contains("xmm[20]"));
}

#[test]
fn a_baked_heap_address_is_rejected() {
    let mut p = good_point();
    p.frame_state.locals[1] = FrameValue::Object(0x7f00_1234);
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::BakedObjectAddress { .. })
        ),
        "{}",
        rendered(&errs)
    );
    // A null constant is fine — nothing to update.
    let mut p2 = good_point();
    p2.frame_state.locals[1] = FrameValue::Object(0);
    assert!(verifier().violations(&[p2]).is_empty());
}

/// A DECREASING pair is a producer that recorded out of order, and is
/// rejected. Renamed from `unsorted_points_break_the_binary_search_…` in
/// round 10: the binary search it named (`CompiledMethod::find_deopt_point`)
/// had no production caller and has since been deleted, and the invariant is
/// worth holding on the producer's account, not on that search's.
#[test]
fn a_decreasing_native_offset_pair_is_rejected() {
    let mut a = good_point();
    a.native_offset = 0x80;
    let mut b = good_point();
    b.native_offset = 0x40;
    let v = DeoptVerifier::new();
    let errs = v.violations(&[a, b]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::DeoptPointsUnsorted {
                first: 0x80,
                second: 0x40
            })
        ),
        "{}",
        rendered(&errs)
    );
}

/// …and the EQUAL pair is not, deliberately. `build_and_record_deopt_point`
/// emits no machine code and the three recorder families key their
/// idempotence in three maps that do not see each other, so two adjacent
/// metadata-only records at one pc land on one `native_offset`. Tightening
/// this lane to `>=` would refuse such an artifact to prove a hypothesis;
/// see
/// `docs/feature-designs/deopt-osr.md`.
///
/// The consequence, recorded where it is load-bearing: `native_offset` is not
/// an identity, so nothing may resolve a point by offset and assume it got
/// the right one.
#[test]
fn an_equal_native_offset_pair_is_not_a_violation() {
    let mut a = good_point();
    a.native_offset = 0x80;
    let mut b = good_point();
    b.native_offset = 0x80;
    let errs = DeoptVerifier::new().violations(&[a, b]);
    assert!(errs.is_empty(), "{}", rendered(&errs));
}

#[test]
fn point_bci_must_match_its_frame_state_bci() {
    let mut p = good_point();
    p.bci = 13; // frame_state.bci is 12
    let errs = verifier().violations(&[p]);
    assert!(
        matches!(
            errs.first(),
            Some(DeoptMetadataError::PointBciMismatch { .. })
        ),
        "{}",
        rendered(&errs)
    );
}

#[test]
fn verify_returns_a_bailout_naming_every_violation() {
    let mut p = good_point();
    p.bci = 99;
    p.frame_state.bci = 99;
    p.frame_state.locals = vec![FrameValue::Int(0); 4];
    let err = verifier().verify(&[p]).unwrap_err();
    assert_eq!(err.category(), "deopt_metadata");
    assert_eq!(err.context.as_deref(), Some("phase=install"));
    let s = err.to_string();
    assert!(s.contains("2 deopt-metadata violation(s)"), "{s}");
    assert!(s.contains("resume bci 99"), "{s}");
    assert!(s.contains("max_locals is 3"), "{s}");
}

#[test]
fn free_function_wrapper_matches_the_builder() {
    let points = [good_point()];
    assert!(verify_deopt_metadata(
        &points,
        [limits()],
        [(0x40u32, OopCoverage::complete([40]))],
    )
    .is_ok());
}

// ── eliminated vs. undefined ─────────────────────────────────────

/// The distinction that keeps a scalar-replaced object from silently
/// reconstructing as `null`: `Undefined` is resumable (the sink maps it to
/// `Int(0)` because nothing reads the slot), `MaterializationRequired` is
/// not (the sink refuses, forcing a safe re-run), and the two are never
/// equal — so a producer that emits one can never be mistaken for the other.
#[test]
fn eliminated_and_undefined_are_distinct_states() {
    let eliminated = FrameValue::MaterializationRequired(EliminatedValue::allocation(
        12,
        77,
        EliminationCause::ScalarReplacedObject,
    ));
    assert_ne!(eliminated, FrameValue::Undefined);
    assert_ne!(eliminated, FrameValue::Unsupported);

    let undefined_frame = FrameState {
        class_id: 0,
        method_key: M.to_string(),
        bci: 0,
        locals: vec![FrameValue::Undefined],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let eliminated_frame = FrameState {
        class_id: 0,
        method_key: M.to_string(),
        bci: 0,
        locals: vec![eliminated.clone()],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    assert!(
        frame_state_is_resumable(&undefined_frame),
        "a genuinely undefined slot must stay resumable"
    );
    assert!(
        !frame_state_is_resumable(&eliminated_frame),
        "an eliminated-but-unrebuildable slot must NOT resume as a value"
    );
    assert_eq!(count_materialization_required(&undefined_frame), 0);
    assert_eq!(count_materialization_required(&eliminated_frame), 1);
}

/// The marker survives machine-state resolution unchanged — softening it to
/// `Undefined` (or to a machine read) anywhere in the pipeline restores the
/// silent-null reconstruction.
#[test]
fn eliminated_marker_round_trips_through_reconstruction() {
    let eliminated = FrameValue::MaterializationRequired(EliminatedValue::new(
        3,
        EliminationCause::NestedVirtualObject,
    ));
    let dp = DeoptimizationPoint {
        native_offset: 0,
        bci: 4,
        reason: DeoptReason::UncommonTrap,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: M.to_string(),
            bci: 4,
            locals: vec![eliminated.clone(), FrameValue::Undefined],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let regs = SavedRegisters::default();
    let rf = reconstruct_frame_from_machine_state(&dp, &regs, 0);
    assert_eq!(rf.locals[0], eliminated);
    assert_eq!(rf.locals[1], FrameValue::Undefined);
}

/// A `MaterializationRequired` field inside an otherwise-complete virtual
/// object poisons the whole object: materializing it would store a null
/// into a field that held a live reference.
#[test]
fn an_unrebuildable_field_makes_the_virtual_object_unresumable() {
    let fs = FrameState {
        class_id: 0,
        method_key: M.to_string(),
        bci: 0,
        locals: vec![FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 1,
            class_id: 2,
            num_fields: 2,
            field_values: vec![
                FrameValue::Int(4),
                FrameValue::MaterializationRequired(EliminatedValue::unknown(
                    EliminationCause::EliminatedStore,
                )),
            ],
        })],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    assert!(!frame_state_is_resumable(&fs));
    assert_eq!(count_materialization_required(&fs), 1);
    // The object itself is still a well-formed *description* — the verifier
    // reports no violation, because refusing to resume is the correct,
    // already-safe outcome. Only the resumability predicate rejects it.
    assert!(DeoptVerifier::new()
        .violations(&[DeoptimizationPoint {
            native_offset: 0,
            bci: 0,
            reason: DeoptReason::UncommonTrap,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            frame_state: fs,
            semantics: ResumeSemantics::REEXECUTE,
        }])
        .is_empty());
}

/// `EliminationCause` reaches the report: the rendered marker names the
/// pass that deleted the value and the node it deleted.
#[test]
fn eliminated_value_renders_its_cause_and_producer() {
    let ev = EliminatedValue::allocation(21, 9, EliminationCause::ScalarReplacedObject);
    let s = ev.to_string();
    assert!(s.contains("scalar-replaced object"), "{s}");
    assert!(s.contains("n21"), "{s}");
    assert!(s.contains("class_id=9"), "{s}");
    assert_eq!(
        EliminatedValue::unknown(EliminationCause::Unclassified).to_string(),
        "unclassified elimination"
    );
}

/// A null `DeoptimizationPoint` must not be dereferenced inside the
/// trampoline: the entry stashes the `u32::MAX` re-run sentinel so the VM
/// takes the safe whole-method path and never reads the `i64::MIN` return
/// as a legitimate `Long.MIN_VALUE`.
#[test]
fn ir_deopt_entry_survives_a_null_point() {
    let _ = take_last_deopt();
    assert_eq!(
        ir_deopt_entry(std::ptr::null(), 0, std::ptr::null()),
        i64::MIN
    );
    let frame = take_last_deopt().expect("null point stashes the re-run sentinel");
    assert_eq!(frame.bci, u32::MAX);
    assert!(frame.method_key.is_empty());
}

/// The runtime resolver must not panic on a malformed register descriptor:
/// it degrades to `Unsupported` (refuse + safe re-run) and the structured
/// error is available to the checked path.
#[test]
fn out_of_range_register_resolves_to_unsupported_instead_of_panicking() {
    let regs = SavedRegisters::default();
    assert_eq!(
        resolve_value(&FrameValue::Register(200), &regs, 0),
        FrameValue::Unsupported
    );
    assert_eq!(
        try_resolve_value(&FrameValue::XmmFloat(99), &regs, 0),
        Err(DeoptMetadataError::RegisterFileIndexOutOfRange {
            bank: "xmm",
            index: 99,
        })
    );
}
