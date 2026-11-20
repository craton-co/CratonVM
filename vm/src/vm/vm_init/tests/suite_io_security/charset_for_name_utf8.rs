#[allow(unused_imports)]
use super::*;


// ========================================================================
// Phase 25: Charset tests
// ========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_for_name_utf8() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let name = create_java_string(&shared, "UTF-8");
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name))],
    )
    .unwrap();
    let charset = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected charset"),
    };

    let cs_name = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(charset))],
    )
    .unwrap();
    let name_obj = match cs_name {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, name_obj),
        Some("UTF-8".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_for_name_case_insensitive() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let name = create_java_string(&shared, "utf-8");
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name))],
    )
    .unwrap();
    let charset = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected charset"),
    };

    let cs_name = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(charset))],
    )
    .unwrap();
    let name_obj = match cs_name {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, name_obj),
        Some("UTF-8".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_for_name_unsupported_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let name = create_java_string(&shared, "UNSUPPORTED-CHARSET");
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name))],
    );
    assert!(result.is_err());
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_default_is_utf8() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "defaultCharset",
        "()Ljava/nio/charset/Charset;",
        &[],
    )
    .unwrap();
    let charset = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected charset"),
    };

    let cs_name = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(charset))],
    )
    .unwrap();
    let name_obj = match cs_name {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, name_obj),
        Some("UTF-8".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_is_supported() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let utf8 = create_java_string(&shared, "UTF-8");
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "isSupported",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(utf8))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(1)));

    let nope = create_java_string(&shared, "NOPE");
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "isSupported",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(nope))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(0)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_equals_and_hashcode() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let name1 = create_java_string(&shared, "UTF-8");
    let cs1_result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name1))],
    )
    .unwrap();
    let cs1 = match cs1_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected charset"),
    };

    let name2 = create_java_string(&shared, "utf-8");
    let cs2_result = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name2))],
    )
    .unwrap();
    let cs2 = match cs2_result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected charset"),
    };

    // equals should return true
    let eq = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(cs1)), Value::Object(Some(cs2))],
    )
    .unwrap();
    assert_eq!(eq, Some(Value::Int(1)));

    // hashCode should return same value
    let h1 = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "hashCode",
        "()I",
        &[Value::Object(Some(cs1))],
    )
    .unwrap();
    let h2 = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "hashCode",
        "()I",
        &[Value::Object(Some(cs2))],
    )
    .unwrap();
    assert_eq!(h1, h2);
}


#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn standard_charsets_utf8() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let charset = standard_charset_static(&shared, &mut thread, "UTF_8");

    let cs_name = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(charset))],
    )
    .unwrap();
    let name_obj = match cs_name {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, name_obj),
        Some("UTF-8".to_string())
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn standard_charsets_all_variants() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let variants = [
        ("UTF_8", "UTF-8"),
        ("UTF_16", "UTF-16"),
        ("UTF_16BE", "UTF-16BE"),
        ("UTF_16LE", "UTF-16LE"),
        ("US_ASCII", "US-ASCII"),
        ("ISO_8859_1", "ISO-8859-1"),
    ];

    for (field, expected) in variants {
        let charset = standard_charset_static(&shared, &mut thread, field);

        let cs_name = call_native(
            &shared,
            &mut thread,
            "java/nio/charset/Charset",
            "name",
            "()Ljava/lang/String;",
            &[Value::Object(Some(charset))],
        )
        .unwrap();
        let name_obj = match cs_name {
            Some(Value::Object(Some(o))) => o,
            _ => panic!("expected string for {}", field),
        };
        assert_eq!(
            read_java_string(&shared.mem.heap, name_obj),
            Some(expected.to_string())
        );
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charset_for_name_aliases() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let aliases = ["ascii", "ASCII", "US-ASCII", "ISO-8859-1", "latin1"];
    for alias in aliases {
        let name = create_java_string(&shared, alias);
        let result = call_native(
            &shared,
            &mut thread,
            "java/nio/charset/Charset",
            "forName",
            "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
            &[Value::Object(Some(name))],
        );
        assert!(result.is_ok(), "forName failed for alias: {}", alias);
    }
}

// =========================================================================
// Phase 26: StringReader / StringWriter / DataInputStream / DataOutputStream
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn string_reader_read_chars() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sr = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let content = create_java_string(&shared, "Hello");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sr)), Value::Object(Some(content))],
    );
    // Read 'H'
    let r = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('H' as i32));
    // Read 'e'
    let r = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('e' as i32));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn string_reader_ready_and_eof() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sr = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let content = create_java_string(&shared, "AB");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sr)), Value::Object(Some(content))],
    );
    // ready should be true
    let ready = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "ready",
        "()Z",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ready, Value::Int(1));
    // Read both chars
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    );
    // Now should return -1
    let eof = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eof, Value::Int(-1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn string_reader_skip_and_reset() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sr = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let content = create_java_string(&shared, "ABCDE");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sr)), Value::Object(Some(content))],
    );
    // Skip 3
    let skipped = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "skip",
        "(J)J",
        &[Value::Object(Some(sr)), Value::Long(3)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(skipped, Value::Long(3));
    // Read 'D'
    let r = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('D' as i32));
    // Reset to beginning
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "reset",
        "()V",
        &[Value::Object(Some(sr))],
    );
    // Read 'A'
    let r = call_native(
        &shared,
        &mut thread,
        "java/io/StringReader",
        "read",
        "()I",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('A' as i32));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn string_writer_write_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sw = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "<init>",
        "()V",
        &[Value::Object(Some(sw))],
    );
    // Write 'A'
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "write",
        "(I)V",
        &[Value::Object(Some(sw)), Value::Int('A' as i32)],
    );
    // Write 'B'
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "write",
        "(I)V",
        &[Value::Object(Some(sw)), Value::Int('B' as i32)],
    );
    // toString
    let result = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sw))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "AB");
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn string_writer_write_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sw = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "<init>",
        "()V",
        &[Value::Object(Some(sw))],
    );
    let hello = create_java_string(&shared, "Hello");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "write",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sw)), Value::Object(Some(hello))],
    );
    let world = create_java_string(&shared, " World");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "write",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sw)), Value::Object(Some(world))],
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sw))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "Hello World"
        );
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn string_writer_append_char() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sw = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "<init>",
        "()V",
        &[Value::Object(Some(sw))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "append",
        "(C)Ljava/io/StringWriter;",
        &[Value::Object(Some(sw)), Value::Int('X' as i32)],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "append",
        "(C)Ljava/io/StringWriter;",
        &[Value::Object(Some(sw)), Value::Int('Y' as i32)],
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/io/StringWriter",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sw))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "XY");
    } else {
        panic!("expected string");
    }
}

