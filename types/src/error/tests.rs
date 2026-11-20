use super::*;

/// Interpreter round i4 wave 85, lane L2: the interpreter's deferral
/// request is never a Java-visible message, wherever the error is
/// converted; an ordinary message is untouched.
#[test]
fn interp_deferred_npe_request_is_no_message() {
    let deferred = RuntimeError::NullPointerException {
        message: Some(INTERP_DEFERRED_NPE_MESSAGE.to_string()),
    };
    let (cls, msg) = deferred.as_java_throwable().unwrap();
    assert_eq!(cls, "java/lang/NullPointerException");
    assert_eq!(msg, None);
    let plain = RuntimeError::NullPointerException {
        message: Some("Cannot read the array length".to_string()),
    };
    let (_, msg) = plain.as_java_throwable().unwrap();
    assert_eq!(msg.as_deref(), Some("Cannot read the array length"));
}

/// gc-common w2-d: the ladders' diagnostic suffix never reaches
/// `Throwable.detailMessage`; every other OOME message is untouched.
#[test]
fn oome_site_detail_is_not_java_visible() {
    let e = RuntimeError::OutOfMemoryError {
        message: "Java heap space (alloc_array length 268435456)".into(),
    };
    let (cls, msg) = e.as_java_throwable().unwrap();
    assert_eq!(cls, "java/lang/OutOfMemoryError");
    assert_eq!(msg.as_deref(), Some("Java heap space"));
    for unchanged in [
        "Java heap space",
        "Requested array size exceeds VM limit",
        "Metaspace",
        "Java heap spaceship",
        "heap full",
    ] {
        let e = RuntimeError::OutOfMemoryError {
            message: unchanged.into(),
        };
        assert_eq!(
            e.as_java_throwable().unwrap().1.as_deref(),
            Some(unchanged),
            "{unchanged:?} must pass through"
        );
    }
    // The Rust-side Display keeps the site detail for diagnostics.
    let e = RuntimeError::OutOfMemoryError {
        message: "Java heap space (String of 3 UTF-8 bytes)".into(),
    };
    assert_eq!(
        format!("{e}"),
        "OutOfMemoryError: Java heap space (String of 3 UTF-8 bytes)"
    );
}

// -- as_java_throwable detail messages --

/// The two variants that carry an `i32` payload used to convert with a
/// `None` message, so every VM-thrown AIOOBE reached Java as
/// `java.lang.ArrayIndexOutOfBoundsException: null`. That is a behavioural
/// divergence for anything reading `getMessage()`, and it is what made a
/// Spring AOT failure undiagnosable.
///
/// Expected text measured on HotSpot (Temurin jdk-25.0.3+9).
#[test]
fn payload_carrying_variants_synthesise_hotspots_message() {
    let err = RuntimeError::aioobe_index_only(7);
    let (cls, msg) = err.as_java_throwable().expect("AIOOBE is a Java throwable");
    assert_eq!(cls, "java/lang/ArrayIndexOutOfBoundsException");
    // `new ArrayIndexOutOfBoundsException(7)` on HotSpot — the wording for
    // a site that knows the index and not the length. An array access
    // takes `aioobe` and says something else; see the test below.
    assert_eq!(msg.as_deref(), Some("Array index out of range: 7"));

    let (cls, msg) = RuntimeError::NegativeArraySizeException { size: -1 }
        .as_java_throwable()
        .expect("NegativeArraySizeException is a Java throwable");
    assert_eq!(cls, "java/lang/NegativeArraySizeException");
    // `new int[-1]` on HotSpot: the size alone, no prose.
    assert_eq!(msg.as_deref(), Some("-1"));
}

/// `None` must keep meaning "construct with the no-arg constructor, so
/// `getMessage()` is null". Several variants depend on that distinction and
/// the synthesising path must not have blurred it.
#[test]
fn variants_with_no_message_still_convert_to_none() {
    for err in [
        RuntimeError::StackOverflowError,
        RuntimeError::InterruptedException,
        RuntimeError::BufferUnderflowException,
        RuntimeError::BufferOverflowException,
        RuntimeError::ReadOnlyBufferException,
        RuntimeError::ConcurrentModificationException,
        RuntimeError::EmptyStackException,
    ] {
        let (_, msg) = err.as_java_throwable().expect("is a Java throwable");
        assert!(msg.is_none(), "{err:?} must have a null detail message");
    }

    // The empty-string markers are "no message" too, not an empty one.
    // Bound to a `let` because the returned `Cow` borrows from the error.
    let npe = RuntimeError::NullPointerException {
        message: Some(String::new()),
    };
    let (_, msg) = npe.as_java_throwable().unwrap();
    assert!(
        msg.is_none(),
        "an empty NPE marker means getMessage() == null"
    );

    let uoe = RuntimeError::UnsupportedOperationException {
        message: String::new(),
    };
    let (_, msg) = uoe.as_java_throwable().unwrap();
    assert!(
        msg.is_none(),
        "an empty UOE message means getMessage() == null"
    );

    // `Array.set(new int[4], 0, null)` on HotSpot: an IllegalArgumentException
    // with a null message, not an empty one.
    let iae = RuntimeError::IllegalArgumentException {
        message: String::new(),
    };
    let (_, msg) = iae.as_java_throwable().unwrap();
    assert!(
        msg.is_none(),
        "an empty IAE message means getMessage() == null"
    );

    let iae = RuntimeError::IllegalArgumentException {
        message: "argument type mismatch".to_string(),
    };
    let (_, msg) = iae.as_java_throwable().unwrap();
    assert_eq!(msg.as_deref(), Some("argument type mismatch"));
}

/// A variant that owns a message is still BORROWED, not copied — the `Cow`
/// exists so only the two synthesising variants pay for an allocation.
#[test]
fn owned_messages_are_borrowed_not_copied() {
    let err = RuntimeError::IllegalStateException {
        message: "boom".to_string(),
    };
    let (_, msg) = err.as_java_throwable().unwrap();
    assert!(matches!(msg, Some(Cow::Borrowed("boom"))));

    // The AIOOBE variant owns its message since 2026-08-06, so it
    // borrows like the rest. `NegativeArraySizeException` is now the only
    // variant that still has to build one.
    let err = RuntimeError::aioobe(9, 4);
    let (_, msg) = err.as_java_throwable().unwrap();
    assert!(matches!(
        msg,
        Some(Cow::Borrowed("Index 9 out of bounds for length 4"))
    ));

    let err = RuntimeError::NegativeArraySizeException { size: -1 };
    let (_, msg) = err.as_java_throwable().unwrap();
    assert!(matches!(msg, Some(Cow::Owned(_))));
}

