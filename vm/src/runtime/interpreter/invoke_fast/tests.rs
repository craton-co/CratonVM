use super::*;

fn facts_of(descriptor: &str) -> cratonvm_jit_api::DescriptorFacts {
    cratonvm_jit_api::DescriptorFacts::of(descriptor)
}

fn stack_with(values: &[(CompactValue, u8)]) -> crate::runtime::ValueStack {
    let mut s = crate::runtime::ValueStack::new(values.len() + 8);
    for (cv, kind) in values {
        match *kind {
            crate::runtime::ValueStack::KIND_MARK_LONG => s.push_compact_long(*cv),
            crate::runtime::ValueStack::KIND_MARK_DOUBLE => s.push_compact_double(*cv),
            _ => s.push_compact(*cv),
        }
    }
    s
}

/// The tag walk must line up with `ParamTags::get_with_receiver`: slot 0
/// is the receiver when there is one, and the descriptor's tags follow.
#[test]
fn the_receiver_shifts_the_parameter_tags_by_one() {
    let facts = facts_of("(IJ)V");
    let long_bits = CompactValue::long(7);
    let stack = stack_with(&[
        (CompactValue::null(), 0),
        (CompactValue::int(1), 0),
        (long_bits, crate::runtime::ValueStack::KIND_MARK_LONG),
    ]);
    let mut slots = empty_arg_slots();
    assert!(read_args_verbatim(&stack, &facts, 3, true, &mut slots));
    assert_eq!(slots.filled()[0].1, b'L', "receiver");
    assert_eq!(slots.filled()[1].1, b'I');
    assert_eq!(slots.filled()[2].1, b'J');
    // Without a receiver the same stack is an (I, J) static call.
    let stack = stack_with(&[
        (CompactValue::int(1), 0),
        (long_bits, crate::runtime::ValueStack::KIND_MARK_LONG),
    ]);
    let mut slots = empty_arg_slots();
    assert!(read_args_verbatim(&stack, &facts, 2, false, &mut slots));
    assert_eq!(slots.filled()[0].1, b'I');
    assert_eq!(slots.filled()[1].1, b'J');
}

/// `ArgSlots` starts uninitialized (wave 27): `filled()` must expose
/// exactly the prefix the last successful read wrote, nothing before the
/// first read, and nothing after a failed one -- even a failed re-read
/// of slots a successful read had filled (the virtual door re-reads after
/// an inline compile).
#[test]
fn filled_is_the_prefix_of_the_last_successful_read() {
    let mut slots = empty_arg_slots();
    assert!(slots.filled().is_empty(), "nothing read yet");
    assert!(slots.receiver_ptr().is_none());
    let facts = facts_of("(IJ)V");
    let stack = stack_with(&[
        (CompactValue::int(1), 0),
        (
            CompactValue::long(2),
            crate::runtime::ValueStack::KIND_MARK_LONG,
        ),
    ]);
    assert!(read_args_verbatim(&stack, &facts, 2, false, &mut slots));
    assert_eq!(slots.filled().len(), 2);
    assert_eq!(slots.filled()[0].0.as_int(), Some(1));
    // The same stack read as `(II)V` fails on the second slot, after the
    // first was rewritten: the failure must leave no claim behind.
    let facts = facts_of("(II)V");
    assert!(!read_args_verbatim(&stack, &facts, 2, false, &mut slots));
    assert!(slots.filled().is_empty(), "a failed read exposes nothing");
    // A read longer than the array is refused before any write.
    let facts = facts_of("(IIIIIIII)V"); // eight: the inline maximum
    let stack = stack_with(&[(CompactValue::int(3), 0); 10]);
    assert!(!read_args_verbatim(&stack, &facts, 10, true, &mut slots));
    assert!(slots.filled().is_empty());
    // The maximum is accepted: receiver plus eight.
    assert!(
        !read_args_verbatim(&stack, &facts, 9, true, &mut slots),
        "an int is no receiver"
    );
    let mut s = crate::runtime::ValueStack::new(16);
    s.push_compact(CompactValue::null());
    for _ in 0..8 {
        s.push_compact(CompactValue::int(4));
    }
    assert!(read_args_verbatim(&s, &facts, 9, true, &mut slots));
    assert_eq!(slots.filled().len(), 9);
    assert!(
        slots.receiver_ptr().is_none(),
        "a null receiver has no address"
    );
}

/// A category-2 argument whose slot carries no kind mark is exactly the
/// shape `pop_arg_for_descriptor_checked` exists to disambiguate, and the
/// door must decline it rather than guess.
#[test]
fn an_unmarked_category_two_argument_is_declined() {
    let facts = facts_of("(J)V");
    let stack = stack_with(&[(CompactValue::long(7), 0)]); // pushed unmarked
    let mut slots = empty_arg_slots();
    assert!(!read_args_verbatim(&stack, &facts, 1, false, &mut slots));
    // Marked, it is accepted.
    let stack = stack_with(&[(
        CompactValue::long(7),
        crate::runtime::ValueStack::KIND_MARK_LONG,
    )]);
    let mut slots = empty_arg_slots();
    assert!(read_args_verbatim(&stack, &facts, 1, false, &mut slots));
}

/// An `int` where the descriptor says reference (the `Int(0)`-as-null
/// shape) is declined, and so is a reference where it says `int`.
#[test]
fn a_slot_that_disagrees_with_the_descriptor_is_declined() {
    let facts = facts_of("(Ljava/lang/Object;)V");
    let stack = stack_with(&[(CompactValue::int(0), 0)]);
    let mut slots = empty_arg_slots();
    assert!(!read_args_verbatim(&stack, &facts, 1, false, &mut slots));

    let facts = facts_of("(I)V");
    let stack = stack_with(&[(CompactValue::null(), 0)]);
    let mut slots = empty_arg_slots();
    assert!(!read_args_verbatim(&stack, &facts, 1, false, &mut slots));
}

/// Reading never pops: a decline must leave the stack exactly as it was
/// for the general dispatcher.
#[test]
fn reading_arguments_does_not_disturb_the_stack() {
    let facts = facts_of("(IJ)V");
    let stack = stack_with(&[(CompactValue::int(1), 0), (CompactValue::long(2), 0)]);
    let before = stack.len();
    let mut slots = empty_arg_slots();
    assert!(!read_args_verbatim(&stack, &facts, 2, false, &mut slots));
    assert_eq!(stack.len(), before);
}

/// A descriptor with more parameters than `DescriptorFacts` keeps inline
/// has no tag array to walk, so every door declines it.
#[test]
fn an_overflowing_descriptor_is_declined() {
    let facts = facts_of("(IIIIIIIII)V"); // nine
    assert!(facts.param_tags_overflow);
    let stack = stack_with(&[(CompactValue::int(1), 0); 9]);
    let mut slots = empty_arg_slots();
    assert!(!read_args_verbatim(&stack, &facts, 9, false, &mut slots));
}

/// A `(J)V` callee entry the doors treat as plain bytecode (the general
/// path has answered `force_native_cache` with `false`).
fn long_taking_callee(declaring: ClassId, is_static: bool) -> Arc<CachedBytecodeMethod> {
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("p/DoorCountTarget"),
            method_name: Arc::from("take"),
            method_descriptor: Arc::from("(J)V"),
            source_file: None,
            // `nop; return`: not a body that returns at pc 0, which the
            // special door serves without its tier-up bookkeeping.
            code: Arc::from(vec![0x00u8, 0xB1].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 0,
            max_locals: if is_static { 2 } else { 3 },
            num_params: 1,
            is_synchronized: false,
            is_static,
        },
    ));
    let _ = cached.force_native_cache.set(false);
    cached
}

fn caller_thread(caller: ClassId) -> JvmThread {
    let mut thread =
        JvmThread::new(crate::threading::jvm_thread::ThreadId(4_848), "door-count");
    thread.frames.push(Frame::new(
        caller,
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    thread
}

/// The virtual door for an `invokevirtual` at constant-pool index 5, pc 0,
/// of the caller in frame 0, with no quickened-field heap.
fn virtual_door_at_cp5(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let mut probe = DoorProbe::NotProbed;
    // The hand-off flag as the dispatch loop hoists it under the door's
    // admission (`nonvirtual_fast_door_on`).
    let handoff_on = !crate::runtime::env_cache::no_nonvirtual_fast_door();
    execute_invokevirtual_fast_door(
        shared, thread, 0, 5, false, handoff_on, None, 0, &mut probe,
    )
}

/// Wave 8: a call the virtual door declines because an argument slot needs
/// coercion must not have been counted by the door first -- the general
/// path counts it again, and the callee reached its compile threshold at
/// half the calls. The unmarked `long` is the shape: it declines, and the
/// callee's counter must not move.
#[test]
fn the_virtual_door_does_not_count_a_call_it_declines_for_coercion() {
    use std::sync::atomic::Ordering;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // High enough that no other VM in the test process is likely to have
    // defined a `java.util` class at this id (the bitmap is process-wide).
    let receiver_class = ClassId::new(987_001);
    let caller = ClassId::new(987_002);
    let cached = long_taking_callee(receiver_class, false);
    let entry = || CachedInvokeTarget::VirtualBytecode {
        receiver_class_id: receiver_class,
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };
    let receiver = shared.mem.heap.alloc_object(receiver_class, 0);

    // A marked long: the door serves the call, and counts it once.
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(caller, 5, false, 0, entry());
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = virtual_door_at_cp5(&shared, &mut thread);
    if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
        // Declined for a reason of this stripped VM or of the test
        // process (a redefinition, a tier-up switch): nothing to compare.
        return;
    }
    let counted = cached.interp_invocations.load(Ordering::Relaxed);
    if counted == 0 {
        // The door's tier-up block did not run in this process.
        return;
    }
    assert_eq!(counted, 1);

    // An unmarked long: declined, uncounted, stack untouched.
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(caller, 5, false, 0, entry());
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    thread.frames[0].stack.push_compact(CompactValue::long(7));
    let declined = virtual_door_at_cp5(&shared, &mut thread);
    assert!(declined.is_none(), "an unmarked long needs coercion");
    assert_eq!(
        cached.interp_invocations.load(Ordering::Relaxed),
        1,
        "the declined call is the general path's to count"
    );
    assert_eq!(thread.frames[0].stack.len(), 2, "nothing was popped");
}

/// The `invokestatic` door's twin of the test above: its argument read
/// now runs before `note_invocation_for_tierup`.
#[test]
fn the_static_door_does_not_count_a_call_it_declines_for_coercion() {
    use std::sync::atomic::Ordering;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_003);
    let caller = ClassId::new(987_004);
    let cached = long_taking_callee(declaring, true);
    let entry = || CachedInvokeTarget::Bytecode {
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };

    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(caller, 6, false, 0, entry());
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0);
    if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
        return;
    }
    let counted = cached.interp_invocations.load(Ordering::Relaxed);
    if counted == 0 {
        // `CRATONVM_DISABLE_JIT` is set for this process: nothing counts.
        return;
    }
    assert_eq!(counted, 1);

    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(caller, 6, false, 0, entry());
    thread.frames[0].stack.push_compact(CompactValue::long(7));
    assert!(
        execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0).is_none(),
        "an unmarked long needs coercion"
    );
    assert_eq!(
        cached.interp_invocations.load(Ordering::Relaxed),
        1,
        "the declined call is the general path's to count"
    );
    assert_eq!(thread.frames[0].stack.len(), 1, "nothing was popped");
}

