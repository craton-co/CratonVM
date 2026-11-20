use super::{
    first_unresumable_local, first_unresumable_slot, frame_state_is_resumable,
    inlined_chain_refusal_with, DeoptAction, DeoptMetadataError, DeoptReason, DeoptVerifier,
    DeoptimizationPoint, FrameState, FrameValue, MonitorInfo, ResumeSemantics,
    VirtualObjectState,
};

fn scope(
    key: &str,
    bci: u32,
    locals: Vec<FrameValue>,
    caller: Option<FrameState>,
) -> FrameState {
    FrameState {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals,
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: caller.map(Box::new),
    }
}

fn vobj(id: usize, fields: Vec<FrameValue>) -> FrameValue {
    FrameValue::VirtualObject(VirtualObjectState {
        array_element_type: None,
        id,
        class_id: 7,
        num_fields: fields.len(),
        field_values: fields,
    })
}

/// `outer@7` holds the object in a local (the definition); the spliced
/// `callee@3` received it as its argument (a reference).
fn shared_object_chain() -> FrameState {
    let outer = scope(
        "T.outer:()V",
        7,
        vec![vobj(4, vec![FrameValue::Int(9)])],
        None,
    );
    scope(
        "P.callee:(LP;)I",
        3,
        vec![FrameValue::VirtualObjectRef(4)],
        Some(outer),
    )
}

#[test]
fn a_chain_naming_one_object_in_two_scopes_is_published_only_with_the_switch() {
    let fs = shared_object_chain();
    assert_eq!(inlined_chain_refusal_with(&fs, || true), None);
    assert!(inlined_chain_refusal_with(&fs, || false).is_some());
    // The switch is not read for a chain that names no virtual object.
    let plain = scope(
        "P.callee:(I)I",
        3,
        vec![FrameValue::Int(1)],
        Some(scope("T.outer:()V", 7, vec![FrameValue::Int(2)], None)),
    );
    assert_eq!(
        inlined_chain_refusal_with(&plain, || panic!("the switch was read")),
        None
    );
}

#[test]
fn a_chain_virtual_graph_the_materialiser_cannot_build_is_refused() {
    let on = || true;
    // A reference no scope defines.
    let dangling = scope(
        "P.callee:(LP;)I",
        3,
        vec![FrameValue::VirtualObjectRef(5)],
        Some(scope("T.outer:()V", 7, vec![FrameValue::Int(0)], None)),
    );
    assert!(inlined_chain_refusal_with(&dangling, on).is_some());
    // One id defined in two scopes.
    let twice = scope(
        "P.callee:(LP;)I",
        3,
        vec![vobj(4, Vec::new())],
        Some(scope("T.outer:()V", 7, vec![vobj(4, Vec::new())], None)),
    );
    assert!(inlined_chain_refusal_with(&twice, on).is_some());
    // A field the materialiser cannot store.
    let bad_field = scope(
        "P.callee:(LP;)I",
        3,
        vec![vobj(4, vec![FrameValue::Unsupported])],
        Some(scope("T.outer:()V", 7, Vec::new(), None)),
    );
    assert!(inlined_chain_refusal_with(&bad_field, on).is_some());
    // A reference array (not a primitive atype).
    let mut ref_array = shared_object_chain();
    if let Some(outer) = ref_array.caller.as_deref_mut() {
        outer.locals[0] = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: Some(1),
            id: 4,
            class_id: 0,
            num_fields: 0,
            field_values: Vec::new(),
        });
    }
    assert!(inlined_chain_refusal_with(&ref_array, on).is_some());
    // Any monitor: a failed re-allocation would abandon a held lock.
    let mut locked = shared_object_chain();
    if let Some(outer) = locked.caller.as_deref_mut() {
        outer.monitors.push(MonitorInfo {
            object: FrameValue::StackSlotRef(-24),
            lock_depth: 1,
            relock: false,
        });
    }
    assert!(inlined_chain_refusal_with(&locked, on).is_some());
}

/// The in-place OSR transfers cannot materialise: OSR admission refuses a
/// chain point naming a virtual object, and still admits a FLAT frame
/// naming one (the single-frame builder materialises it). The
/// method-entry question (`frame_state_is_resumable`) is not narrowed:
/// the chain builder re-allocates the object.
#[test]
fn osr_admission_refuses_a_virtual_in_a_chain_only() {
    let fs = shared_object_chain();
    assert!(frame_state_is_resumable(&fs));
    let slot = first_unresumable_slot(&fs).expect("the chain's virtual is named");
    assert!(slot.contains("local 0"), "{slot}");
    assert!(first_unresumable_local(&fs).is_some());
    let flat = scope(
        "T.outer:()V",
        7,
        vec![vobj(4, vec![FrameValue::Int(9)])],
        None,
    );
    assert!(frame_state_is_resumable(&flat));
    assert_eq!(first_unresumable_slot(&flat), None);
    assert_eq!(first_unresumable_local(&flat), None);
}

fn point(fs: FrameState) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x40,
        bci: fs.bci,
        reason: DeoptReason::NullCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: fs,
        semantics: ResumeSemantics::for_reason(DeoptReason::NullCheck),
    }
}

/// The install verifier reads ids chain-wide: a reference to an object
/// another scope defines is well formed, a reference nothing defines is
/// not, and a second definition in another scope is a duplicate.
#[test]
fn the_verifier_reads_virtual_ids_chain_wide() {
    let v = DeoptVerifier::new();
    let undefined = |errs: &[DeoptMetadataError]| {
        errs.iter()
            .any(|e| matches!(e, DeoptMetadataError::UndefinedVirtualObjectRef { .. }))
    };
    let ok = v.violations(&[point(shared_object_chain())]);
    assert!(!undefined(&ok), "{ok:?}");
    let dangling = scope(
        "P.callee:(LP;)I",
        3,
        vec![FrameValue::VirtualObjectRef(5)],
        Some(scope("T.outer:()V", 7, vec![vobj(4, Vec::new())], None)),
    );
    assert!(undefined(&v.violations(&[point(dangling)])));
    let twice = scope(
        "P.callee:(LP;)I",
        3,
        vec![vobj(4, Vec::new())],
        Some(scope("T.outer:()V", 7, vec![vobj(4, Vec::new())], None)),
    );
    let errs = v.violations(&[point(twice)]);
    assert!(
        errs.iter().any(|e| matches!(
            e,
            DeoptMetadataError::DuplicateVirtualObjectDefinition { id: 4, .. }
        )),
        "{errs:?}"
    );
}