/// The three wordings are three different HotSpot behaviours, not three
/// spellings of one. A single blanket message for the variant — which is
/// what `synthesised_detail_message` used to do — cannot be right for all
/// three at once, and that is why the call site chooses.
#[test]
fn the_three_aioobe_constructors_do_not_agree() {
    let access = RuntimeError::aioobe(9, 4);
    let index_only = RuntimeError::aioobe_index_only(9);
    let reflective = RuntimeError::aioobe_no_message(9);
    assert_eq!(
        access.as_java_throwable().and_then(|(_, m)| m).as_deref(),
        Some("Index 9 out of bounds for length 4")
    );
    assert_eq!(
        index_only
            .as_java_throwable()
            .and_then(|(_, m)| m)
            .as_deref(),
        Some("Array index out of range: 9")
    );
    assert_eq!(
        reflective
            .as_java_throwable()
            .and_then(|(_, m)| m)
            .as_deref(),
        None
    );
}

/// `NotImplemented` is a VM gap, not something Java can catch.
#[test]
fn not_implemented_is_not_a_java_throwable() {
    let err = RuntimeError::NotImplemented {
        feature: "whatever".to_string(),
    };
    assert!(err.as_java_throwable().is_none());
}

// -- VmError Display tests --

#[test]
fn vm_error_class_file_display() {
    let err = VmError::ClassFile(ClassFileError::ClassNotFound {
        class_name: "com/example/Foo".into(),
    });
    assert_eq!(
        format!("{err}"),
        "class file error: class not found: com/example/Foo"
    );
}

#[test]
fn vm_error_linkage_display() {
    let err = VmError::Linkage(LinkageError::NoClassDefFoundError {
        class_name: "Bar".into(),
    });
    assert_eq!(format!("{err}"), "linkage error: no class def found: Bar");
}

#[test]
fn vm_error_runtime_display() {
    let err = VmError::Runtime(RuntimeError::StackOverflowError);
    assert_eq!(format!("{err}"), "runtime error: StackOverflowError");
}

#[test]
fn vm_error_internal_display() {
    let err = VmError::Internal {
        message: "something broke".into(),
    };
    assert_eq!(format!("{err}"), "internal error: something broke");
}

// -- VmError From conversions --

#[test]
fn vm_error_from_class_file_error() {
    let cfe = ClassFileError::ClassNotFound {
        class_name: "X".into(),
    };
    let vm_err: VmError = cfe.into();
    assert!(matches!(vm_err, VmError::ClassFile(_)));
}

#[test]
fn vm_error_from_linkage_error() {
    let le = LinkageError::IllegalAccessError {
        message: "denied".into(),
    };
    let vm_err: VmError = le.into();
    assert!(matches!(vm_err, VmError::Linkage(_)));
}

#[test]
fn vm_error_from_runtime_error() {
    let re = RuntimeError::StackOverflowError;
    let vm_err: VmError = re.into();
    assert!(matches!(vm_err, VmError::Runtime(_)));
}

// -- ClassFileError variants --

#[test]
fn class_file_error_class_not_found() {
    let err = ClassFileError::ClassNotFound {
        class_name: "java/lang/Object".into(),
    };
    assert_eq!(format!("{err}"), "class not found: java/lang/Object");
}

#[test]
fn class_file_error_io_error() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
    let err = ClassFileError::IoError {
        class_name: "Test".into(),
        source: io_err,
    };
    let display = format!("{err}");
    assert!(display.contains("I/O error reading class Test"));
    assert!(display.contains("file gone"));
}

#[test]
fn class_file_error_invalid_class_file() {
    let err = ClassFileError::InvalidClassFile {
        class_name: "Bad".into(),
        message: "bad magic number".into(),
    };
    assert_eq!(format!("{err}"), "invalid class file Bad: bad magic number");
}

#[test]
fn class_file_error_unsupported_version() {
    let err = ClassFileError::UnsupportedVersion {
        class_name: "Future".into(),
        major: 99,
        minor: 0,
    };
    assert_eq!(
        format!("{err}"),
        "unsupported class version 99.0 for class Future"
    );
}

// -- LinkageError variants --

#[test]
fn linkage_error_class_format_error() {
    let err = LinkageError::ClassFormatError {
        class_name: "X".into(),
        message: "corrupt".into(),
    };
    assert_eq!(format!("{err}"), "class format error in X: corrupt");
}

#[test]
fn linkage_error_verify_error() {
    let err = LinkageError::VerifyError {
        class_name: "A".into(),
        method_name: "foo".into(),
        message: "bad stack".into(),
    };
    assert_eq!(format!("{err}"), "verification error in A.foo: bad stack");
}

#[test]
fn linkage_error_no_such_field() {
    let err = LinkageError::NoSuchFieldError {
        class_name: "C".into(),
        field_name: "x".into(),
        field_descriptor: "I".into(),
    };
    assert_eq!(format!("{err}"), "no such field: C.x");
}

#[test]
fn linkage_error_no_such_method() {
    let err = LinkageError::NoSuchMethodError {
        class_name: "C".into(),
        method_name: "run".into(),
        method_descriptor: "(I)V".into(),
    };
    assert_eq!(format!("{err}"), "no such method: C.run(I)V");
}

#[test]
fn linkage_error_incompatible_class_change() {
    let err = LinkageError::IncompatibleClassChangeError {
        message: "interface became class".into(),
    };
    assert_eq!(
        format!("{err}"),
        "incompatible class change: interface became class"
    );
}

#[test]
fn linkage_error_illegal_access() {
    let err = LinkageError::IllegalAccessError {
        message: "private".into(),
    };
    assert_eq!(format!("{err}"), "illegal access: private");
}

