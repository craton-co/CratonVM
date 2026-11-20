use super::*;
use crate::threading::jvm_thread::ThreadId;

fn vm() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(crate::config::VmConfig::default()))
}

/// A config with the cast-site table and the negative memo on and the
/// crosscheck instrument off, whatever the environment says (wave 57,
/// lane L7: the gates are per-VM, so a memo test no longer skips itself
/// when the environment arms a lever).
fn memo_config() -> crate::config::VmConfig {
    let mut config = crate::config::VmConfig::default();
    config.cast_site_cache = true;
    config.negative_cast_memo = true;
    config.cast_memo_crosscheck = false;
    config
}

fn memo_vm() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(memo_config()))
}

fn thread_with_frame() -> JvmThread {
    let mut thread = JvmThread::new(ThreadId(4242), "l2-decoded");
    thread.frames.push(Frame::new(
        ClassId::new(0),
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

fn runtime_error(outcome: Result<InstructionResult, MethodCallFailed>) -> RuntimeError {
    match outcome {
        Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => e,
        Err(other) => panic!("expected a Java runtime error, got {other:?}"),
        Ok(_) => panic!("expected a Java runtime error, the instruction completed"),
    }
}

fn message(err: &RuntimeError) -> Option<String> {
    err.as_java_throwable()
        .and_then(|(_, m)| m)
        .map(|m| m.into_owned())
}

#[test]
fn a_negative_index_is_named_as_itself_not_as_i32_max() {
    let refused = nonnegative_array_index(-1, || 3).unwrap_err();
    assert_eq!(
        message(&refused).as_deref(),
        Some("Index -1 out of bounds for length 3"),
        "HotSpot 25 names the negative index verbatim"
    );
    // The conversion this replaced: `-1 as usize` reaches the heap as
    // `usize::MAX`, and the heap's error channel saturates that.
    assert_eq!(cratonvm_types::oob_index_code((-1i32) as usize), i32::MAX);
    // A non-negative index never reads the length.
    let never = || -> i32 { panic!("length read on the accept path") };
    assert_eq!(nonnegative_array_index(2, never).ok(), Some(2));
}

/// Every decoded array arm, not just the helper: a helper nobody calls
/// would pass the row above.
#[test]
fn decoded_array_arms_report_a_negative_index_like_hotspot() {
    let shared = vm();
    let cases: [(Instruction, ArrayElementType); 4] = [
        (Instruction::Iaload, ArrayElementType::Int),
        (Instruction::Iastore, ArrayElementType::Int),
        (Instruction::Lastore, ArrayElementType::Long),
        (Instruction::Dastore, ArrayElementType::Double),
    ];
    for (insn, et) in cases {
        let mut thread = thread_with_frame();
        let arr = shared.mem.heap.alloc_array(ClassId::new(0), et, 3);
        let stack = &mut thread.frames[0].stack;
        stack.push(Value::Object(Some(arr))).unwrap();
        stack.push_int(-1).unwrap();
        match insn {
            Instruction::Iastore => stack.push_int(7).unwrap(),
            Instruction::Lastore => stack.push_long(7).unwrap(),
            Instruction::Dastore => stack.push_double(7.0).unwrap(),
            _ => {}
        }
        let err = runtime_error(execute_instruction(&shared, &mut thread, 0, &insn, 0));
        assert!(
            matches!(
                err,
                RuntimeError::ArrayIndexOutOfBoundsException { index: -1, .. }
            ),
            "{insn:?}: expected AIOOBE for index -1, got {err:?}"
        );
        assert_eq!(
            message(&err).as_deref(),
            Some("Index -1 out of bounds for length 3"),
            "{insn:?}"
        );
    }
}

/// The null-array path now lives in `array_operand_slow`; it must still be
/// a `NullPointerException`, and a non-null array must still load.
#[test]
fn decoded_array_load_still_raises_npe_on_null_and_loads_otherwise() {
    let shared = vm();
    let mut thread = thread_with_frame();
    thread.frames[0].stack.push(Value::Object(None)).unwrap();
    thread.frames[0].stack.push_int(0).unwrap();
    let err = runtime_error(execute_instruction(
        &shared,
        &mut thread,
        0,
        &Instruction::Iaload,
        0,
    ));
    assert!(
        matches!(err, RuntimeError::NullPointerException { .. }),
        "null array: expected NPE, got {err:?}"
    );

    let heap = &shared.mem.heap;
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
    heap.set_array_element(arr, 2, Value::Int(41)).unwrap();

    let mut thread = thread_with_frame();
    let stack = &mut thread.frames[0].stack;
    stack.push(Value::Object(Some(arr))).unwrap();
    stack.push_int(2).unwrap();
    let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Iaload, 0);
    assert!(outcome.is_ok());
    assert_eq!(thread.frames[0].stack.pop_int().unwrap(), 41);

    let mut thread = thread_with_frame();
    let stack = &mut thread.frames[0].stack;
    stack.push(Value::Object(Some(arr))).unwrap();
    let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Arraylength, 0);
    assert!(outcome.is_ok());
    assert_eq!(thread.frames[0].stack.pop_int().unwrap(), 3);
}

/// The array exclusion is the soundness condition of the receiver memo: a
/// reference array's header carries its COMPONENT's class id, so without
/// it a `String[]` would pass a `checkcast String` memoised for a `String`.
#[test]
fn the_cast_memo_answers_only_its_own_non_array_receiver() {
    let receiver = ClassId::new(17);
    let memo = CastSite {
        target: ClassId::new(3),
        positive_receiver: Some(receiver),
        older_positive_receiver: None,
        negative_receivers: [None; 2],
        negative_catch: None,
        positive_array: None,
        negative_array: None,
        older_positive_array_component: CastSite::NO_OLDER_ARRAY_COMPONENT,
        memo_epoch: 0,
    };
    assert!(cast_memo_answers(&memo, false, receiver));
    assert!(
        !cast_memo_answers(&memo, true, receiver),
        "an array whose component id equals the memo must not be answered"
    );
    assert!(!cast_memo_answers(&memo, false, ClassId::new(18)));
    assert!(
        !cast_memo_answers(&CastSite::resolved(ClassId::new(3)), false, receiver),
        "a freshly resolved site carries no verdict"
    );

    // i16-L4, the array half: the array memo answers only an ARRAY
    // receiver of exactly its (element, header id) key, and a class memo
    // never answers an array (nor the array memo a plain object).
    let key = (ArrayElementType::Reference, receiver);
    let array_memo = CastSite {
        positive_array: Some(key),
        ..memo
    };
    assert!(array_cast_memo_answers(&array_memo, true, key));
    assert!(
        !array_cast_memo_answers(&array_memo, false, key),
        "a plain object whose class id equals the memoised component is no array"
    );
    let other_component = (ArrayElementType::Reference, ClassId::new(18));
    assert!(
        !array_cast_memo_answers(&array_memo, true, other_component),
        "another component"
    );
    assert!(
        !array_cast_memo_answers(&array_memo, true, (ArrayElementType::Int, receiver)),
        "another element kind"
    );
    assert!(
        !array_cast_memo_answers(&memo, true, key),
        "no array memo, no answer"
    );
    assert!(
        !cast_memo_answers(&array_memo, true, receiver),
        "the class memo still refuses an array whatever the array memo holds"
    );

    // i52-L7, the negative array memo (`cast_memo_wide`): it answers only
    // an ARRAY receiver of exactly its key, a class slot never answers
    // an array, and the array key is not read for a plain receiver.
    let refused = CastSite {
        negative_array: Some(key),
        ..memo.with_negative(receiver)
    };
    let array = ReceiverHeader {
        is_array: true,
        class_id: receiver,
    };
    let plain = ReceiverHeader {
        is_array: false,
        class_id: receiver,
    };
    assert!(negative_memo_answers_known(&refused, array, || key));
    assert!(!negative_memo_answers_known(&refused, array, || {
        other_component
    }));
    assert!(negative_memo_answers_known(
        &refused,
        plain,
        || unreachable!()
    ));
    let only_class = memo.with_negative(receiver);
    assert!(
        !negative_memo_answers_known(&only_class, array, || unreachable!()),
        "a class refusal never answers an array, and an empty slot reads no key"
    );
}

/// i16-L4: the array memo goes where the other memos go when the
/// definition epoch moves, and `resolved` starts without one.
#[test]
fn i16_l4_the_array_memo_follows_the_definition_epoch() {
    let key = (ArrayElementType::Reference, ClassId::new(21));
    let proved = CastSite {
        positive_array: Some(key),
        memo_epoch: 7,
        ..CastSite::resolved(ClassId::new(3))
    };
    assert_eq!(CastSite::resolved(ClassId::new(3)).positive_array, None);
    assert_eq!(proved.observed_at(7).positive_array, Some(key));
    let moved = proved.observed_at(8);
    assert_eq!(moved.positive_array, None);
    assert_eq!(moved.target, ClassId::new(3));
    assert!(!array_cast_memo_answers(&moved, true, key));
}

/// TCARRAY21-3 (tca21, `VmConfig::cast_memo_two_positive`): a site whose
/// receivers alternate between two reference-array types answers both from
/// the memo, as `i53_l7` does for plain receivers. Derived from the memo's
/// own contract (a verdict is a function of the key while the epoch holds),
/// not measured.
#[test]
fn tca21_two_array_slots_answer_two_alternating_array_types() {
    let target = ClassId::new(3);
    let a = (ArrayElementType::Reference, ClassId::new(21));
    let b = (ArrayElementType::Reference, ClassId::new(22));
    let c = (ArrayElementType::Reference, ClassId::new(23));
    let ints = (ArrayElementType::Int, ClassId::new(0));
    let fresh = CastSite::resolved(target);
    assert!(!array_cast_memo_answers(&fresh, true, a));
    assert_eq!(
        fresh.older_positive_array_component,
        CastSite::NO_OLDER_ARRAY_COMPONENT
    );

    // Switch on: the second type rotates the first into the older slot.
    let site = fresh.with_positive_array(a, true);
    assert_eq!(site.positive_array, Some(a));
    assert_eq!(
        site.older_positive_array_component,
        CastSite::NO_OLDER_ARRAY_COMPONENT
    );
    let site = site.with_positive_array(b, true);
    assert_eq!(site.positive_array, Some(b));
    assert_eq!(site.older_positive_array_component, 21);
    assert!(array_cast_memo_answers(&site, true, a));
    assert!(array_cast_memo_answers(&site, true, b));
    assert!(!array_cast_memo_answers(&site, true, c));
    // A hit never reorders: two alternating types never evict each other.
    assert_eq!(site.with_positive_array(a, true), site);
    assert_eq!(site.with_positive_array(b, true), site);
    // A third type pushes the oldest out.
    let site3 = site.with_positive_array(c, true);
    assert_eq!(site3.positive_array, Some(c));
    assert_eq!(site3.older_positive_array_component, 22);
    assert!(!array_cast_memo_answers(&site3, true, a));
    assert!(array_cast_memo_answers(&site3, true, b));
    assert!(array_cast_memo_answers(&site3, true, c));

    // The older slot answers an ARRAY receiver of a reference key only:
    // never a plain object whose class id equals the component, never
    // another element kind.
    assert!(!array_cast_memo_answers(&site, false, a));
    assert!(!array_cast_memo_answers(
        &site,
        true,
        (ArrayElementType::Int, ClassId::new(21))
    ));
    // The empty slot's sentinel names no component.
    assert!(!array_cast_memo_answers(
        &fresh,
        true,
        (ArrayElementType::Reference, ClassId::new(u32::MAX))
    ));

    // Switch off: slot 0 is overwritten and the older slot stays empty.
    let one = fresh
        .with_positive_array(a, false)
        .with_positive_array(b, false);
    assert_eq!(one.positive_array, Some(b));
    assert_eq!(
        one.older_positive_array_component,
        CastSite::NO_OLDER_ARRAY_COMPONENT
    );
    assert!(!array_cast_memo_answers(&one, true, a));

    // A primitive array's key is dropped, not rotated.
    let prim_then_ref = fresh
        .with_positive_array(ints, true)
        .with_positive_array(a, true);
    assert_eq!(
        prim_then_ref.older_positive_array_component,
        CastSite::NO_OLDER_ARRAY_COMPONENT
    );
    assert!(!array_cast_memo_answers(&prim_then_ref, true, ints));

    // The definition epoch clears both slots; the target survives.
    let moved = site.observed_at(5);
    assert_eq!(moved.positive_array, None);
    assert_eq!(
        moved.older_positive_array_component,
        CastSite::NO_OLDER_ARRAY_COMPONENT
    );
    assert_eq!(moved.target, target);
    assert!(!array_cast_memo_answers(&moved, true, a));
}

/// i16-L4, end to end through `execute_instruction`: a `String[]` cast to
/// `[Ljava/lang/Object;` (the erasure of `(T[]) x`) fills the site on the
/// first execution, memoises the array verdict on the second and answers
/// the third from the memo. An `int[]` at the same site is not answered by
/// the `String[]` memo (it runs the full path and is refused), and the
/// `String[]` receiver is still admitted afterwards.
#[test]
fn i16_l4_an_array_cast_site_is_answered_from_its_array_memo() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    let shared = memo_vm();
    let holder = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l4/ArrayMemoHolder", 0)
        else {
            return; // --real-jdk policy in the environment: nothing to fabricate
        };
        let Some(class) = cm.class_store.get_mut(id) else {
            return;
        };
        class.constant_pool = ConstantPool::new(vec![
            E::Tombstone,
            E::Utf8("[Ljava/lang/Object;".into()),
            E::ClassReference { name_index: 1 },
        ]);
        id
    };
    let string_id = {
        let cm = shared.classes.class_manager.read();
        match cm.find_unique_class_by_name("java/lang/String") {
            Some(id) => id,
            None => return, // stripped VM without `String`
        }
    };
    let mut thread = JvmThread::new(ThreadId(4245), "l4-array-memo");
    thread.frames.push(Frame::new(
        holder,
        "cratonvm/test/l4/ArrayMemoHolder".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    let strings = shared
        .mem
        .heap
        .alloc_array(string_id, ArrayElementType::Reference, 1);
    let ints = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
    let key = (ArrayElementType::Reference, string_id);
    let memo_of = |thread: &mut JvmThread| {
        thread
            .cast_sites
            .get(holder, 2)
            .copied()
            .map(|site| site.observed_in(&shared))
            .and_then(|site| site.positive_array)
    };
    let run = |thread: &mut JvmThread, receiver: ObjectRef, insn: Instruction| {
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .unwrap();
        let outcome = execute_instruction(&shared, thread, 0, &insn, 0);
        let answer = match insn {
            Instruction::Instanceof(_) => {
                outcome.is_ok() && thread.frames[0].stack.pop_int().unwrap() == 1
            }
            _ => outcome.is_ok(),
        };
        thread.frames[0].stack.clear();
        answer
    };
    for round in 0..3 {
        assert!(
            run(&mut thread, strings, Instruction::Checkcast(2)),
            "round {round}: String[] is an Object[]"
        );
    }
    // Epochs are process-wide and other tests define classes
    // concurrently, so the memo is asserted only when nothing moved.
    let stable = memo_of(&mut thread);
    if stable.is_some() {
        assert_eq!(stable, Some(key), "the memo holds the receiver's key");
    }
    assert!(
        !run(&mut thread, ints, Instruction::Instanceof(2)),
        "int[] is no Object[]: the String[] memo must not answer it"
    );
    assert!(
        !run(&mut thread, ints, Instruction::Checkcast(2)),
        "int[] cannot be cast to Object[]"
    );
    assert!(run(&mut thread, strings, Instruction::Instanceof(2)));
}

/// i51-L7: both type checks read their reference-typed operand through
/// `plain_ref_on_top` before any context-free decode, as `op_athrow` and
/// the wave-3 reference arms do. A source witness: a payload without
/// recorded provenance cannot be made in a unit VM, whose every
/// allocation records its granule.
#[test]
fn i51_l7_the_type_checks_read_a_plain_reference_slot_as_a_reference() {
    let src = std::fs::read_to_string(format!(
        "{}/src/runtime/interpreter/opcodes.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read opcodes.rs");
    for (start, end, decode) in [
        (
            "pub(super) fn op_instanceof(",
            "fn instanceof_full_path(",
            ".stack.pop()?",
        ),
        (
            "pub(super) fn op_checkcast(",
            "fn checkcast_full_path(",
            ".stack.peek_checked()?",
        ),
    ] {
        let from = src.find(start).expect(start);
        let body = &src[from..from + src[from..].find(end).expect(end)];
        let plain = body
            .find("plain_ref_on_top(")
            .expect("plain_ref_on_top in the body");
        let decoded = body.rfind(decode).expect("the decode it guards");
        assert!(plain < decoded, "{start}: the plain read must come first");
    }
}

/// i51-L7: a non-reference operand is `checkcast`'s internal error for
/// `instanceof` too (it answered `0`), naming the opcode, the value and
/// the site; both pop the operand.
#[test]
fn i51_l7_a_non_reference_type_check_operand_is_an_internal_error_for_both_opcodes() {
    let shared = vm();
    for (insn, name) in [
        (Instruction::Instanceof(1), "instanceof"),
        (Instruction::Checkcast(1), "checkcast"),
    ] {
        let mut thread = thread_with_frame();
        thread.frames[0].stack.push_int(5).unwrap();
        let message = match execute_instruction(&shared, &mut thread, 0, &insn, 0) {
            Err(MethodCallFailed::InternalError(VmError::Internal { message })) => message,
            Err(other) => panic!("{name}: expected an internal error, got {other:?}"),
            Ok(_) => panic!("{name}: expected an internal error, the instruction completed"),
        };
        assert!(
            message.starts_with(&format!(
                "{name}: not an object reference (got Int(5)) at T.m()V"
            )),
            "{message}"
        );
        assert_eq!(thread.frames[0].stack.len(), 0, "{name} pops its operand");
    }
}

/// i51-L7 (`VmConfig::cast_hit_raw`): the raw cast-site probe answers
/// what the decoding probe answers -- an array receiver from the array
/// memo, a plain receiver from the receiver memo, a refusal through the
/// full path -- `checkcast`'s hit leaves its operand in place, and
/// `instanceof`'s hit leaves exactly its answer where the operand was.
#[test]
fn i51_l7_the_raw_cast_probe_answers_as_the_decoding_probe() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    for raw in [false, true] {
        let mut config = memo_config();
        config.cast_hit_raw = raw;
        let shared = Arc::new(SharedVm::new(config));
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l7/RawCastHolder", 0)
            else {
                return; // --real-jdk policy in the environment: nothing to fabricate
            };
            let Some(class) = cm.class_store.get_mut(id) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Utf8("[Ljava/lang/Object;".into()),
                E::ClassReference { name_index: 1 },
                E::Utf8("java/lang/Object".into()),
                E::ClassReference { name_index: 3 },
            ]);
            id
        };
        let string_id = {
            let cm = shared.classes.class_manager.read();
            match cm.find_unique_class_by_name("java/lang/String") {
                Some(id) => id,
                None => return, // stripped VM without `String`
            }
        };
        let mut thread = JvmThread::new(ThreadId(4251), "l7-raw-cast");
        thread.frames.push(Frame::new(
            holder,
            "cratonvm/test/l7/RawCastHolder".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        let heap = &shared.mem.heap;
        let strings = heap.alloc_array(string_id, ArrayElementType::Reference, 1);
        let ints = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        let plain = heap.alloc_object(string_id, 0);
        // `Some(answer)` on success; the stack must hold exactly the
        // operand (checkcast) or the answer (instanceof) afterwards.
        let run = |thread: &mut JvmThread, receiver: ObjectRef, insn: Instruction| {
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .unwrap();
            let outcome = execute_instruction(&shared, thread, 0, &insn, 0);
            let answer = match (insn, outcome.is_ok()) {
                (_, false) => None,
                (Instruction::Instanceof(_), true) => {
                    assert_eq!(thread.frames[0].stack.len(), 1, "raw={raw}");
                    Some(thread.frames[0].stack.pop_int().unwrap() == 1)
                }
                (_, true) => {
                    assert_eq!(thread.frames[0].stack.len(), 1, "raw={raw}");
                    let kept = thread.frames[0].stack.pop().unwrap();
                    assert!(
                        matches!(kept, Value::Object(Some(o)) if o.as_ptr() == receiver.as_ptr()),
                        "raw={raw}: checkcast hands back its operand"
                    );
                    Some(true)
                }
            };
            thread.frames[0].stack.clear();
            answer
        };
        for round in 0..3 {
            let rows = [
                (strings, Instruction::Checkcast(2), Some(true)),
                (strings, Instruction::Instanceof(2), Some(true)),
                (plain, Instruction::Checkcast(4), Some(true)),
                (plain, Instruction::Instanceof(4), Some(true)),
                (ints, Instruction::Instanceof(2), Some(false)),
                (plain, Instruction::Instanceof(2), Some(false)),
                (ints, Instruction::Checkcast(2), None),
            ];
            for (i, (receiver, insn, expected)) in rows.into_iter().enumerate() {
                let got = run(&mut thread, receiver, insn);
                assert_eq!(got, expected, "raw={raw} round {round} row {i}");
            }
        }
    }
}