/// A gate for a target class redefined once BEFORE the entry was filled:
/// generation 1, and not stale.
fn redefined_before_the_fill() -> RedefineGate {
    let gate = RedefineGate::snapshot(Arc::new(std::sync::atomic::AtomicU32::new(1)));
    assert!(!gate.is_stale());
    gate
}

/// Interpreter round i1 wave 18, lane L4: the static door serves an entry
/// whose target was redefined before it was filled — it used to decline
/// every such entry, and every call at all once any class anywhere had
/// been redefined. It serves it the way `execute_invokestatic_cached`
/// does: counted and, since wave 39 (lane L2,
/// [`REDEFINED_TARGETS_TIER_UP`]), probed for a compiled body and offered
/// to the tiered manager like any other target (the count is the same
/// either way).
#[test]
fn the_static_door_serves_a_target_redefined_before_the_fill() {
    use std::sync::atomic::Ordering;
    if !DOORS_SURVIVE_REDEFINITION || cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_011);
    let caller = ClassId::new(987_012);
    let cached = long_taking_callee(declaring, true);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        6,
        false,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: redefined_before_the_fill(),
        },
    );
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0);
    assert!(
        matches!(served, Some(Ok(CachedCallResult::FramePushed))),
        "a redefined target is served like any other"
    );
    let counted = cached.interp_invocations.load(Ordering::Relaxed);
    if crate::runtime::env_cache::disable_jit() {
        assert_eq!(counted, 0, "nothing counts under --nojit");
    } else {
        assert_eq!(
            counted, 1,
            "the general path counts a redefined target's calls"
        );
    }
}

/// The special door's twin: a `Bytecode` entry for a target redefined
/// before the fill is served and, since wave 39 (lane L2,
/// [`REDEFINED_TARGETS_TIER_UP`]), counted once like any other `Bytecode`
/// entry (`the_special_door_counts_a_bytecode_entry_once`); the general
/// `Bytecode` arm probes and offers it too. Under the kill switch it is
/// served uncounted, as before.
#[test]
fn the_special_door_serves_a_target_redefined_before_the_fill() {
    if !DOORS_SURVIVE_REDEFINITION || cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_013);
    let caller = ClassId::new(987_014);
    let cached = long_taking_callee(declaring, false);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        7,
        true,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: redefined_before_the_fill(),
        },
    );
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = execute_nonvirtual_fast_door(
        &shared,
        &mut thread,
        0,
        7,
        true,
        0,
        crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
        &mut DoorProbe::NotProbed,
    );
    assert!(
        matches!(served, Some(Ok(CachedCallResult::FramePushed))),
        "a redefined target is served like any other"
    );
    let expected = u32::from(
        SPECIAL_DOOR_TIERS_UP
            && REDEFINED_TARGETS_TIER_UP
            && !crate::runtime::env_cache::disable_jit(),
    );
    assert_eq!(
        cached
            .interp_invocations
            .load(std::sync::atomic::Ordering::Relaxed),
        expected,
        "a redefined target's call is counted like any other `Bytecode` entry's"
    );
}

/// The special door at constant-pool index 7, pc 0, of the caller in
/// frame 0, for a `(J)V` callee whose receiver and marked `long` argument
/// are pushed first.
fn special_door_call(
    shared: &SharedVm,
    receiver: ObjectRef,
    entry: CachedInvokeTarget,
    caller: ClassId,
) -> (
    JvmThread,
    Option<Result<CachedCallResult, MethodCallFailed>>,
) {
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(caller, 7, true, 0, entry);
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = execute_nonvirtual_fast_door(
        shared,
        &mut thread,
        0,
        7,
        true,
        0,
        crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
        &mut DoorProbe::NotProbed,
    );
    (thread, served)
}

/// Interpreter round i1 wave 19, lane L4: the special door counts a
/// `Bytecode` entry's call once, as the general `Bytecode` arm does since
/// `600311cbe` — it used to count nothing, so a hot private method, super
/// call or constructor it served never tiered up through this route.
#[test]
fn the_special_door_counts_a_bytecode_entry_once() {
    use std::sync::atomic::Ordering;
    if !SPECIAL_DOOR_TIERS_UP || cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_021);
    let caller = ClassId::new(987_022);
    let cached = long_taking_callee(declaring, false);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let entry = CachedInvokeTarget::Bytecode {
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };
    let (_thread, served) = special_door_call(&shared, receiver, entry, caller);
    if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
        // Declined for a reason of this stripped VM or of the test
        // process (a loader-split memo, a tier-up switch).
        return;
    }
    let expected = u32::from(!crate::runtime::env_cache::disable_jit());
    assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), expected);
}

/// The general arm's tier-up chain refuses `is_special` for a
/// `VirtualBytecode` entry, so the door leaves that entry uncounted.
#[test]
fn the_special_door_does_not_count_a_virtual_bytecode_entry() {
    use std::sync::atomic::Ordering;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_023);
    let caller = ClassId::new(987_024);
    let cached = long_taking_callee(declaring, false);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let entry = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id: declaring,
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };
    let (_thread, served) = special_door_call(&shared, receiver, entry, caller);
    if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
        return;
    }
    assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
}

/// A `Bytecode` callee with a published compiled body is declined, with
/// the operand stack untouched and the call uncounted: the general arm
/// enters the body (`execute_jit_call_decoded`). The door used to run it
/// interpreted.
#[test]
fn the_special_door_declines_a_callee_with_a_compiled_body() {
    use std::sync::atomic::Ordering;
    if !SPECIAL_DOOR_TIERS_UP
        || crate::runtime::env_cache::disable_jit()
        || cratonvm_jit_api::descriptor_facts_disabled()
    {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_025);
    let caller = ClassId::new(987_026);
    let cached = long_taking_callee(declaring, false);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // ret
    shared.jit.jit_cache.write().put(
        Arc::clone(&cached.class_name),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
        declaring,
        cratonvm_jit::CompiledMethod::new(buf),
    );
    let entry = CachedInvokeTarget::Bytecode {
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };
    let (thread, served) = special_door_call(&shared, receiver, entry, caller);
    assert!(served.is_none(), "the general arm enters the compiled body");
    assert_eq!(thread.frames.len(), 1, "no frame was pushed");
    assert_eq!(thread.frames[0].stack.len(), 2, "nothing was popped");
    assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
}

/// A body that returns at pc 0 (`Object.<init>`'s shape) is served
/// interpreted and uncounted even with a compiled body published: the
/// door keeps the wave-18 behaviour for it.
#[test]
fn the_special_door_serves_an_empty_body_without_tier_up() {
    use std::sync::atomic::Ordering;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_029);
    let caller = ClassId::new(987_030);
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("p/DoorEmptyTarget"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("(J)V"),
            source_file: None,
            code: Arc::from(vec![0xB1u8, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 0,
            max_locals: 3,
            num_params: 1,
            is_synchronized: false,
            is_static: false,
        },
    ));
    let _ = cached.force_native_cache.set(false);
    assert!(body_returns_at_entry(&cached));
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // ret
    shared.jit.jit_cache.write().put(
        Arc::clone(&cached.class_name),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
        declaring,
        cratonvm_jit::CompiledMethod::new(buf),
    );
    let entry = CachedInvokeTarget::Bytecode {
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };
    let (_thread, served) = special_door_call(&shared, receiver, entry, caller);
    // Served with a frame, or (wave 20, `EMPTY_BODY_ELISION`) without one.
    if !matches!(
        served,
        Some(Ok(CachedCallResult::FramePushed | CachedCallResult::Handled))
    ) {
        return;
    }
    assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
}

/// The virtual door's hand-off of a private `invokevirtual` (a `Bytecode`
/// entry) counts the call as the special door does.
#[test]
fn the_virtual_door_hand_off_counts_a_private_callee() {
    use std::sync::atomic::Ordering;
    if !SPECIAL_DOOR_TIERS_UP
        || crate::runtime::env_cache::no_nonvirtual_fast_door()
        || cratonvm_jit_api::descriptor_facts_disabled()
    {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_027);
    let caller = ClassId::new(987_028);
    let cached = long_taking_callee(declaring, false);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        5,
        false,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        },
    );
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = virtual_door_at_cp5(&shared, &mut thread);
    if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
        return;
    }
    let expected = u32::from(!crate::runtime::env_cache::disable_jit());
    assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), expected);
}

/// The virtual door's twin, which reads the generation itself: a
/// `VirtualBytecode` entry for a receiver's method redefined before the
/// fill is served and, since wave 40 (lane L2,
/// [`REDEFINED_TARGETS_TIER_UP`]), counted exactly as the same entry of a
/// class never redefined is (the door's tier-up block may not run in a
/// stripped VM or under a tier-up switch; then neither is counted). Under
/// the kill switch it is served uncounted, as before.
#[test]
fn the_virtual_door_serves_a_target_redefined_before_the_fill() {
    use std::sync::atomic::Ordering;
    if !DOORS_SURVIVE_REDEFINITION || cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let receiver_class = ClassId::new(987_015);
    let caller = ClassId::new(987_016);
    // One call at the door with `gate`; the served result and the callee's
    // count.
    let call = |gate: RedefineGate| {
        let cached = long_taking_callee(receiver_class, false);
        let receiver = shared.mem.heap.alloc_object(receiver_class, 0);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            5,
            false,
            0,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: receiver_class,
                cached: Arc::clone(&cached),
                gate,
            },
        );
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = virtual_door_at_cp5(&shared, &mut thread);
        (
            matches!(served, Some(Ok(CachedCallResult::FramePushed))),
            cached.interp_invocations.load(Ordering::Relaxed),
        )
    };
    let (plain_served, plain_counted) = call(RedefineGate::never_stale());
    let (served, counted) = call(redefined_before_the_fill());
    if !plain_served {
        // Declined for a reason of this stripped VM or of the test
        // process: nothing to compare.
        return;
    }
    assert!(served, "a redefined target is served like any other");
    if REDEFINED_TARGETS_TIER_UP {
        assert_eq!(
            counted, plain_counted,
            "a redefined target's call is counted like any other"
        );
    } else {
        assert_eq!(counted, 0);
    }
}