#[test]
fn linkage_error_abstract_method() {
    let err = LinkageError::AbstractMethodError {
        class_name: "I".into(),
        method_name: "doIt".into(),
    };
    assert_eq!(format!("{err}"), "abstract method error: I.doIt");
}

// -- RuntimeError variants --

#[test]
fn runtime_error_null_pointer_with_message() {
    let err = RuntimeError::NullPointerException {
        message: Some("field access".into()),
    };
    assert_eq!(format!("{err}"), "NullPointerException: field access");
}

#[test]
fn runtime_error_null_pointer_without_message() {
    let err = RuntimeError::NullPointerException { message: None };
    assert_eq!(format!("{err}"), "NullPointerException");
}

#[test]
fn runtime_error_array_index_out_of_bounds() {
    let err = RuntimeError::aioobe_no_message(-1);
    assert_eq!(format!("{err}"), "ArrayIndexOutOfBoundsException: index -1");
    assert_eq!(
        err.as_java_throwable().and_then(|(_, m)| m).as_deref(),
        None,
        "aioobe_no_message must produce a message-less throwable, i.e. the \
         no-arg constructor — HotSpot's reflect.Array behaviour"
    );
}

#[test]
fn aioobe_carries_hotspots_array_access_wording() {
    let err = RuntimeError::aioobe(9, 4);
    let (cls, msg) = err.as_java_throwable().expect("is a Java throwable");
    assert_eq!(cls, "java/lang/ArrayIndexOutOfBoundsException");
    assert_eq!(msg.as_deref(), Some("Index 9 out of bounds for length 4"));
    // The array-access wording IS `Preconditions.checkIndex`'s; if these
    // two ever diverge, one of them has been rewritten by hand.
    assert_eq!(
        out_of_bounds_message::check_index(9, 4),
        "Index 9 out of bounds for length 4"
    );
}

#[test]
fn aioobe_negative_index_still_names_the_length() {
    // HotSpot prints the negative index verbatim rather than clamping.
    let err = RuntimeError::aioobe(-1, 4);
    assert_eq!(
        err.as_java_throwable().and_then(|(_, m)| m).as_deref(),
        Some("Index -1 out of bounds for length 4")
    );
}

#[test]
fn arraycopy_wordings_match_hotspot() {
    use arraycopy_message as ac;
    assert_eq!(
        ac::last_source_index(0, 9, "int", 4),
        "arraycopy: last source index 9 out of bounds for int[4]"
    );
    assert_eq!(
        ac::last_destination_index(0, 9, "char", 4),
        "arraycopy: last destination index 9 out of bounds for char[4]"
    );
    assert_eq!(
        ac::source_index(-1, "short", 4),
        "arraycopy: source index -1 out of bounds for short[4]"
    );
    assert_eq!(
        ac::destination_index(-1, "float", 4),
        "arraycopy: destination index -1 out of bounds for float[4]"
    );
    assert_eq!(ac::negative_length(-1), "arraycopy: length -1 is negative");
    // A reference array is "object array", never its own class name.
    assert_eq!(
        ac::last_source_index(0, 9, "object array", 4),
        "arraycopy: last source index 9 out of bounds for object array[4]"
    );
    assert_eq!(
        ac::type_mismatch("int", "object array"),
        "arraycopy: type mismatch: can not copy int[] into object array[]"
    );
    assert_eq!(
        ac::source_not_an_array("java.lang.String"),
        "arraycopy: source type java.lang.String is not an array"
    );
}

#[test]
fn arraycopy_element_type_names_are_hotspots() {
    use arraycopy_message::element_type_name as name;
    assert_eq!(name(crate::ArrayElementType::Int), "int");
    assert_eq!(name(crate::ArrayElementType::Boolean), "boolean");
    assert_eq!(name(crate::ArrayElementType::Char), "char");
    assert_eq!(name(crate::ArrayElementType::Byte), "byte");
    assert_eq!(name(crate::ArrayElementType::Short), "short");
    assert_eq!(name(crate::ArrayElementType::Long), "long");
    assert_eq!(name(crate::ArrayElementType::Float), "float");
    assert_eq!(name(crate::ArrayElementType::Double), "double");
    // Not "java.lang.String", not "Object" — HotSpot prints this literal
    // for every reference array, including an array of arrays.
    assert_eq!(name(crate::ArrayElementType::Reference), "object array");
}

/// JIT round 21 wave 26 (lane vhase): the pre-rendered sentences keep a
/// hidden class's `/0x<hex>` tail verbatim (HotSpot 25.0.3,
/// `R21Fz10VhCovariant1`), and the classic ones are unchanged.
#[test]
fn arraycopy_rendered_sentences_keep_a_hidden_tail() {
    use arraycopy_message::*;
    assert_eq!(
        reference_type_mismatch_rendered("java.lang.String", "p.Outer$Hc/0x00000001"),
        "arraycopy: type mismatch: can not copy java.lang.String[] into p.Outer$Hc/0x00000001[]"
    );
    assert_eq!(
        element_type_mismatch_rendered("java.lang.Object", "p.Outer$Hc/0x00000001"),
        "arraycopy: element type mismatch: can not cast one of the elements of \
         java.lang.Object[] to the type of the destination array, p.Outer$Hc/0x00000001"
    );
    assert_eq!(
        reference_type_mismatch("java/lang/String", "java/lang/Integer"),
        "arraycopy: type mismatch: can not copy java.lang.String[] into java.lang.Integer[]"
    );
}

#[test]
fn arraycopy_element_type_mismatch_matches_hotspot() {
    use arraycopy_message::element_type_mismatch as msg;
    // MEASURED on HotSpot 25.0.4+7, `probes/DodArrayStoreSweep`. The source
    // half is an external name and the destination half is the component
    // Klass's dotted NAME -- two dialects in one sentence, which is
    // HotSpot's and not ours to normalise.
    assert_eq!(
        msg("java/lang/Object", "java/lang/String"),
        "arraycopy: element type mismatch: can not cast one of the elements \
         of java.lang.Object[] to the type of the destination array, java.lang.String"
    );
    assert_eq!(
        msg("java/lang/Object", "[Ljava/lang/Integer;"),
        "arraycopy: element type mismatch: can not cast one of the elements \
         of java.lang.Object[] to the type of the destination array, [Ljava.lang.Integer;"
    );
    assert_eq!(
        msg("java/lang/Object", "[I"),
        "arraycopy: element type mismatch: can not cast one of the elements \
         of java.lang.Object[] to the type of the destination array, [I"
    );
}

