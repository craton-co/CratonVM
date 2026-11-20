//! JIT round 20 wave 18, lane xThrow (THROW20-5,
//! `CRATONVM_JIT_IMPLICIT_THROW_LEAN_BUILD`): the lean direct build makes
//! the same message text and the same throwable as before.
use super::*;
use crate::config::VmConfig;
use crate::vm::Vm;

#[test]
fn the_buffered_aioobe_message_is_the_formatted_one() {
    for (index, length) in [
        (5_i64, 3_i64),
        (-1, 0),
        (0, 0),
        (i64::from(i32::MIN), i64::from(i32::MAX)),
        (i64::MIN, i64::MIN),
        (i64::MAX, i64::MIN),
    ] {
        let mut buf: AioobeMessageBuf = [0; 96];
        let want = format!("Index {index} out of bounds for length {length}");
        assert_eq!(
            format_aioobe_message(&mut buf, index, length),
            Some(want.as_str()),
            "{index} {length}"
        );
    }
}

/// Three classes always; three more (JIT round 23 wave 1, lane L3) only
/// under `CRATONVM_THROWABLE_DIRECT_IMPLICIT_BUILD_EVERY_DOOR`; every slot
/// fits the per-thread row table.
#[test]
fn only_the_implicit_classes_have_a_class_slot() {
    for (name, off, on) in [
        ("java/lang/ArrayIndexOutOfBoundsException", Some(0), Some(0)),
        ("java/lang/ArithmeticException", Some(1), Some(1)),
        ("java/lang/NullPointerException", Some(2), Some(2)),
        ("java/lang/ClassCastException", None, Some(3)),
        ("java/lang/ArrayStoreException", None, Some(4)),
        ("java/lang/NegativeArraySizeException", None, Some(5)),
        ("java/lang/IndexOutOfBoundsException", None, None),
        ("java/lang/StringIndexOutOfBoundsException", None, None),
        ("java/lang/IllegalMonitorStateException", None, None),
    ] {
        assert_eq!(implicit_direct_class_slot_as(name, false), off, "{name}");
        assert_eq!(implicit_direct_class_slot_as(name, true), on, "{name}");
        if let Some(slot) = on {
            assert!(slot < IMPLICIT_DIRECT_CLASS_SLOTS);
        }
    }
}

/// JIT round 23 wave 1, lane L3: the operand tables name what the loaded
/// JDK's bytes name. A method whose opcodes have the modelled shape must
/// also pass the operand check, or the direct build would refuse every
/// throw on this class library (a silent loss, not a wrong answer). Skipped
/// where the class is not loaded or its `Code` is not decoded.
#[test]
fn the_loaded_jdk_chain_names_the_modelled_members() {
    let vm = Vm::new(VmConfig::default());
    let cm = vm.shared.classes.class_manager.read();
    let class_of = |name: &str| {
        cm.loaded_class_under_exact_key(name, cratonvm_types::ClassLoaderId::Bootstrap)
            .and_then(|id| cm.get_class(id))
    };
    if let Some(cls) = class_of("java/lang/Throwable") {
        let cp = &cls.constant_pool;
        for (name, descriptor, shape, refs) in [
            (
                "<init>",
                "(Ljava/lang/String;)V",
                THROWABLE_INIT_STRING,
                THROWABLE_INIT_STRING_REFS,
            ),
            (
                "<init>",
                "()V",
                THROWABLE_INIT_EMPTY,
                THROWABLE_INIT_EMPTY_REFS,
            ),
            (
                "fillInStackTrace",
                "()Ljava/lang/Throwable;",
                THROWABLE_FILL_IN_STACK_TRACE,
                THROWABLE_FILL_REFS,
            ),
        ] {
            let Some(m) = cls.find_method(name, descriptor) else {
                continue;
            };
            if method_has_shape(m, shape) {
                assert!(
                    method_has_shape_and_refs(cp, m, shape, refs),
                    "Throwable.{name}{descriptor}"
                );
            }
        }
    }
    if let Some(cls) = class_of("java/lang/NullPointerException") {
        if let Some(m) = cls.find_method("fillInStackTrace", "()Ljava/lang/Throwable;") {
            if method_has_shape(m, NPE_FILL_IN_STACK_TRACE) {
                assert!(method_has_shape_and_refs(
                    &cls.constant_pool,
                    m,
                    NPE_FILL_IN_STACK_TRACE,
                    NPE_FILL_REFS
                ));
            }
        }
    }
    // Every delegating constructor of the six names its superclass's.
    for name in [
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/IndexOutOfBoundsException",
        "java/lang/ArithmeticException",
        "java/lang/NullPointerException",
        "java/lang/ClassCastException",
        "java/lang/ArrayStoreException",
        "java/lang/NegativeArraySizeException",
        "java/lang/RuntimeException",
        "java/lang/Exception",
    ] {
        let Some(cls) = class_of(name) else {
            continue;
        };
        let Some(super_name) = cls
            .superclass
            .and_then(|s| cm.get_class(s))
            .map(|s| s.name.clone())
        else {
            continue;
        };
        for (descriptor, shape, operand) in [
            ("(Ljava/lang/String;)V", DELEGATE_INIT_STRING, 3),
            ("()V", DELEGATE_INIT_EMPTY, 2),
        ] {
            let Some(code) = cls
                .find_method("<init>", descriptor)
                .filter(|m| method_has_shape(m, shape))
                .and_then(|m| m.code())
            else {
                continue;
            };
            assert_eq!(
                cp_member_at(&cls.constant_pool, &code.code, operand),
                Some((&*super_name, "<init>", descriptor)),
                "{name}.<init>{descriptor}"
            );
        }
    }
}