// =========================================================================
// Phase 27: BigInteger
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_init_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bi = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let val = create_java_string(&shared, "12345");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(bi)), Value::Object(Some(val))],
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bi))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "12345");
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_add_and_subtract() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let va = create_java_string(&shared, "100");
    let vb = create_java_string(&shared, "25");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(va))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(b)), Value::Object(Some(vb))],
    );
    // add
    let sum = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "add",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = sum {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(rs)) = r {
            assert_eq!(read_java_string(&shared.mem.heap, rs).unwrap(), "125");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
    // subtract
    let diff = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "subtract",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = diff {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(rs)) = r {
            assert_eq!(read_java_string(&shared.mem.heap, rs).unwrap(), "75");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_multiply_and_divide() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let va = create_java_string(&shared, "12");
    let vb = create_java_string(&shared, "5");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(va))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(b)), Value::Object(Some(vb))],
    );
    // multiply
    let prod = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "multiply",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = prod {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(rs)) = r {
            assert_eq!(read_java_string(&shared.mem.heap, rs).unwrap(), "60");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
    // divide
    let quot = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "divide",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = quot {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(rs)) = r {
            assert_eq!(read_java_string(&shared.mem.heap, rs).unwrap(), "2");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_negate_and_abs() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let va = create_java_string(&shared, "42");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(va))],
    );
    // negate
    let neg = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "negate",
        "()Ljava/math/BigInteger;",
        &[Value::Object(Some(a))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = neg {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(rs)) = r {
            assert_eq!(read_java_string(&shared.mem.heap, rs).unwrap(), "-42");
        } else {
            panic!("expected string");
        }
        // abs of negated
        let abs = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "abs",
            "()Ljava/math/BigInteger;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(abs_obj)) = abs {
            let r2 = call_native(
                &shared,
                &mut thread,
                "java/math/BigInteger",
                "toString",
                "()Ljava/lang/String;",
                &[Value::Object(Some(abs_obj))],
            )
            .unwrap()
            .unwrap();
            if let Value::Object(Some(rs2)) = r2 {
                assert_eq!(read_java_string(&shared.mem.heap, rs2).unwrap(), "42");
            } else {
                panic!("expected string");
            }
        } else {
            panic!("expected object");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_compare_and_equals() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let c = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let va = create_java_string(&shared, "100");
    let vb = create_java_string(&shared, "200");
    let vc = create_java_string(&shared, "100");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(va))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(b)), Value::Object(Some(vb))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(c)), Value::Object(Some(vc))],
    );
    // a < b
    let cmp = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "compareTo",
        "(Ljava/math/BigInteger;)I",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cmp, Value::Int(-1));
    // a == c
    let eq = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(a)), Value::Object(Some(c))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_int_value_and_long_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bi = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let val = create_java_string(&shared, "42");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(bi)), Value::Object(Some(val))],
    );
    let iv = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "intValue",
        "()I",
        &[Value::Object(Some(bi))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(iv, Value::Int(42));
    let lv = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "longValue",
        "()J",
        &[Value::Object(Some(bi))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(lv, Value::Long(42));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn biginteger_value_of() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bi = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(999)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = bi {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = r {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "999");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

// =========================================================================
// Phase 27: BigDecimal
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn bigdecimal_init_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bd = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let val = create_java_string(&shared, "3.14");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(bd)), Value::Object(Some(val))],
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bd))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "3.14");
    } else {
        panic!("expected string");
    }
}

/// F31 — the three `(unscaled, scale)` roads that used to abort the VM,
/// end to end through the registered natives.
///
/// These use the SYNTHETIC-stub layout (slot 0 = decimal `String`, slot 1 =
/// `scale`), which is what `bd_layout` falls back to when the real
/// `java.math.BigDecimal` is not loaded — the only layout reachable from
/// this module. That is enough for all three roads, because every guard
/// added by F31 sits between `bd_unscaled_bigint` and the arithmetic, and
/// `bd_unscaled_bigint` reads those two slots.
///
/// MEASURED on `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft
/// build), `scratchpad/f31/{Bd,Bd2}.java`:
///
/// ```text
/// new BigDecimal(ONE, MIN).intValue()      = 0                        [0 ms]
/// new BigDecimal(ONE, MIN).longValue()     = 0                        [0 ms]
/// new BigDecimal(ONE, MIN).toBigInteger() !! ArithmeticException: Underflow   [0 ms]
/// new BigDecimal(ONE, MIN).add(new BigDecimal(ONE, MAX))
///                                         !! ArithmeticException: Underflow   [0 ms]
/// ```
///
/// What each row was before, in this VM: `intValue()` negated `i32::MIN`
/// (debug panic; release wrapped, took `bigint_mul_pow10`'s `n <= 0` arm
/// and answered **1**); `toBigInteger()` did the same and could not refuse
/// at all; `add` computed `i32::MAX - i32::MIN` in `i32`. A Rust panic is
/// not a Java throwable — it takes the VM down and no `catch` sees it.
#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn bigdecimal_extreme_scale_refusals_f31() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // `new BigDecimal(BigInteger.ONE, Integer.MIN_VALUE)`.
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let one = create_java_string(&shared, "1");
    shared.mem.heap.set_field(a, 0, Value::Object(Some(one)));
    shared.mem.heap.set_field(a, 1, Value::Int(i32::MIN));
    shared.mem.heap.set_field(a, 2, Value::Int(0));

    // The two narrowing conversions must ANSWER, and answer 0 — the JDK
    // fast-paths `scale <= -64` before it ever considers `10^scale`.
    let iv = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "intValue",
        "()I",
        &[Value::Object(Some(a))],
    )
    .expect("intValue must not refuse: HotSpot answers 0 in 0 ms")
    .unwrap();
    assert_eq!(
        iv,
        Value::Int(0),
        "new BigDecimal(ONE, Integer.MIN_VALUE).intValue() is 0 on HotSpot; \
             a guard on the shared truncation helper would refuse here, and the \
             pre-F31 body answered 1"
    );
    let lv = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "longValue",
        "()J",
        &[Value::Object(Some(a))],
    )
    .expect("longValue must not refuse: HotSpot answers 0 in 0 ms")
    .unwrap();
    assert_eq!(lv, Value::Long(0));

    // `toBigInteger()` is `setScale(0, DOWN)` and DOES refuse the same
    // receiver — with the clamping `checkScale`'s word, "Underflow".
    let tbi = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "toBigInteger",
        "()Ljava/math/BigInteger;",
        &[Value::Object(Some(a))],
    )
    .expect_err("toBigInteger must refuse where HotSpot refuses");
    assert!(
        matches!(
            &tbi,
            crate::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::ArithmeticException { message }
                )
            ) if message.as_str() == "Underflow"
        ),
        "toBigInteger() at scale Integer.MIN_VALUE is \
             ArithmeticException(\"Underflow\") — the CLAMPING instance \
             checkScale, not toPlainString's casting checkScaleNonZero, which \
             says \"Overflow\" for the same scale. Got {tbi:?}"
    );

    // `add` aligns to `max(sa, sb)`; that difference does not fit an `i32`.
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let one_b = create_java_string(&shared, "1");
    shared.mem.heap.set_field(b, 0, Value::Object(Some(one_b)));
    shared.mem.heap.set_field(b, 1, Value::Int(i32::MAX));
    shared.mem.heap.set_field(b, 2, Value::Int(0));
    let sum = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "add",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .expect_err("the scale alignment overflows i32 and HotSpot refuses it");
    assert!(
        matches!(
            &sum,
            crate::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::ArithmeticException { message }
                )
            ) if message.as_str() == "Underflow"
        ),
        "add() across Integer.MIN_VALUE/MAX_VALUE scales is \
             ArithmeticException(\"Underflow\"); the old body computed \
             `s - sa` in i32. Got {sum:?}"
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn bigdecimal_add_and_subtract() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let va = create_java_string(&shared, "10.5");
    let vb = create_java_string(&shared, "3.2");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(va))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(b)), Value::Object(Some(vb))],
    );
    // add
    let sum = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "add",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = sum {
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigDecimal",
            "doubleValue",
            "()D",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Double(d) = r {
            assert!((d - 13.7).abs() < 0.001);
        } else {
            panic!("expected double");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn bigdecimal_scale_and_precision() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bd = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let val = create_java_string(&shared, "123.456");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(bd)), Value::Object(Some(val))],
    );
    let scale = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "scale",
        "()I",
        &[Value::Object(Some(bd))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(scale, Value::Int(3));
    let prec = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "precision",
        "()I",
        &[Value::Object(Some(bd))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(prec, Value::Int(6)); // "123456" has 6 digits
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn bigdecimal_compare_to() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let va = create_java_string(&shared, "1.5");
    let vb = create_java_string(&shared, "2.5");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(va))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(b)), Value::Object(Some(vb))],
    );
    let cmp = call_native(
        &shared,
        &mut thread,
        "java/math/BigDecimal",
        "compareTo",
        "(Ljava/math/BigDecimal;)I",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cmp, Value::Int(-1));
}