#[test]
fn external_class_name_is_hotspots_external_name() {
    use arraycopy_message::external_class_name as ext;
    assert_eq!(ext("java/lang/String"), "java.lang.String");
    assert_eq!(ext("[Ljava/lang/String;"), "java.lang.String[]");
    assert_eq!(ext("[[Ljava/lang/String;"), "java.lang.String[][]");
    assert_eq!(ext("[I"), "int[]");
    assert_eq!(ext("[[D"), "double[][]");
}

#[test]
fn arraycopy_last_index_is_printed_unsigned() {
    // HotSpot formats `pos + length` with `%u`, so an addition that
    // overflows `int` renders as a large positive number. Printing it
    // signed would produce a negative "last index", which no HotSpot
    // message ever shows.
    assert_eq!(
        arraycopy_message::last_source_index(i32::MAX, 1, "int", 4),
        "arraycopy: last source index 2147483648 out of bounds for int[4]"
    );
}

#[test]
fn arraycopy_message_carries_through_to_the_throwable() {
    let err = RuntimeError::aioobe_with_message(
        9,
        arraycopy_message::last_source_index(0, 9, "int", 4),
    );
    let (cls, msg) = err.as_java_throwable().expect("is a Java throwable");
    assert_eq!(cls, "java/lang/ArrayIndexOutOfBoundsException");
    assert_eq!(
        msg.as_deref(),
        Some("arraycopy: last source index 9 out of bounds for int[4]")
    );
}

#[test]
fn runtime_error_arithmetic() {
    let err = RuntimeError::ArithmeticException {
        message: "/ by zero".into(),
    };
    assert_eq!(format!("{err}"), "ArithmeticException: / by zero");
}

#[test]
fn runtime_error_class_cast() {
    let err = RuntimeError::ClassCastException {
        message: "String cannot be cast to Integer".into(),
    };
    assert_eq!(
        format!("{err}"),
        "ClassCastException: String cannot be cast to Integer"
    );
}

#[test]
fn runtime_error_stack_overflow() {
    let err = RuntimeError::StackOverflowError;
    assert_eq!(format!("{err}"), "StackOverflowError");
}

#[test]
fn runtime_error_out_of_memory() {
    let err = RuntimeError::OutOfMemoryError {
        message: "heap full".into(),
    };
    assert_eq!(format!("{err}"), "OutOfMemoryError: heap full");
}

#[test]
fn runtime_error_negative_array_size() {
    let err = RuntimeError::NegativeArraySizeException { size: -5 };
    assert_eq!(format!("{err}"), "NegativeArraySizeException: -5");
}

#[test]
fn runtime_error_array_store() {
    let err = RuntimeError::ArrayStoreException {
        message: "wrong type".into(),
    };
    assert_eq!(format!("{err}"), "ArrayStoreException: wrong type");
}

/// JIT round 21 wave 25 (lane vhase): an empty `ArrayStoreException`
/// message is a null `getMessage()` (`VarHandleReferences.Array`'s
/// `new ArrayStoreException()`); a class name stays the message.
#[test]
fn an_empty_array_store_message_is_no_message() {
    let empty = RuntimeError::ArrayStoreException {
        message: String::new(),
    };
    let (cls, msg) = empty.as_java_throwable().unwrap();
    assert_eq!(cls, "java/lang/ArrayStoreException");
    assert_eq!(msg, None);
    let named = RuntimeError::ArrayStoreException {
        message: "java.lang.Integer".into(),
    };
    assert_eq!(
        named.as_java_throwable().unwrap().1.as_deref(),
        Some("java.lang.Integer")
    );
}

#[test]
fn runtime_error_string_index_out_of_bounds() {
    let err = RuntimeError::sioobe_no_length(99);
    assert_eq!(
        format!("{err}"),
        "StringIndexOutOfBoundsException: index 99"
    );
}

#[test]
fn runtime_error_class_not_found() {
    let err = RuntimeError::ClassNotFoundException {
        class_name: "Missing".into(),
    };
    assert_eq!(format!("{err}"), "ClassNotFoundException: Missing");
}

#[test]
fn runtime_error_unsatisfied_link() {
    let err = RuntimeError::UnsatisfiedLinkError {
        message: "native lib".into(),
    };
    assert_eq!(format!("{err}"), "UnsatisfiedLinkError: native lib");
}

#[test]
fn runtime_error_illegal_monitor_state() {
    let err = RuntimeError::IllegalMonitorStateException {
        message: "not owner".into(),
    };
    assert_eq!(format!("{err}"), "IllegalMonitorStateException: not owner");
}

#[test]
fn runtime_error_number_format() {
    let err = RuntimeError::NumberFormatException {
        message: "abc".into(),
    };
    assert_eq!(format!("{err}"), "NumberFormatException: abc");
}

#[test]
fn runtime_error_interrupted() {
    let err = RuntimeError::InterruptedException;
    assert_eq!(format!("{err}"), "InterruptedException");
}

#[test]
fn runtime_error_not_implemented() {
    let err = RuntimeError::NotImplemented {
        feature: "invokedynamic".into(),
    };
    assert_eq!(format!("{err}"), "not implemented: invokedynamic");
}

#[test]
fn runtime_error_concurrent_modification() {
    let err = RuntimeError::ConcurrentModificationException;
    assert_eq!(format!("{err}"), "ConcurrentModificationException");
}

#[test]
fn runtime_error_io_exception() {
    let err = RuntimeError::IOException {
        message: "broken pipe".into(),
    };
    assert_eq!(format!("{err}"), "IOException: broken pipe");
}

#[test]
fn runtime_error_file_not_found() {
    let err = RuntimeError::FileNotFoundException {
        path: "/tmp/missing.txt".into(),
    };
    assert_eq!(format!("{err}"), "FileNotFoundException: /tmp/missing.txt");
}