/// i52-L7 (`VmConfig::cast_memo_wide`): with the switch on, a lambda
/// receiver admitted by `lambda_proxy_satisfies` fills the positive
/// receiver memo, a refused array the negative array memo, and a
/// non-array receiver refused by an array target (a lambda included) the
/// negative receiver slots; with it off none of them is filled. The
/// answers are the same in all four arms (raw or decoding probe, switch
/// on or off), on the first round and the memoised ones.
#[test]
fn i52_l7_wide_cast_memos_hold_lambdas_refused_arrays_and_array_targets() {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    // Wave 56 (lane L7): the in-place hit (`cast_hit_in_place`) is a third
    // arm of the raw probe and must answer every row the same.
    for (raw, wide, in_place) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
        (true, true, true),
    ] {
        let mut config = memo_config();
        config.cast_hit_raw = raw;
        config.cast_memo_wide = wide;
        config.cast_hit_in_place = in_place;
        let shared = Arc::new(SharedVm::new(config));
        // Everything synthetic, so the unit VM (no `java.base`) runs it:
        // the lambda's interface, and the holder, whose instances are the
        // plain receivers (the interface's name is the site's, which is
        // all `lambda_proxy_satisfies` needs to admit the proxy).
        let iface = "cratonvm/test/l7/WideIface";
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            if cm.try_ensure_synthetic_class(iface, 0).is_err() {
                return; // --real-jdk policy in the environment: nothing to fabricate
            }
            let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l7/WideCastHolder", 0)
            else {
                return; // --real-jdk policy in the environment: nothing to fabricate
            };
            let Some(class) = cm.class_store.get_mut(id) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Utf8("[Ljava/lang/Object;".into()),
                E::ClassReference { name_index: 1 },
                E::Utf8(iface.into()),
                E::ClassReference { name_index: 3 },
            ]);
            id
        };
        let proxy = shared.alloc_lambda_proxy_id();
        shared.classes.lambda_proxies.write().insert(
            proxy,
            Arc::new(LambdaCallSite {
                functional_interface_id: None,
                functional_interface: iface.into(),
                sam_method_name: "run".into(),
                sam_descriptor: "()V".into(),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: "cratonvm/test/l7/WideCastHolder".into(),
                    member_name: "lambda$0".into(),
                    descriptor: "()V".into(),
                },
                instantiated_descriptor: "()V".into(),
                capture_types: Vec::new(),
                proxy_class_id: proxy,
                serializable_flag: false,
            }),
        );
        let mut thread = JvmThread::new(ThreadId(4252), "l7-wide-cast");
        thread.frames.push(Frame::new(
            holder,
            "cratonvm/test/l7/WideCastHolder".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        let heap = &shared.mem.heap;
        let holders = heap.alloc_array(holder, ArrayElementType::Reference, 1);
        let ints = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        let plain = heap.alloc_object(holder, 0);
        let lambda = heap.alloc_object(proxy, 0);
        let run = |thread: &mut JvmThread, receiver: ObjectRef, insn: Instruction| {
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .unwrap();
            let outcome = execute_instruction(&shared, thread, 0, &insn, 0);
            let answer = match (insn, outcome.is_ok()) {
                (_, false) => None,
                (Instruction::Instanceof(_), true) => {
                    Some(thread.frames[0].stack.pop_int().unwrap() == 1)
                }
                (_, true) => Some(true),
            };
            thread.frames[0].stack.clear();
            answer
        };
        let epochs_now = || {
            (
                CastSiteCache::epochs_for(&shared),
                cast_memo_epoch_in(&shared),
            )
        };
        // Taken after the first round, whose resolutions define classes
        // (the array classes the array verdict synthesises).
        let mut epochs_before = epochs_now();
        for round in 0..4 {
            if round == 1 {
                epochs_before = epochs_now();
            }
            let rows = [
                (lambda, Instruction::Checkcast(4), Some(true)),
                (lambda, Instruction::Instanceof(4), Some(true)),
                (ints, Instruction::Instanceof(2), Some(false)),
                (plain, Instruction::Instanceof(2), Some(false)),
                (lambda, Instruction::Instanceof(2), Some(false)),
                (holders, Instruction::Instanceof(2), Some(true)),
                // A class receiver at the lambda's site.
                (plain, Instruction::Instanceof(4), Some(false)),
                (lambda, Instruction::Checkcast(4), Some(true)),
            ];
            for (i, (receiver, insn, expected)) in rows.into_iter().enumerate() {
                let got = run(&mut thread, receiver, insn);
                assert_eq!(
                    got, expected,
                    "raw={raw} wide={wide} in_place={in_place} round {round} row {i}"
                );
            }
        }
        let mut site = |index: u16| {
            thread
                .cast_sites
                .get(holder, index)
                .copied()
                .map(|site| site.observed_in(&shared))
                .expect("the site was filled")
        };
        let (lambda_site, array_site) = (site(4), site(2));
        if !wide {
            assert_ne!(lambda_site.positive_receiver, Some(proxy), "raw={raw}");
            assert_eq!(array_site.negative_array, None, "raw={raw}");
            assert!(!array_site.refuses(proxy), "raw={raw}");
            continue;
        }
        // Epochs are process-wide and other tests define classes
        // concurrently, so the fills are asserted only when nothing moved.
        let epochs_after = epochs_now();
        if epochs_after == epochs_before {
            assert_eq!(lambda_site.positive_receiver, Some(proxy), "raw={raw}");
            assert_eq!(
                array_site.negative_array,
                Some((ArrayElementType::Int, ClassId::new(0))),
                "raw={raw}"
            );
            assert!(array_site.refuses(proxy), "raw={raw}");
            assert!(array_site.refuses(holder), "raw={raw}");
        }
    }
}