/// A `(J)V` instance method whose body is a lone `return`, in the padded
/// shape the caches hold.
fn empty_callee(declaring: ClassId, is_synchronized: bool) -> Arc<CachedBytecodeMethod> {
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("p/DoorEmptyTarget"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("(J)V"),
            source_file: None,
            code: Arc::from(vec![0xB1u8, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 0,
            max_locals: 3,
            num_params: 1,
            is_synchronized,
            is_static: false,
        },
    ));
    let _ = cached.force_native_cache.set(false);
    cached
}

/// Interpreter round i1 wave 20, lane L3: the special door answers a call
/// of an empty body by popping its arguments — no frame is pushed, the
/// call is uncounted, and the caller continues (`Handled`).
#[test]
fn the_special_door_calls_an_empty_body_without_a_frame() {
    use std::sync::atomic::Ordering;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_033);
    let caller = ClassId::new(987_034);
    let cached = empty_callee(declaring, false);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let entry = CachedInvokeTarget::Bytecode {
        cached: Arc::clone(&cached),
        gate: RedefineGate::never_stale(),
    };
    let (thread, served) = special_door_call(&shared, receiver, entry, caller);
    if served.is_none() {
        // Declined for a reason of this stripped VM or of the test
        // process (a loader-split memo): the general path owns the call.
        return;
    }
    if empty_body_elidable(&shared, &cached) {
        assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
        assert_eq!(thread.frames.len(), 1, "no frame was pushed");
        assert_eq!(
            thread.frames[0].stack.len(),
            0,
            "the receiver and the argument were popped"
        );
    } else {
        assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
        assert_eq!(thread.frames.len(), 2);
    }
    assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
}

/// A `synchronized` empty body keeps its frame: the monitor enter and
/// exit are what the call does. A plain one is elidable in a VM with no
/// agent and no debugger.
#[test]
fn only_an_unobserved_unsynchronized_empty_body_is_elidable() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let plain = empty_callee(ClassId::new(987_035), false);
    let synchronized = empty_callee(ClassId::new(987_036), true);
    assert!(body_returns_at_entry(&plain) && body_returns_at_entry(&synchronized));
    assert!(!empty_body_elidable(&shared, &synchronized));
    assert_eq!(empty_body_elidable(&shared, &plain), EMPTY_BODY_ELISION);
}

/// A `(J)V` STATIC method whose body is a lone `return`.
fn empty_static_callee(declaring: ClassId) -> Arc<CachedBytecodeMethod> {
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("p/DoorEmptyStatic"),
            method_name: Arc::from("hook"),
            method_descriptor: Arc::from("(J)V"),
            source_file: None,
            code: Arc::from(vec![0xB1u8, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 0,
            max_locals: 2,
            num_params: 1,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let _ = cached.force_native_cache.set(false);
    cached
}

/// Interpreter round i1 wave 22, lane L4 (`STATIC_DOOR_ELIDES_EMPTY_BODY`):
/// the static door answers an empty static body by popping its argument,
/// uncounted, with no frame.
#[test]
fn the_static_door_calls_an_empty_body_without_a_frame() {
    use std::sync::atomic::Ordering;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_045);
    let caller = ClassId::new(987_046);
    let cached = empty_static_callee(declaring);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        6,
        false,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        },
    );
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0);
    if served.is_none() {
        // Declined for a reason of this stripped VM (a loader-split
        // memo): the general path owns the call.
        return;
    }
    if STATIC_DOOR_ELIDES_EMPTY_BODY && empty_body_elidable(&shared, &cached) {
        assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
        assert_eq!(thread.frames.len(), 1, "no frame was pushed");
        assert_eq!(thread.frames[0].stack.len(), 0, "the argument was popped");
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
    } else {
        assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
        assert_eq!(thread.frames.len(), 2);
    }
}

/// Interpreter round i1 wave 22, lane L4 (`VIRTUAL_DOOR_ELIDES_EMPTY_BODY`):
/// the virtual door answers a monomorphic call of an empty body by
/// popping the receiver and the argument, uncounted, with no frame.
#[test]
fn the_virtual_door_calls_an_empty_body_without_a_frame() {
    use std::sync::atomic::Ordering;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let receiver_class = ClassId::new(987_047);
    let caller = ClassId::new(987_048);
    let cached = empty_callee(receiver_class, false);
    let receiver = shared.mem.heap.alloc_object(receiver_class, 0);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        5,
        false,
        0,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: receiver_class,
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        },
    );
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    thread.frames[0]
        .stack
        .push_compact_long(CompactValue::long(7));
    let served = virtual_door_at_cp5(&shared, &mut thread);
    if served.is_none() {
        // Declined for a reason of this stripped VM or of the test
        // process: the general path owns the call.
        return;
    }
    if VIRTUAL_DOOR_ELIDES_EMPTY_BODY && empty_body_elidable(&shared, &cached) {
        assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
        assert_eq!(thread.frames.len(), 1, "no frame was pushed");
        assert_eq!(
            thread.frames[0].stack.len(),
            0,
            "the receiver and the argument were popped"
        );
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
    } else {
        assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
        assert_eq!(thread.frames.len(), 2);
    }
}

/// One more `int` than the receiver leaves room for in the compiled-code
/// ABI, so the doors decline the call (see [`nine_argument_callee`]).
const WIDE_JAVA_PARAMS: usize = crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS;

/// An instance method taking [`WIDE_JAVA_PARAMS`] `int`s: with the receiver
/// that is one argument more than the compiled-code ABI passes
/// (`JIT_ABI_MAX_JAVA_ARGS`, sixteen on x86-64 since round 12 wave 2), so `execute_jit_call_decoded` declines it
/// before entering any body and the `Bytecode` arm pushes the interpreted
/// frame. That lets a test drive the arm's compiled branch with a dummy
/// body that never runs.
fn nine_argument_callee(declaring: ClassId) -> Arc<CachedBytecodeMethod> {
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("p/WideSpecialTarget"),
            method_name: Arc::from("wide"),
            method_descriptor: Arc::from(
                format!("({})V", "I".repeat(WIDE_JAVA_PARAMS)).as_str(),
            ),
            source_file: None,
            code: Arc::from(vec![0x00u8, 0xB1].as_slice()),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 0,
            max_locals: (WIDE_JAVA_PARAMS + 1) as _,
            num_params: WIDE_JAVA_PARAMS as _,
            is_synchronized: false,
            is_static: false,
        },
    ));
    let _ = cached.force_native_cache.set(false);
    cached
}

/// A caller frame with the receiver and [`WIDE_JAVA_PARAMS`] `int`s on its stack.
fn nine_argument_caller(caller: ClassId, receiver: ObjectRef) -> JvmThread {
    let mut thread =
        JvmThread::new(crate::threading::jvm_thread::ThreadId(4_849), "special-jit");
    thread.frames.push(Frame::new(
        caller,
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        32,
        4,
        &[],
    ));
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("a 32-slot stack takes the receiver");
    for i in 0..WIDE_JAVA_PARAMS as i32 {
        thread.frames[0]
            .stack
            .push_int(i)
            .expect("a 32-slot stack takes the ints");
    }
    thread
}

/// A `ret` stub to publish as a compiled body; never entered by these
/// tests (see [`nine_argument_callee`]).
fn dummy_body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // ret
    cratonvm_jit::CompiledMethod::new(buf)
}

/// Interpreter round i1 wave 20, lane L3 (`NONVIRTUAL_ARM_UPGRADES_TO_JIT`):
/// a `Jit` entry under a special key is served by the general dispatcher's
/// `Bytecode` arm with the entry's body in hand. The `Jit` arm used to
/// answer `CacheMiss`, sending every such call to `execute_invoke`. The
/// special door declines the entry on its kind, the stack untouched.
#[test]
fn a_special_jit_entry_is_served_by_the_bytecode_arm() {
    if !NONVIRTUAL_ARM_UPGRADES_TO_JIT {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_037);
    let caller = ClassId::new(987_038);
    let cached = nine_argument_callee(declaring);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let compiled = cratonvm_jit::RetainedCode::new(Arc::new(dummy_body()));
    let needs_heap = compiled.needs_heap();
    let mut thread = nine_argument_caller(caller, receiver);
    thread.invoke_cache.put(
        caller,
        7,
        true,
        0,
        CachedInvokeTarget::Jit {
            compiled,
            num_params: WIDE_JAVA_PARAMS as _,
            return_type: b'V',
            needs_heap,
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
            supersede_epoch: shared.jit.supersede_gate(),
        },
    );
    assert!(
        execute_nonvirtual_fast_door(
            &shared,
            &mut thread,
            0,
            7,
            true,
            0,
            crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
            &mut DoorProbe::NotProbed
        )
        .is_none(),
        "the door declines a compiled entry on its kind"
    );
    assert_eq!(
        thread.frames[0].stack.len(),
        WIDE_JAVA_PARAMS + 1,
        "nothing was popped"
    );
    let outcome = execute_invokevirtual_cached(&shared, &mut thread, 0, 7, 0, true, false);
    assert!(
        matches!(outcome, Ok(CachedCallResult::FramePushed)),
        "the arm runs the call: {outcome:?}"
    );
    assert_eq!(thread.frames.len(), 2, "the declined body ran interpreted");
    assert_eq!(thread.frames[0].stack.len(), 0);
}

