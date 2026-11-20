use super::*;

fn cached(num_params: u16) -> Arc<CachedBytecodeMethod> {
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(7),
            class_name: Arc::from("p/Target"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("(II)V"),
            source_file: None,
            code: Arc::from(vec![0xB1u8].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 0,
            max_locals: 3,
            num_params,
            is_synchronized: false,
            is_static: false,
        },
    ))
}

#[test]
fn receiver_guard_covers_guarded_kinds_only() {
    let virtual_bytecode = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id: ClassId::new(42),
        cached: cached(2),
        gate: RedefineGate::never_stale(),
    };
    assert_eq!(
        receiver_guard_of(&virtual_bytecode),
        Some((ClassId::new(42), 2))
    );
    // `Bytecode` serves private / special targets and carries no guard.
    let bytecode = CachedInvokeTarget::Bytecode {
        cached: cached(2),
        gate: RedefineGate::never_stale(),
    };
    assert_eq!(receiver_guard_of(&bytecode), None);
    let kind = cratonvm_native_api::InterpIntrinsic::ThreadOnSpinWait;
    let intrinsic = |receiver_class_id| CachedInvokeTarget::Intrinsic {
        kind,
        callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
        num_params: 1,
        param_descs: Arc::from(Vec::<Arc<str>>::new()),
        return_type: b'V',
        receiver_class_id,
        gate: RedefineGate::never_stale(),
        owner: ClassId::new(7),
    };
    assert_eq!(
        receiver_guard_of(&intrinsic(Some(ClassId::new(9)))),
        Some((ClassId::new(9), 1))
    );
    // A static intrinsic has no receiver to compare.
    assert_eq!(receiver_guard_of(&intrinsic(None)), None);
}

/// What a late decline of the virtual door hands the general dispatcher
/// (wave 8): a served poly entry with its own gate, the primary entry
/// without its gate only at generation 0, and nothing once an inline
/// compile attempt may have safepointed.
#[test]
fn a_late_door_decline_hands_over_the_accepted_entry() {
    let receiver = ClassId::new(42);
    assert!(matches!(
        late_decline_probe(receiver, cached(2), None, 0, false),
        DoorProbe::Primary { receiver_class_id, .. } if receiver_class_id == receiver
    ));
    assert!(matches!(
        late_decline_probe(receiver, cached(2), None, 1, false),
        DoorProbe::NotProbed
    ));
    assert!(matches!(
        late_decline_probe(
            receiver,
            cached(2),
            Some(RedefineGate::never_stale()),
            0,
            false
        ),
        DoorProbe::Found(CachedInvokeTarget::VirtualBytecode { receiver_class_id, .. })
            if receiver_class_id == receiver
    ));
    assert!(matches!(
        late_decline_probe(
            receiver,
            cached(2),
            Some(RedefineGate::never_stale()),
            0,
            true
        ),
        DoorProbe::NotProbed
    ));
    // The placeholder a `Primary` target carries is never stale and has
    // the only generation a door hands over.
    let placeholder = DEFERRED_DOOR_GATE.with(RedefineGate::clone);
    assert_eq!(placeholder.generation, 0);
    assert!(!placeholder.is_stale());
}

/// Why the cached `Bytecode` / `Native` arms of
/// `execute_invokevirtual_cached` defer every receiver slot that does not
/// PEEK as a non-null object: an `Int(0)`-as-null slot peeks as `Int(0)`,
/// which the old `Object(None)` test let through, and then pops as a null
/// reference under the `L` tag the arms pop the receiver with.
#[test]
fn an_int_zero_receiver_slot_peeks_as_int_but_pops_as_null() {
    let mut stack = crate::runtime::ValueStack::new(4);
    stack.push_compact(CompactValue::int(0));
    assert!(!matches!(stack.peek_at(0), Value::Object(None)));
    assert!(!matches!(stack.peek_at(0), Value::Object(Some(_))));
    assert!(matches!(
        stack.pop_arg_for_descriptor_checked(b'L'),
        Ok(Value::Object(None))
    ));
}