/// i7-L2: the cast-site entry is tagged with the NAME generation, so its
/// receiver memos carry the definition epoch themselves. A memo proved
/// under another definition epoch is invisible once observed; one proved
/// under the observing epoch survives; the target always survives; and
/// the observed site is stamped with the observing epoch, so a memo
/// written from it (`..site`) carries the epoch read before its verdict.
#[test]
fn i7_l2_cast_memos_are_dropped_when_their_definition_epoch_moves() {
    let receiver = ClassId::new(17);
    let proved = CastSite {
        target: ClassId::new(3),
        positive_receiver: Some(receiver),
        older_positive_receiver: Some(ClassId::new(23)),
        negative_receivers: [Some(ClassId::new(18)), Some(ClassId::new(22))],
        negative_catch: Some(ClassId::new(19)),
        positive_array: Some((ArrayElementType::Reference, ClassId::new(20))),
        negative_array: Some((ArrayElementType::Int, ClassId::new(0))),
        older_positive_array_component: 24,
        memo_epoch: 40,
    };
    let same = proved.observed_at(40);
    assert_eq!(same, proved, "same epoch: every memo kept");
    let moved = proved.observed_at(41);
    assert_eq!(moved.target, ClassId::new(3), "the resolution survives");
    assert_eq!(moved.memo_epoch, 41);
    assert_eq!(
        (
            moved.positive_receiver,
            moved.older_positive_receiver,
            moved.negative_receivers,
            moved.negative_catch,
            moved.positive_array,
            moved.negative_array,
            moved.older_positive_array_component
        ),
        (
            None,
            None,
            [None; 2],
            None,
            None,
            None,
            CastSite::NO_OLDER_ARRAY_COMPONENT
        ),
        "a verdict proved under another definition epoch is not answered"
    );
    assert!(!cast_memo_answers(&moved, false, receiver));
    assert!(!cast_memo_answers(&moved, false, ClassId::new(23)));
    let refilled = CastSite {
        positive_receiver: Some(receiver),
        ..moved
    };
    assert_eq!(refilled.observed_at(41), refilled);
}