/// Interpreter round i1 wave 22, lane L4 (`SPECIAL_DOOR_HANDS_OVER_ITS_PROBE`):
/// the special door hands the `Jit` entry it declined to the general
/// dispatcher, which serves the call from it without probing the key.
/// The key is evicted between the two, so a second probe would miss.
#[test]
fn the_special_door_hands_a_declined_entry_to_the_general_dispatcher() {
    if !NONVIRTUAL_ARM_UPGRADES_TO_JIT || !SPECIAL_DOOR_HANDS_OVER_ITS_PROBE {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_043);
    let caller = ClassId::new(987_044);
    let cached = nine_argument_callee(declaring);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let compiled = cratonvm_jit::RetainedCode::new(Arc::new(dummy_body()));
    let needs_heap = compiled.needs_heap();
    let mut thread = nine_argument_caller(caller, receiver);
    thread.invoke_cache.put(
        caller,
        7,
        true,
        0,
        CachedInvokeTarget::Jit {
            compiled,
            num_params: WIDE_JAVA_PARAMS as _,
            return_type: b'V',
            needs_heap,
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
            supersede_epoch: shared.jit.supersede_gate(),
        },
    );
    let mut probe = DoorProbe::NotProbed;
    assert!(
        execute_nonvirtual_fast_door(&shared, &mut thread, 0, 7, true, 0, None, &mut probe)
            .is_none(),
        "the door declines a compiled entry on its kind"
    );
    assert!(
        matches!(probe, DoorProbe::Found(CachedInvokeTarget::Jit { .. })),
        "the declined entry is handed over"
    );
    thread.invoke_cache.evict(caller, 7, true);
    let outcome =
        execute_invokevirtual_cached_probed(&shared, &mut thread, 0, 7, 0, true, false, probe);
    assert!(
        matches!(outcome, Ok(CachedCallResult::FramePushed)),
        "served from the handed-over entry: {outcome:?}"
    );

    // A miss is handed over as a miss: the general dispatcher answers
    // `CacheMiss` without probing.
    let mut thread = nine_argument_caller(caller, receiver);
    let mut probe = DoorProbe::NotProbed;
    assert!(execute_nonvirtual_fast_door(
        &shared,
        &mut thread,
        0,
        7,
        true,
        0,
        None,
        &mut probe
    )
    .is_none());
    assert!(matches!(probe, DoorProbe::Miss));
    assert!(matches!(
        execute_invokevirtual_cached_probed(&shared, &mut thread, 0, 7, 0, true, false, probe),
        Ok(CachedCallResult::CacheMiss)
    ));
    assert_eq!(
        thread.frames[0].stack.len(),
        WIDE_JAVA_PARAMS + 1,
        "nothing was popped"
    );
}

/// The `Bytecode` arm stores a compiled body it found for a special site
/// as the site's `Jit` entry, as the static arm does, so neither the door
/// nor the arm probes `JitCache` for it again.
#[test]
fn the_bytecode_arm_upgrades_a_special_site_whose_body_it_found() {
    if !NONVIRTUAL_ARM_UPGRADES_TO_JIT || crate::runtime::env_cache::disable_jit() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_039);
    let caller = ClassId::new(987_040);
    let cached = nine_argument_callee(declaring);
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    shared.jit.jit_cache.write().put(
        Arc::clone(&cached.class_name),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
        declaring,
        dummy_body(),
    );
    let mut thread = nine_argument_caller(caller, receiver);
    thread.invoke_cache.put(
        caller,
        7,
        true,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        },
    );
    let outcome = execute_invokevirtual_cached(&shared, &mut thread, 0, 7, 0, true, false);
    assert!(
        matches!(outcome, Ok(CachedCallResult::FramePushed)),
        "the arm runs the call: {outcome:?}"
    );
    assert!(
        matches!(
            thread.invoke_cache.get(caller, 7, true, 0),
            Some(CachedInvokeTarget::Jit { .. })
        ),
        "the site now holds the compiled entry"
    );
}

/// A private `invokevirtual` and an `invokestatic` of one `Methodref`
/// share a cache key, so the upgrade above can leave an INSTANCE `Jit`
/// entry where an `invokestatic` looks. The static arm refuses it before
/// popping (JVMS §6.5: the slow path raises the
/// `IncompatibleClassChangeError`), as its `Bytecode` arm refuses an
/// instance `Bytecode` entry.
#[test]
fn invokestatic_refuses_an_instance_jit_entry_before_popping() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_041);
    let caller = ClassId::new(987_042);
    let cached = nine_argument_callee(declaring);
    let compiled = cratonvm_jit::RetainedCode::new(Arc::new(dummy_body()));
    let needs_heap = compiled.needs_heap();
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        5,
        false,
        0,
        CachedInvokeTarget::Jit {
            compiled,
            num_params: WIDE_JAVA_PARAMS as _,
            return_type: b'V',
            needs_heap,
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
            supersede_epoch: shared.jit.supersede_gate(),
        },
    );
    thread.frames[0]
        .stack
        .push_int(3)
        .expect("an 8-slot stack takes one int");
    let outcome = execute_invokestatic_cached(&shared, &mut thread, 0, 5, 0);
    assert!(
        matches!(outcome, Ok(CachedCallResult::CacheMiss)),
        "declined: {outcome:?}"
    );
    assert_eq!(thread.frames[0].stack.len(), 1, "nothing was popped");
    assert!(
        thread.invoke_cache.get(caller, 5, false, 0).is_none(),
        "the wrong-kind entry is evicted"
    );
}

// ── Wave 25: the trivial constructor (`TRIVIAL_CTOR_ELISION`) ──────────

/// A `<init>()V` entry with `code` in the padded shape the caches hold.
/// (Until wave 28 this helper said so and built the UNPADDED body, which
/// is why the tests below passed while the real build never elided.)
fn ctor_entry(declaring: ClassId, code: &[u8]) -> Arc<CachedBytecodeMethod> {
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("w25/TrivialCtor"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        },
    ));
    let _ = cached.force_native_cache.set(false);
    cached
}

/// The shape test is the JIT's byte test plus the name and descriptor:
/// javac's default constructor is exactly `2a b7 hi lo b1`.
#[test]
fn only_the_default_constructor_bytes_have_the_trivial_shape() {
    let declaring = ClassId::new(987_101);
    let trivial: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
    assert!(body_is_trivial_ctor_shape(&ctor_entry(declaring, &trivial)));
    // A field store before the super call, a longer body, another opcode.
    assert!(!body_is_trivial_ctor_shape(&ctor_entry(
        declaring,
        &[0x2a, 0xb7, 0x00, 0x05, 0x00, 0xb1]
    )));
    assert!(!body_is_trivial_ctor_shape(&ctor_entry(
        declaring,
        &[0x2a, 0xb6, 0x00, 0x05, 0xb1]
    )));
    assert!(!body_is_trivial_ctor_shape(&ctor_entry(
        declaring,
        &[0x2b, 0xb7, 0x00, 0x05, 0xb1]
    )));
    // The same bytes under another name or descriptor are not a
    // constructor the rule speaks about.
    let mut other = CachedBytecodeMethod::clone(&ctor_entry(declaring, &trivial));
    other.method_name = Arc::from("init");
    assert!(!body_is_trivial_ctor_shape(&other));
    let mut other = CachedBytecodeMethod::clone(&ctor_entry(declaring, &trivial));
    other.method_descriptor = Arc::from("(I)V");
    assert!(!body_is_trivial_ctor_shape(&other));
}

/// Wave 28 regression test: the shape is read on the body the invoke
/// caches really hold -- `frame::padded_bytecode_for_method`, as
/// `populate_invoke_cache` builds it, two zero bytes longer than the class
/// file's -- and not on the class-file length. The wave-25 test compared
/// the padded length with 5, so on the real build no constructor ever had
/// the shape and the elision never engaged.
#[test]
fn the_trivial_shape_is_read_on_the_padded_body_the_caches_hold() {
    let declaring = ClassId::new(987_105);
    let class_file_body: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
    let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: declaring,
        class_name: Arc::from("w28/PaddedCtor"),
        method_name: Arc::from("<init>"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: crate::runtime::frame::padded_bytecode_for_method(
            declaring,
            "<init>",
            "()V",
            &class_file_body,
            &[],
        ),
        exception_table: Arc::from(vec![].as_slice()),
        max_stack: 1,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: false,
    });
    assert_eq!(
        cached.code.len(),
        class_file_body.len() + 2,
        "the cache pads"
    );
    assert!(body_is_trivial_ctor_shape(&cached));
}

/// A trivial-SHAPED constructor whose declaring class the class manager
/// cannot show (no constant pool to read) keeps its frame: the verdict
/// never rests on the bytes alone.
#[test]
fn the_special_door_keeps_the_frame_of_a_constructor_it_cannot_prove_trivial() {
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let declaring = ClassId::new(987_102);
    let caller = ClassId::new(987_103);
    let cached = ctor_entry(declaring, &[0x2a, 0xb7, 0x00, 0x05, 0xb1]);
    assert!(
        !trivial_object_ctor(&shared, &cached),
        "no class, no verdict"
    );
    let receiver = shared.mem.heap.alloc_object(declaring, 0);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        7,
        true,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        },
    );
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    let served = execute_nonvirtual_fast_door(
        &shared,
        &mut thread,
        0,
        7,
        true,
        0,
        crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
        &mut DoorProbe::NotProbed,
    );
    if served.is_none() {
        // Declined for a reason of this stripped VM (a loader-split memo).
        return;
    }
    assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
    assert_eq!(thread.frames.len(), 2, "the constructor's frame was pushed");
}

/// Class-file bytes for `this_name extends super_name` with one method,
/// `<init>()V` = `aload_0; invokespecial ctor_owner.<init>()V; return`.
/// Constant pool: #1 Class this, #3 Class super, #5 Methodref #6.#8,
/// #6 Class ctor_owner, #8 NameAndType <init> ()V, #11 "Code".
fn w25_class_bytes(this_name: &str, super_name: &str, ctor_owner: &str) -> Vec<u8> {
    fn utf8(b: &mut Vec<u8>, s: &str) {
        b.push(1);
        b.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        b.extend_from_slice(s.as_bytes());
    }
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
    b.extend_from_slice(&12u16.to_be_bytes()); // constant_pool_count
    b.extend_from_slice(&[7, 0x00, 0x02]); // #1 Class #2
    utf8(&mut b, this_name); // #2
    b.extend_from_slice(&[7, 0x00, 0x04]); // #3 Class #4
    utf8(&mut b, super_name); // #4
    b.extend_from_slice(&[10, 0x00, 0x06, 0x00, 0x08]); // #5 Methodref #6.#8
    b.extend_from_slice(&[7, 0x00, 0x07]); // #6 Class #7
    utf8(&mut b, ctor_owner); // #7
    b.extend_from_slice(&[12, 0x00, 0x09, 0x00, 0x0A]); // #8 NameAndType #9:#10
    utf8(&mut b, "<init>"); // #9
    utf8(&mut b, "()V"); // #10
    utf8(&mut b, "Code"); // #11
                          // public super, this #1, super #3, no interfaces, no fields.
    b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00]);
    b.extend_from_slice(&[0x00, 0x01]); // methods_count
    b.extend_from_slice(&[0x00, 0x01, 0x00, 0x09, 0x00, 0x0A, 0x00, 0x01]); // public <init>()V
    b.extend_from_slice(&[0x00, 0x0B]); // "Code"
    b.extend_from_slice(&17u32.to_be_bytes()); // attribute_length
    b.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // max_stack, max_locals
    b.extend_from_slice(&5u32.to_be_bytes()); // code_length
    b.extend_from_slice(&[0x2a, 0xb7, 0x00, 0x05, 0xb1]);
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no handlers, no attributes
    b.extend_from_slice(&[0x00, 0x00]); // no class attributes
    b
}

