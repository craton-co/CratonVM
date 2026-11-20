use super::*;

// The message templates are the half of the contract a test can pin
// without a VM: they are `Preconditions.outOfBoundsMessage` verbatim, and
// the expected strings below were taken from HotSpot JDK 25 via
// `probes/PreconditionsFormatterProbe`.

#[test]
fn check_index_message_matches_hotspot() {
    assert_eq!(
        CheckKind::Index.message(&[-1, 12]),
        "Index -1 out of bounds for length 12"
    );
    assert_eq!(
        CheckKind::Index.message(&[5, 5]),
        "Index 5 out of bounds for length 5"
    );
}

#[test]
fn check_from_to_index_message_matches_hotspot() {
    assert_eq!(
        CheckKind::FromToIndex.message(&[-1, 12, 12]),
        "Range [-1, 12) out of bounds for length 12"
    );
    assert_eq!(
        CheckKind::FromToIndex.message(&[3, 2, 12]),
        "Range [3, 2) out of bounds for length 12"
    );
    assert_eq!(
        CheckKind::FromToIndex.message(&[i64::from(i32::MIN), i64::from(i32::MAX), 12]),
        "Range [-2147483648, 2147483647) out of bounds for length 12"
    );
}

#[test]
fn check_from_index_size_message_repeats_the_from_index() {
    // `%<s` in the JDK's format string — the second placeholder is `from`
    // again, not `size`. Getting it wrong reads as an off-by-one in the
    // message rather than as a typo.
    assert_eq!(
        CheckKind::FromIndexSize.message(&[-1, 2, 12]),
        "Range [-1, -1 + 2) out of bounds for length 12"
    );
    assert_eq!(
        CheckKind::FromIndexSize.message(&[0, 999, 12]),
        "Range [0, 0 + 999) out of bounds for length 12"
    );
}

#[test]
fn wrong_arity_takes_the_jdk_default_arm() {
    assert_eq!(
        CheckKind::Index.message(&[1, 2, 3]),
        "Range check failed: checkIndex [1, 2, 3]"
    );
}

#[test]
fn check_kind_names_are_the_strings_out_of_bounds_message_switches_on() {
    assert_eq!(CheckKind::Index.name(), "checkIndex");
    assert_eq!(CheckKind::FromToIndex.name(), "checkFromToIndex");
    assert_eq!(CheckKind::FromIndexSize.name(), "checkFromIndexSize");
}

/// The regression that silently restores the whole bug.
///
/// With `<clinit>` stubbed out, `SIOOBE_FORMATTER` and its two siblings are
/// null, so `String.checkBoundsBeginEnd` hands this module a null formatter
/// and every `String` bounds failure takes the no-formatter fallback —
/// `IndexOutOfBoundsException` instead of
/// `StringIndexOutOfBoundsException`. Nothing errors; the class is just
/// quietly wrong again. The previous stub was justified by a
/// `LambdaMetafactory` bootstrap hazard that the JDK removed several
/// releases ago by rewriting the formatters as anonymous inner classes.
#[test]
fn clinit_is_not_stubbed_out() {
    let mut registry = NativeMethodRegistry::new();
    register(&mut registry);
    assert!(
        registry
            .find("jdk/internal/util/Preconditions", "<clinit>", "()V")
            .is_none(),
        "`Preconditions.<clinit>` must run: it builds the three static \
         formatters that `throw_out_of_bounds` reads to tell \
         StringIndexOutOfBoundsException from IndexOutOfBoundsException"
    );
}

#[test]
fn every_bounds_check_overload_is_covered() {
    let mut registry = NativeMethodRegistry::new();
    register(&mut registry);
    // The formatter-taking overloads are the ones the defect was about;
    // the bare shapes route through the same code with an absent
    // formatter. A missing entry hands that method to bytecode, which is
    // correct but costs a Java frame on `String.charAt`'s per-character
    // path — the only reason any of this is native.
    for (name, descriptor) in [
        ("checkIndex", "(IILjava/util/function/BiFunction;)I"),
        ("checkFromToIndex", "(IIILjava/util/function/BiFunction;)I"),
        (
            "checkFromIndexSize",
            "(IIILjava/util/function/BiFunction;)I",
        ),
        ("checkIndex", "(II)I"),
        ("checkFromToIndex", "(III)I"),
        ("checkFromIndexSize", "(III)I"),
    ] {
        assert!(
            registry
                .find("jdk/internal/util/Preconditions", name, descriptor)
                .is_some(),
            "jdk/internal/util/Preconditions.{name}{descriptor} is not registered"
        );
    }
}