#[test]
fn monitorexit_and_monitorenter_have_distinct_npe_actions() {
    assert_eq!(monitorexit_npe_action(), "Cannot exit synchronized block");
    assert_ne!(
        crate::runtime::exceptions::helpful_npe::action_monitor(),
        monitorexit_npe_action()
    );
}

/// The negative memo answers only a NON-ARRAY receiver of exactly a
/// memoised class: a reference array's header carries its component's id.
/// i20-L5: either of the two slots answers, and nothing else does.
#[test]
fn the_negative_cast_memo_answers_only_its_own_non_array_receiver() {
    let shared = vm();
    // Wave 47: the validated-once header and the pre-wave-47 reads
    // (`None`) give the same answer on every receiver.
    fn answers(shared: &SharedVm, site: &CastSite, obj: ObjectRef) -> bool {
        let unvalidated = negative_memo_answers(shared, site, obj, None);
        let header = validated_receiver_header(shared, obj);
        assert!(header.is_some(), "a live receiver validates");
        assert_eq!(
            negative_memo_answers(shared, site, obj, header),
            unvalidated
        );
        unvalidated
    }
    let refused = ClassId::new(0);
    let site = CastSite {
        target: ClassId::new(3),
        positive_receiver: None,
        older_positive_receiver: None,
        negative_receivers: [Some(refused), None],
        negative_catch: None,
        positive_array: None,
        negative_array: None,
        older_positive_array_component: CastSite::NO_OLDER_ARRAY_COMPONENT,
        memo_epoch: 0,
    };
    let plain = shared.mem.heap.alloc_object(refused, 0);
    assert!(answers(&shared, &site, plain));
    let array = shared
        .mem
        .heap
        .alloc_array(refused, ArrayElementType::Reference, 1);
    assert!(
        !answers(&shared, &site, array),
        "an array whose component id equals the memo must not be answered"
    );
    assert!(!answers(
        &shared,
        &CastSite::resolved(ClassId::new(3)),
        plain
    ));

    // The older slot answers too, and the array exclusion holds for it.
    let other = ClassId::new(5);
    let older = CastSite {
        negative_receivers: [Some(other), Some(refused)],
        ..site
    };
    assert!(answers(&shared, &older, plain));
    assert!(
        !answers(&shared, &older, array),
        "slot 1 must not answer an array either"
    );
    let unrelated = shared.mem.heap.alloc_object(ClassId::new(6), 0);
    assert!(
        !answers(&shared, &older, unrelated),
        "a class in neither slot runs the full path"
    );

    // i52-L7: the negative ARRAY memo answers exactly its array key, in
    // both readings, and never a plain receiver.
    let refused_array = CastSite {
        negative_array: Some((ArrayElementType::Reference, refused)),
        ..CastSite::resolved(ClassId::new(3))
    };
    assert!(answers(&shared, &refused_array, array));
    assert!(!answers(&shared, &refused_array, plain));
    let ints = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
    assert!(
        !answers(&shared, &refused_array, ints),
        "another element kind"
    );
}