/// The operand check refuses a pool entry of the wrong member, of the
/// wrong kind, and an operand past the code (pure, on a hand-built pool).
#[test]
fn the_operand_check_refuses_another_member() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry, CpUtf8};
    const CAUSE: &str = "Ljava/lang/Throwable;";
    let utf8 = |s: &str| ConstantPoolEntry::Utf8(CpUtf8::from_arc(std::sync::Arc::from(s)));
    // Indices 0..=6: the unused slot, the owner's name and class, the
    // member's name and type, the field reference.
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        utf8("java/lang/Throwable"),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        utf8("cause"),
        utf8(CAUSE),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        ConstantPoolEntry::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        },
    ]);
    let code = [0x2a, 0xb5, 0x00, 0x06, 0xb5, 0x00, 0x05];
    let check = |r: CpRef| code_refs_match(&cp, &code, &[r]);
    let t = THROWABLE;
    assert!(check((2, t, "cause", CAUSE)));
    assert!(check((2, None, "cause", CAUSE)));
    // Another owner, another name, another descriptor.
    assert!(!check((2, Some("java/lang/Object"), "cause", CAUSE)));
    assert!(!check((2, t, "detailMessage", CAUSE)));
    assert!(!check((2, t, "cause", "I")));
    // A NameAndType where a member reference belongs.
    assert!(!check((5, t, "cause", CAUSE)));
    // Past the end.
    assert!(!check((6, t, "cause", CAUSE)));
}