/// Wave 25: against classes the class manager really holds, the verdict
/// reads the constant pool -- a default constructor of a direct `Object`
/// subclass is trivial, one that calls another superclass constructor is
/// not -- and a proven trivial constructor is called without a frame.
/// Skips (saying so) when this unit-test VM cannot define the classes, or
/// when its `java/lang/Object` is not the plain one (a synthetic image, a
/// registered constructor native): the verdict must then be `false`.
#[test]
fn a_proven_trivial_constructor_is_called_without_a_frame() {
    use crate::classloading::ClassLoaderId;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    // The wave-25 rule: `Object` only. The super chain (wave 94) is set
    // explicitly here, and on in its own test below.
    let mut config = crate::config::VmConfig::default();
    config.interp_trivial_ctor_super_chain = false;
    let shared = Arc::new(SharedVm::new(config));
    let define = |name: &str, bytes: &[u8]| {
        shared
            .classes
            .class_manager_write()
            .define_class_with_options(
                name,
                bytes,
                ClassLoaderId::Bootstrap,
                cratonvm_classloading::DefineClassOptions {
                    skip_verification: true,
                    ..Default::default()
                },
            )
            .ok()
    };
    let object = "java/lang/Object";
    let (Some(trivial), Some(base)) = (
        define(
            "w25/Trivial",
            &w25_class_bytes("w25/Trivial", object, object),
        ),
        define("w25/Base", &w25_class_bytes("w25/Base", object, object)),
    ) else {
        eprintln!("skipped: this unit-test VM cannot define a class");
        return;
    };
    let Some(sub) = define(
        "w25/Sub",
        &w25_class_bytes("w25/Sub", "w25/Base", "w25/Base"),
    ) else {
        eprintln!("skipped: this unit-test VM cannot define a subclass");
        return;
    };
    let code: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
    // Not `java/lang/Object.<init>`: never trivial without the super
    // chain, whatever `Object` is.
    assert_eq!(
        trivial_object_ctor_uncached(&shared, &ctor_entry(sub, &code)),
        None
    );
    // Bytes that are not the class's own `<init>()V` (a stale entry).
    assert_eq!(
        trivial_object_ctor_uncached(
            &shared,
            &ctor_entry(trivial, &[0x2a, 0xb7, 0x00, 0x06, 0xb1])
        ),
        None
    );
    let object_plain = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(trivial)
            .and_then(|c| c.superclass)
            .and_then(|id| cm.get_class(id))
            .and_then(|o| o.find_method("<init>", "()V"))
            .is_some_and(|m| {
                !m.is_synchronized() && m.code().is_some_and(|c| c.code.first() == Some(&0xb1))
            })
    } && !super::super::jit_bridge::elidable_ctor_native_would_run(
        &shared, object,
    );
    let cached = ctor_entry(trivial, &code);
    assert_eq!(
        trivial_object_ctor_uncached(&shared, &cached),
        object_plain.then_some(1)
    );
    assert_eq!(
        trivial_object_ctor_uncached(&shared, &ctor_entry(base, &code)),
        object_plain.then_some(1)
    );
    if !object_plain {
        eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
        return;
    }
    // The memo answers what the verdict answered.
    assert!(trivial_object_ctor(&shared, &cached));
    assert!(trivial_object_ctor(&shared, &cached));
    let caller = ClassId::new(987_104);
    let receiver = shared.mem.heap.alloc_object(trivial, 0);
    let mut thread = caller_thread(caller);
    thread.invoke_cache.put(
        caller,
        7,
        true,
        0,
        CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        },
    );
    thread.frames[0]
        .stack
        .push(Value::Object(Some(receiver)))
        .expect("an 8-slot stack takes the receiver");
    let served = execute_nonvirtual_fast_door(
        &shared,
        &mut thread,
        0,
        7,
        true,
        0,
        crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
        &mut DoorProbe::NotProbed,
    );
    if served.is_none() {
        return;
    }
    if trivial_ctor_elidable(&shared, &caller_thread(caller).frames, &cached).is_some() {
        assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
        assert_eq!(thread.frames.len(), 1, "no frame was pushed");
        assert_eq!(thread.frames[0].stack.len(), 0, "the receiver was popped");
    } else {
        assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
    }
}


// ── Wave 28: the field-store constructor (`FIELD_CTOR_ELISION`) ────────

/// javac's field-assigning constructors of a direct `Object` subclass
/// parse into their stores, in order; any other body does not.
#[test]
fn field_ctor_plan_reads_only_field_store_constructors() {
    // Point(int a, int b) { this.a = a; this.b = b; }
    let point: [u8; 15] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0x1c, 0xb5, 0x00, 0x08,
        0xb1,
    ];
    let (plan, n) = field_ctor_plan(&point).expect("Point's constructor");
    assert_eq!(n, 2);
    assert_eq!(plan[0], (7, CtorValue::Local(1)));
    assert_eq!(plan[1], (8, CtorValue::Local(2)));
    // class WithInit { int x = 7; }
    let with_init: [u8; 11] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x10, 0x07, 0xb5, 0x00, 0x07, 0xb1,
    ];
    let (plan, n) = field_ctor_plan(&with_init).expect("an initializer");
    assert_eq!((n, plan[0]), (1, (7, CtorValue::Int(7))));
    // sipush -200, fconst_1, aconst_null, aload 4.
    let mixed: [u8; 28] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x11, 0xff, 0x38, 0xb5, 0x00, 0x07, 0x2a, 0x0c, 0xb5,
        0x00, 0x08, 0x2a, 0x01, 0xb5, 0x00, 0x09, 0x2a, 0x19, 0x04, 0xb5, 0x00, 0x0a, 0xb1,
    ];
    let (plan, n) = field_ctor_plan(&mixed).expect("four stores");
    assert_eq!(n, 4);
    assert_eq!(plan[0], (7, CtorValue::Int(-200)));
    assert_eq!(plan[1], (8, CtorValue::Float(1.0)));
    assert_eq!(plan[2], (9, CtorValue::Null));
    assert_eq!(plan[3], (10, CtorValue::Local(4)));
    let refuse: [&[u8]; 9] = [
        // the default constructor: the trivial rule's, not this one's
        &[0x2a, 0xb7, 0x00, 0x01, 0xb1],
        // a store before the super call (JDK 25 flexible constructor bodies)
        &[0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0xb7, 0x00, 0x01, 0xb1],
        // `this.self = this`
        &[0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2a, 0xb5, 0x00, 0x07, 0xb1],
        // `iload 0`
        &[
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x15, 0x00, 0xb5, 0x00, 0x07, 0xb1,
        ],
        // a call after the stores
        &[
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x09, 0xb1,
        ],
        // `ldc` (a String constant)
        &[
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x12, 0x03, 0xb5, 0x00, 0x07, 0xb1,
        ],
        // `lconst_1` into a long field
        &[0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x0a, 0xb5, 0x00, 0x07, 0xb1],
        // a byte after the `return`
        &[
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0xb1, 0x00,
        ],
        // five stores: over `CTOR_FIELD_STORES_MAX`
        &[
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x03, 0xb5, 0x00, 0x07, 0x2a, 0x03, 0xb5, 0x00, 0x07,
            0x2a, 0x03, 0xb5, 0x00, 0x07, 0x2a, 0x03, 0xb5, 0x00, 0x07, 0x2a, 0x03, 0xb5, 0x00,
            0x07, 0xb1,
        ],
    ];
    for body in refuse {
        assert!(field_ctor_plan(body).is_none(), "{body:02x?}");
    }
}

/// Interpreter round i6 wave 115, lane L7
/// (`CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`): the wide parse reads the
/// category-2 loads and constants, and the verdict maps the locals to
/// argument indices by the descriptor's slot widths.
#[test]
fn the_wide_field_ctor_plan_maps_locals_to_arguments() {
    // Entry(int key, long value, Object tag) { this.key = key;
    // this.value = value; this.tag = tag; this.flag = 1L; }
    // locals: 1 = key, 2-3 = value, 4 = tag.
    let entry: [u8; 25] = [
        0x2a, 0xb7, 0x00, 0x01, // aload_0; invokespecial Object.<init>
        0x2a, 0x1b, 0xb5, 0x00, 0x07, // iload_1
        0x2a, 0x20, 0xb5, 0x00, 0x08, // lload_2
        0x2a, 0x19, 0x04, 0xb5, 0x00, 0x09, // aload 4
        0x2a, 0x0a, 0xb5, 0x00, // lconst_1; putfield 0x000a (continued)
        0x0a,
    ];
    let mut body = entry.to_vec();
    body.push(0xb1);
    assert!(field_ctor_plan(&body).is_none(), "off: no category-2 load");
    let (mut plan, n) = field_ctor_plan_with(&body, true, false).expect("the wide parse");
    assert_eq!(n, 4);
    assert_eq!(plan[1], (8, CtorValue::LocalWide(2)));
    assert_eq!(plan[3], (10, CtorValue::Long(1)));
    assert!(ctor_plan_locals_to_args(
        &mut plan[..n],
        "(IJLjava/lang/Object;)V"
    ));
    assert_eq!(plan[0], (7, CtorValue::Local(1)));
    assert_eq!(plan[1], (8, CtorValue::Local(2)));
    assert_eq!(plan[2], (9, CtorValue::Local(3)));
    assert_eq!(plan[3], (10, CtorValue::Long(1)));
    // Vec2(double x, double y): dload_1, dload_3, and dconst_0.
    let vec2: [u8; 20] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x27, 0xb5, 0x00, 0x07, 0x2a, 0x29, 0xb5, 0x00, 0x08,
        0x2a, 0x0e, 0xb5, 0x00, 0x09, 0xb1,
    ];
    let (mut plan, n) = field_ctor_plan_with(&vec2, true, false).expect("Vec2");
    assert!(ctor_plan_locals_to_args(&mut plan[..n], "(DD)V"));
    assert_eq!(plan[0], (7, CtorValue::Local(1)));
    assert_eq!(plan[1], (8, CtorValue::Local(2)));
    assert_eq!(plan[2], (9, CtorValue::Double(0.0)));
    // A load of the second half of a long, a category mismatch, the
    // receiver as a wide load, a slot past the parameters: refused.
    for (load, descriptor) in [
        (CtorValue::Local(2), "(J)V"),
        (CtorValue::LocalWide(1), "(I)V"),
        (CtorValue::Local(1), "(J)V"),
        (CtorValue::LocalWide(0), "(J)V"),
        (CtorValue::Local(3), "(II)V"),
    ] {
        let mut plan = [(7u16, load)];
        assert!(!ctor_plan_locals_to_args(&mut plan, descriptor), "{load:?} {descriptor}");
    }
    // Arrays and references take one slot whatever their element type.
    let slots = ctor_param_slots("([JLjava/lang/Long;D[[DI)V").expect("a descriptor");
    assert_eq!(slots[1], Some((1, false)));
    assert_eq!(slots[2], Some((2, false)));
    assert_eq!(slots[3], Some((3, true)));
    assert_eq!(slots[4], None);
    assert_eq!(slots[5], Some((4, false)));
    assert_eq!(slots[6], Some((5, false)));
    assert_eq!(slots[7], None);
    assert!(ctor_param_slots("not a descriptor").is_none());
    assert!(ctor_param_slots("(I").is_none());
}