#[test]
fn runtime_error_unsupported_operation() {
    let err = RuntimeError::UnsupportedOperationException {
        message: "immutable".into(),
    };
    assert_eq!(format!("{err}"), "UnsupportedOperationException: immutable");
}

#[test]
fn runtime_error_illegal_state() {
    let err = RuntimeError::IllegalStateException {
        message: "closed".into(),
    };
    assert_eq!(format!("{err}"), "IllegalStateException: closed");
}

#[test]
fn runtime_error_illegal_caller_display() {
    // The new variant exists and formats consistently with its siblings —
    // bare class name followed by ": <message>", no double prefix.
    let err = RuntimeError::IllegalCallerException {
        message: "Native access is not enabled for this module".into(),
    };
    assert_eq!(
        format!("{err}"),
        "IllegalCallerException: Native access is not enabled for this module"
    );
}

#[test]
fn runtime_error_illegal_caller_is_distinct_from_illegal_state() {
    // Regression guard for task #57: the Panama native-access gate used
    // to fold IllegalCallerException into IllegalStateException because
    // the variant did not exist. The two variants must remain distinct
    // at the Rust level so the exception-mapping table can route them
    // to different Java classes.
    let caller = RuntimeError::IllegalCallerException {
        message: "denied".into(),
    };
    let state = RuntimeError::IllegalStateException {
        message: "denied".into(),
    };
    assert!(matches!(
        caller,
        RuntimeError::IllegalCallerException { .. }
    ));
    assert!(matches!(state, RuntimeError::IllegalStateException { .. }));
    // Display strings must not collide.
    assert_ne!(format!("{caller}"), format!("{state}"));
}

#[test]
fn runtime_error_no_such_element() {
    let err = RuntimeError::NoSuchElementException {
        message: "empty".into(),
    };
    assert_eq!(format!("{err}"), "NoSuchElementException: empty");
}

#[test]
fn runtime_error_input_mismatch() {
    let err = RuntimeError::InputMismatchException {
        message: "expected int".into(),
    };
    assert_eq!(format!("{err}"), "InputMismatchException: expected int");
}

#[test]
fn runtime_error_no_such_field_exception() {
    let err = RuntimeError::NoSuchFieldException {
        field_name: "value".into(),
    };
    assert_eq!(format!("{err}"), "NoSuchFieldException: value");
}

#[test]
fn runtime_error_no_such_method_exception() {
    let err = RuntimeError::NoSuchMethodException {
        message: "run()".into(),
    };
    assert_eq!(format!("{err}"), "NoSuchMethodException: run()");
}

#[test]
fn runtime_error_illegal_access_exception() {
    let err = RuntimeError::IllegalAccessException {
        message: "private method".into(),
    };
    assert_eq!(format!("{err}"), "IllegalAccessException: private method");
}

#[test]
fn runtime_error_illegal_argument_exception() {
    let err = RuntimeError::IllegalArgumentException {
        message: "negative".into(),
    };
    assert_eq!(format!("{err}"), "IllegalArgumentException: negative");
}

// -- MethodCallFailed tests --

#[test]
fn method_call_failed_internal_error_display() {
    let err = MethodCallFailed::InternalError(VmError::Internal {
        message: "oops".into(),
    });
    // `MethodCallFailed::InternalError` delegates to the inner `VmError`'s
    // `Display`, which already supplies the "internal error: " prefix —
    // so the prefix must appear exactly once, not twice.
    assert_eq!(format!("{err}"), "internal error: oops");
}

#[test]
fn method_call_failed_exception_thrown_display() {
    let fake_ptr = 0xDEAD_BEE0_u64 as *mut u8; // must be 8-byte aligned
    let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
    let err = MethodCallFailed::ExceptionThrown(obj);
    let display = format!("{err}");
    assert!(display.starts_with("exception thrown: ref(0x"));
}

#[test]
fn method_call_failed_from_vm_error() {
    let vm_err = VmError::Internal {
        message: "bug".into(),
    };
    let mcf: MethodCallFailed = vm_err.into();
    assert!(matches!(mcf, MethodCallFailed::InternalError(_)));
}

#[test]
fn method_call_failed_from_class_file_error() {
    let cfe = ClassFileError::ClassNotFound {
        class_name: "X".into(),
    };
    let mcf: MethodCallFailed = cfe.into();
    assert!(matches!(
        mcf,
        MethodCallFailed::InternalError(VmError::ClassFile(_))
    ));
}

#[test]
fn method_call_failed_from_linkage_error() {
    let le = LinkageError::NoClassDefFoundError {
        class_name: "Y".into(),
    };
    let mcf: MethodCallFailed = le.into();
    assert!(matches!(
        mcf,
        MethodCallFailed::InternalError(VmError::Linkage(_))
    ));
}

#[test]
fn method_call_failed_from_runtime_error() {
    let re = RuntimeError::StackOverflowError;
    let mcf: MethodCallFailed = re.into();
    assert!(matches!(
        mcf,
        MethodCallFailed::InternalError(VmError::Runtime(_))
    ));
}

// -- MethodCallResult pattern matching --

#[test]
fn method_call_result_ok_value() {
    let result: MethodCallResult = Ok(Some(Value::Int(42)));
    assert!(result.is_ok());
    assert_eq!(result.unwrap().unwrap().as_int(), Some(42));
}

#[test]
fn method_call_result_ok_void() {
    let result: MethodCallResult = Ok(None);
    assert!(result.unwrap().is_none());
}

#[test]
fn method_call_result_err_internal() {
    let result: MethodCallResult = Err(MethodCallFailed::InternalError(VmError::Internal {
        message: "fail".into(),
    }));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, MethodCallFailed::InternalError(_)));
}

// -- JdkOnlyViolation --