/// i53-L7 (`VmConfig::cast_memo_two_positive`): with the switch on, a
/// positive fill rotates slot 0 into the older slot, so two alternating
/// receivers are both answered by `cast_memo_answers` and never evict
/// each other; a third evicts the oldest. With it off, the fill is the
/// one-slot overwrite it always was and the older slot stays empty. The
/// array exclusion holds for the older slot, and the epoch clears it.
#[test]
fn i53_l7_two_positive_slots_keep_a_bimorphic_site() {
    let (a, b, c) = (ClassId::new(30), ClassId::new(31), ClassId::new(32));
    let site = CastSite::resolved(ClassId::new(3)).with_positive(a, true);
    assert_eq!(
        (site.positive_receiver, site.older_positive_receiver),
        (Some(a), None)
    );
    let site = site.with_positive(b, true);
    assert_eq!(
        (site.positive_receiver, site.older_positive_receiver),
        (Some(b), Some(a))
    );
    assert!(cast_memo_answers(&site, false, a) && cast_memo_answers(&site, false, b));
    assert!(!cast_memo_answers(&site, false, c));
    assert!(
        !cast_memo_answers(&site, true, a),
        "never for an array receiver"
    );
    assert_eq!(
        site.with_positive(a, true),
        site,
        "a known receiver is a no-op"
    );
    assert_eq!(site.with_positive(b, true), site);
    let site3 = site.with_positive(c, true);
    assert_eq!(
        (site3.positive_receiver, site3.older_positive_receiver),
        (Some(c), Some(b))
    );
    assert!(!site3.admits(a), "the oldest is dropped");
    // Switch off: one slot, overwritten, the older slot never written.
    let one = CastSite::resolved(ClassId::new(3))
        .with_positive(a, false)
        .with_positive(b, false);
    assert_eq!(
        (one.positive_receiver, one.older_positive_receiver),
        (Some(b), None)
    );
    assert!(!cast_memo_answers(&one, false, a));
    // The other memos ride along; the epoch clears both slots.
    let site = CastSite {
        negative_catch: Some(ClassId::new(41)),
        memo_epoch: 9,
        ..site
    };
    assert_eq!(
        site.with_positive(c, true).negative_catch,
        Some(ClassId::new(41))
    );
    let moved = site.observed_at(10);
    assert_eq!(
        (moved.positive_receiver, moved.older_positive_receiver),
        (None, None)
    );
}