// =========================================================================
// Phase 28: Executors / Future / CompletableFuture
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn executors_new_single_thread() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let exec = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Executors",
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(exec, Value::Object(Some(_))));
    // isShutdown should be false
    let shut = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorService",
        "isShutdown",
        "()Z",
        &[exec],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn executors_new_fixed_pool() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let exec = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Executors",
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(exec, Value::Object(Some(_))));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn completable_future_completed_and_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let val = create_java_string(&shared, "hello");
    let cf = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(val))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(cf_obj)) = cf {
        // isDone
        let done = call_native(
            &shared,
            &mut thread,
            "java/util/concurrent/CompletableFuture",
            "isDone",
            "()Z",
            &[Value::Object(Some(cf_obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(done, Value::Int(1));
        // get
        let result = call_native(
            &shared,
            &mut thread,
            "java/util/concurrent/CompletableFuture",
            "get",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(cf_obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = result {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "hello");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn completable_future_complete() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let cf = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "<init>",
        "()V",
        &[Value::Object(Some(cf))],
    );
    // isDone should be false
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(0));
    // complete
    let val = create_java_string(&shared, "result");
    let completed = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "complete",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(cf)), Value::Object(Some(val))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(completed, Value::Int(1));
    // isDone should be true
    let done2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done2, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn future_get_now_with_default() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let cf = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "<init>",
        "()V",
        &[Value::Object(Some(cf))],
    );
    let default_val = create_java_string(&shared, "default");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "getNow",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(cf)), Value::Object(Some(default_val))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "default");
    } else {
        panic!("expected string");
    }
}

// =========================================================================
// Phase 29: NumberFormat / DecimalFormat
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn number_format_get_instance() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let nf = call_native(
        &shared,
        &mut thread,
        "java/text/NumberFormat",
        "getInstance",
        "()Ljava/text/NumberFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(nf, Value::Object(Some(_))));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn decimal_format_init_and_format() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let df = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "<init>",
        "()V",
        &[Value::Object(Some(df))],
    );
    // format long
    let result = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "format",
        "(J)Ljava/lang/String;",
        &[Value::Object(Some(df)), Value::Long(12345)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        // Default DecimalFormat uses grouping separators
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "12,345");
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn decimal_format_pattern() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let df = alloc_receiver(&shared, &mut thread, "java/text/DecimalFormat", 3);
    let pattern = create_java_string(&shared, "#0.00");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(df)), Value::Object(Some(pattern))],
    );
    let pat = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "toPattern",
        "()Ljava/lang/String;",
        &[Value::Object(Some(df))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = pat {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "#0.00");
    } else {
        panic!("expected string");
    }
}