/// JIT round 23 wave 1, lane L3: for each of the six implicit kinds, the
/// direct build stores what the constructor chain stores -- the class, the
/// message, `cause = this`, `stackTrace = UNASSIGNED_STACK`, the
/// suppressed sentinel, the depth, and an NPE's `extendedMessageState`.
/// A kind the class library does not let the door build is skipped (the
/// door then refuses and the constructors run, which is the other arm).
#[test]
fn every_door_builds_what_the_constructors_build() {
    let mut vm = Vm::new(VmConfig::default());
    for (class, msg) in [
        (
            "java/lang/ArrayIndexOutOfBoundsException",
            Some("Index 3 out of bounds for length 2"),
        ),
        ("java/lang/ArithmeticException", Some("/ by zero")),
        ("java/lang/NullPointerException", None),
        (
            "java/lang/ClassCastException",
            Some("class java.lang.Integer cannot be cast to class java.lang.String"),
        ),
        ("java/lang/ArrayStoreException", Some("java.lang.Integer")),
        ("java/lang/NegativeArraySizeException", Some("-1")),
    ] {
        let Ok(constructed) =
            create_exception_object_constructed(&vm.shared, &mut vm.main_thread, class, msg)
        else {
            continue;
        };
        let pin = vm.main_thread.native_pin_roots.len();
        vm.main_thread.native_pin_roots.push(constructed);
        let direct =
            try_build_implicit_throwable_directly(&vm.shared, &mut vm.main_thread, class, msg);
        let constructed = vm.main_thread.native_pin_roots[pin];
        vm.main_thread.native_pin_roots.truncate(pin);
        let Some(direct) = direct else {
            continue;
        };
        let direct = direct.expect("a direct build never answers an error");
        let shared = &vm.shared;
        let heap = &shared.mem.heap;
        assert_ne!(direct, constructed, "{class}");
        assert_eq!(
            heap.class_id_of(direct),
            heap.class_id_of(constructed),
            "{class}"
        );
        let text = |exc: ObjectRef| {
            let slot = throwable_own_field_index(shared, "detailMessage")?;
            match heap.get_field(exc, slot) {
                Value::Object(Some(s)) => crate::vm::read_java_string(heap, s),
                _ => None,
            }
        };
        assert_eq!(text(direct), text(constructed), "{class}: detailMessage");
        assert_eq!(text(direct).as_deref(), msg, "{class}: detailMessage text");
        if let Some(slot) = throwable_own_field_index(shared, "cause") {
            assert_eq!(
                heap.get_field(direct, slot),
                Value::Object(Some(direct)),
                "{class}"
            );
        }
        for name in ["stackTrace", "suppressedExceptions", "depth"] {
            if let Some(slot) = throwable_own_field_index(shared, name) {
                assert_eq!(
                    heap.get_field(direct, slot),
                    heap.get_field(constructed, slot),
                    "{class}: {name}"
                );
            }
        }
        if let Some((_, state)) = npe_extended_message_slots(shared, heap.class_id_of(direct)) {
            assert_eq!(
                heap.get_field(direct, state),
                heap.get_field(constructed, state),
                "{class}: extendedMessageState"
            );
        }
    }
}

/// Two direct builds of the same class: the second answers its class from
/// the per-thread row the first kept, and both carry the class and the
/// message the constructor chain would store. Skipped where the class
/// library does not let the door build at all.
#[test]
fn a_kept_class_row_builds_the_same_throwable() {
    const AIOOBE: &str = "java/lang/ArrayIndexOutOfBoundsException";
    let mut vm = Vm::new(VmConfig::default());
    let msg = "Index 5 out of bounds for length 3";
    let Some(Ok(first)) = try_build_implicit_throwable_directly(
        &vm.shared,
        &mut vm.main_thread,
        AIOOBE,
        Some(msg),
    ) else {
        return;
    };
    let class_id = vm.shared.mem.heap.class_id_of(first);
    let loaded = vm
        .shared
        .classes
        .class_manager
        .read()
        .loaded_class_under_exact_key(AIOOBE, cratonvm_types::ClassLoaderId::Bootstrap);
    assert_eq!(Some(class_id), loaded);
    if implicit_throw_lean_build_enabled() {
        assert_eq!(
            cached_implicit_direct_class(&vm.shared, 0).map(|(id, _)| id),
            Some(class_id)
        );
        // A slot nothing was kept in answers nothing.
        assert_eq!(cached_implicit_direct_class(&vm.shared, 1), None);
    }
    // Pinned across the second build, which captures (and may collect).
    let pin = vm.main_thread.native_pin_roots.len();
    vm.main_thread.native_pin_roots.push(first);
    let Some(Ok(second)) = try_build_implicit_throwable_directly(
        &vm.shared,
        &mut vm.main_thread,
        AIOOBE,
        Some(msg),
    ) else {
        panic!("a class that built once builds again");
    };
    let first = vm.main_thread.native_pin_roots[pin];
    vm.main_thread.native_pin_roots.truncate(pin);
    let shared = &vm.shared;
    assert_ne!(first, second);
    assert_eq!(shared.mem.heap.class_id_of(second), class_id);
    let Some(slot) = throwable_own_field_index(shared, "detailMessage") else {
        return;
    };
    for exc in [first, second] {
        let Value::Object(Some(text)) = shared.mem.heap.get_field(exc, slot) else {
            panic!("the message is stored");
        };
        assert_eq!(
            crate::vm::read_java_string(&shared.mem.heap, text).as_deref(),
            Some(msg)
        );
    }
}