/// i20-L5: a fill pushes the newest refusal into slot 0 and slot 0 into
/// slot 1, so a rung refusing two alternating receivers keeps both; a
/// class already in either slot leaves the order alone; a third class
/// evicts the oldest. The epoch clears both slots at once.
#[test]
fn i20_l5_the_negative_memo_keeps_the_two_newest_refusals() {
    let (a, b, c) = (ClassId::new(30), ClassId::new(31), ClassId::new(32));
    let site = CastSite::resolved(ClassId::new(3)).with_negative(a);
    assert_eq!(site.negative_receivers, [Some(a), None]);
    let site = site.with_negative(b);
    assert_eq!(site.negative_receivers, [Some(b), Some(a)]);
    assert!(site.refuses(a) && site.refuses(b) && !site.refuses(c));
    assert_eq!(site.with_negative(a), site, "a known refusal is a no-op");
    assert_eq!(site.with_negative(b), site);
    // Alternating a, b, a, b ... never evicts either.
    let mut rotating = site;
    for receiver in [a, b, a, b, a] {
        if !rotating.refuses(receiver) {
            rotating = rotating.with_negative(receiver);
        }
    }
    assert_eq!(rotating, site);
    let site = site.with_negative(c);
    assert_eq!(site.negative_receivers, [Some(c), Some(b)]);
    assert!(!site.refuses(a), "the oldest refusal is dropped");
    // The other memos are carried through untouched.
    let with_positive = CastSite {
        positive_receiver: Some(ClassId::new(40)),
        negative_catch: Some(ClassId::new(41)),
        ..site
    };
    let pushed = with_positive.with_negative(a);
    assert_eq!(pushed.positive_receiver, Some(ClassId::new(40)));
    assert_eq!(pushed.negative_catch, Some(ClassId::new(41)));
    assert_eq!(pushed.target, ClassId::new(3));
    let stamped = CastSite {
        memo_epoch: 9,
        ..pushed
    };
    assert_eq!(stamped.observed_at(10).negative_receivers, [None; 2]);
    assert_eq!(
        stamped.observed_at(9).negative_receivers,
        [Some(a), Some(c)]
    );
}