/// Interpreter round i6 wave 118, lane L7
/// (`CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES`): the `ldc` parse reads the
/// three constant loads as pool indices; without the flag it refuses them.
#[test]
fn the_ldc_field_ctor_plan_reads_pool_indices() {
    // this.a = 100000 (ldc #9); this.b = 2.5f (ldc_w #0x0102);
    // this.c = 5L (ldc2_w #11)
    let body: [u8; 21] = [
        0x2a, 0xb7, 0x00, 0x01, // aload_0; invokespecial Object.<init>
        0x2a, 0x12, 0x09, 0xb5, 0x00, 0x07, // ldc #9
        0x2a, 0x13, 0x01, 0x02, 0xb5, 0x00, 0x08, // ldc_w #258
        0x2a, 0x14, 0x00, 0x0b, // ldc2_w #11 (putfield follows)
    ];
    let mut code = body.to_vec();
    code.extend_from_slice(&[0xb5, 0x00, 0x0a, 0xb1]);
    assert!(field_ctor_plan(&code).is_none(), "off: no ldc");
    assert!(field_ctor_plan_with(&code, true, false).is_none(), "wide alone: no ldc");
    let (plan, n) = field_ctor_plan_with(&code, false, true).expect("the ldc parse");
    assert_eq!(n, 3);
    assert_eq!(plan[0], (7, CtorValue::Ldc(9)));
    assert_eq!(plan[1], (8, CtorValue::Ldc(258)));
    assert_eq!(plan[2], (10, CtorValue::Ldc2(11)));
}

/// The screen reads the padded body the caches hold, like the trivial
/// constructor's shape test.
#[test]
fn field_ctor_prefilter_reads_the_padded_body() {
    let declaring = ClassId::new(987_106);
    let point: [u8; 15] = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0x1c, 0xb5, 0x00, 0x08,
        0xb1,
    ];
    let entry = |code: &[u8]| {
        CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("w28/Point"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("(II)V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 3,
            num_params: 2,
            is_synchronized: false,
            is_static: false,
        })
    };
    assert!(field_ctor_prefilter(&entry(&point)));
    assert!(!field_ctor_prefilter(&entry(&[
        0x2a, 0xb7, 0x00, 0x01, 0xb1
    ])));
    assert!(
        !field_ctor_prefilter(&entry(&point[..14])),
        "no `return` at the end"
    );
}

// ── t1 wave 6, lane G6: stores before `super()` ───────────────────────

/// javac's inner-class constructor shape, `Sub(K key) { this.key = key; }`
/// of a non-static `Sub`: `this$0` stored, then `super()`, then `key`.
const G6_INNER: [u8; 15] = [
    0x2a, 0x2b, 0xb5, 0x00, 0x0c, // aload_0; aload_1; putfield #12 (this$0)
    0x2a, 0xb7, 0x00, 0x05, // aload_0; invokespecial #5 (super())
    0x2a, 0x2c, 0xb5, 0x00, 0x0d, // aload_0; aload_2; putfield #13 (key)
    0xb1,
];

/// The pre-`super()` parse reads the inner / local class shapes, in body
/// order with the call's pc, and nothing else: not the plain field-store
/// shape (no store before the call), not a body without a call, with two
/// calls, with something after `return`, or with too many stores.
#[test]
fn g6_pre_super_plan_reads_only_inner_class_constructors() {
    let (plan, n, call_pc) =
        field_ctor_pre_super_plan(&G6_INNER, false, false).expect("Sub's constructor");
    assert_eq!((n, call_pc), (2, 5));
    assert_eq!(plan[0], (12, CtorValue::Local(1)));
    assert_eq!(plan[1], (13, CtorValue::Local(2)));
    // `HashMap$KeySet`: only `this$0`, then a (trivial) `super()`.
    let view = [0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0xb1];
    let (plan, n, call_pc) = field_ctor_pre_super_plan(&view, false, false).expect("a view");
    assert_eq!((n, call_pc, plan[0]), (1, 5, (12, CtorValue::Local(1))));
    // An anonymous class capturing an int and the outer instance, with a
    // field initializer after `super()`: `val$n`, `this$0`, super, `x = 7`.
    let anon = [
        0x2a, 0x1c, 0xb5, 0x00, 0x0e, 0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05,
        0x2a, 0x10, 0x07, 0xb5, 0x00, 0x0f, 0xb1,
    ];
    let (plan, n, call_pc) = field_ctor_pre_super_plan(&anon, false, false).expect("anon");
    assert_eq!((n, call_pc), (3, 10));
    assert_eq!(
        plan[..3],
        [
            (14, CtorValue::Local(2)),
            (12, CtorValue::Local(1)),
            (15, CtorValue::Int(7)),
        ]
    );
    // The plain shape is `field_ctor_plan`'s, not this parse's.
    let point = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0x1c, 0xb5, 0x00, 0x08,
        0xb1,
    ];
    assert!(field_ctor_pre_super_plan(&point, false, false).is_none());
    // No `super()` call at all.
    let no_call = [
        0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0x2c, 0xb5, 0x00, 0x0d, 0xb1,
    ];
    assert!(field_ctor_pre_super_plan(&no_call, false, false).is_none());
    // Two calls.
    let mut two = G6_INNER.to_vec();
    two.splice(14..14, [0x2a, 0xb7, 0x00, 0x05]);
    assert!(field_ctor_pre_super_plan(&two, false, false).is_none());
    // Bytes after `return`.
    let mut tail = G6_INNER.to_vec();
    tail.push(0x00);
    assert!(field_ctor_pre_super_plan(&tail, false, false).is_none());
    // Five stores (the door makes at most four).
    let mut five = vec![];
    for f in 0..3u8 {
        five.extend_from_slice(&[0x2a, 0x2b, 0xb5, 0x00, 0x20 + f]);
    }
    five.extend_from_slice(&[0x2a, 0xb7, 0x00, 0x05]);
    for f in 3..5u8 {
        five.extend_from_slice(&[0x2a, 0x2b, 0xb5, 0x00, 0x20 + f]);
    }
    five.push(0xb1);
    assert!(field_ctor_pre_super_plan(&five, false, false).is_none());
    // A call into something else between the stores (`getfield` value).
    let getfield = [
        0x2a, 0x2b, 0xb4, 0x00, 0x09, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0xb1,
    ];
    assert!(field_ctor_pre_super_plan(&getfield, false, false).is_none());
    // The refactored plain parse still reads the plain shape.
    assert_eq!(field_ctor_plan(&point).map(|(_, n)| n), Some(2));
}

/// javac 25's inner-class constructor, `AbstractClassLoaderValue$Sub`'s
/// on the JDK 25 image: `this$0` goes through `dup; invokestatic
/// Objects.requireNonNull; pop` before its store. The G6 parse refused
/// it (the value was not followed by `putfield`), so the switch never
/// answered the i147 `Sub` it was built for.
const G6_JAVAC25_INNER: [u8; 20] = [
    0x2a, 0x2b, 0x59, 0xb8, 0x00, 0x11, 0x57, 0xb5, 0x00, 0x0c, // this$0, null-checked
    0x2a, 0xb7, 0x00, 0x05, // super()
    0x2a, 0x2c, 0xb5, 0x00, 0x0d, // key
    0xb1,
];

/// The pre-`super()` parse reads javac 25's null-checked `this$0` store
/// as [`CtorValue::NullChecked`] with the `invokestatic`'s pool index,
/// before the call only: the plain parse and the stores after `super()`
/// still refuse it, and a check followed by anything but `pop; putfield`
/// is refused.
#[test]
fn g6_pre_super_plan_reads_javac25_null_checked_outer_store() {
    let (plan, n, call_pc) = field_ctor_pre_super_plan(&G6_JAVAC25_INNER, false, false)
        .expect("javac 25's Sub constructor");
    assert_eq!((n, call_pc), (2, 10));
    assert_eq!(plan[0], (12, CtorValue::NullChecked(1, 0x11)));
    assert_eq!(plan[1], (13, CtorValue::Local(2)));
    // The same store after `super()` is not javac's shape.
    let after = [
        0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0x2a, 0x2c, 0x59, 0xb8, 0x00,
        0x11, 0x57, 0xb5, 0x00, 0x0d, 0xb1,
    ];
    assert!(field_ctor_pre_super_plan(&after, false, false).is_none());
    // Nor in the plain shape.
    let plain = [
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2b, 0x59, 0xb8, 0x00, 0x11, 0x57, 0xb5, 0x00, 0x07,
        0xb1,
    ];
    assert!(field_ctor_plan(&plain).is_none());
    // `dup; invokestatic` kept on the stack (no `pop`) is refused.
    let mut no_pop = G6_JAVAC25_INNER.to_vec();
    no_pop[6] = 0x00;
    assert!(field_ctor_pre_super_plan(&no_pop, false, false).is_none());
    // An int value is never null-checked.
    let mut int = G6_JAVAC25_INNER.to_vec();
    int[1] = 0x1b;
    assert!(field_ctor_pre_super_plan(&int, false, false).is_none());
}