/// One of each variant, fully populated, so a test can sweep all seven.
fn all_variants() -> Vec<JdkOnlyViolation> {
    vec![
        JdkOnlyViolation::CompatibilityClassRequested {
            class: "org/jboss/Absent".into(),
            initiating_loader: Some("app".into()),
            requester: Some("com/example/Boot.start()V".into()),
            reason: "enterprise-prefix fallback".into(),
            class_origin: Some("compatibility-stub".into()),
        },
        JdkOnlyViolation::SyntheticNativeRegistered {
            class: "com/example/Strict".into(),
            method: "fake".into(),
            descriptor: "(I)Ljava/lang/String;".into(),
            registered_by: Some("native-builtins/src/lib.rs:1234".into()),
            // The interesting arm: a refusal that did NOT retire its
            // method, because an earlier registration still owns the slot.
            survivor: Some("intrinsic@native-builtins/src/phases_early.rs:21878".into()),
        },
        JdkOnlyViolation::SyntheticNativeInvocation {
            class: "com/example/Strict".into(),
            method: "fake".into(),
            descriptor: "(Ljava/lang/Object;)Z".into(),
            call_site: None,
        },
        JdkOnlyViolation::MissingNative {
            class: "java/net/Socket".into(),
            method: "socket0".into(),
            descriptor: "(ZZZZ)I".into(),
            module: Some("java.base".into()),
        },
        JdkOnlyViolation::NativeShadowsBytecode {
            class: "java/util/HashMap".into(),
            method: "put".into(),
            descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".into(),
            native_kind: "jit-thin-direct-helper",
            class_origin: Some("boot-image".into()),
        },
        JdkOnlyViolation::MissingBootClass {
            class: "java/lang/Object".into(),
            searched_image: "jdk-25".into(),
        },
        JdkOnlyViolation::MissingImplementation {
            class: "com/example/Owner".into(),
            method: "compute".into(),
            descriptor: "()J".into(),
        },
    ]
}

/// §1.7's class-origin field is on the two CLASS-FACING variants and on
/// NO others — which is the whole content of the criterion, not an
/// implementation detail.
///
/// `jdk-only/the-six-acceptance-criteria-measured-20260910-RETIRED-20260921.md`
/// §5 measured class origin as the one field neither surface carried, and
/// said in the same breath why adding it everywhere would be worse than
/// leaving it out: six construction sites build
/// `SyntheticNativeRegistered`, which fires at REGISTRATION, before any
/// class is loaded — "a change that adds `origin: null` to the latter to
/// tick the box would make the criterion read as met without telling
/// anyone anything."
///
/// So the gate is two-sided on purpose. Adding the field to a
/// registration-time variant fails here just as loudly as removing it from
/// a class-facing one.
#[test]
fn class_origin_is_on_the_class_facing_variants_and_no_others() {
    let expected_with: std::collections::BTreeSet<&str> =
        ["compatibility-class-requested", "native-shadows-bytecode"]
            .into_iter()
            .collect();
    let actual_with: std::collections::BTreeSet<&str> = all_variants()
        .iter()
        .filter(|v| v.has_class_origin_field())
        .map(|v| v.kind())
        .collect();
    assert_eq!(
        actual_with, expected_with,
        "the set of variants carrying a class origin moved. Adding one is \
         only right if the variant NAMES A LOADED CLASS at the moment it \
         fires; a registration-time refusal does not, and `origin: null` on \
         it ticks the criterion without answering it."
    );

    for v in all_variants() {
        let rendered = v.render(Some(25), true);
        let json = v.to_json();
        if v.has_class_origin_field() {
            assert!(
                rendered.contains("class origin:"),
                "{} renders no class-origin line:\n{rendered}",
                v.kind()
            );
            assert!(
                json.contains("\"class_origin\""),
                "{} has no class_origin JSON field: {json}",
                v.kind()
            );
        } else {
            assert!(
                !rendered.contains("class origin:"),
                "{} renders a class-origin line it cannot answer:\n{rendered}",
                v.kind()
            );
            assert!(
                !json.contains("\"class_origin\""),
                "{} emits a vacuous class_origin JSON field: {json}",
                v.kind()
            );
        }
    }
}

/// An applicable variant whose producer did not learn the origin renders
/// `<unknown>` rather than dropping the line.
///
/// The distinction the criterion turns on. A dropped line is
/// indistinguishable from a variant that has no origin at all, so a reader
/// cannot tell "nobody looked" from "there is nothing to look at" — and the
/// three dispatch-path producers of `NativeShadowsBytecode` all start out
/// in the first state, by design, because resolving a class from there
/// costs a store read lock on the hottest path in strict mode.
#[test]
fn an_unknown_class_origin_is_printed_not_omitted() {
    let v = JdkOnlyViolation::NativeShadowsBytecode {
        class: "java/util/HashMap".into(),
        method: "put".into(),
        descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".into(),
        native_kind: "jit-thin-direct-helper",
        class_origin: None,
    };
    let rendered = v.render(Some(25), true);
    assert!(
        rendered.contains("class origin:") && rendered.contains(UNKNOWN),
        "an unlearned origin must render as {UNKNOWN}, not vanish:\n{rendered}"
    );
    assert!(
        v.to_json().contains("\"class_origin\":null"),
        "and must be an explicit null on the wire: {}",
        v.to_json()
    );
}

/// `with_class_origin` fills a gap and never overwrites an answer.
///
/// The consumers apply it blindly across a whole `violations[]`, so it has
/// to be a no-op on the variants without the field and on rows whose
/// producer already knew — otherwise a report writer holding a coarse
/// class-name lookup could overwrite the precise origin a producer
/// recorded at the moment of the refusal.
#[test]
fn with_class_origin_fills_only_a_gap() {
    let known = JdkOnlyViolation::NativeShadowsBytecode {
        class: "java/util/HashMap".into(),
        method: "put".into(),
        descriptor: "()V".into(),
        native_kind: "bridge",
        class_origin: Some("application-class-path".into()),
    };
    assert_eq!(
        known
            .clone()
            .with_class_origin(Some("boot-image".into()))
            .class_origin(),
        Some("application-class-path"),
        "an origin the producer knew must not be overwritten"
    );

    let unknown = JdkOnlyViolation::NativeShadowsBytecode {
        class: "java/util/HashMap".into(),
        method: "put".into(),
        descriptor: "()V".into(),
        native_kind: "bridge",
        class_origin: None,
    };
    assert_eq!(
        unknown
            .with_class_origin(Some("boot-image".into()))
            .class_origin(),
        Some("boot-image")
    );

    // And a variant without the field absorbs it silently.
    let registration = JdkOnlyViolation::SyntheticNativeRegistered {
        class: "com/example/Strict".into(),
        method: "fake".into(),
        descriptor: "()V".into(),
        registered_by: None,
        survivor: None,
    };
    let after = registration.with_class_origin(Some("boot-image".into()));
    assert_eq!(after.class_origin(), None);
    assert!(!after.to_json().contains("class_origin"));
}