/// i20-L5, end to end through `execute_instruction`: one `instanceof`
/// site refusing receivers that alternate between two unrelated classes
/// memoises BOTH refusals, keeps answering `false` for both, and still
/// answers `true` for a subclass of the target.
#[test]
fn i20_l5_an_alternating_refusal_is_answered_from_either_negative_slot() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    let shared = memo_vm();
    let ids = {
        let mut cm = shared.classes.class_manager.write();
        let mut ids = Vec::new();
        for name in [
            "cratonvm/test/l5/NegHolder",
            "cratonvm/test/l5/NegTarget",
            "cratonvm/test/l5/NegA",
            "cratonvm/test/l5/NegB",
            "cratonvm/test/l5/NegSub",
        ] {
            match cm.try_ensure_synthetic_class(name, 0) {
                Ok(id) => ids.push(id),
                Err(_) => return, // --real-jdk policy: nothing to fabricate
            }
        }
        ids
    };
    let (holder, target, a, b, sub) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    {
        let mut cm = shared.classes.class_manager.write();
        cm.set_superclass(sub, Some(target));
        let Some(class) = cm.class_store.get_mut(holder) else {
            return;
        };
        class.constant_pool = ConstantPool::new(vec![
            E::Tombstone,
            E::Utf8("cratonvm/test/l5/NegTarget".into()),
            E::ClassReference { name_index: 1 },
        ]);
    }
    let mut thread = JvmThread::new(ThreadId(4246), "l5-neg-memo");
    thread.frames.push(Frame::new(
        holder,
        "cratonvm/test/l5/NegHolder".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    let objects = [
        shared.mem.heap.alloc_object(a, 0),
        shared.mem.heap.alloc_object(b, 0),
        shared.mem.heap.alloc_object(sub, 0),
    ];
    // `None`: the resolution failed in this stripped VM.
    let run = |thread: &mut JvmThread, receiver: ObjectRef| -> Option<bool> {
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .unwrap();
        let outcome = execute_instruction(&shared, thread, 0, &Instruction::Instanceof(2), 0);
        let answer = outcome
            .ok()
            .map(|_| thread.frames[0].stack.pop_int().unwrap() == 1);
        thread.frames[0].stack.clear();
        answer
    };
    for round in 0..6 {
        let receiver = objects[round % 2];
        match run(&mut thread, receiver) {
            None => return,
            Some(answer) => assert!(!answer, "round {round}: no subtype of the target"),
        }
    }
    // Epochs are process-wide and other tests define classes
    // concurrently, so the memo is asserted only when nothing moved.
    let memo = thread
        .cast_sites
        .get(holder, 2)
        .copied()
        .map(|site| site.observed_in(&shared))
        .map(|site| site.negative_receivers);
    if let Some(slots) = memo.filter(|slots| slots.iter().all(Option::is_some)) {
        assert!(slots.contains(&Some(a)) && slots.contains(&Some(b)));
    }
    assert_eq!(
        run(&mut thread, objects[2]),
        Some(true),
        "NegSub is a NegTarget"
    );
    assert_eq!(run(&mut thread, objects[0]), Some(false));
    assert_eq!(run(&mut thread, objects[1]), Some(false));
}

/// i56-L7 (`VmConfig::cast_hit_in_place`): the in-place hit answers
/// exactly the memos the copying protocol would -- either positive slot
/// `true`, a negative slot `false` for `instanceof` only, nothing for a
/// receiver no memo names or an entry whose memo epoch is not the
/// current one -- and leaves the entry as it was. (Array receivers never
/// reach it; `i52_l7_...` runs them through the raw probe with the
/// switch on.)
#[test]
fn i56_l7_the_in_place_cast_hit_answers_only_what_a_memo_names() {
    let shared = memo_vm();
    let ids = {
        let mut cm = shared.classes.class_manager.write();
        let mut ids = Vec::new();
        for name in [
            "cratonvm/test/l7/InPlaceHolder",
            "cratonvm/test/l7/InPlaceA",
            "cratonvm/test/l7/InPlaceB",
            "cratonvm/test/l7/InPlaceNo",
            "cratonvm/test/l7/InPlaceOther",
        ] {
            match cm.try_ensure_synthetic_class(name, 0) {
                Ok(id) => ids.push(id),
                Err(_) => return, // --real-jdk policy: nothing to fabricate
            }
        }
        ids
    };
    let (holder, oa, ob, ono, oother) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    let (a, b, no) = (oa, ob, ono);
    let mut thread = JvmThread::new(ThreadId(4256), "l7-in-place-cast");
    let site = CastSite {
        memo_epoch: cast_memo_epoch_in(&shared),
        ..CastSite::resolved(ClassId::new(3))
            .with_positive(a, true)
            .with_positive(b, true)
            .with_negative(no)
    };
    thread
        .cast_sites
        .put(holder, 7, CastSiteCache::epochs_for(&shared), site);
    if thread.cast_sites.get(holder, 7).is_none() {
        return; // an epoch moved under the put (another test defined a class)
    }
    let hit = |thread: &mut JvmThread, receiver: ClassId, instanceof: bool| {
        cast_site_hit_in_place(&shared, thread, holder, 7, receiver, instanceof)
    };
    for instanceof in [false, true] {
        assert_eq!(hit(&mut thread, oa, instanceof), Some(true), "slot 0");
        assert_eq!(
            hit(&mut thread, ob, instanceof),
            Some(true),
            "the older slot"
        );
        assert_eq!(
            hit(&mut thread, oother, instanceof),
            None,
            "no memo names it"
        );
    }
    assert_eq!(
        hit(&mut thread, ono, true),
        Some(false),
        "instanceof's refusal"
    );
    assert_eq!(
        hit(&mut thread, ono, false),
        None,
        "checkcast asks no negative memo"
    );
    assert_eq!(
        cast_site_hit_in_place(&shared, &mut thread, holder, 8, oa, false),
        None,
        "another site"
    );
    assert_eq!(
        thread.cast_sites.get(holder, 7).copied(),
        Some(site),
        "read only"
    );
    // A memo proved under another definition epoch is not answered.
    let stale = CastSite {
        memo_epoch: site.memo_epoch.wrapping_sub(1),
        ..site
    };
    thread
        .cast_sites
        .put(holder, 7, CastSiteCache::epochs_for(&shared), stale);
    if thread.cast_sites.get(holder, 7).is_some() {
        assert_eq!(hit(&mut thread, oa, false), None, "the epoch moved");
        assert_eq!(hit(&mut thread, ono, true), None, "the epoch moved");
    }
}

/// Lambda-proxy ids (and the autobox sentinel above them) read per-proxy
/// side tables, so their refusals are never memoised; an id the class
/// store does not know is not memoised either.
#[test]
fn instance_dependent_receivers_are_not_class_determined() {
    let shared = vm();
    assert!(!receiver_class_determines_cast_verdict(
        &shared,
        ClassId::new(0x8000_0001)
    ));
    assert!(!receiver_class_determines_cast_verdict(
        &shared,
        ClassId::new(u32::MAX)
    ));
    assert!(!receiver_class_determines_cast_verdict(
        &shared,
        ClassId::new(0x7fff_fff0)
    ));
}

/// The synthetic-`Boolean` screen is an identity test once the id is
/// known, and falls back to "ask by name" (answer `true`) before that.
#[test]
fn the_boolean_static_screen_is_by_identity_once_known() {
    use std::sync::atomic::Ordering::Relaxed;
    let shared = vm();
    let unknown = shared.classes.boolean_class_id.load(Relaxed);
    if unknown == u32::MAX {
        assert!(static_may_be_boolean_constant(&shared, ClassId::new(5)));
    }
    shared.classes.boolean_class_id.store(5, Relaxed);
    assert!(static_may_be_boolean_constant(&shared, ClassId::new(5)));
    assert!(!static_may_be_boolean_constant(&shared, ClassId::new(6)));
}

/// JVMS §6.5 `anewarray`: the component is resolved before the count is
/// checked, so a missing component with a negative count raises the
/// linkage error (HotSpot: `NoClassDefFoundError`), not
/// `NegativeArraySizeException`.
#[test]
fn anewarray_resolves_its_component_before_checking_the_count() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    let shared = vm();
    let holder = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l2/AnewarrayHolder", 0)
        else {
            return; // --real-jdk policy in the environment: nothing to fabricate
        };
        let Some(class) = cm.class_store.get_mut(id) else {
            return;
        };
        class.constant_pool = ConstantPool::new(vec![
            E::Tombstone,
            E::Utf8("cratonvm/test/l2/NoSuchComponent".into()),
            E::ClassReference { name_index: 1 },
            E::Utf8("[[Lcratonvm/test/l2/NoSuchComponent;".into()),
            E::ClassReference { name_index: 3 },
        ]);
        id
    };
    let is_nase = |outcome: &Result<InstructionResult, MethodCallFailed>| {
        matches!(
            outcome,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NegativeArraySizeException { .. }
            )))
        )
    };
    let mut thread = JvmThread::new(ThreadId(4243), "l2-anewarray");
    thread.frames.push(Frame::new(
        holder,
        "cratonvm/test/l2/AnewarrayHolder".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    thread.frames[0].stack.push_int(-1).unwrap();
    let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Anewarray(2), 0);
    assert!(outcome.is_err(), "a missing component cannot allocate");
    assert!(
        !is_nase(&outcome),
        "anewarray: resolution precedes the count check"
    );

    // `multianewarray`, outermost count negative: same rule.
    thread.frames[0].stack.push_int(-1).unwrap();
    thread.frames[0].stack.push_int(2).unwrap();
    let insn = Instruction::Multianewarray {
        index: 4,
        dimensions: 2,
    };
    let outcome = execute_instruction(&shared, &mut thread, 0, &insn, 0);
    assert!(outcome.is_err(), "a missing component cannot allocate");
    assert!(
        !is_nase(&outcome),
        "multianewarray: resolution precedes the count check"
    );
}