/// The screen passes the inner shape on the padded body, and refuses the
/// plain shape (the other screen's) and a body without `return` last.
#[test]
fn g6_pre_super_prefilter_reads_the_padded_body() {
    let entry = |code: &[u8]| {
        CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(987_606),
            class_name: Arc::from("g6/Outer$Sub"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("(Lg6/Outer;Ljava/lang/Object;)V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 3,
            num_params: 2,
            is_synchronized: false,
            is_static: false,
        })
    };
    assert!(field_ctor_pre_super_prefilter(&entry(&G6_INNER)));
    assert!(!field_ctor_prefilter(&entry(&G6_INNER)));
    assert!(!field_ctor_pre_super_prefilter(&entry(&[
        0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0xb1
    ])));
    assert!(!field_ctor_pre_super_prefilter(&entry(&G6_INNER[..14])));
}

/// Class-file bytes for `this_name extends java/lang/Object` with a field
/// `outer` (`Ljava/lang/Object;`, synthetic final, as `this$0`) and one
/// method `<init>(Ljava/lang/Object;)V` = `aload_0; aload_1; putfield
/// #12; aload_0; invokespecial java/lang/Object.<init>()V; return`.
/// Constant pool as [`w25_class_bytes`]'s, plus #12 Fieldref #1.#13,
/// #13 NameAndType #14:#15, #14 "outer", #15 "Ljava/lang/Object;",
/// #16 "(Ljava/lang/Object;)V".
fn g6_inner_class_bytes(this_name: &str) -> Vec<u8> {
    g6_inner_class_bytes_with(this_name, None)
}

/// [`g6_inner_class_bytes`], or with `Some(check_class)` javac 25's
/// body ([`G6_JAVAC25_INNER`]'s first ten bytes, then the call and
/// `return`: `this$0` null-checked by `invokestatic #17`), the pool
/// extended by #17 Methodref #18.#20, #18 Class #19, #19 `check_class`,
/// #20 NameAndType #21:#22, #21 "requireNonNull",
/// #22 "(Ljava/lang/Object;)Ljava/lang/Object;".
fn g6_inner_class_bytes_with(this_name: &str, check_class: Option<&str>) -> Vec<u8> {
    fn utf8(b: &mut Vec<u8>, s: &str) {
        b.push(1);
        b.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        b.extend_from_slice(s.as_bytes());
    }
    let object = "java/lang/Object";
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
    let count: u16 = if check_class.is_some() { 23 } else { 17 };
    b.extend_from_slice(&count.to_be_bytes()); // constant_pool_count
    b.extend_from_slice(&[7, 0x00, 0x02]); // #1 Class #2
    utf8(&mut b, this_name); // #2
    b.extend_from_slice(&[7, 0x00, 0x04]); // #3 Class #4
    utf8(&mut b, object); // #4
    b.extend_from_slice(&[10, 0x00, 0x06, 0x00, 0x08]); // #5 Methodref #6.#8
    b.extend_from_slice(&[7, 0x00, 0x07]); // #6 Class #7
    utf8(&mut b, object); // #7
    b.extend_from_slice(&[12, 0x00, 0x09, 0x00, 0x0A]); // #8 NameAndType #9:#10
    utf8(&mut b, "<init>"); // #9
    utf8(&mut b, "()V"); // #10
    utf8(&mut b, "Code"); // #11
    b.extend_from_slice(&[9, 0x00, 0x01, 0x00, 0x0D]); // #12 Fieldref #1.#13
    b.extend_from_slice(&[12, 0x00, 0x0E, 0x00, 0x0F]); // #13 NameAndType #14:#15
    utf8(&mut b, "outer"); // #14
    utf8(&mut b, "Ljava/lang/Object;"); // #15
    utf8(&mut b, "(Ljava/lang/Object;)V"); // #16
    if let Some(check_class) = check_class {
        b.extend_from_slice(&[10, 0x00, 0x12, 0x00, 0x14]); // #17 Methodref #18.#20
        b.extend_from_slice(&[7, 0x00, 0x13]); // #18 Class #19
        utf8(&mut b, check_class); // #19
        b.extend_from_slice(&[12, 0x00, 0x15, 0x00, 0x16]); // #20 NameAndType #21:#22
        utf8(&mut b, "requireNonNull"); // #21
        utf8(&mut b, "(Ljava/lang/Object;)Ljava/lang/Object;"); // #22
    }
    let body: Vec<u8> = match check_class {
        Some(_) => [&G6_JAVAC25_INNER[..10], &[0x2a, 0xb7, 0x00, 0x05, 0xb1][..]].concat(),
        None => G6_INNER_BODY.to_vec(),
    };
    let (header, field): ([u8; 8], [u8; 10]) = (
        // public super, this #1, super #3, no interfaces.
        [0x00, 0x21, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00],
        // One field: final synthetic, #14, #15, no attributes.
        [0x00, 0x01, 0x10, 0x10, 0x00, 0x0E, 0x00, 0x0F, 0x00, 0x00],
    );
    b.extend_from_slice(&header);
    b.extend_from_slice(&field);
    b.extend_from_slice(&[0x00, 0x01]); // methods_count
    b.extend_from_slice(&[0x00, 0x01, 0x00, 0x09, 0x00, 0x10, 0x00, 0x01]); // <init>(Object)V
    b.extend_from_slice(&[0x00, 0x0B]); // "Code"
    let code_len = u32::try_from(body.len()).unwrap();
    b.extend_from_slice(&(12 + code_len).to_be_bytes()); // attribute_length
    b.extend_from_slice(&[0x00, 0x03, 0x00, 0x02]); // max_stack, max_locals
    b.extend_from_slice(&code_len.to_be_bytes()); // code_length
    b.extend_from_slice(&body);
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no handlers, no attributes
    b.extend_from_slice(&[0x00, 0x00]); // no class attributes
    b
}

/// [`g6_inner_class_bytes`]'s constructor body.
const G6_INNER_BODY: [u8; 10] = [0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0xb1];

/// Against a class the class manager really holds: with
/// `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES` the verdict reads the
/// `super()` call where the parse found it (pc 5) and carries the
/// pre-`super()` store; without it the constructor is not answered
/// (`object_init_first` false, no plan), exactly as before. Skips (saying
/// so) when this unit-test VM cannot define the class or its
/// `java/lang/Object.<init>` is not plain bytecode.
#[test]
fn g6_the_verdict_reads_the_super_call_after_the_stores_only_with_the_switch() {
    use crate::classloading::ClassLoaderId;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let verdict_with = |on: bool| -> Option<(Option<u8>, CtorVerdict)> {
        let config = crate::config::VmConfig {
            interp_field_ctor_pre_super_stores: on,
            ..crate::config::VmConfig::default()
        };
        let shared = SharedVm::new(config);
        let id = shared
            .classes
            .class_manager_write()
            .define_class_with_options(
                "g6/Inner",
                &g6_inner_class_bytes("g6/Inner"),
                ClassLoaderId::Bootstrap,
                cratonvm_classloading::DefineClassOptions {
                    skip_verification: true,
                    ..Default::default()
                },
            )
            .ok()?;
        let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: id,
            class_name: Arc::from("g6/Inner"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("(Ljava/lang/Object;)V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&G6_INNER_BODY),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 2,
            max_locals: 2,
            num_params: 1,
            is_synchronized: false,
            is_static: false,
        });
        Some((
            trivial_object_ctor_uncached_at(&shared, &cached, 5),
            ctor_verdict_uncached(&shared, &cached),
        ))
    };
    let Some((chain_on, on)) = verdict_with(true) else {
        eprintln!("skipped: this unit-test VM cannot define a class");
        return;
    };
    let Some((_, off)) = verdict_with(false) else {
        eprintln!("skipped: this unit-test VM cannot define a class");
        return;
    };
    // Off: unchanged (the first instruction is not the call).
    assert!(!off.object_init_first);
    assert_eq!(off.plan_len, 0);
    if chain_on != Some(1) {
        eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
        assert!(!on.object_init_first && on.plan_len == 0);
        return;
    }
    assert!(on.object_init_first);
    assert_eq!(on.chain_frames, 1);
    assert_eq!(on.plan_len, 1);
    assert_eq!(on.plan[0], (12, CtorValue::Local(1)));
}

/// Class-file bytes for a stand-in `java/util/Objects` (for a unit-test
/// VM without the JDK image) whose one method is `public static
/// requireNonNull(Ljava/lang/Object;)Ljava/lang/Object;` =
/// `aload_0; ifnonnull +5; aconst_null; athrow; aload_0; areturn`: the
/// shape [`is_jdk_require_non_null`] reads.
fn g6_objects_class_bytes() -> Vec<u8> {
    fn utf8(b: &mut Vec<u8>, s: &str) {
        b.push(1);
        b.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        b.extend_from_slice(s.as_bytes());
    }
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
    b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
    b.extend_from_slice(&[7, 0x00, 0x02]); // #1 Class #2
    utf8(&mut b, "java/util/Objects"); // #2
    b.extend_from_slice(&[7, 0x00, 0x04]); // #3 Class #4
    utf8(&mut b, "java/lang/Object"); // #4
    utf8(&mut b, "requireNonNull"); // #5
    utf8(&mut b, "(Ljava/lang/Object;)Ljava/lang/Object;"); // #6
    utf8(&mut b, "Code"); // #7
    // public final super, this #1, super #3, no interfaces, no fields.
    b.extend_from_slice(&[0x00, 0x31, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00]);
    b.extend_from_slice(&[0x00, 0x01]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0x00, 0x05, 0x00, 0x06, 0x00, 0x01]); // public static
    b.extend_from_slice(&[0x00, 0x07]); // "Code"
    b.extend_from_slice(&20u32.to_be_bytes()); // attribute_length
    b.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // max_stack, max_locals
    b.extend_from_slice(&8u32.to_be_bytes()); // code_length
    b.extend_from_slice(&[0x2a, 0xc7, 0x00, 0x05, 0x01, 0xbf, 0x2a, 0xb0]);
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no handlers, no attributes
    b.extend_from_slice(&[0x00, 0x00]); // no class attributes
    b
}