/// The kind tags are a wire format: `difftest/src/census.rs` tallies
/// `--real-jdk-report` rows by exactly these strings, and
/// `vm/tests/jdk_only_dispatch.rs` asserts three of them literally. Pinning
/// them here means a re-spelling fails in this crate rather than silently
/// producing a report whose every row tallies as an unknown kind.
#[test]
fn kind_tags_are_pinned() {
    let expected = [
        "compatibility-class-requested",
        "synthetic-native-registered",
        "synthetic-native-invocation",
        "missing-native",
        "native-shadows-bytecode",
        "missing-boot-class",
        "missing-implementation",
    ];
    let actual: Vec<&str> = all_variants().iter().map(|v| v.kind()).collect();
    assert_eq!(actual, expected);
}

/// Two kinds sharing a tag would make the report unable to say which policy
/// rule fired, and would silently merge two counters into one.
#[test]
fn kind_tags_are_distinct() {
    let mut tags: Vec<&str> = all_variants().iter().map(|v| v.kind()).collect();
    let before = tags.len();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(before, tags.len(), "two variants share a kind tag");
    assert_eq!(before, 7, "the contract defines exactly seven variants");
}

/// Every method-bearing variant must be identifiable down to the overload.
/// A refusal naming `HashMap.put` without the descriptor cannot be turned
/// into a work item (§1.7), and both the dispatch and registry suites
/// assert the descriptor is present in `summary()`.
#[test]
fn summary_names_the_class_and_the_overload() {
    for v in all_variants() {
        let summary = v.summary();
        assert!(
            summary.contains(v.class()),
            "{}: summary omits the class: {summary}",
            v.kind()
        );
        if let Some((method, descriptor)) = v.member() {
            assert!(
                summary.contains(method) && summary.contains(descriptor),
                "{}: summary omits the overload: {summary}",
                v.kind()
            );
        }
    }
}

#[test]
fn display_is_exactly_summary() {
    for v in all_variants() {
        assert_eq!(format!("{v}"), v.summary());
    }
}

/// `render` ends with the two fixed lines, in that order. An operator who
/// scrolls to the bottom of a wall of diagnostics has to find the fallback
/// and the capture hint in the same place every time; the ordering is also
/// the one thing about the layout other agents were told to rely on.
#[test]
fn remediation_ends_with_the_two_fixed_lines() {
    for v in all_variants() {
        let rendered = v.render(Some(25), true);
        let lines: Vec<&str> = rendered.lines().collect();
        let last = lines[lines.len() - 1].trim();
        let penultimate = lines[lines.len() - 2].trim();
        assert_eq!(
            penultimate,
            REMEDIATION_FALLBACK,
            "{}: fallback is not the penultimate line\n{rendered}",
            v.kind()
        );
        assert_eq!(
            last,
            REMEDIATION_CAPTURE,
            "{}: capture hint is not the last line\n{rendered}",
            v.kind()
        );
        assert!(
            rendered.contains("--compatible"),
            "§1.7 requires the fallback to be named"
        );
        assert!(rendered.contains("--real-jdk-report <FILE>"));

        // The blocks the contract requires, in order.
        let remediation_at = rendered.find("  Remediation:").expect("remediation block");
        let jdk_at = rendered.find("  JDK:").expect("JDK block");
        let reason_at = rendered.find("  reason:").expect("reason line");
        let from_at = rendered
            .find("  requested from:")
            .expect("requested-from line");
        let class_at = rendered
            .find("  requested class:")
            .expect("requested-class line");
        assert!(
            class_at < from_at && from_at < reason_at && reason_at < jdk_at,
            "{}: block order is wrong\n{rendered}",
            v.kind()
        );
        assert!(jdk_at < remediation_at, "remediation must come last");
        assert!(rendered.contains("feature version: 25"));
    }
}

#[test]
fn render_reports_an_unknown_feature_version_rather_than_guessing() {
    let v = JdkOnlyViolation::MissingNative {
        class: "java/net/Socket".into(),
        method: "socket0".into(),
        descriptor: "(ZZZZ)I".into(),
        module: Some("java.base".into()),
    };
    let rendered = v.render(None, true);
    assert!(
        rendered.contains("feature version: <unknown>"),
        "{rendered}"
    );
    // The module the reporter *did* know is still named.
    assert!(
        rendered.contains("module:          java.base"),
        "{rendered}"
    );
}

/// The three absolute forms leak the layout of the machine the run happened
/// on and are redacted; a relative path is the useful, harmless one and
/// passes through byte-for-byte.
#[test]
fn absolute_paths_are_redacted_unless_verbose() {
    let absolute = [
        "/opt/jdk-25/lib/modules",
        "C:\\Program\\jdk-25",
        "c:/jdk-25/lib",
        "\\\\build01\\share\\jdk-25",
        "//build01/share/jdk-25",
        "\\Windows\\jdk",
    ];
    for path in absolute {
        assert!(is_absolute_path(path), "{path} should count as absolute");
        assert_eq!(redact_paths(path, false), REDACTED, "{path} not redacted");
        assert_eq!(redact_paths(path, true), path, "verbose must not redact");
    }

    let relative = [
        "native-builtins/src/lib.rs:1234",
        "java/lang/Object",
        "(Ljava/lang/String;)V",
        "com/example/Boot.start()V",
        "jdk-25",
    ];
    for path in relative {
        assert!(!is_absolute_path(path), "{path} is not absolute");
        assert_eq!(
            redact_paths(path, false),
            path,
            "a relative path must pass through untouched"
        );
    }

    // Mixed text: only the absolute token goes.
    assert_eq!(
        redact_paths("searched /opt/jdk-25 and lib/modules", false),
        "searched <redacted> and lib/modules"
    );
}