/// i11-L2: JVMS §6.5 resolves the `checkcast` / `instanceof` target for
/// every non-null receiver. Before, an ARRAY on either side answered from
/// names — `false` / `ClassCastException` — where HotSpot throws
/// `NoClassDefFoundError` for a class missing at run time.
#[test]
fn array_casts_resolve_their_class_constant() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    let shared = vm();
    let holder = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l2/CastHolder", 0) else {
            return; // --real-jdk policy in the environment: nothing to fabricate
        };
        let Some(class) = cm.class_store.get_mut(id) else {
            return;
        };
        class.constant_pool = ConstantPool::new(vec![
            E::Tombstone,
            E::Utf8("cratonvm/test/l2/NoSuchCastTarget".into()),
            E::ClassReference { name_index: 1 },
            E::Utf8("[Lcratonvm/test/l2/NoSuchCastTarget;".into()),
            E::ClassReference { name_index: 3 },
            E::Utf8("[I".into()),
            E::ClassReference { name_index: 5 },
        ]);
        id
    };
    let mut thread = JvmThread::new(ThreadId(4244), "l2-array-cast");
    thread.frames.push(Frame::new(
        holder,
        "cratonvm/test/l2/CastHolder".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    let is_cce = |outcome: &Result<InstructionResult, MethodCallFailed>| {
        matches!(
            outcome,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            )))
        )
    };
    let ints = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
    let plain = shared.mem.heap.alloc_object(holder, 0);
    let cases = [
        (ints, 2u16, "int[] against a missing class"),
        (plain, 4, "an object against a missing class's array"),
        (ints, 4, "int[] against a missing class's array"),
    ];
    for (receiver, index, label) in cases {
        for insn in [
            Instruction::Instanceof(index),
            Instruction::Checkcast(index),
        ] {
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .unwrap();
            let outcome = execute_instruction(&shared, &mut thread, 0, &insn, 0);
            assert!(outcome.is_err(), "{insn:?}, {label}: must not answer");
            assert!(
                !is_cce(&outcome),
                "{insn:?}, {label}: resolution fails before the type test"
            );
            thread.frames[0].stack.clear();
        }
    }

    // Control: a primitive-array target resolves trivially and answers.
    for (receiver, expected) in [(ints, 1), (plain, 0)] {
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .unwrap();
        let outcome =
            execute_instruction(&shared, &mut thread, 0, &Instruction::Instanceof(6), 0);
        assert!(outcome.is_ok());
        assert_eq!(thread.frames[0].stack.pop_int().unwrap(), expected);
    }
    // A null receiver resolves nothing (JVMS §6.5).
    thread.frames[0].stack.push(Value::Object(None)).unwrap();
    let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Instanceof(2), 0);
    assert!(outcome.is_ok());
    assert_eq!(thread.frames[0].stack.pop_int().unwrap(), 0);
}

/// i11-L2: which targets are resolved for an array receiver or an
/// array-type target. A primitive-element array and the three array
/// supertypes cannot fail to resolve and decide nothing the verdict needs.
#[test]
fn trivial_cast_targets_are_exactly_the_unfailing_ones() {
    for name in [
        "[I",
        "[[B",
        "java/lang/Object",
        "java/io/Serializable",
        "[Ljava/lang/Cloneable;",
        "[[Ljava/lang/Object;",
    ] {
        assert!(cast_target_resolution_is_trivial(name), "{name}");
    }
    for name in [
        "p/Missing",
        "[Lp/Missing;",
        "[[Lp/Missing;",
        "java/lang/String",
        "[Ljava/lang/String;",
        "I",
        "[Ljava/lang/Object",
    ] {
        assert!(!cast_target_resolution_is_trivial(name), "{name}");
    }
}

#[test]
fn array_opcodes_map_to_their_jep358_element_kinds() {
    assert_eq!(
        array_elem_kind_of(&Instruction::Baload),
        ArrayElemKind::Byte
    );
    assert_eq!(
        array_elem_kind_of(&Instruction::Bastore),
        ArrayElemKind::Byte
    );
    assert_eq!(
        array_elem_kind_of(&Instruction::Aastore),
        ArrayElemKind::Object
    );
    assert_eq!(
        array_elem_kind_of(&Instruction::Saload),
        ArrayElemKind::Short
    );
    assert_eq!(
        array_elem_kind_of(&Instruction::Caload),
        ArrayElemKind::Char
    );
    assert_eq!(
        array_elem_kind_of(&Instruction::Laload),
        ArrayElemKind::Long
    );
}

/// i10-L2: `checkcast`'s failure path throws; it no longer rewrites the
/// value for a caller it recognises by NAME (Spring's
/// `ConfigurationClassParser` / `ServletComponentHandler`, the log4j
/// app-loader stand-in). Those hatches were deleted after a census of
/// both modes read zero uses. A source witness: the hatches needed a
/// Spring or log4j class tree to trigger, which the unit VM does not have.
#[test]
fn checkcast_has_no_caller_name_hatches() {
    let src = std::fs::read_to_string(format!(
        "{}/src/runtime/interpreter/opcodes.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read opcodes.rs");
    let start = src
        .find("pub(super) fn op_checkcast(")
        .expect("op_checkcast must exist");
    let end = start
        + src[start..]
            .find("pub(super) fn op_new(")
            .expect("op_new follows op_checkcast");
    let body = &src[start..end];
    for needle in [
        "ConfigurationClassParser",
        "ServletComponentHandler",
        "peek_app_loader",
        "convert_class_values_to_strings",
        "AnnotationAttributes",
    ] {
        assert!(
            !body.contains(needle),
            "op_checkcast names `{needle}` again: fix the producer of the \
             value, do not rewrite it at the cast"
        );
    }
}