/// i147 close-out, against classes the class manager really holds: with
/// `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`, javac 25's inner-class
/// constructor (`this$0` through `Objects.requireNonNull`) is answered,
/// its outer instance a [`CtorValue::LocalNonNull`], when the check is
/// `java/util/Objects`'s; the same call into another class keeps the
/// frame. Skips (saying so) as
/// [`g6_the_verdict_reads_the_super_call_after_the_stores_only_with_the_switch`].
#[test]
fn g6_the_verdict_answers_javac25_null_checked_outer_only_for_the_jdk_check() {
    use crate::classloading::ClassLoaderId;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let options = || cratonvm_classloading::DefineClassOptions {
        skip_verification: true,
        ..Default::default()
    };
    let verdict_for = |check_class: &str| -> Option<(Option<u8>, CtorVerdict)> {
        let config = crate::config::VmConfig {
            interp_field_ctor_pre_super_stores: true,
            ..crate::config::VmConfig::default()
        };
        let shared = SharedVm::new(config);
        let id = {
            let mut cm = shared.classes.class_manager_write();
            if cm
                .find_class_by_name_for_loader("java/util/Objects", ClassLoaderId::Bootstrap)
                .is_none()
            {
                cm.define_class_with_options(
                    "java/util/Objects",
                    &g6_objects_class_bytes(),
                    ClassLoaderId::Bootstrap,
                    options(),
                )
                .ok()?;
            }
            cm.define_class_with_options(
                "g6/Inner25",
                &g6_inner_class_bytes_with("g6/Inner25", Some(check_class)),
                ClassLoaderId::Bootstrap,
                options(),
            )
            .ok()?
        };
        let body = [&G6_JAVAC25_INNER[..10], &[0x2a, 0xb7, 0x00, 0x05, 0xb1][..]].concat();
        let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: id,
            class_name: Arc::from("g6/Inner25"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("(Ljava/lang/Object;)V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&body),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 3,
            max_locals: 2,
            num_params: 1,
            is_synchronized: false,
            is_static: false,
        });
        Some((
            trivial_object_ctor_uncached_at(&shared, &cached, 10),
            ctor_verdict_uncached(&shared, &cached),
        ))
    };
    let Some((chain, jdk)) = verdict_for("java/util/Objects") else {
        eprintln!("skipped: this unit-test VM cannot define a class");
        return;
    };
    let Some((_, other)) = verdict_for("g6/Objects") else {
        eprintln!("skipped: this unit-test VM cannot define a class");
        return;
    };
    // Another class's `requireNonNull`: the frame stays.
    assert!(!other.object_init_first);
    assert_eq!(other.plan_len, 0);
    if chain != Some(1) {
        eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
        assert!(!jdk.object_init_first && jdk.plan_len == 0);
        return;
    }
    assert!(jdk.object_init_first);
    assert_eq!(jdk.chain_frames, 1);
    assert_eq!(jdk.plan_len, 1);
    assert_eq!(jdk.plan[0], (12, CtorValue::LocalNonNull(1)));
}

/// Interpreter round i6 wave 119, lane L7
/// (`CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY`): with the switch, a verdict
/// the entry carries under the current stamp answers; a stale stamp does
/// not (the per-thread memo / the uncached verdict answer instead).
#[test]
fn an_entry_verdict_answers_under_its_own_stamp_only() {
    let config = crate::config::VmConfig {
        interp_ctor_verdict_on_entry: true,
        ..crate::config::VmConfig::default()
    };
    let shared = SharedVm::new(config);
    let entry = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(987_119),
            class_name: Arc::from("w119/NoSuchClass"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&[0x2a, 0xb7, 0x00, 0x01, 0xb1]),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        },
    ));
    let planted = CtorVerdict {
        object_init_first: true,
        chain_frames: 3,
        plan_len: 0,
        plan: [(0u16, CtorValue::Null); super::super::field_fast::CTOR_FIELD_STORES_MAX],
    };
    let now = crate::classloading::class_redefinition_count();
    let _ = entry
        .ctor_verdict_memo
        .set(Box::new(EntryCtorVerdictCell::new(EntryCtorVerdict {
            vm: shared.vm_identity,
            redefinitions: now,
            verdict: planted,
        })));
    let answered = ctor_verdict(&shared, &entry);
    if crate::classloading::class_redefinition_count() == now {
        assert_eq!(answered.chain_frames, 3, "the entry's own verdict answers");
    }
    // Another VM's (or a stale) stamp is not redeemed: the class does not
    // exist in this VM, so the real verdict is "not trivial".
    let other = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(987_120),
            class_name: Arc::from("w119/NoSuchClass2"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(&[0x2a, 0xb7, 0x00, 0x01, 0xb1]),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        },
    ));
    let _ = other
        .ctor_verdict_memo
        .set(Box::new(EntryCtorVerdictCell::new(EntryCtorVerdict {
            vm: shared.vm_identity.wrapping_add(1),
            redefinitions: now,
            verdict: planted,
        })));
    assert!(!ctor_verdict(&shared, &other).object_init_first);
    // Wave 120 (L9's review): the stale cell was refilled with this VM's
    // stamp and real verdict, not left stale.
    let refilled = other
        .ctor_verdict_memo
        .get()
        .and_then(|cell| cell.downcast_ref::<EntryCtorVerdictCell>())
        .expect("the cell stays installed");
    if crate::classloading::class_redefinition_count() == now {
        assert_eq!(refilled.load().vm, shared.vm_identity);
        assert!(!refilled.load().verdict.object_init_first);
    }
}

/// Interpreter round i6 wave 120, lane L7: a refill keeps a reference
/// loaded before it valid, and refills stop at the cap.
#[test]
fn an_entry_verdict_cell_refills_and_keeps_old_answers_alive() {
    let verdict = |frames: u8| CtorVerdict {
        object_init_first: true,
        chain_frames: frames,
        plan_len: 0,
        plan: [(0u16, CtorValue::Null); super::super::field_fast::CTOR_FIELD_STORES_MAX],
    };
    let cell = EntryCtorVerdictCell::new(EntryCtorVerdict {
        vm: 1,
        redefinitions: 0,
        verdict: verdict(1),
    });
    let before = cell.load();
    cell.refill(EntryCtorVerdict {
        vm: 1,
        redefinitions: 1,
        verdict: verdict(2),
    });
    assert_eq!(before.verdict.chain_frames, 1, "the old answer is still readable");
    assert_eq!(cell.load().redefinitions, 1);
    for i in 0..(EntryCtorVerdictCell::MAX_REFILLS as u64 + 10) {
        cell.refill(EntryCtorVerdict {
            vm: 1,
            redefinitions: 2 + i,
            verdict: verdict(3),
        });
    }
    // The cap: the last refill that landed was the 64th.
    assert_eq!(
        cell.load().redefinitions,
        EntryCtorVerdictCell::MAX_REFILLS as u64
    );
}

/// Wave 28 (L7b): a `long` / `double` parameter is found by the
/// descriptor, once per method, and only at the top level of the
/// parameter list (not inside a class name or an array's element class).
#[test]
fn category2_parameters_are_read_from_the_descriptor() {
    for (descriptor, cat2) in [
        ("()V", false),
        ("(II)V", false),
        ("(JI)V", true),
        ("(ID)V", true),
        ("(Ljava/lang/Double;I)V", false),
        ("([JI)V", false),
        ("([[Ljava/lang/Long;D)V", true),
        ("(Ljava/util/List;[D)V", false),
        ("not a descriptor", true),
    ] {
        assert_eq!(
            descriptor_has_category2_param(descriptor),
            cat2,
            "{descriptor}"
        );
    }
}

/// Interpreter round i4 wave 94, lane L7: with
/// `VmConfig::interp_trivial_ctor_super_chain` the verdict follows a chain
/// of default constructors (`w94/Leaf` -> `w94/Mid` -> `w94/Base` ->
/// `Object`) and answers the frames the framed call would push; without
/// it the same constructor is not trivial. Skips (saying so) when this
/// unit-test VM cannot define the classes.
#[test]
fn the_super_chain_verdict_follows_default_constructors_to_object() {
    use crate::classloading::ClassLoaderId;
    if cratonvm_jit_api::descriptor_facts_disabled() {
        return;
    }
    let verdict_with = |chain: bool| -> Option<(Option<u8>, Option<u8>, bool)> {
        let mut config = crate::config::VmConfig::default();
        config.interp_trivial_ctor_super_chain = chain;
        let shared = Arc::new(SharedVm::new(config));
        let define = |name: &str, bytes: &[u8]| {
            shared
                .classes
                .class_manager_write()
                .define_class_with_options(
                    name,
                    bytes,
                    ClassLoaderId::Bootstrap,
                    cratonvm_classloading::DefineClassOptions {
                        skip_verification: true,
                        ..Default::default()
                    },
                )
                .ok()
        };
        let object = "java/lang/Object";
        let base = define("w94/Base", &w25_class_bytes("w94/Base", object, object))?;
        let mid = define(
            "w94/Mid",
            &w25_class_bytes("w94/Mid", "w94/Base", "w94/Base"),
        )?;
        let leaf = define(
            "w94/Leaf",
            &w25_class_bytes("w94/Leaf", "w94/Mid", "w94/Mid"),
        )?;
        // What the chain rests on besides the bytes: a plain
        // `Object.<init>`, real class bytes for the two inner links, and
        // no registered native in front of any link.
        let plain = {
            let cm = shared.classes.class_manager.read();
            let object_plain = cm
                .get_class(base)
                .and_then(|c| c.superclass)
                .and_then(|id| cm.get_class(id))
                .and_then(|o| o.find_method("<init>", "()V"))
                .is_some_and(|m| {
                    !m.is_synchronized()
                        && m.code().is_some_and(|c| c.code.first() == Some(&0xb1))
                });
            let real = [base, mid]
                .iter()
                .all(|&id| cm.get_class(id).is_some_and(|c| c.origin.has_real_bytes()));
            object_plain && real
        } && ["w94/Mid", "w94/Base", object].iter().all(|name| {
            !super::super::jit_bridge::elidable_ctor_native_would_run(&shared, name)
        });
        let code: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
        Some((
            trivial_object_ctor_uncached(&shared, &ctor_entry(leaf, &code)),
            trivial_object_ctor_uncached(&shared, &ctor_entry(mid, &code)),
            plain,
        ))
    };
    let Some((leaf_off, mid_off, _)) = verdict_with(false) else {
        eprintln!("skipped: this unit-test VM cannot define the classes");
        return;
    };
    assert_eq!((leaf_off, mid_off), (None, None), "Object only without the chain");
    let Some((leaf_on, mid_on, plain)) = verdict_with(true) else {
        eprintln!("skipped: this unit-test VM cannot define the classes");
        return;
    };
    assert_eq!(leaf_on, plain.then_some(3), "Leaf -> Mid -> Base -> Object");
    assert_eq!(mid_on, plain.then_some(2), "Mid -> Base -> Object");
}