// =========================================================================
// Phase 30: URL / URI / InetAddress
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn url_parse_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let url = alloc_receiver(&shared, &mut thread, "java/net/URL", 6);
    let url_str = create_java_string(&shared, "https://example.com:8080/path?key=value");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(url)), Value::Object(Some(url_str))],
    );
    // getProtocol
    let proto = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "getProtocol",
        "()Ljava/lang/String;",
        &[Value::Object(Some(url))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = proto {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "https");
    } else {
        panic!("expected string");
    }
    // getHost
    let host = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "getHost",
        "()Ljava/lang/String;",
        &[Value::Object(Some(url))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = host {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "example.com"
        );
    } else {
        panic!("expected string");
    }
    // getPort
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "getPort",
        "()I",
        &[Value::Object(Some(url))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(8080));
    // getPath
    let path = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "getPath",
        "()Ljava/lang/String;",
        &[Value::Object(Some(url))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = path {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "/path");
    } else {
        panic!("expected string");
    }
    // getQuery
    let query = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "getQuery",
        "()Ljava/lang/String;",
        &[Value::Object(Some(url))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = query {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "key=value");
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn url_to_string_and_equals() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let url1 = alloc_receiver(&shared, &mut thread, "java/net/URL", 6);
    let url2 = alloc_receiver(&shared, &mut thread, "java/net/URL", 6);
    let s1 = create_java_string(&shared, "http://test.com/page");
    let s2 = create_java_string(&shared, "http://test.com/page");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(url1)), Value::Object(Some(s1))],
    );
    let _ = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(url2)), Value::Object(Some(s2))],
    );
    // equals
    let eq = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(url1)), Value::Object(Some(url2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
    // toString
    let ts = call_native(
        &shared,
        &mut thread,
        "java/net/URL",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(url1))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = ts {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "http://test.com/page"
        );
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn uri_create_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let uri_str = create_java_string(&shared, "https://api.example.com/v1/users");
    let uri = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
        &[Value::Object(Some(uri_str))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(u)) = uri {
        let scheme = call_native(
            &shared,
            &mut thread,
            "java/net/URI",
            "getScheme",
            "()Ljava/lang/String;",
            &[Value::Object(Some(u))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = scheme {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "https");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn inet_address_localhost() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // `getLocalHost()` is machine-hostname dependent; this test targets the
    // loopback address specifically, so use `getLoopbackAddress()`.
    let ia = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = ia {
        let host = call_native(
            &shared,
            &mut thread,
            "java/net/InetAddress",
            "getHostName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = host {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "localhost");
        } else {
            panic!("expected string");
        }
        let addr = call_native(
            &shared,
            &mut thread,
            "java/net/InetAddress",
            "getHostAddress",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = addr {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "127.0.0.1");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

// =========================================================================
// Phase 31: Logger / Level / Locale
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn logger_get_and_info() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let name = create_java_string(&shared, "MyLogger");
    let logger = call_native(
        &shared,
        &mut thread,
        "java/util/logging/Logger",
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(logger, Value::Object(Some(_))));
    if let Value::Object(Some(l)) = logger {
        // getName
        let n = call_native(
            &shared,
            &mut thread,
            "java/util/logging/Logger",
            "getName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = n {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "MyLogger");
        } else {
            panic!("expected string");
        }
        // info — should log
        let msg = create_java_string(&shared, "test info message");
        let _ = call_native(
            &shared,
            &mut thread,
            "java/util/logging/Logger",
            "info",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(l)), Value::Object(Some(msg))],
        );
        assert!(thread
            .printed_lines
            .iter()
            .any(|p| p.contains("test info message")));
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn logger_get_global() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let logger = call_native(
        &shared,
        &mut thread,
        "java/util/logging/Logger",
        "getGlobal",
        "()Ljava/util/logging/Logger;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(logger, Value::Object(Some(_))));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn level_constants() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let info = call_native(
        &shared,
        &mut thread,
        "java/util/logging/Level",
        "INFO",
        "()Ljava/util/logging/Level;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(l)) = info {
        let name = call_native(
            &shared,
            &mut thread,
            "java/util/logging/Level",
            "getName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = name {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "INFO");
        } else {
            panic!("expected string");
        }
        let iv = call_native(
            &shared,
            &mut thread,
            "java/util/logging/Level",
            "intValue",
            "()I",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(iv, Value::Int(800));
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn locale_default_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // `Locale.getDefault()` and the `user.language`/`user.country` system
    // properties are the SAME source of truth in a real JVM, so derive the
    // expectation from the properties rather than hardcoding `en`/`US`.
    // Hardcoding made this test host-dependent: it passed on Windows and on
    // a bare CI shell (both resolve to en_US) but would disagree with the
    // VM's own properties under e.g. `LANG=en_GB.UTF-8`.
    // `Locale` normalises language to lower case and country to upper case,
    // so the expectation is normalised the same way.
    let (expect_lang, expect_country) = {
        let props = shared.system_properties.read();
        (
            props
                .get("user.language")
                .cloned()
                .unwrap_or_default()
                .to_ascii_lowercase(),
            props
                .get("user.country")
                .cloned()
                .unwrap_or_default()
                .to_ascii_uppercase(),
        )
    };
    // The POSIX `C` and `POSIX` locales — what a Linux shell with no
    // `LANG`/`LC_ALL` reports — are mapped to English by the HotSpot
    // launcher (`java_props_md.c`). The VM must never surface them raw:
    // `Locale.getDefault().getLanguage()` is "en" there, never "C".
    assert!(
        !expect_lang.is_empty() && expect_lang != "c" && expect_lang != "posix",
        "user.language must be a real language code, got {expect_lang:?}"
    );
    let loc = call_native(
        &shared,
        &mut thread,
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(l)) = loc {
        let lang = call_native(
            &shared,
            &mut thread,
            "java/util/Locale",
            "getLanguage",
            "()Ljava/lang/String;",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = lang {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), expect_lang);
        } else {
            panic!("expected string");
        }
        let country = call_native(
            &shared,
            &mut thread,
            "java/util/Locale",
            "getCountry",
            "()Ljava/lang/String;",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = country {
            assert_eq!(
                read_java_string(&shared.mem.heap, s).unwrap(),
                expect_country
            );
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn locale_to_string_and_to_tag() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let loc = alloc_receiver(&shared, &mut thread, "java/util/Locale", 3);
    let lang = create_java_string(&shared, "en");
    let country = create_java_string(&shared, "US");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/Locale",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(loc)),
            Value::Object(Some(lang)),
            Value::Object(Some(country)),
        ],
    );
    // toString
    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/Locale",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(loc))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = ts {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "en_US");
    } else {
        panic!("expected string");
    }
    // toLanguageTag
    let tag = call_native(
        &shared,
        &mut thread,
        "java/util/Locale",
        "toLanguageTag",
        "()Ljava/lang/String;",
        &[Value::Object(Some(loc))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = tag {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "en-US");
    } else {
        panic!("expected string");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn locale_for_language_tag() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tag = create_java_string(&shared, "fr-FR");
    let loc = call_native(
        &shared,
        &mut thread,
        "java/util/Locale",
        "forLanguageTag",
        "(Ljava/lang/String;)Ljava/util/Locale;",
        &[Value::Object(Some(tag))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(l)) = loc {
        let lang = call_native(
            &shared,
            &mut thread,
            "java/util/Locale",
            "getLanguage",
            "()Ljava/lang/String;",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = lang {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "fr");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

/// The script subtag survives `forLanguageTag` in synthetic-JDK mode too.
///
/// This mode never reaches the real `sun.util.locale` BCP-47 parser the
/// native prefers (there is no such class here), so it exercises the Rust
/// fallback split and the `locale_data` side table's script slot — the
/// parts that a real-JDK run alone would leave completely untested.
/// `zh-hant-CN` used to come back as bare `zh_CN`
/// (`TestAcceptLanguage.bug56848`).
#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn locale_for_language_tag_keeps_script() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tag = create_java_string(&shared, "zh-hant-CN");
    let loc = call_native(
        &shared,
        &mut thread,
        "java/util/Locale",
        "forLanguageTag",
        "(Ljava/lang/String;)Ljava/util/Locale;",
        &[Value::Object(Some(tag))],
    )
    .unwrap()
    .unwrap();
    let Value::Object(Some(l)) = loc else {
        panic!("expected object");
    };
    let read = |thread: &mut JvmThread, method: &str| -> String {
        let v = call_native(
            &shared,
            thread,
            "java/util/Locale",
            method,
            "()Ljava/lang/String;",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        match v {
            Value::Object(Some(s)) => read_java_string(&shared.mem.heap, s).unwrap(),
            _ => panic!("expected string from {method}"),
        }
    };
    assert_eq!(read(&mut thread, "getLanguage"), "zh");
    // Title-cased, and NOT mistaken for the region.
    assert_eq!(read(&mut thread, "getScript"), "Hant");
    assert_eq!(read(&mut thread, "getCountry"), "CN");
    assert_eq!(read(&mut thread, "getVariant"), "");
    assert_eq!(read(&mut thread, "toString"), "zh_CN_#Hant");
    assert_eq!(read(&mut thread, "toLanguageTag"), "zh-Hant-CN");
}

// =========================================================================
// Phase 32: java.nio.file — Path, Paths, Files
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn paths_get_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, "/home/user/file.txt");
    let path = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(p)) = path {
        let ts = call_native(
            &shared,
            &mut thread,
            "java/nio/file/Path",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(p))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(str_obj)) = ts {
            // `Path.toString()` renders the OS-native separator (see
            // `nio_file.rs`'s `Path.toString` registration: CratonVM stores
            // paths with `/` internally but converts to `\` at this display
            // boundary on Windows, matching HotSpot's own
            // `WindowsPath.toString()`). The input here is POSIX-spelled, so
            // only Windows changes the expected separator.
            let expected = if cfg!(windows) {
                "\\home\\user\\file.txt"
            } else {
                "/home/user/file.txt"
            };
            assert_eq!(
                read_java_string(&shared.mem.heap, str_obj).unwrap(),
                expected
            );
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected path");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn path_get_file_name_and_parent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, "/home/user/file.txt");
    let path = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(p)) = path {
        // getFileName
        let name = call_native(
            &shared,
            &mut thread,
            "java/nio/file/Path",
            "getFileName",
            "()Ljava/nio/file/Path;",
            &[Value::Object(Some(p))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(n)) = name {
            let ts = call_native(
                &shared,
                &mut thread,
                "java/nio/file/Path",
                "toString",
                "()Ljava/lang/String;",
                &[Value::Object(Some(n))],
            )
            .unwrap()
            .unwrap();
            if let Value::Object(Some(str_obj)) = ts {
                assert_eq!(
                    read_java_string(&shared.mem.heap, str_obj).unwrap(),
                    "file.txt"
                );
            } else {
                panic!("expected string");
            }
        } else {
            panic!("expected path");
        }
        // getParent
        let parent = call_native(
            &shared,
            &mut thread,
            "java/nio/file/Path",
            "getParent",
            "()Ljava/nio/file/Path;",
            &[Value::Object(Some(p))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(parent, Value::Object(Some(_))));
    } else {
        panic!("expected path");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn path_resolve() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let base = create_java_string(&shared, "/home/user");
    let path = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(base))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(p)) = path {
        let child = create_java_string(&shared, "docs");
        let resolved = call_native(
            &shared,
            &mut thread,
            "java/nio/file/Path",
            "resolve",
            "(Ljava/lang/String;)Ljava/nio/file/Path;",
            &[Value::Object(Some(p)), Value::Object(Some(child))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(resolved, Value::Object(Some(_))));
    } else {
        panic!("expected path");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn path_name_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, "home/user/docs");
    let path = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(p)) = path {
        let count = call_native(
            &shared,
            &mut thread,
            "java/nio/file/Path",
            "getNameCount",
            "()I",
            &[Value::Object(Some(p))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(count, Value::Int(3));
    } else {
        panic!("expected path");
    }
}

// =========================================================================
// Phase 33: MessageDigest, SecureRandom
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn message_digest_sha256() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let algo = create_java_string(&shared, "SHA-256");
    let md = call_native(
        &shared,
        &mut thread,
        "java/security/MessageDigest",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/MessageDigest;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(md_obj)) = md {
        // update with some bytes
        let arr = shared.mem.heap.alloc_array(
            ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        for i in 0..5 {
            let _ = shared
                .mem
                .heap
                .set_array_element(arr, i, Value::Int(i as i32 + 1));
        }
        let _ = call_native(
            &shared,
            &mut thread,
            "java/security/MessageDigest",
            "update",
            "([B)V",
            &[Value::Object(Some(md_obj)), Value::Object(Some(arr))],
        );
        // digest
        let result = call_native(
            &shared,
            &mut thread,
            "java/security/MessageDigest",
            "digest",
            "()[B",
            &[Value::Object(Some(md_obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(hash)) = result {
            assert_eq!(shared.mem.heap.array_length(hash), 32); // SHA-256 = 32 bytes
        } else {
            panic!("expected byte array");
        }
        // getAlgorithm
        let a = call_native(
            &shared,
            &mut thread,
            "java/security/MessageDigest",
            "getAlgorithm",
            "()Ljava/lang/String;",
            &[Value::Object(Some(md_obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = a {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "SHA-256");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn message_digest_md5_length() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let algo = create_java_string(&shared, "MD5");
    let md = call_native(
        &shared,
        &mut thread,
        "java/security/MessageDigest",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/MessageDigest;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(md_obj)) = md {
        let len = call_native(
            &shared,
            &mut thread,
            "java/security/MessageDigest",
            "getDigestLength",
            "()I",
            &[Value::Object(Some(md_obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(len, Value::Int(16));
    } else {
        panic!("expected object");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn secure_random_next_int() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/security/SecureRandom",
        "<init>",
        "()V",
        &[Value::Object(Some(sr))],
    );
    let v = call_native(
        &shared,
        &mut thread,
        "java/security/SecureRandom",
        "nextInt",
        "()I",
        &[Value::Object(Some(sr))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(v, Value::Int(_)));
    // nextInt with bound
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/security/SecureRandom",
        "nextInt",
        "(I)I",
        &[Value::Object(Some(sr)), Value::Int(100)],
    )
    .unwrap()
    .unwrap();
    if let Value::Int(n) = v2 {
        assert!((0..100).contains(&n));
    } else {
        panic!("expected int");
    }
}

// =========================================================================
// Phase 34: ReadWriteLock, LongAdder, AtomicIntegerArray
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn rwlock_init_and_locks() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // NO `<init>` CALL HERE, deliberately. `register_synthetic_rwlock_natives`
    // does not intercept the `ReentrantReadWriteLock` constructors, and says
    // why: "Their three reference fields are { readerLock, writerLock, sync };
    // the historical synthetic initializer wrote integer state into those
    // slots, so a method reference such as ReentrantReadWriteLock::readLock
    // received null. Let genuine bytecode initialize the layout, while keeping
    // the native lock-operation backend below for the returned lock views."
    // The receiver is therefore left in its allocated (all-default) state,
    // which is what an object looks like between `new` and the constructor —
    // and the surviving natives must not depend on constructor state.
    let rwl = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantReadWriteLock",
        3,
    );
    // Real-JDK covariant return: ReentrantReadWriteLock.readLock/writeLock
    // return the concrete subclasses, not the Lock interface.
    let rl = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantReadWriteLock",
        "readLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$ReadLock;",
        &[Value::Object(Some(rwl))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(rl, Value::Object(Some(_))));
    let wl = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantReadWriteLock",
        "writeLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$WriteLock;",
        &[Value::Object(Some(rwl))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(wl, Value::Object(Some(_))));
    // Each view must point back at the parent lock in slot 0 — that back
    // pointer is what `rwl_parent_addr` keys the shared read/write state on,
    // so a null there would silently turn every lock()/unlock() into a no-op.
    let rl_ref = rl.as_object().unwrap();
    let wl_ref = wl.as_object().unwrap();
    assert_eq!(
        shared.mem.heap.get_field(rl_ref, 0),
        Value::Object(Some(rwl)),
        "ReadLock view must reference its parent ReentrantReadWriteLock"
    );
    assert_eq!(
        shared.mem.heap.get_field(wl_ref, 0),
        Value::Object(Some(rwl)),
        "WriteLock view must reference its parent ReentrantReadWriteLock"
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn long_adder_operations() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let la = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "<init>",
        "()V",
        &[Value::Object(Some(la))],
    );
    // increment 3 times
    for _ in 0..3 {
        let _ = call_native(
            &shared,
            &mut thread,
            "java/util/concurrent/atomic/LongAdder",
            "increment",
            "()V",
            &[Value::Object(Some(la))],
        );
    }
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "sum",
        "()J",
        &[Value::Object(Some(la))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Long(3));
    // add 10
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "add",
        "(J)V",
        &[Value::Object(Some(la)), Value::Long(10)],
    );
    let sum2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "sum",
        "()J",
        &[Value::Object(Some(la))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum2, Value::Long(13));
    // sumThenReset
    let sr = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "sumThenReset",
        "()J",
        &[Value::Object(Some(la))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sr, Value::Long(13));
    let sum3 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "sum",
        "()J",
        &[Value::Object(Some(la))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum3, Value::Long(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn atomic_integer_array_ops() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let aia = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "<init>",
        "(I)V",
        &[Value::Object(Some(aia)), Value::Int(5)],
    );
    // set and get
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "set",
        "(II)V",
        &[Value::Object(Some(aia)), Value::Int(2), Value::Int(42)],
    );
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "get",
        "(I)I",
        &[Value::Object(Some(aia)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(42));
    // compareAndSet
    let cas = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "compareAndSet",
        "(III)Z",
        &[
            Value::Object(Some(aia)),
            Value::Int(2),
            Value::Int(42),
            Value::Int(100),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cas, Value::Int(1));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "get",
        "(I)I",
        &[Value::Object(Some(aia)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(100));
    // length
    let len = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "length",
        "()I",
        &[Value::Object(Some(aia))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(5));
}

// =========================================================================
// Phase 37: StackTraceElement
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn stack_trace_element_init_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ste = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let cls = create_java_string(&shared, "com.example.Main");
    let method = create_java_string(&shared, "main");
    let file = create_java_string(&shared, "Main.java");
    let _ = call_native(
        &shared,
        &mut thread,
        "java/lang/StackTraceElement",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V",
        &[
            Value::Object(Some(ste)),
            Value::Object(Some(cls)),
            Value::Object(Some(method)),
            Value::Object(Some(file)),
            Value::Int(42),
        ],
    );
    // toString
    let ts = call_native(
        &shared,
        &mut thread,
        "java/lang/StackTraceElement",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ste))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = ts {
        let text = read_java_string(&shared.mem.heap, s).unwrap();
        assert!(text.contains("com.example.Main"));
        assert!(text.contains("main"));
        assert!(text.contains("42"));
    } else {
        panic!("expected string");
    }
    // getClassName
    let cn = call_native(
        &shared,
        &mut thread,
        "java/lang/StackTraceElement",
        "getClassName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ste))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = cn {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "com.example.Main"
        );
    } else {
        panic!("expected string");
    }
}

// =========================================================================
// Phase 38: java.time completion — LocalDateTime, ZonedDateTime, Period, DTF
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn local_date_time_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ldt = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDateTime",
        "of",
        "(IIIIIII)Ljava/time/LocalDateTime;",
        &[
            Value::Int(2024),
            Value::Int(3),
            Value::Int(15),
            Value::Int(10),
            Value::Int(30),
            Value::Int(45),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = ldt {
        let y = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getYear",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(y, Value::Int(2024));
        let mo = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getMonthValue",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(mo, Value::Int(3));
        let d = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getDayOfMonth",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(d, Value::Int(15));
        let h = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getHour",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(h, Value::Int(10));
        let mi = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getMinute",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(mi, Value::Int(30));
    } else {
        panic!("expected LocalDateTime");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn local_date_time_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ldt = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDateTime",
        "of",
        "(IIIIII)Ljava/time/LocalDateTime;",
        &[
            Value::Int(2024),
            Value::Int(12),
            Value::Int(25),
            Value::Int(14),
            Value::Int(30),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = ldt {
        let s = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(str_ref)) = s {
            let text = read_java_string(&shared.mem.heap, str_ref).unwrap();
            assert!(text.contains("2024-12-25"));
            assert!(text.contains("14:30"));
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected LDT");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn local_date_time_plus_days() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ldt = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDateTime",
        "of",
        "(IIIII)Ljava/time/LocalDateTime;",
        &[
            Value::Int(2024),
            Value::Int(1),
            Value::Int(30),
            Value::Int(10),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = ldt {
        let result = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "plusDays",
            "(J)Ljava/time/LocalDateTime;",
            &[Value::Object(Some(obj)), Value::Long(3)],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(r)) = result {
            let d = call_native(
                &shared,
                &mut thread,
                "java/time/LocalDateTime",
                "getDayOfMonth",
                "()I",
                &[Value::Object(Some(r))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(d, Value::Int(2)); // Jan 30 + 3 = Feb 2
            let mo = call_native(
                &shared,
                &mut thread,
                "java/time/LocalDateTime",
                "getMonthValue",
                "()I",
                &[Value::Object(Some(r))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(mo, Value::Int(2));
        } else {
            panic!("expected LDT");
        }
    } else {
        panic!("expected LDT");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn local_date_time_parse() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let input = create_java_string(&shared, "2024-06-15T09:30:00");
    let ldt = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDateTime",
        "parse",
        "(Ljava/lang/CharSequence;)Ljava/time/LocalDateTime;",
        &[Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = ldt {
        let y = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getYear",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(y, Value::Int(2024));
        let h = call_native(
            &shared,
            &mut thread,
            "java/time/LocalDateTime",
            "getHour",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(h, Value::Int(9));
    } else {
        panic!("expected LDT");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn zoned_date_time_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Create a ZoneId
    let zone_str = create_java_string(&shared, "Europe/Paris");
    let zone = call_native(
        &shared,
        &mut thread,
        "java/time/ZoneId",
        "of",
        "(Ljava/lang/String;)Ljava/time/ZoneId;",
        &[Value::Object(Some(zone_str))],
    )
    .unwrap()
    .unwrap();
    // Create LDT
    let ldt = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDateTime",
        "of",
        "(IIIII)Ljava/time/LocalDateTime;",
        &[
            Value::Int(2024),
            Value::Int(7),
            Value::Int(4),
            Value::Int(12),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    // Create ZDT
    let zdt = call_native(
        &shared,
        &mut thread,
        "java/time/ZonedDateTime",
        "of",
        "(Ljava/time/LocalDateTime;Ljava/time/ZoneId;)Ljava/time/ZonedDateTime;",
        &[ldt, zone],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = zdt {
        let y = call_native(
            &shared,
            &mut thread,
            "java/time/ZonedDateTime",
            "getYear",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(y, Value::Int(2024));
        let z = call_native(
            &shared,
            &mut thread,
            "java/time/ZonedDateTime",
            "getZone",
            "()Ljava/time/ZoneId;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(zr)) = z {
            let id = call_native(
                &shared,
                &mut thread,
                "java/time/ZoneId",
                "getId",
                "()Ljava/lang/String;",
                &[Value::Object(Some(zr))],
            )
            .unwrap()
            .unwrap();
            if let Value::Object(Some(s)) = id {
                assert_eq!(
                    read_java_string(&shared.mem.heap, s).unwrap(),
                    "Europe/Paris"
                );
            } else {
                panic!("expected string");
            }
        } else {
            panic!("expected zone");
        }
    } else {
        panic!("expected ZDT");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn period_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let p = call_native(
        &shared,
        &mut thread,
        "java/time/Period",
        "of",
        "(III)Ljava/time/Period;",
        &[Value::Int(1), Value::Int(6), Value::Int(15)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = p {
        let y = call_native(
            &shared,
            &mut thread,
            "java/time/Period",
            "getYears",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(y, Value::Int(1));
        let m = call_native(
            &shared,
            &mut thread,
            "java/time/Period",
            "getMonths",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(m, Value::Int(6));
        let d = call_native(
            &shared,
            &mut thread,
            "java/time/Period",
            "getDays",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(d, Value::Int(15));
        // toString
        let s = call_native(
            &shared,
            &mut thread,
            "java/time/Period",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(sr)) = s {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            assert_eq!(text, "P1Y6M15D");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected Period");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn zone_offset_of_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let zo = call_native(
        &shared,
        &mut thread,
        "java/time/ZoneOffset",
        "ofHours",
        "(I)Ljava/time/ZoneOffset;",
        &[Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = zo {
        let total = call_native(
            &shared,
            &mut thread,
            "java/time/ZoneOffset",
            "getTotalSeconds",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(total, Value::Int(18000)); // 5 * 3600
        let id = call_native(
            &shared,
            &mut thread,
            "java/time/ZoneOffset",
            "getId",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = id {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert!(text.contains("+05"));
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected ZoneOffset");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn dtf_of_pattern_and_format() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pattern = create_java_string(&shared, "yyyy-MM-dd HH:mm");
    let dtf = call_native(
        &shared,
        &mut thread,
        "java/time/format/DateTimeFormatter",
        "ofPattern",
        "(Ljava/lang/String;)Ljava/time/format/DateTimeFormatter;",
        &[Value::Object(Some(pattern))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(dtf, Value::Object(Some(_))));
}

// =========================================================================
// Phase 39: Collections extras
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn collections_empty_map() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "emptyMap",
        "()Ljava/util/Map;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(m)) = map {
        let size = call_native(
            &shared,
            &mut thread,
            "java/util/HashMap",
            "size",
            "()I",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(size, Value::Int(0));
    } else {
        panic!("expected map");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn collections_singleton_map() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let key = create_java_string(&shared, "hello");
    let val = create_java_string(&shared, "world");
    let map = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        &[Value::Object(Some(key)), Value::Object(Some(val))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(m)) = map {
        let size = call_native(
            &shared,
            &mut thread,
            "java/util/HashMap",
            "size",
            "()I",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(size, Value::Int(1));
    } else {
        panic!("expected map");
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn collections_n_copies() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let elem = create_java_string(&shared, "x");
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        &[Value::Int(5), Value::Object(Some(elem))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(l)) = list {
        let size = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "size",
            "()I",
            &[Value::Object(Some(l))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(size, Value::Int(5));
    } else {
        panic!("expected list");
    }
}

// =========================================================================
// Phase 40: BufferedInputStream/OutputStream
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn buffered_input_stream_init_and_available() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Create a BIS with null inner stream (testing init only)
    let bis = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/BufferedInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(bis)), Value::Object(None)],
    )
    .unwrap();

    // Available should be 0 (empty buffer)
    let avail = call_native(
        &shared,
        &mut thread,
        "java/io/BufferedInputStream",
        "available",
        "()I",
        &[Value::Object(Some(bis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avail, Value::Int(0));

    // markSupported should return true
    let ms = call_native(
        &shared,
        &mut thread,
        "java/io/BufferedInputStream",
        "markSupported",
        "()Z",
        &[Value::Object(Some(bis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ms, Value::Int(1));
}

// =========================================================================
// Phase 41: BlockingQueue
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn linked_blocking_queue_basic_ops() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Create LBQ
    let q = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(q))],
    )
    .unwrap();

    // Check empty
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(q))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));

    // Offer elements
    let a = create_java_string(&shared, "alpha");
    let b = create_java_string(&shared, "beta");
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(q)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(q)), Value::Object(Some(b))],
    )
    .unwrap();

    // Size = 2
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "size",
        "()I",
        &[Value::Object(Some(q))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));

    // Poll returns first
    let first = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(q))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = first {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "alpha");
    } else {
        panic!("expected string");
    }

    // Size now 1
    let size2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "size",
        "()I",
        &[Value::Object(Some(q))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size2, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn linked_blocking_queue_contains_and_remove() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let q = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(q))],
    )
    .unwrap();
    let a = create_java_string(&shared, "one");
    let b = create_java_string(&shared, "two");
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(q)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(q)), Value::Object(Some(b))],
    )
    .unwrap();

    // Contains
    let search = create_java_string(&shared, "two");
    let found = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(q)), Value::Object(Some(search))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found, Value::Int(1));

    // Remove
    let search2 = create_java_string(&shared, "one");
    let removed = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "remove",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(q)), Value::Object(Some(search2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(removed, Value::Int(1));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/LinkedBlockingQueue",
        "size",
        "()I",
        &[Value::Object(Some(q))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

// =========================================================================
// Phase 42: CharBuffer + typed NIO buffers
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn char_buffer_allocate_put_get_flip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Allocate CharBuffer with capacity 10
    let cb = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "allocate",
        "(I)Ljava/nio/CharBuffer;",
        &[Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    let cb_obj = match cb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected CharBuffer"),
    };

    // Put chars 'H', 'i'
    call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "put",
        "(C)Ljava/nio/CharBuffer;",
        &[Value::Object(Some(cb_obj)), Value::Int('H' as i32)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "put",
        "(C)Ljava/nio/CharBuffer;",
        &[Value::Object(Some(cb_obj)), Value::Int('i' as i32)],
    )
    .unwrap();

    // Position should be 2
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "position",
        "()I",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(2));

    // Flip
    call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "flip",
        "()Ljava/nio/CharBuffer;",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap();

    // Now remaining should be 2
    let rem = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "remaining",
        "()I",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rem, Value::Int(2));

    // Get chars
    let c1 = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "get",
        "()C",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(c1, Value::Int('H' as i32));
    let c2 = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "get",
        "()C",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(c2, Value::Int('i' as i32));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn char_buffer_wrap_charsequence_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "Hello NIO");
    let cb = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "wrap",
        "(Ljava/lang/CharSequence;)Ljava/nio/CharBuffer;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    let cb_obj = match cb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected CharBuffer"),
    };

    // toString should give "Hello NIO"
    let ts = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("Hello NIO".to_string())
    );

    // capacity should be 9
    let cap = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "capacity",
        "()I",
        &[Value::Object(Some(cb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(9));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn int_buffer_allocate_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ib = call_native(
        &shared,
        &mut thread,
        "java/nio/IntBuffer",
        "allocate",
        "(I)Ljava/nio/IntBuffer;",
        &[Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    let ib_obj = match ib {
        Value::Object(Some(o)) => o,
        _ => panic!("expected IntBuffer"),
    };

    // Put values 10, 20, 30
    for val in [10, 20, 30] {
        call_native(
            &shared,
            &mut thread,
            "java/nio/IntBuffer",
            "put",
            "(I)Ljava/nio/IntBuffer;",
            &[Value::Object(Some(ib_obj)), Value::Int(val)],
        )
        .unwrap();
    }

    // Flip and read back
    call_native(
        &shared,
        &mut thread,
        "java/nio/IntBuffer",
        "flip",
        "()Ljava/nio/IntBuffer;",
        &[Value::Object(Some(ib_obj))],
    )
    .unwrap();

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/IntBuffer",
        "get",
        "()I",
        &[Value::Object(Some(ib_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(10));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/nio/IntBuffer",
        "get",
        "()I",
        &[Value::Object(Some(ib_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(20));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn long_buffer_allocate_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lb = call_native(
        &shared,
        &mut thread,
        "java/nio/LongBuffer",
        "allocate",
        "(I)Ljava/nio/LongBuffer;",
        &[Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    let lb_obj = match lb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected LongBuffer"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/LongBuffer",
        "put",
        "(J)Ljava/nio/LongBuffer;",
        &[Value::Object(Some(lb_obj)), Value::Long(100_000_000_000)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/LongBuffer",
        "flip",
        "()Ljava/nio/LongBuffer;",
        &[Value::Object(Some(lb_obj))],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/LongBuffer",
        "get",
        "()J",
        &[Value::Object(Some(lb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Long(100_000_000_000));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn float_buffer_allocate_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let fb = call_native(
        &shared,
        &mut thread,
        "java/nio/FloatBuffer",
        "allocate",
        "(I)Ljava/nio/FloatBuffer;",
        &[Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    let fb_obj = match fb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected FloatBuffer"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/FloatBuffer",
        "put",
        "(F)Ljava/nio/FloatBuffer;",
        &[Value::Object(Some(fb_obj)), Value::Float(1.5)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/FloatBuffer",
        "flip",
        "()Ljava/nio/FloatBuffer;",
        &[Value::Object(Some(fb_obj))],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/FloatBuffer",
        "get",
        "()F",
        &[Value::Object(Some(fb_obj))],
    )
    .unwrap()
    .unwrap();
    match v {
        Value::Float(f) => assert!((f - 1.5).abs() < 0.001),
        _ => panic!("expected float"),
    };
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn double_buffer_allocate_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let db = call_native(
        &shared,
        &mut thread,
        "java/nio/DoubleBuffer",
        "allocate",
        "(I)Ljava/nio/DoubleBuffer;",
        &[Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    let db_obj = match db {
        Value::Object(Some(o)) => o,
        _ => panic!("expected DoubleBuffer"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/DoubleBuffer",
        "put",
        "(D)Ljava/nio/DoubleBuffer;",
        &[Value::Object(Some(db_obj)), Value::Double(1.234567890)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/DoubleBuffer",
        "flip",
        "()Ljava/nio/DoubleBuffer;",
        &[Value::Object(Some(db_obj))],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/DoubleBuffer",
        "get",
        "()D",
        &[Value::Object(Some(db_obj))],
    )
    .unwrap()
    .unwrap();
    match v {
        Value::Double(d) => assert!((d - 1.234567890).abs() < 0.0001),
        _ => panic!("expected double"),
    };
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn short_buffer_allocate_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sb = call_native(
        &shared,
        &mut thread,
        "java/nio/ShortBuffer",
        "allocate",
        "(I)Ljava/nio/ShortBuffer;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let sb_obj = match sb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected ShortBuffer"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/ShortBuffer",
        "put",
        "(S)Ljava/nio/ShortBuffer;",
        &[Value::Object(Some(sb_obj)), Value::Int(42)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ShortBuffer",
        "put",
        "(S)Ljava/nio/ShortBuffer;",
        &[Value::Object(Some(sb_obj)), Value::Int(99)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/ShortBuffer",
        "flip",
        "()Ljava/nio/ShortBuffer;",
        &[Value::Object(Some(sb_obj))],
    )
    .unwrap();

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/ShortBuffer",
        "get",
        "()S",
        &[Value::Object(Some(sb_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(42));
}

// =========================================================================
// Phase 43: Exception hierarchy
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn exception_hierarchy_constructors() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Test a few exception types with string constructor
    for exc_class in &[
        "java/lang/IllegalArgumentException",
        "java/lang/UnsupportedOperationException",
        "java/util/NoSuchElementException",
        "java/lang/SecurityException",
    ] {
        let msg = create_java_string(&shared, "test error");
        let exc = shared.mem.heap.alloc_object(ClassId::new(0), 2);
        call_native(
            &shared,
            &mut thread,
            exc_class,
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(exc)), Value::Object(Some(msg))],
        )
        .unwrap();

        let got_msg = call_native(
            &shared,
            &mut thread,
            exc_class,
            "getMessage",
            "()Ljava/lang/String;",
            &[Value::Object(Some(exc))],
        )
        .unwrap()
        .unwrap();
        let msg_obj = match got_msg {
            Value::Object(Some(o)) => o,
            _ => panic!("expected string for {}", exc_class),
        };
        assert_eq!(
            read_java_string(&shared.mem.heap, msg_obj),
            Some("test error".to_string())
        );
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn exception_no_arg_constructor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let exc = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ArithmeticException",
        "<init>",
        "()V",
        &[Value::Object(Some(exc))],
    )
    .unwrap();

    // getMessage should return null
    let msg = call_native(
        &shared,
        &mut thread,
        "java/lang/ArithmeticException",
        "getMessage",
        "()Ljava/lang/String;",
        &[Value::Object(Some(exc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(msg, Value::Object(None));
}

// =========================================================================
// Phase 44: Iterator protocol
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn generic_iterator_has_next_and_next() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a snapshot iterator manually: field 0 = Object[], field 1 = cursor Int
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let s1 = create_java_string(&shared, "a");
    let s2 = create_java_string(&shared, "b");
    let s3 = create_java_string(&shared, "c");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(s1)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(s2)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Object(Some(s3)));

    let itr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(itr, 0, Value::Object(Some(arr)));
    shared.mem.heap.set_field(itr, 1, Value::Int(0));

    // hasNext via generic Iterator interface
    let has = call_native(
        &shared,
        &mut thread,
        "java/util/Iterator",
        "hasNext",
        "()Z",
        &[Value::Object(Some(itr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(1));

    // next
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(itr))],
    )
    .unwrap()
    .unwrap();
    let o1 = match v1 {
        Value::Object(Some(o)) => o,
        _ => panic!("expected obj"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, o1),
        Some("a".to_string())
    );

    // Advance through all elements
    call_native(
        &shared,
        &mut thread,
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(itr))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(itr))],
    )
    .unwrap();

    // hasNext should be false now
    let has2 = call_native(
        &shared,
        &mut thread,
        "java/util/Iterator",
        "hasNext",
        "()Z",
        &[Value::Object(Some(itr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has2, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn list_iterator_previous() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let s1 = create_java_string(&shared, "x");
    let s2 = create_java_string(&shared, "y");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(s1)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(s2)));

    let itr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(itr, 0, Value::Object(Some(arr)));
    shared.mem.heap.set_field(itr, 1, Value::Int(2)); // cursor at end

    let has_prev = call_native(
        &shared,
        &mut thread,
        "java/util/ListIterator",
        "hasPrevious",
        "()Z",
        &[Value::Object(Some(itr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has_prev, Value::Int(1));

    let prev = call_native(
        &shared,
        &mut thread,
        "java/util/ListIterator",
        "previous",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(itr))],
    )
    .unwrap()
    .unwrap();
    let o = match prev {
        Value::Object(Some(o)) => o,
        _ => panic!("expected obj"),
    };
    assert_eq!(read_java_string(&shared.mem.heap, o), Some("y".to_string()));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn empty_iterator_has_no_elements() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let itr = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "emptyIterator",
        "()Ljava/util/Iterator;",
        &[],
    )
    .unwrap()
    .unwrap();
    let itr_obj = match itr {
        Value::Object(Some(o)) => o,
        _ => panic!("expected iterator"),
    };

    let has = call_native(
        &shared,
        &mut thread,
        "java/util/Iterator",
        "hasNext",
        "()Z",
        &[Value::Object(Some(itr_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(0));
}

// =========================================================================
// Phase 45: java.lang extras
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn charsequence_length_and_char_at() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "ABCDE");
    let len = call_native(
        &shared,
        &mut thread,
        "java/lang/CharSequence",
        "length",
        "()I",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(5));

    let ch = call_native(
        &shared,
        &mut thread,
        "java/lang/CharSequence",
        "charAt",
        "(I)C",
        &[Value::Object(Some(s)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ch, Value::Int('C' as i32));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_io_security) fn classloader_get_system_and_load_class() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cl = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(cl, Value::Object(Some(_))));

    // toString should return "SystemClassLoader"
    let cl_obj = match cl {
        Value::Object(Some(o)) => o,
        _ => panic!("expected obj"),
    };
    let ts = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(cl_obj))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("SystemClassLoader".to_string())
    );
}