#[test]
fn the_null_formatter_fallback_is_not_a_subclass_of_the_promised_class() {
    // Defect 2 of the known-issues record: the old code threw
    // `ArrayIndexOutOfBoundsException` here, which is a *subclass* of what
    // `Objects.check*` promises — the direction that breaks a `catch`.
    assert_eq!(FALLBACK_EXCEPTION, "java/lang/IndexOutOfBoundsException");
}

// --- JIT round 16 wave 8 (lane trace): the raise through the JDK body ---

/// A body that RETURNED, or failed inside the VM, raised nothing: the
/// caller keeps its own raise.
#[test]
fn a_body_that_did_not_throw_is_not_a_raise() {
    let mut ctx = crate::test_utils::mock_ctx();
    assert!(accept_bytecode_raise(&mut ctx, Ok(Some(Value::Int(3))), None).is_none());
    assert!(accept_bytecode_raise(&mut ctx, Ok(None), None).is_none());
    let internal: MethodCallResult = Err(MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::Internal {
            message: "early boot".to_string(),
        },
    ));
    assert!(accept_bytecode_raise(&mut ctx, internal, None).is_none());
}

/// With no expected class (an application formatter) any throwable is
/// the raise; with one, only an instance of it (or a subclass).
#[test]
fn the_raise_is_screened_by_its_expected_class() {
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    let mut ctx = crate::test_utils::mock_ctx();
    let ioobe = ctx
        .ensure_class_initialized("java/lang/IndexOutOfBoundsException")
        .unwrap();
    let sioobe = ctx
        .ensure_class_initialized("java/lang/StringIndexOutOfBoundsException")
        .unwrap();
    ctx.set_superclass(sioobe, ioobe);
    let thrown = match ctx.new_object("java/lang/StringIndexOutOfBoundsException") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("mock allocation failed: {other:?}"),
    };
    let raised = |ctx: &mut crate::test_utils::MockNativeContext, expected| {
        accept_bytecode_raise(
            ctx,
            Err(MethodCallFailed::ExceptionThrown(thrown)),
            expected,
        )
    };
    assert!(matches!(
        raised(&mut ctx, None),
        Some(MethodCallFailed::ExceptionThrown(o)) if o == thrown
    ));
    assert!(raised(&mut ctx, Some("java/lang/StringIndexOutOfBoundsException")).is_some());
    assert!(
        raised(&mut ctx, Some("java/lang/IndexOutOfBoundsException")).is_some(),
        "a subclass of the expected class is accepted"
    );
    let _aioobe = ctx
        .ensure_class_initialized("java/lang/ArrayIndexOutOfBoundsException")
        .unwrap();
    assert!(
        raised(&mut ctx, Some("java/lang/ArrayIndexOutOfBoundsException")).is_none(),
        "a sibling class is refused: the native's own raise is kept"
    );
    assert!(
        raised(&mut ctx, Some("no/such/Class")).is_none(),
        "an expected class that is not loaded refuses (fail-closed)"
    );
}

/// The JDK-shaped rows name a method with a JDK body; the check still
/// raises when that body cannot (the mock runs no bytecode).
#[test]
fn a_failed_check_still_raises_when_the_body_cannot() {
    assert_eq!(
        formatter_descriptor(CheckKind::Index),
        "(IILjava/util/function/BiFunction;)I"
    );
    assert_eq!(
        formatter_descriptor(CheckKind::FromToIndex),
        "(IIILjava/util/function/BiFunction;)I"
    );
    assert_eq!(
        formatter_descriptor(CheckKind::FromIndexSize),
        "(IIILjava/util/function/BiFunction;)I"
    );
    let mut ctx = crate::test_utils::mock_ctx();
    let four = [Value::Int(5), Value::Int(3), Value::Object(None)];
    assert!(check_index(&mut ctx, &four).is_err());
    let bare = [Value::Int(5), Value::Int(3)];
    assert!(check_index(&mut ctx, &bare).is_err());
    let range = [
        Value::Int(2),
        Value::Int(1),
        Value::Int(3),
        Value::Object(None),
    ];
    assert!(check_from_to_index(&mut ctx, &range).is_err());
    let size = [Value::Int(2), Value::Int(9), Value::Int(3), Value::Int(0)];
    assert!(check_from_index_size(&mut ctx, &size).is_err());
    // The in-range answers are unchanged.
    assert_eq!(
        check_index(
            &mut ctx,
            &[Value::Int(1), Value::Int(3), Value::Object(None)]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
}