#[test]
fn render_redacts_the_searched_image_but_keeps_the_registrar_line() {
    let boot = JdkOnlyViolation::MissingBootClass {
        class: "java/lang/Object".into(),
        searched_image: "/opt/jdk-25/lib/modules".into(),
    };
    let quiet = boot.render(Some(25), false);
    assert!(!quiet.contains("/opt/jdk-25"), "{quiet}");
    assert!(quiet.contains(REDACTED), "{quiet}");
    assert!(boot
        .render(Some(25), true)
        .contains("/opt/jdk-25/lib/modules"));

    // `#[track_caller]` provenance is workspace-relative: redacting it
    // would delete the only actionable fact in the report.
    let registered = JdkOnlyViolation::SyntheticNativeRegistered {
        class: "com/example/Strict".into(),
        method: "fake".into(),
        descriptor: "(I)Ljava/lang/String;".into(),
        registered_by: Some("native-builtins/src/lib.rs:1234".into()),
        survivor: None,
    };
    assert!(registered
        .render(Some(25), false)
        .contains("native-builtins/src/lib.rs:1234"));
}

/// The JSON is hand-rolled, so the three ways a hand-rolled dump goes wrong
/// — unbalanced braces, a trailing comma, an unescaped quote — are pinned
/// here rather than discovered by a consumer that cannot parse the report.
#[test]
fn to_json_is_well_formed() {
    for v in all_variants() {
        let json = v.to_json();
        assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
        assert!(!json.contains(",}"), "trailing comma in {json}");
        assert!(!json.contains("{,"), "leading comma in {json}");

        // Braces balance, counting only those outside string literals.
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for ch in json.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "unbalanced braces in {json}");
        }
        assert_eq!(depth, 0, "unbalanced braces in {json}");
        assert!(!in_string, "unterminated string in {json}");

        assert!(
            json.contains(&format!("\"kind\":\"{}\"", v.kind())),
            "every row must be internally tagged: {json}"
        );
        assert!(json.contains("\"summary\":\""), "{json}");
        assert!(json.contains("\"class\":\""), "{json}");
    }
}

/// Absent optionals are `null`, not omitted: every row of a kind then has
/// the same shape, so a consumer can index columns without probing.
#[test]
fn to_json_emits_null_for_absent_optionals() {
    let invocation = JdkOnlyViolation::SyntheticNativeInvocation {
        class: "C".into(),
        method: "m".into(),
        descriptor: "()V".into(),
        call_site: None,
    };
    assert!(
        invocation.to_json().contains("\"call_site\":null"),
        "{}",
        invocation.to_json()
    );

    let present = JdkOnlyViolation::SyntheticNativeInvocation {
        class: "C".into(),
        method: "m".into(),
        descriptor: "()V".into(),
        call_site: Some("D.n()V".into()),
    };
    assert!(present.to_json().contains("\"call_site\":\"D.n()V\""));

    let missing = JdkOnlyViolation::MissingNative {
        class: "C".into(),
        method: "m".into(),
        descriptor: "()V".into(),
        module: None,
    };
    assert!(missing.to_json().contains("\"module\":null"));
}

#[test]
fn to_json_escapes_strings() {
    let v = JdkOnlyViolation::CompatibilityClassRequested {
        class: "org/x/Q".into(),
        initiating_loader: None,
        requester: None,
        class_origin: Some("compatibility-stub".into()),
        reason: "said \"no\" at C:\\build\\x\tand\nstopped".into(),
    };
    let json = v.to_json();
    assert!(json.contains("\\\"no\\\""), "{json}");
    assert!(json.contains("C:\\\\build\\\\x"), "{json}");
    assert!(json.contains("\\t"), "{json}");
    assert!(json.contains("\\n"), "{json}");
    // The raw control characters must not survive into the output.
    assert!(!json.contains('\t'));
    assert!(!json.contains('\n'));
    assert!(json.contains("\"initiating_loader\":null"));

    let mut escaped = String::new();
    json_string(&mut escaped, "\u{1}");
    assert_eq!(escaped, "\"\\u0001\"");
}

// -- the two new VmError variants --

#[test]
fn vm_error_invalid_configuration_display() {
    let err =
        VmError::InvalidConfiguration("--real-jdk conflicts with --synthetic-jdk".to_string());
    assert_eq!(
        format!("{err}"),
        "invalid configuration: --real-jdk conflicts with --synthetic-jdk"
    );
    assert!(matches!(err, VmError::InvalidConfiguration(_)));
}

/// `From` is hand-written because `#[from]` would also make the violation
/// the error's `source()`, which needs `JdkOnlyViolation: std::error::Error`
/// — a bound the contract's `Display`-only type does not have. This pins
/// both that the conversion exists and that the Display text is the
/// violation's own summary behind one category prefix.
#[test]
fn vm_error_jdk_only_variant_and_conversion() {
    let violation = JdkOnlyViolation::MissingNative {
        class: "java/net/Socket".into(),
        method: "socket0".into(),
        descriptor: "(ZZZZ)I".into(),
        module: Some("java.base".into()),
    };
    let err: VmError = violation.clone().into();
    assert!(matches!(err, VmError::JdkOnly(_)));
    assert_eq!(format!("{err}"), format!("jdk-only violation: {violation}"));
    assert!(format!("{err}").contains("(ZZZZ)I"));

    // And it travels through the two-layer exception model unchanged: the
    // JNI and interpreter surfacing paths match on exactly this shape.
    let failed: MethodCallFailed = VmError::JdkOnly(violation.clone()).into();
    match failed {
        MethodCallFailed::InternalError(VmError::JdkOnly(inner)) => {
            assert_eq!(inner, violation);
        }
        other => panic!("expected InternalError(JdkOnly), got {other:?}"),
    }
}

// -- format_optional_message --

#[test]
fn format_optional_message_some() {
    let result = format_optional_message(&Some("detail".into()));
    assert_eq!(result, ": detail");
}

#[test]
fn format_optional_message_none() {
    let result = format_optional_message(&None);
    assert_eq!(result, "");
}
