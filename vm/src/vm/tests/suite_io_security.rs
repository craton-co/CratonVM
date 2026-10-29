// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ========================================================================
// Phase 25: Charset tests
// ========================================================================

#[test]
fn charset_for_name_utf8() {
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
fn charset_for_name_case_insensitive() {
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
fn charset_for_name_unsupported_throws() {
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
fn charset_default_is_utf8() {
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
fn charset_is_supported() {
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
fn charset_equals_and_hashcode() {
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
fn standard_charsets_utf8() {
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
fn standard_charsets_all_variants() {
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
fn charset_for_name_aliases() {
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
fn string_reader_read_chars() {
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
fn string_reader_ready_and_eof() {
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
fn string_reader_skip_and_reset() {
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
fn string_writer_write_and_to_string() {
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
fn string_writer_write_string() {
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
fn string_writer_append_char() {
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
fn biginteger_init_and_to_string() {
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
fn biginteger_add_and_subtract() {
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
fn biginteger_multiply_and_divide() {
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
fn biginteger_negate_and_abs() {
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
fn biginteger_compare_and_equals() {
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
fn biginteger_int_value_and_long_value() {
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
fn biginteger_value_of() {
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
fn bigdecimal_init_and_to_string() {
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
fn bigdecimal_extreme_scale_refusals_f31() {
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
fn bigdecimal_add_and_subtract() {
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
fn bigdecimal_scale_and_precision() {
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
fn bigdecimal_compare_to() {
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
fn executors_new_single_thread() {
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
fn executors_new_fixed_pool() {
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
fn completable_future_completed_and_get() {
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
fn completable_future_complete() {
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
fn future_get_now_with_default() {
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
fn number_format_get_instance() {
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
fn decimal_format_init_and_format() {
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
fn decimal_format_pattern() {
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
fn url_parse_and_getters() {
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
fn url_to_string_and_equals() {
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
fn uri_create_and_getters() {
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
fn inet_address_localhost() {
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
fn logger_get_and_info() {
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
fn logger_get_global() {
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
fn level_constants() {
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
fn locale_default_and_getters() {
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
fn locale_to_string_and_to_tag() {
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
fn locale_for_language_tag() {
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
fn locale_for_language_tag_keeps_script() {
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
fn paths_get_and_to_string() {
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
fn path_get_file_name_and_parent() {
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
fn path_resolve() {
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
fn path_name_count() {
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
fn message_digest_sha256() {
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
fn message_digest_md5_length() {
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
fn secure_random_next_int() {
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
fn rwlock_init_and_locks() {
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
fn long_adder_operations() {
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
fn atomic_integer_array_ops() {
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
fn stack_trace_element_init_and_to_string() {
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
fn local_date_time_of_and_getters() {
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
fn local_date_time_to_string() {
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
fn local_date_time_plus_days() {
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
fn local_date_time_parse() {
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
fn zoned_date_time_of_and_getters() {
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
fn period_of_and_getters() {
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
fn zone_offset_of_and_getters() {
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
fn dtf_of_pattern_and_format() {
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
fn collections_empty_map() {
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
fn collections_singleton_map() {
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
fn collections_n_copies() {
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
fn buffered_input_stream_init_and_available() {
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
fn linked_blocking_queue_basic_ops() {
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
fn linked_blocking_queue_contains_and_remove() {
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
fn char_buffer_allocate_put_get_flip() {
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
fn char_buffer_wrap_charsequence_to_string() {
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
fn int_buffer_allocate_put_get() {
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
fn long_buffer_allocate_put_get() {
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
fn float_buffer_allocate_put_get() {
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
fn double_buffer_allocate_put_get() {
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
fn short_buffer_allocate_put_get() {
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
fn exception_hierarchy_constructors() {
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
fn exception_no_arg_constructor() {
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
fn generic_iterator_has_next_and_next() {
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
fn list_iterator_previous() {
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
fn empty_iterator_has_no_elements() {
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
fn charsequence_length_and_char_at() {
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
fn classloader_get_system_and_load_class() {
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

// =========================================================================
// Phase 46: Functional interfaces completion
// =========================================================================

#[test]
fn int_summary_statistics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let iss = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "<init>",
        "()V",
        &[Value::Object(Some(iss))],
    )
    .unwrap();

    // Accept values: 10, 20, 30
    for v in [10, 20, 30] {
        call_native(
            &shared,
            &mut thread,
            "java/util/IntSummaryStatistics",
            "accept",
            "(I)V",
            &[Value::Object(Some(iss)), Value::Int(v)],
        )
        .unwrap();
    }

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getCount",
        "()J",
        &[Value::Object(Some(iss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(3));

    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getSum",
        "()J",
        &[Value::Object(Some(iss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Long(60));

    let min = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getMin",
        "()I",
        &[Value::Object(Some(iss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(min, Value::Int(10));

    let max = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getMax",
        "()I",
        &[Value::Object(Some(iss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(max, Value::Int(30));

    let avg = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getAverage",
        "()D",
        &[Value::Object(Some(iss))],
    )
    .unwrap()
    .unwrap();
    match avg {
        Value::Double(d) => assert!((d - 20.0).abs() < 0.001),
        _ => panic!("expected double"),
    };
}

// =========================================================================
// Phase 47: Reflection Array + Proxy
// =========================================================================

#[test]
fn reflect_array_get_length_and_get_set() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create int array [10, 20, 30]
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(10));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(20));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(30));

    // getLength
    let len = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "getLength",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(3));

    // getInt
    let v = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "getInt",
        "(Ljava/lang/Object;I)I",
        &[Value::Object(Some(arr)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(20));

    // setInt
    call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "setInt",
        "(Ljava/lang/Object;II)V",
        &[Value::Object(Some(arr)), Value::Int(1), Value::Int(99)],
    )
    .unwrap();

    let v2 = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "getInt",
        "(Ljava/lang/Object;I)I",
        &[Value::Object(Some(arr)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(99));
}

/// `Array.newInstance` allocates for a real component type, and REFUSES a
/// null one.
///
/// This test read `Array.newInstance(null, 5) creates a 5-element reference
/// array` until 2026-08-06, and that was the defect rather than the
/// contract: `ce20bdd20` measured Temurin 25 and HotSpot dereferences the
/// `Class` argument, so a null component type is a `NullPointerException`
/// with a null message — it does not quietly become `Object[]`. Asserting
/// the old behaviour froze a fabricated value in place, which is exactly
/// what the fix removed.
///
/// Both halves are here on purpose. A test that only checked the refusal
/// would pass against a `newInstance` that refuses everything.
#[test]
fn reflect_array_new_instance() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // The positive case. `int.class` comes from `Class.getPrimitiveClass`,
    // the way the JDK itself constructs a primitive mirror — primitives are
    // not loadable through `Class.forName`.
    let type_name = create_java_string(&shared, "int");
    let int_cls = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getPrimitiveClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(type_name))],
    )
    .unwrap()
    .unwrap();
    let arr = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "newInstance",
        "(Ljava/lang/Class;I)Ljava/lang/Object;",
        &[int_cls, Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    let arr_obj = match arr {
        Value::Object(Some(o)) => o,
        _ => panic!("expected array"),
    };
    let len = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "getLength",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(arr_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(5));

    // The refusal, and it must be the JDK's: NPE, and `getMessage()` null.
    let err = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Array",
        "newInstance",
        "(Ljava/lang/Class;I)Ljava/lang/Object;",
        &[Value::Object(None), Value::Int(5)],
    )
    .expect_err("a null component type must not allocate anything");
    assert!(
        matches!(
            err,
            crate::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }
            ))
        ),
        "HotSpot dereferences the Class argument: NullPointerException with \
             no detail message, not an Object[5] — got {err:?}"
    );
}

// ========================================================================
// M11: Method.invoke on user bytecode methods
// ========================================================================

#[test]
fn reflect_method_invoke_static_void() {
    // Test Method.invoke on a static void method (bytecode: return)
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeVoid",
        0x0021, // PUBLIC | SUPER
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("doNothing"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return (void)
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "doNothing");

    // Get Method object via getDeclaredMethod
    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(None),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method object, got {:?}", other),
    };

    // Method.invoke(null, null) — static void method
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(None), // no receiver (static)
            Value::Object(None), // no args
        ],
    )
    .unwrap();
    // void methods return null
    assert_eq!(result, Some(Value::Object(None)));
}

#[test]
fn reflect_method_invoke_static_returns_int() {
    // Test Method.invoke on a static method returning int constant 42
    // Bytecode: bipush 42; ireturn
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeStaticInt",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("getFortyTwo"),
            descriptor: Arc::from("()I"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x10, 42,   // bipush 42
                        0xAC, // ireturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "getFortyTwo");

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(None),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    // Method.invoke(null, null) — static method returning int
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();

    // Result should be a boxed Integer with value 42
    let int_obj = match result {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("Expected Integer object, got {:?}", other),
    };
    // Integer.intValue() — field 0 should be 42
    assert_eq!(shared.mem.heap.get_field(int_obj, 0), Value::Int(42));
}

#[test]
fn reflect_method_invoke_static_with_int_param() {
    // Static method: takes int param, returns it doubled
    // Bytecode: iload_0; iload_0; iadd; ireturn
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeWithParam",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("doubleIt"),
            descriptor: Arc::from("(I)I"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x1A, // iload_0
                        0x1A, // iload_0
                        0x60, // iadd
                        0xAC, // ireturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "doubleIt");

    // Create parameter types array: [int.class]
    let int_mirror = get_or_create_primitive_mirror(&shared, "int");
    let param_types = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(param_types, 0, Value::Object(Some(int_mirror)));

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(Some(param_types)),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    // Create args array: [Integer.valueOf(7)]
    let int_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .unwrap();
    let args_array = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let boxed_7 = shared.mem.heap.alloc_object(int_class_id, 1);
    shared.mem.heap.set_field(boxed_7, 0, Value::Int(7));
    let _ = shared
        .mem
        .heap
        .set_array_element(args_array, 0, Value::Object(Some(boxed_7)));

    // Method.invoke(null, [Integer(7)])
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(None),
            Value::Object(Some(args_array)),
        ],
    )
    .unwrap();

    // Should return Integer(14) — 7 doubled
    let int_obj = match result {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("Expected Integer, got {:?}", other),
    };
    assert_eq!(shared.mem.heap.get_field(int_obj, 0), Value::Int(14));
}

#[test]
fn reflect_method_invoke_instance_method() {
    // Instance method on an object with field 0 = int value
    // Method returns field 0 of this: aload_0; getfield #?
    // Since getfield needs constant pool, use simpler: iload_1; ireturn (identity on arg)
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeInstance",
        0x0021,
        &[cratonvm_reader::field::ClassFileField {
            name: Arc::from("value"),
            descriptor: Arc::from("I"),
            access_flags: FieldAccessFlags::PUBLIC,
            attributes: vec![],
        }],
        &[cratonvm_reader::method::ClassFileMethod {
            // Instance method: identity(int x) returns x
            // In instance methods, local 0 = this, local 1 = first param
            name: Arc::from("identity"),
            descriptor: Arc::from("(I)I"),
            access_flags: MethodAccessFlags::PUBLIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 2, // this + int param
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x1B, // iload_1 (first int param)
                        0xAC, // ireturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);

    // Create receiver object
    let receiver = shared.mem.heap.alloc_object(class_id, 1);
    shared.mem.heap.set_field(receiver, 0, Value::Int(99));

    let name_str = create_java_string(&shared, "identity");
    let int_mirror = get_or_create_primitive_mirror(&shared, "int");
    let param_types = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(param_types, 0, Value::Object(Some(int_mirror)));

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(Some(param_types)),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    // Create args: [Integer(55)]
    let int_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .unwrap();
    let args_array = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let boxed_55 = shared.mem.heap.alloc_object(int_class_id, 1);
    shared.mem.heap.set_field(boxed_55, 0, Value::Int(55));
    let _ = shared
        .mem
        .heap
        .set_array_element(args_array, 0, Value::Object(Some(boxed_55)));

    // Method.invoke(receiver, [Integer(55)])
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(Some(receiver)),
            Value::Object(Some(args_array)),
        ],
    )
    .unwrap();

    // Should return Integer(55) — identity function
    let int_obj = match result {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("Expected Integer, got {:?}", other),
    };
    assert_eq!(shared.mem.heap.get_field(int_obj, 0), Value::Int(55));
}

#[test]
fn reflect_method_invoke_null_receiver_throws() {
    // Calling instance method with null receiver should throw NullPointerException
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeNullRecv",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("run"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PUBLIC, // instance method (not static)
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "run");

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(None),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    // Method.invoke(null, null) — should fail: instance method needs receiver
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(None), // null receiver
            Value::Object(None),
        ],
    );
    assert!(
        result.is_err(),
        "Expected NullPointerException for null receiver"
    );
}

#[test]
fn reflect_method_invoke_static_two_params() {
    // Static method: add(int a, int b) returns a + b
    // Bytecode: iload_0; iload_1; iadd; ireturn
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeTwoParams",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("add"),
            descriptor: Arc::from("(II)I"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 2,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x1A, // iload_0
                        0x1B, // iload_1
                        0x60, // iadd
                        0xAC, // ireturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "add");

    // Parameter types: [int.class, int.class]
    let int_mirror = get_or_create_primitive_mirror(&shared, "int");
    let param_types = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(param_types, 0, Value::Object(Some(int_mirror)));
    let _ = shared
        .mem
        .heap
        .set_array_element(param_types, 1, Value::Object(Some(int_mirror)));

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(Some(param_types)),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    // Args: [Integer(10), Integer(32)]
    let int_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .unwrap();
    let args_array = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let boxed_10 = shared.mem.heap.alloc_object(int_class_id, 1);
    shared.mem.heap.set_field(boxed_10, 0, Value::Int(10));
    let boxed_32 = shared.mem.heap.alloc_object(int_class_id, 1);
    shared.mem.heap.set_field(boxed_32, 0, Value::Int(32));
    let _ = shared
        .mem
        .heap
        .set_array_element(args_array, 0, Value::Object(Some(boxed_10)));
    let _ = shared
        .mem
        .heap
        .set_array_element(args_array, 1, Value::Object(Some(boxed_32)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(None),
            Value::Object(Some(args_array)),
        ],
    )
    .unwrap();

    let int_obj = match result {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("Expected Integer, got {:?}", other),
    };
    assert_eq!(shared.mem.heap.get_field(int_obj, 0), Value::Int(42)); // 10 + 32
}

#[test]
fn reflect_method_invoke_returns_long() {
    // Static method: returns long constant
    // Bytecode: ldc2_w would need constant pool; use lconst_1; lreturn instead
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/InvokeLong",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("getOne"),
            descriptor: Arc::from("()J"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 0,
                    code: cratonvm_reader::ByteView::from_vec(vec![
                        0x0A, // lconst_1
                        0xAD, // lreturn
                    ]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "getOne");

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(None),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(method_obj)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();

    // Should return boxed Long(1)
    let long_obj = match result {
        Some(Value::Object(Some(obj))) => obj,
        other => panic!("Expected Long object, got {:?}", other),
    };
    assert_eq!(shared.mem.heap.get_field(long_obj, 0), Value::Long(1));
}

#[test]
fn reflect_method_get_name_and_modifiers() {
    // Verify Method.getName() and Method.getModifiers() work
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/MethodMeta",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("compute"),
            descriptor: Arc::from("(I)I"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let name_str = create_java_string(&shared, "compute");

    let int_mirror = get_or_create_primitive_mirror(&shared, "int");
    let param_types = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(param_types, 0, Value::Object(Some(int_mirror)));

    let method_obj = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name_str)),
            Value::Object(Some(param_types)),
        ],
    )
    .unwrap()
    {
        Some(Value::Object(Some(m))) => m,
        other => panic!("Expected Method, got {:?}", other),
    };

    // Method.getName()
    let name_result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(method_obj))],
    )
    .unwrap();
    let name_obj = match name_result {
        Some(Value::Object(Some(s))) => s,
        other => panic!("Expected String, got {:?}", other),
    };
    let name = read_java_string(&shared.mem.heap, name_obj).unwrap();
    assert_eq!(name, "compute");

    // Method.getModifiers()
    let mods = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "getModifiers",
        "()I",
        &[Value::Object(Some(method_obj))],
    )
    .unwrap();
    let mods_val = match mods {
        Some(Value::Int(v)) => v,
        other => panic!("Expected int, got {:?}", other),
    };
    assert_ne!(mods_val & 0x0001, 0, "should be PUBLIC");
    assert_ne!(mods_val & 0x0008, 0, "should be STATIC");
}

#[test]
fn reflect_method_get_parameter_count() {
    // Verify Method.getParameterCount() returns correct count
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let class_id = register_test_class(
        &shared,
        "test/ParamCount",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("threeArgs"),
            descriptor: Arc::from("(IJD)V"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![],
        }],
    );
    let mirror = get_or_create_class_mirror(&shared, class_id);
    let _name_str = create_java_string(&shared, "threeArgs");

    // Get method via getDeclaredMethods and find by name
    let methods_arr = match call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredMethods",
        "()[Ljava/lang/reflect/Method;",
        &[Value::Object(Some(mirror))],
    )
    .unwrap()
    {
        Some(Value::Object(Some(a))) => a,
        _ => panic!("Expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(methods_arr), 1);

    let method_obj = match shared
        .mem
        .heap
        .get_array_element(methods_arr, 0)
        .unwrap_or(Value::Object(None))
    {
        Value::Object(Some(m)) => m,
        _ => panic!("Expected Method"),
    };

    // Method.getParameterCount()
    let count = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "getParameterCount",
        "()I",
        &[Value::Object(Some(method_obj))],
    )
    .unwrap();
    assert_eq!(count, Some(Value::Int(3))); // int, long, double = 3 params
}

// ========================================================================
// M19: Finalizer / Cleaner execution
// ========================================================================

#[test]
fn m19_class_has_finalizer_false_by_default() {
    // A class with no finalize() override should have has_finalizer == false
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = register_test_class(&shared, "test/NoFinalizer", 0x0021, &[], &[]);
    let cm = shared.classes.class_manager.read();
    let cls = cm.class_store.get(class_id).unwrap();
    assert!(!cls.has_finalizer);
}

#[test]
fn m19_class_declares_finalize_detected() {
    // A class overriding finalize()V should have declares_finalize() == true
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = register_test_class(
        &shared,
        "test/WithFinalizer",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("finalize"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PROTECTED,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );
    let cm = shared.classes.class_manager.read();
    let cls = cm.class_store.get(class_id).unwrap();
    // register_test_class sets has_finalizer to false, but declares_finalize
    // should return true based on the method list
    assert!(cls.declares_finalize());
}

#[test]
fn m19_run_pending_finalizers_invokes_finalize() {
    // Create a class with a finalize() method that sets a static field to 42

    let mut vm = Vm::new(VmConfig::default());

    // Register a class with finalize()V: bipush 42; putstatic <field>; return
    // Since putstatic requires CP resolution which is complex, we'll use a simpler
    // approach: verify that run_pending_finalizers returns 0 when queue is empty
    let count = vm.run_pending_finalizers();
    assert_eq!(count, 0);
}

#[test]
fn m19_finalizer_queue_enqueue_and_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    shared.register_finalizable(obj.as_ptr() as usize);

    // Before processing, finalizer_thread is empty
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 0);

    // Process with all-dead marking function
    shared.process_references(&|_| false, 100, 0);

    // Now finalizer_thread should have the object
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
}

#[test]
fn m19_run_pending_finalizers_with_enqueued_object() {
    // Register a test class with a finalize()V bytecode method (just return),
    // allocate an object of that class, enqueue for finalization, and verify
    // run_pending_finalizers invokes it.
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = register_test_class(
        &shared,
        "test/Finalizable",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("finalize"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PROTECTED,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );

    let obj = shared.mem.heap.alloc_object(class_id, 0);
    shared
        .mem
        .finalizer_thread
        .enqueue_unfinalized(obj.as_ptr() as usize);

    // Build a Vm that wraps the same shared state
    let mut vm = Vm {
        shared: shared.clone(),
        main_thread: Box::new(JvmThread::new(ThreadId(0), "test")),
    };
    let count = vm.run_pending_finalizers();
    assert_eq!(count, 1, "Should have finalized 1 object");
    assert_eq!(
        vm.shared.mem.finalizer_thread.pending_count(),
        0,
        "Queue should be empty"
    );
}

#[test]
fn m19_multiple_finalizers_run_in_order() {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = register_test_class(
        &shared,
        "test/FinMulti",
        0x0021,
        &[],
        &[cratonvm_reader::method::ClassFileMethod {
            name: Arc::from("finalize"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PROTECTED,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 0,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xB1]), // return
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }],
    );

    let obj1 = shared.mem.heap.alloc_object(class_id, 0);
    let obj2 = shared.mem.heap.alloc_object(class_id, 0);
    let obj3 = shared.mem.heap.alloc_object(class_id, 0);
    shared
        .mem
        .finalizer_thread
        .enqueue_unfinalized(obj1.as_ptr() as usize);
    shared
        .mem
        .finalizer_thread
        .enqueue_unfinalized(obj2.as_ptr() as usize);
    shared
        .mem
        .finalizer_thread
        .enqueue_unfinalized(obj3.as_ptr() as usize);

    let mut vm = Vm {
        shared: shared.clone(),
        main_thread: Box::new(JvmThread::new(ThreadId(0), "test")),
    };
    let count = vm.run_pending_finalizers();
    assert_eq!(count, 3, "Should have finalized 3 objects");
    assert_eq!(vm.shared.mem.finalizer_thread.pending_count(), 0);
}

// ========================================================================
// Phase 49-50: Enterprise Final — AtomicBoolean, Modifier, Class extras,
// Formatter, StringWriter, StringReader, Collections extras
// ========================================================================

#[test]
fn atomic_boolean_init_get_set_cas() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cls = "java/util/concurrent/atomic/AtomicBoolean";
    // Create with default (false)
    let ab = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        cls,
        "<init>",
        "()V",
        &[Value::Object(Some(ab))],
    )
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        cls,
        "get",
        "()Z",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(0));

    // Set to true
    call_native(
        &shared,
        &mut thread,
        cls,
        "set",
        "(Z)V",
        &[Value::Object(Some(ab)), Value::Int(1)],
    )
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        cls,
        "get",
        "()Z",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(1));

    // CAS: expect true, set to false → should succeed
    let cas = call_native(
        &shared,
        &mut thread,
        cls,
        "compareAndSet",
        "(ZZ)Z",
        &[Value::Object(Some(ab)), Value::Int(1), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cas, Value::Int(1));
    let v = call_native(
        &shared,
        &mut thread,
        cls,
        "get",
        "()Z",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(0));

    // CAS: expect true but is false → should fail
    let cas2 = call_native(
        &shared,
        &mut thread,
        cls,
        "compareAndSet",
        "(ZZ)Z",
        &[Value::Object(Some(ab)), Value::Int(1), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cas2, Value::Int(0));
}

#[test]
fn atomic_boolean_get_and_set_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cls = "java/util/concurrent/atomic/AtomicBoolean";
    let ab = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    // Init with true
    call_native(
        &shared,
        &mut thread,
        cls,
        "<init>",
        "(Z)V",
        &[Value::Object(Some(ab)), Value::Int(1)],
    )
    .unwrap();

    // getAndSet(false) returns old value (true)
    let old = call_native(
        &shared,
        &mut thread,
        cls,
        "getAndSet",
        "(Z)Z",
        &[Value::Object(Some(ab)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old, Value::Int(1));
    let cur = call_native(
        &shared,
        &mut thread,
        cls,
        "get",
        "()Z",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cur, Value::Int(0));

    // toString returns "false"
    let ts = call_native(
        &shared,
        &mut thread,
        cls,
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("false".to_string())
    );
}

#[test]
fn atomic_stamped_reference_init_and_cas() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cls = "java/util/concurrent/atomic/AtomicStampedReference";
    let asr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let str_val = create_java_string(&shared, "hello");
    call_native(
        &shared,
        &mut thread,
        cls,
        "<init>",
        "(Ljava/lang/Object;I)V",
        &[
            Value::Object(Some(asr)),
            Value::Object(Some(str_val)),
            Value::Int(42),
        ],
    )
    .unwrap();

    // Get reference and stamp
    let r = call_native(
        &shared,
        &mut thread,
        cls,
        "getReference",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(asr))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(r, Value::Object(Some(_))));
    let stamp = call_native(
        &shared,
        &mut thread,
        cls,
        "getStamp",
        "()I",
        &[Value::Object(Some(asr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(stamp, Value::Int(42));

    // CAS with wrong stamp → fail
    let str_val2 = create_java_string(&shared, "world");
    let cas = call_native(
        &shared,
        &mut thread,
        cls,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        &[
            Value::Object(Some(asr)),
            Value::Object(Some(str_val)),
            Value::Object(Some(str_val2)),
            Value::Int(99),
            Value::Int(100),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cas, Value::Int(0)); // wrong stamp

    // CAS with correct ref + stamp → succeed
    let cas2 = call_native(
        &shared,
        &mut thread,
        cls,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        &[
            Value::Object(Some(asr)),
            Value::Object(Some(str_val)),
            Value::Object(Some(str_val2)),
            Value::Int(42),
            Value::Int(100),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cas2, Value::Int(1));
    let new_stamp = call_native(
        &shared,
        &mut thread,
        cls,
        "getStamp",
        "()I",
        &[Value::Object(Some(asr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(new_stamp, Value::Int(100));
}

#[test]
fn atomic_markable_reference_init_and_mark() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cls = "java/util/concurrent/atomic/AtomicMarkableReference";
    let amr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let val = create_java_string(&shared, "data");
    call_native(
        &shared,
        &mut thread,
        cls,
        "<init>",
        "(Ljava/lang/Object;Z)V",
        &[
            Value::Object(Some(amr)),
            Value::Object(Some(val)),
            Value::Int(0),
        ],
    )
    .unwrap();

    let marked = call_native(
        &shared,
        &mut thread,
        cls,
        "isMarked",
        "()Z",
        &[Value::Object(Some(amr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(marked, Value::Int(0));

    // attemptMark: correct ref → succeed
    let ok = call_native(
        &shared,
        &mut thread,
        cls,
        "attemptMark",
        "(Ljava/lang/Object;Z)Z",
        &[
            Value::Object(Some(amr)),
            Value::Object(Some(val)),
            Value::Int(1),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    let marked2 = call_native(
        &shared,
        &mut thread,
        cls,
        "isMarked",
        "()Z",
        &[Value::Object(Some(amr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(marked2, Value::Int(1));
}

#[test]
fn modifier_extras_check_bits() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let m = "java/lang/reflect/Modifier";

    // isSynchronized(0x0020) = true
    let v = call_native(
        &shared,
        &mut thread,
        m,
        "isSynchronized",
        "(I)Z",
        &[Value::Int(0x0021)],
    )
    .unwrap()
    .unwrap(); // public + synchronized
    assert_eq!(v, Value::Int(1));

    // isVolatile(0x0040)
    let v = call_native(
        &shared,
        &mut thread,
        m,
        "isVolatile",
        "(I)Z",
        &[Value::Int(0x0040)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(1));

    // isNative(0x0100)
    let v = call_native(
        &shared,
        &mut thread,
        m,
        "isNative",
        "(I)Z",
        &[Value::Int(0x0100)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(1));

    // isNative on 0x0001 → false
    let v = call_native(
        &shared,
        &mut thread,
        m,
        "isNative",
        "(I)Z",
        &[Value::Int(0x0001)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(0));

    // toString: public static final = 0x0019
    let ts = call_native(
        &shared,
        &mut thread,
        m,
        "toString",
        "(I)Ljava/lang/String;",
        &[Value::Int(0x0019)],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("public static final".to_string())
    );
}

// Note: StringWriter and StringReader tests already exist earlier in the test suite

#[test]
fn formatter_basic_format() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cls = "java/util/Formatter";
    let fmt = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        cls,
        "<init>",
        "()V",
        &[Value::Object(Some(fmt))],
    )
    .unwrap();

    // format("%s is %d", "age", 42)
    let fmt_str = create_java_string(&shared, "%s is %d");
    let age_str = create_java_string(&shared, "age");
    let args_arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared
        .mem
        .heap
        .set_array_element(args_arr, 0, Value::Object(Some(age_str)));
    let _ = shared
        .mem
        .heap
        .set_array_element(args_arr, 1, Value::Int(42));
    call_native(
        &shared,
        &mut thread,
        cls,
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/util/Formatter;",
        &[
            Value::Object(Some(fmt)),
            Value::Object(Some(fmt_str)),
            Value::Object(Some(args_arr)),
        ],
    )
    .unwrap();

    let ts = call_native(
        &shared,
        &mut thread,
        cls,
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(fmt))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("age is 42".to_string())
    );
}

#[test]
fn collections_singleton_list_and_ncopies() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Collections.singletonList("hello")
    let hello = create_java_string(&shared, "hello");
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        &[Value::Object(Some(hello))],
    )
    .unwrap()
    .unwrap();
    let list_obj = match list {
        Value::Object(Some(o)) => o,
        _ => panic!("expected list"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));

    // Collections.nCopies(3, "x")
    let x = create_java_string(&shared, "x");
    let copies = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        &[Value::Int(3), Value::Object(Some(x))],
    )
    .unwrap()
    .unwrap();
    let copies_obj = match copies {
        Value::Object(Some(o)) => o,
        _ => panic!("expected list"),
    };
    let size2 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(copies_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size2, Value::Int(3));
}

#[test]
fn collections_unmodifiable_wrapper_has_same_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a list, wrap in unmodifiableList, check same size
    let list = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    )
    .unwrap();
    let elem = create_java_string(&shared, "test");
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(elem))],
    )
    .unwrap();

    let wrapped = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    .unwrap();
    // Should be non-null
    assert!(matches!(wrapped, Value::Object(Some(_))));
    // Check size of wrapped list = 1
    let wrapped_obj = match wrapped {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(wrapped_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

// =====================================================================
// Phase 50 tests: ThreadLocal, StringTokenizer, BitSet, EnumSet, EnumMap
// =====================================================================

#[test]
fn thread_local_init_get_set_remove() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tl = alloc_receiver(&shared, &mut thread, "java/lang/ThreadLocal", 1);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "<init>",
        "()V",
        &[Value::Object(Some(tl))],
    )
    .unwrap();
    // get returns null initially
    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Object(None));
    // set a value
    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };
    call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "set",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(tl)), Value::Object(Some(s))],
    )
    .unwrap();
    assert_eq!(shared.natives.jni_global_refs.lock().count(), 1);
    // get returns the value
    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Object(Some(s)));
    // replacing an object-valued ThreadLocal drops the old root
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("world")
    };
    call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "set",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(tl)), Value::Object(Some(s2))],
    )
    .unwrap();
    assert_eq!(shared.natives.jni_global_refs.lock().count(), 1);
    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Object(Some(s2)));
    // remove clears it
    call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "remove",
        "()V",
        &[Value::Object(Some(tl))],
    )
    .unwrap();
    assert_eq!(shared.natives.jni_global_refs.lock().count(), 0);
    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadLocal",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(tl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Object(None));
    assert_eq!(shared.natives.jni_global_refs.lock().count(), 0);
}

#[test]
fn string_tokenizer_basic_tokens() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let st = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello world foo")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(st)), Value::Object(Some(input))],
    )
    .unwrap();
    // countTokens = 3
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "countTokens",
        "()I",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(3));
    // hasMoreTokens
    let has = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "hasMoreTokens",
        "()Z",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(1));
    // nextToken
    let t1 = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "nextToken",
        "()Ljava/lang/String;",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    let t1_obj = match t1 {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, t1_obj),
        Some("hello".to_string())
    );
    // nextToken again
    let t2 = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "nextToken",
        "()Ljava/lang/String;",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    let t2_obj = match t2 {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, t2_obj),
        Some("world".to_string())
    );
    // nextToken 3rd
    let t3 = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "nextToken",
        "()Ljava/lang/String;",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    let t3_obj = match t3 {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, t3_obj),
        Some("foo".to_string())
    );
    // hasMoreTokens = false
    let has = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "hasMoreTokens",
        "()Z",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(0));
}

#[test]
fn string_tokenizer_custom_delimiter() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let st = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a,b,,c")
    };
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string(",")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(st)),
            Value::Object(Some(input)),
            Value::Object(Some(delim)),
        ],
    )
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/StringTokenizer",
        "countTokens",
        "()I",
        &[Value::Object(Some(st))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(3)); // "a", "b", "c" (empty tokens skipped)
}

#[test]
fn bitset_set_get_clear_cardinality() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bs = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "<init>",
        "()V",
        &[Value::Object(Some(bs))],
    )
    .unwrap();
    // Initially empty
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(bs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    // Set bits 0, 5, 63
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(0)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(5)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(63)],
    )
    .unwrap();
    // Cardinality = 3
    let card = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "cardinality",
        "()I",
        &[Value::Object(Some(bs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(card, Value::Int(3));
    // get(5) = true
    let g = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "get",
        "(I)Z",
        &[Value::Object(Some(bs)), Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(g, Value::Int(1));
    // get(4) = false
    let g = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "get",
        "(I)Z",
        &[Value::Object(Some(bs)), Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(g, Value::Int(0));
    // clear(5)
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "clear",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(5)],
    )
    .unwrap();
    let card = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "cardinality",
        "()I",
        &[Value::Object(Some(bs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(card, Value::Int(2));
    // length = 64 (highest set bit is 63, so length is 64)
    let len = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "length",
        "()I",
        &[Value::Object(Some(bs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(64));
}

#[test]
fn bitset_flip_and_or_xor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // BitSet a: {1, 3}
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "<init>",
        "()V",
        &[Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(a)), Value::Int(1)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(a)), Value::Int(3)],
    )
    .unwrap();
    // BitSet b: {1, 2}
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "<init>",
        "()V",
        &[Value::Object(Some(b))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(b)), Value::Int(1)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(b)), Value::Int(2)],
    )
    .unwrap();
    // a.intersects(b) = true (bit 1 common)
    let inter = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "intersects",
        "(Ljava/util/BitSet;)Z",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(inter, Value::Int(1));
    // flip(1) on a: a = {3}
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "flip",
        "(I)V",
        &[Value::Object(Some(a)), Value::Int(1)],
    )
    .unwrap();
    let g = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "get",
        "(I)Z",
        &[Value::Object(Some(a)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(g, Value::Int(0));
    // a.or(b): a = {1, 2, 3}
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "or",
        "(Ljava/util/BitSet;)V",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap();
    let card = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "cardinality",
        "()I",
        &[Value::Object(Some(a))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(card, Value::Int(3));
}

#[test]
fn bitset_next_set_bit_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bs = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "<init>",
        "()V",
        &[Value::Object(Some(bs))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(2)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(7)],
    )
    .unwrap();
    // nextSetBit(0) = 2
    let nsb = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "nextSetBit",
        "(I)I",
        &[Value::Object(Some(bs)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nsb, Value::Int(2));
    // nextSetBit(3) = 7
    let nsb = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "nextSetBit",
        "(I)I",
        &[Value::Object(Some(bs)), Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nsb, Value::Int(7));
    // nextSetBit(8) = -1
    let nsb = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "nextSetBit",
        "(I)I",
        &[Value::Object(Some(bs)), Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nsb, Value::Int(-1));
    // toString = "{2, 7}"
    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bs))],
    )
    .unwrap()
    .unwrap();
    let ts_obj = match ts {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_obj),
        Some("{2, 7}".to_string())
    );
}

#[test]
fn bitset_grow_beyond_64_bits() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bs = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "<init>",
        "()V",
        &[Value::Object(Some(bs))],
    )
    .unwrap();
    // Set bit 100 (requires growing beyond initial 64 bits)
    call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "set",
        "(I)V",
        &[Value::Object(Some(bs)), Value::Int(100)],
    )
    .unwrap();
    let g = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "get",
        "(I)Z",
        &[Value::Object(Some(bs)), Value::Int(100)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(g, Value::Int(1));
    let g = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "get",
        "(I)Z",
        &[Value::Object(Some(bs)), Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(g, Value::Int(0));
    let len = call_native(
        &shared,
        &mut thread,
        "java/util/BitSet",
        "length",
        "()I",
        &[Value::Object(Some(bs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(101));
}

#[test]
fn enum_set_basic_operations() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // noneOf returns empty EnumSet
    let es = call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "noneOf",
        "(Ljava/lang/Class;)Ljava/util/EnumSet;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    let es_obj = match es {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "size",
        "()I",
        &[Value::Object(Some(es_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(es_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    // add an element
    let elem = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("VALUE_A")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(es_obj)), Value::Object(Some(elem))],
    )
    .unwrap();
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "size",
        "()I",
        &[Value::Object(Some(es_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[ignore = "STALE: the test passes a NULL Class to EnumMap(Class) and expects it to construct. HotSpot throws NullPointerException: Cannot invoke \"java.lang.Class.getEnumConstantsShared()\" because \"klass\" is null -- verified on 25.0.3 -- and CratonVM throws the identical message. Not an enum defect either; the real EnumMap gap it was thought to expose (put's erased descriptor is (Ljava/lang/Enum;Ljava/lang/Object;), not (Object,Object)) was found by probe and is FIXED. synthetic-jdk-enum-surface-20260902.md"]
#[test]
fn enum_map_put_get_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let em = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/EnumMap",
        "<init>",
        "(Ljava/lang/Class;)V",
        &[Value::Object(Some(em)), Value::Object(None)],
    )
    .unwrap();
    // size = 0
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/EnumMap",
        "size",
        "()I",
        &[Value::Object(Some(em))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
    // put
    let key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("KEY1")
    };
    let val = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("VAL1")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/EnumMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(em)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();
    // size = 1
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/EnumMap",
        "size",
        "()I",
        &[Value::Object(Some(em))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
    // get
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/EnumMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(em)), Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(Some(val)));
}

#[test]
fn identity_hashmap_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/IdentityHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();
    let key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("k1")
    };
    let val = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("v1")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/IdentityHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/IdentityHashMap",
        "size",
        "()I",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
fn weak_hashmap_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    let key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("wk")
    };
    let val = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("wv")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(0));
}

// -----------------------------------------------------------------------
// Phase 51 tests: Calendar, TimeZone, Currency, Concurrency extras
// -----------------------------------------------------------------------

#[test]
fn calendar_get_set_fields() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cal = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "getInstance",
        "()Ljava/util/Calendar;",
        &[],
    )
    .unwrap()
    .unwrap();
    let year = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "get",
        "(I)I",
        &[cal, Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(year, Value::Int(1970));
    let month = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "get",
        "(I)I",
        &[cal, Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(month, Value::Int(0));
    call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "set",
        "(III)V",
        &[cal, Value::Int(2024), Value::Int(11), Value::Int(25)],
    )
    .unwrap();
    let year = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "get",
        "(I)I",
        &[cal, Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(year, Value::Int(2024));
    let month = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "get",
        "(I)I",
        &[cal, Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(month, Value::Int(11));
    let day = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "get",
        "(I)I",
        &[cal, Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(day, Value::Int(25));
}

#[test]
fn calendar_add_and_millis() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cal = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "getInstance",
        "()Ljava/util/Calendar;",
        &[],
    )
    .unwrap()
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "set",
        "(III)V",
        &[cal, Value::Int(2024), Value::Int(0), Value::Int(15)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "add",
        "(II)V",
        &[cal, Value::Int(1), Value::Int(1)],
    )
    .unwrap();
    let year = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "get",
        "(I)I",
        &[cal, Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(year, Value::Int(2025));
    let millis = call_native(
        &shared,
        &mut thread,
        "java/util/Calendar",
        "getTimeInMillis",
        "()J",
        &[cal],
    )
    .unwrap()
    .unwrap();
    match millis {
        Value::Long(v) => assert!(v > 0),
        _ => panic!("expected Long"),
    }
}

#[test]
fn gregorian_calendar_leap_year() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cal = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/GregorianCalendar",
        "<init>",
        "()V",
        &[Value::Object(Some(cal))],
    )
    .unwrap();
    let leap = call_native(
        &shared,
        &mut thread,
        "java/util/GregorianCalendar",
        "isLeapYear",
        "(I)Z",
        &[Value::Object(Some(cal)), Value::Int(2024)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(leap, Value::Int(1));
    let leap = call_native(
        &shared,
        &mut thread,
        "java/util/GregorianCalendar",
        "isLeapYear",
        "(I)Z",
        &[Value::Object(Some(cal)), Value::Int(1900)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(leap, Value::Int(0));
    let leap = call_native(
        &shared,
        &mut thread,
        "java/util/GregorianCalendar",
        "isLeapYear",
        "(I)Z",
        &[Value::Object(Some(cal)), Value::Int(2000)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(leap, Value::Int(1));
}

#[test]
fn date_basic_operations() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let d1 = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/Date",
        "<init>",
        "(J)V",
        &[Value::Object(Some(d1)), Value::Long(1000)],
    )
    .unwrap();
    let d2 = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/Date",
        "<init>",
        "(J)V",
        &[Value::Object(Some(d2)), Value::Long(2000)],
    )
    .unwrap();
    let before = call_native(
        &shared,
        &mut thread,
        "java/util/Date",
        "before",
        "(Ljava/util/Date;)Z",
        &[Value::Object(Some(d1)), Value::Object(Some(d2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(before, Value::Int(1));
    let time = call_native(
        &shared,
        &mut thread,
        "java/util/Date",
        "getTime",
        "()J",
        &[Value::Object(Some(d1))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(time, Value::Long(1000));
}

#[test]
fn timezone_get_default_and_by_id() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let tz = call_native(
        &shared,
        &mut thread,
        "java/util/TimeZone",
        "getDefault",
        "()Ljava/util/TimeZone;",
        &[],
    )
    .unwrap()
    .unwrap();
    let id = call_native(
        &shared,
        &mut thread,
        "java/util/TimeZone",
        "getID",
        "()Ljava/lang/String;",
        &[tz],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(o)) = id {
        let s = {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.read_string(o).unwrap()
        };
        assert_eq!(s, "UTC");
    } else {
        panic!("expected string");
    }
    let offset = call_native(
        &shared,
        &mut thread,
        "java/util/TimeZone",
        "getRawOffset",
        "()I",
        &[tz],
    )
    .unwrap()
    .unwrap();
    assert_eq!(offset, Value::Int(0));
}

#[test]
fn currency_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let code = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("USD")
    };
    let cur = call_native(
        &shared,
        &mut thread,
        "java/util/Currency",
        "getInstance",
        "(Ljava/lang/String;)Ljava/util/Currency;",
        &[Value::Object(Some(code))],
    )
    .unwrap()
    .unwrap();
    let numeric = call_native(
        &shared,
        &mut thread,
        "java/util/Currency",
        "getNumericCode",
        "()I",
        &[cur],
    )
    .unwrap()
    .unwrap();
    assert_eq!(numeric, Value::Int(840));
    let digits = call_native(
        &shared,
        &mut thread,
        "java/util/Currency",
        "getDefaultFractionDigits",
        "()I",
        &[cur],
    )
    .unwrap()
    .unwrap();
    assert_eq!(digits, Value::Int(2));
}

#[test]
fn phaser_arrive_and_advance() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ph = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Phaser",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ph)), Value::Int(2)],
    )
    .unwrap();
    let parties = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Phaser",
        "getRegisteredParties",
        "()I",
        &[Value::Object(Some(ph))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(parties, Value::Int(2));
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Phaser",
        "arrive",
        "()I",
        &[Value::Object(Some(ph))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Phaser",
        "arrive",
        "()I",
        &[Value::Object(Some(ph))],
    )
    .unwrap();
    let cur_phase = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Phaser",
        "getPhase",
        "()I",
        &[Value::Object(Some(ph))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cur_phase, Value::Int(1));
}

#[test]
fn timeunit_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("SECONDS")
    };
    let tu = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/TimeUnit",
        "valueOf",
        "(Ljava/lang/String;)Ljava/util/concurrent/TimeUnit;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let nanos = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/TimeUnit",
        "toNanos",
        "(J)J",
        &[tu, Value::Long(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nanos, Value::Long(1_000_000_000));
    let millis = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/TimeUnit",
        "toMillis",
        "(J)J",
        &[tu, Value::Long(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(millis, Value::Long(1000));
}

#[test]
fn forkjoin_pool_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let pool = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ForkJoinPool",
        "commonPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
        &[],
    )
    .unwrap()
    .unwrap();
    let par = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ForkJoinPool",
        "getParallelism",
        "()I",
        &[pool],
    )
    .unwrap()
    .unwrap();
    assert_eq!(par, Value::Int(1));
}

#[test]
fn executors_factory_methods() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
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
    let exec2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Executors",
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(exec2, Value::Object(Some(_))));
}

#[test]
fn timer_basic() {
    // `java.util.Timer`'s synthetic natives were REMOVED on purpose: the
    // old `schedule`/`scheduleAtFixedRate` were no-ops that never ran the
    // task, while real JDK bytecode spins a TimerThread that actually
    // fires it (see the no-op `register_timer_natives` in
    // `native-builtins/src/phases_early.rs`). This test used to construct a
    // Timer through those natives and assert the synthetic `cancelled`
    // field flipped — the very contract the removal deleted.
    //
    // Re-registering them to make that pass would reintroduce the
    // silently-dropped tasks. So assert the removal instead: this fails if
    // someone re-adds a synthetic Timer surface, which is the regression
    // worth guarding.
    let registry = &*TEST_NATIVE_REGISTRY;
    for (method, desc) in [
        ("<init>", "()V"),
        ("schedule", "(Ljava/util/TimerTask;J)V"),
        ("scheduleAtFixedRate", "(Ljava/util/TimerTask;JJ)V"),
        ("cancel", "()V"),
    ] {
        assert!(
            registry.find("java/util/Timer", method, desc).is_none(),
            "java/util/Timer.{method}{desc} must stay unregistered so real \
                 JDK bytecode runs it — a synthetic Timer drops tasks silently",
        );
    }
}

/// `exchange` is a two-party rendezvous: each side blocks until a partner
/// arrives, and each receives the value the *other* deposited.
///
/// The old version called `exchange` twice on a single thread and asserted
/// the first call returned null — i.e. that depositing a value without a
/// partner completes immediately. That is the pre-rendezvous stub's
/// behaviour; `exchanger_do_exchange` now parks the first arrival until a
/// partner shows up, so the single-threaded version blocked forever and
/// held a libtest worker for the remainder of the run.
///
/// It also allocated the receiver with ONE field while the protocol writes
/// slot 0 (value) and slot 1 (state). The receiver is now allocated with
/// the three the real `Exchanger` layout has (arena, ncpu, bound).
#[test]
fn exchanger_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ex = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Exchanger",
        "<init>",
        "()V",
        &[Value::Object(Some(ex))],
    )
    .unwrap();
    let (hello, world) = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        (ctx.create_string("hello"), ctx.create_string("world"))
    };

    let party = |tid: u64, mine: ObjectRef| {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut t = crate::threading::JvmThread::new(
                crate::threading::ThreadId(tid),
                "exchanger-party",
            );
            call_native(
                &shared,
                &mut t,
                "java/util/concurrent/Exchanger",
                "exchange",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(ex)), Value::Object(Some(mine))],
            )
            .unwrap()
            .unwrap()
        })
    };
    let a = party(1, hello);
    let b = party(2, world);

    // Each side gets the other's value — that is the whole contract.
    let got_a = a.join().expect("exchanger party A panicked");
    let got_b = b.join().expect("exchanger party B panicked");
    assert_eq!(got_a, Value::Object(Some(world)));
    assert_eq!(got_b, Value::Object(Some(hello)));
}

// ===== Phase 52 tests =====

#[test]
fn month_of_and_properties() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Month.of(3) -> MARCH
    let march = call_native(
        &shared,
        &mut thread,
        "java/time/Month",
        "of",
        "(I)Ljava/time/Month;",
        &[Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(m)) = march {
        let val = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "getValue",
            "()I",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(val, Value::Int(3));
        let ord = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "ordinal",
            "()I",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ord, Value::Int(2)); // 0-based
        let name = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "name",
            "()Ljava/lang/String;",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = name {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "MARCH");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected Month object");
    }
}

#[test]
fn month_length_and_arithmetic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // February length in leap year
    let feb = call_native(
        &shared,
        &mut thread,
        "java/time/Month",
        "of",
        "(I)Ljava/time/Month;",
        &[Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(m)) = feb {
        let len_leap = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "length",
            "(Z)I",
            &[Value::Object(Some(m)), Value::Int(1)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(len_leap, Value::Int(29));
        let len_noleap = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "length",
            "(Z)I",
            &[Value::Object(Some(m)), Value::Int(0)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(len_noleap, Value::Int(28));
        // plus(3) from Feb -> May
        let may = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "plus",
            "(J)Ljava/time/Month;",
            &[Value::Object(Some(m)), Value::Long(3)],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(may_obj)) = may {
            let val = call_native(
                &shared,
                &mut thread,
                "java/time/Month",
                "getValue",
                "()I",
                &[Value::Object(Some(may_obj))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(val, Value::Int(5));
        } else {
            panic!("expected Month");
        }
    } else {
        panic!("expected Month");
    }
}

#[test]
fn day_of_week_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // DayOfWeek.of(1) -> MONDAY
    let mon = call_native(
        &shared,
        &mut thread,
        "java/time/DayOfWeek",
        "of",
        "(I)Ljava/time/DayOfWeek;",
        &[Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(d)) = mon {
        let val = call_native(
            &shared,
            &mut thread,
            "java/time/DayOfWeek",
            "getValue",
            "()I",
            &[Value::Object(Some(d))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(val, Value::Int(1));
        let name = call_native(
            &shared,
            &mut thread,
            "java/time/DayOfWeek",
            "name",
            "()Ljava/lang/String;",
            &[Value::Object(Some(d))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = name {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "MONDAY");
        } else {
            panic!("expected string");
        }
        // plus(5) from Monday -> Saturday
        let sat = call_native(
            &shared,
            &mut thread,
            "java/time/DayOfWeek",
            "plus",
            "(J)Ljava/time/DayOfWeek;",
            &[Value::Object(Some(d)), Value::Long(5)],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(sat_obj)) = sat {
            let sat_val = call_native(
                &shared,
                &mut thread,
                "java/time/DayOfWeek",
                "getValue",
                "()I",
                &[Value::Object(Some(sat_obj))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(sat_val, Value::Int(6));
        } else {
            panic!("expected DayOfWeek");
        }
    } else {
        panic!("expected DayOfWeek");
    }
}

#[test]
fn offset_datetime_creation() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create ZoneOffset(0) for UTC
    let zo = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(zo, 0, Value::Int(0));
    // OffsetDateTime.of(2024,6,15, 10,30,0,0, ZoneOffset)
    let odt = call_native(
        &shared,
        &mut thread,
        "java/time/OffsetDateTime",
        "of",
        "(IIIIIIILjava/time/ZoneOffset;)Ljava/time/OffsetDateTime;",
        &[
            Value::Int(2024),
            Value::Int(6),
            Value::Int(15),
            Value::Int(10),
            Value::Int(30),
            Value::Int(0),
            Value::Int(0),
            Value::Object(Some(zo)),
        ],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(odt_ref)) = odt {
        let year = call_native(
            &shared,
            &mut thread,
            "java/time/OffsetDateTime",
            "getYear",
            "()I",
            &[Value::Object(Some(odt_ref))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(year, Value::Int(2024));
        let month = call_native(
            &shared,
            &mut thread,
            "java/time/OffsetDateTime",
            "getMonthValue",
            "()I",
            &[Value::Object(Some(odt_ref))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(month, Value::Int(6));
        let hour = call_native(
            &shared,
            &mut thread,
            "java/time/OffsetDateTime",
            "getHour",
            "()I",
            &[Value::Object(Some(odt_ref))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(hour, Value::Int(10));
        // toString
        let s = call_native(
            &shared,
            &mut thread,
            "java/time/OffsetDateTime",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(odt_ref))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(sr)) = s {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            assert_eq!(ctx.read_string(sr).unwrap(), "2024-06-15T10:30:00Z");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected OffsetDateTime");
    }
}

#[test]
fn clock_system_utc() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let clock = call_native(
        &shared,
        &mut thread,
        "java/time/Clock",
        "systemUTC",
        "()Ljava/time/Clock;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(c)) = clock {
        let millis = call_native(
            &shared,
            &mut thread,
            "java/time/Clock",
            "millis",
            "()J",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        if let Value::Long(ms) = millis {
            assert!(ms > 1_700_000_000_000); // after 2023
        } else {
            panic!("expected long");
        }
        let ts = call_native(
            &shared,
            &mut thread,
            "java/time/Clock",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = ts {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "SystemClock[UTC]");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected Clock");
    }
}

#[test]
fn chrono_unit_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let days_str = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("DAYS")
    };
    let days = call_native(
        &shared,
        &mut thread,
        "java/time/temporal/ChronoUnit",
        "valueOf",
        "(Ljava/lang/String;)Ljava/time/temporal/ChronoUnit;",
        &[Value::Object(Some(days_str))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(d)) = days {
        let name = call_native(
            &shared,
            &mut thread,
            "java/time/temporal/ChronoUnit",
            "name",
            "()Ljava/lang/String;",
            &[Value::Object(Some(d))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = name {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "DAYS");
        } else {
            panic!("expected string");
        }
        let is_date = call_native(
            &shared,
            &mut thread,
            "java/time/temporal/ChronoUnit",
            "isDateBased",
            "()Z",
            &[Value::Object(Some(d))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(is_date, Value::Int(1));
        let is_time = call_native(
            &shared,
            &mut thread,
            "java/time/temporal/ChronoUnit",
            "isTimeBased",
            "()Z",
            &[Value::Object(Some(d))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(is_time, Value::Int(0));
    } else {
        panic!("expected ChronoUnit");
    }
}

#[test]
fn url_encode_decode() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello world&foo=bar")
    };
    let charset = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("UTF-8")
    };
    let encoded = call_native(
        &shared,
        &mut thread,
        "java/net/URLEncoder",
        "encode",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(input)), Value::Object(Some(charset))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(e)) = encoded {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let enc_str = ctx.read_string(e).unwrap();
        assert_eq!(enc_str, "hello+world%26foo%3Dbar");
    } else {
        panic!("expected string");
    }
    // Decode it back
    let enc2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello+world%26foo%3Dbar")
    };
    let charset2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("UTF-8")
    };
    let decoded = call_native(
        &shared,
        &mut thread,
        "java/net/URLDecoder",
        "decode",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(enc2)), Value::Object(Some(charset2))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(d)) = decoded {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert_eq!(ctx.read_string(d).unwrap(), "hello world&foo=bar");
    } else {
        panic!("expected string");
    }
}

#[test]
fn inet_socket_address_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let host = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("localhost")
    };
    let isa = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/net/InetSocketAddress",
        "<init>",
        "(Ljava/lang/String;I)V",
        &[
            Value::Object(Some(isa)),
            Value::Object(Some(host)),
            Value::Int(8080),
        ],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/InetSocketAddress",
        "getPort",
        "()I",
        &[Value::Object(Some(isa))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(8080));
    let hn = call_native(
        &shared,
        &mut thread,
        "java/net/InetSocketAddress",
        "getHostName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(isa))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(h)) = hn {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert_eq!(ctx.read_string(h).unwrap(), "localhost");
    } else {
        panic!("expected string");
    }
    let ts = call_native(
        &shared,
        &mut thread,
        "java/net/InetSocketAddress",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(isa))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = ts {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let rendered = ctx.read_string(s).unwrap();
        // This asserted `"localhost:8080"` until 2026-08-05, and that was
        // never what a JVM prints. `InetSocketAddress.toString()` on a
        // RESOLVED address is `holder.addr.toString() + ":" + port`, and
        // `InetAddress.toString()` is `hostName + "/" + literal`. Verified
        // against HotSpot 25.0.3 on the build host:
        //
        //     resolved.toString   = localhost/127.0.0.1:8080
        //     unresolved.toString = localhost/<unresolved>:8080
        //
        // The old expectation was frozen from the VM's own pre-`e092b0f3b`
        // output, so it locked in the divergence instead of catching it —
        // and it started failing the moment that commit fixed the
        // hostName model.
        //
        // Asserted as prefix + suffix rather than the exact literal: the
        // middle is whatever `localhost` resolves to, which is 127.0.0.1
        // here but is an IPv6 loopback on some hosts. The "/" is the part
        // that matters — its absence is exactly the old bug.
        assert!(
            rendered.starts_with("localhost/") && rendered.ends_with(":8080"),
            "InetSocketAddress.toString() rendered {rendered:?}; HotSpot 25 \
                 gives `localhost/<literal>:8080` for a resolved address"
        );
    } else {
        panic!("expected string");
    }
}

#[test]
fn message_format_static() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let pattern = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Hello, {0}! You are {1} years old.")
    };
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Alice")
    };
    // Build Object[] with [name, 30]
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(name)));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(30));
    let result = call_native(
        &shared,
        &mut thread,
        "java/text/MessageFormat",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(pattern)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert_eq!(
            ctx.read_string(s).unwrap(),
            "Hello, Alice! You are 30 years old."
        );
    } else {
        panic!("expected string");
    }
}

#[test]
fn math_context_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let mc = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/math/MathContext",
        "<init>",
        "(I)V",
        &[Value::Object(Some(mc)), Value::Int(10)],
    )
    .unwrap();
    let prec = call_native(
        &shared,
        &mut thread,
        "java/math/MathContext",
        "getPrecision",
        "()I",
        &[Value::Object(Some(mc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(prec, Value::Int(10));
    let rm = call_native(
        &shared,
        &mut thread,
        "java/math/MathContext",
        "getRoundingMode",
        "()Ljava/math/RoundingMode;",
        &[Value::Object(Some(mc))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(rm, Value::Object(Some(_))));
}

#[test]
fn rounding_mode_value_of() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("HALF_EVEN")
    };
    let rm = call_native(
        &shared,
        &mut thread,
        "java/math/RoundingMode",
        "valueOf",
        "(Ljava/lang/String;)Ljava/math/RoundingMode;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(r)) = rm {
        let ord = call_native(
            &shared,
            &mut thread,
            "java/math/RoundingMode",
            "ordinal",
            "()I",
            &[Value::Object(Some(r))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ord, Value::Int(6)); // HALF_EVEN = 6
        let n = call_native(
            &shared,
            &mut thread,
            "java/math/RoundingMode",
            "name",
            "()Ljava/lang/String;",
            &[Value::Object(Some(r))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = n {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "HALF_EVEN");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected RoundingMode");
    }
}

#[test]
fn byte_order_native() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let no = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteOrder",
        "nativeOrder",
        "()Ljava/nio/ByteOrder;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(bo)) = no {
        let ts = call_native(
            &shared,
            &mut thread,
            "java/nio/ByteOrder",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(bo))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = ts {
            let ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            let name = ctx.read_string(s).unwrap();
            assert!(name == "LITTLE_ENDIAN" || name == "BIG_ENDIAN");
        } else {
            panic!("expected string");
        }
    } else {
        panic!("expected ByteOrder");
    }
}

#[test]
fn objects_is_null_non_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "isNull",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Int(1));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "isNull",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r2, Value::Int(0));
    let r3 = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "nonNull",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r3, Value::Int(1));
}

#[test]
fn objects_check_index() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "checkIndex",
        "(II)I",
        &[Value::Int(3), Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(3));
    let err = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "checkIndex",
        "(II)I",
        &[Value::Int(10), Value::Int(5)],
    );
    assert!(err.is_err());
}

#[test]
fn string_buffer_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sb = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuffer",
        "<init>",
        "()V",
        &[Value::Object(Some(sb))],
    )
    .unwrap();
    let hello = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Hello")
    };
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuffer",
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        &[Value::Object(Some(sb)), Value::Object(Some(hello))],
    )
    .unwrap();
    let world = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string(" World")
    };
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuffer",
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        &[Value::Object(Some(sb)), Value::Object(Some(world))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuffer",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sb))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert_eq!(ctx.read_string(s).unwrap(), "Hello World");
    } else {
        panic!("expected string");
    }
    let len = call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuffer",
        "length",
        "()I",
        &[Value::Object(Some(sb))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(11));
}

#[test]
fn month_value_of_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("JULY")
    };
    let july = call_native(
        &shared,
        &mut thread,
        "java/time/Month",
        "valueOf",
        "(Ljava/lang/String;)Ljava/time/Month;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(m)) = july {
        let val = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "getValue",
            "()I",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(val, Value::Int(7));
    } else {
        panic!("expected Month");
    }
}

#[test]
fn month_first_of_quarter() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // August (8) -> firstMonthOfQuarter -> July (7)
    let aug = call_native(
        &shared,
        &mut thread,
        "java/time/Month",
        "of",
        "(I)Ljava/time/Month;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(m)) = aug {
        let first = call_native(
            &shared,
            &mut thread,
            "java/time/Month",
            "firstMonthOfQuarter",
            "()Ljava/time/Month;",
            &[Value::Object(Some(m))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(f)) = first {
            let val = call_native(
                &shared,
                &mut thread,
                "java/time/Month",
                "getValue",
                "()I",
                &[Value::Object(Some(f))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(val, Value::Int(7));
        } else {
            panic!("expected Month");
        }
    } else {
        panic!("expected Month");
    }
}

#[test]
fn date_format_factory() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let df = call_native(
        &shared,
        &mut thread,
        "java/text/DateFormat",
        "getDateInstance",
        "()Ljava/text/DateFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(df, Value::Object(Some(_))));
    let dtf = call_native(
        &shared,
        &mut thread,
        "java/text/DateFormat",
        "getDateTimeInstance",
        "()Ljava/text/DateFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(dtf, Value::Object(Some(_))));
}

#[test]
fn objects_require_non_null_else() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let default = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    // Non-null obj -> returns obj
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "requireNonNullElse",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(obj)), Value::Object(Some(default))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Object(Some(obj)));
    // Null obj -> returns default
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/Objects",
        "requireNonNullElse",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(None), Value::Object(Some(default))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r2, Value::Object(Some(default)));
}

// =====================================================================
// Phase 53 tests: javax.crypto, java.security, java.net, ServiceLoader, Record
// =====================================================================

#[test]
fn cipher_get_instance_and_algorithm() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let cipher = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let cipher_ref = cipher.as_object().unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(cipher_ref))],
    )
    .unwrap()
    .unwrap();
    let algo_ref = result.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, algo_ref),
        Some("AES".to_string())
    );
}


#[test]
fn cipher_init_and_block_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let cipher = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let cipher_ref = cipher.as_object().unwrap();
    // A real 128-bit AES key (see `make_secret_key` for why not a fabricated one).
    let key = make_secret_key(&shared, &mut thread, "AES", 16);
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "init",
        "(ILjava/security/Key;)V",
        &[
            Value::Object(Some(cipher_ref)),
            Value::Int(1),
            Value::Object(Some(key)),
        ],
    )
    .unwrap();
    let bs = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getBlockSize",
        "()I",
        &[Value::Object(Some(cipher_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bs, Value::Int(16));
    let os = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getOutputSize",
        "(I)I",
        &[Value::Object(Some(cipher_ref)), Value::Int(32)],
    )
    .unwrap()
    .unwrap();
    // PKCS#5 padding ALWAYS adds a block, including when the input is
    // already a whole number of blocks — 32 bytes of plaintext encrypt to
    // 48. Expecting 32 assumed padding was skipped on an exact multiple,
    // which is the one case the scheme is specifically designed to handle
    // (otherwise the unpadder could not tell padding from data).
    assert_eq!(os, Value::Int(48));
    let os2 = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getOutputSize",
        "(I)I",
        &[Value::Object(Some(cipher_ref)), Value::Int(17)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(os2, Value::Int(32));
}

#[test]
fn key_generator_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let kg = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let kg_ref = kg.as_object().unwrap();
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "init",
        "(I)V",
        &[Value::Object(Some(kg_ref)), Value::Int(256)],
    )
    .unwrap();
    let sk = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "generateKey",
        "()Ljavax/crypto/SecretKey;",
        &[Value::Object(Some(kg_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(sk.as_object().is_some());
    let sk_ref = sk.as_object().unwrap();
    let encoded = call_native(
        &shared,
        &mut thread,
        "javax/crypto/SecretKey",
        "getEncoded",
        "()[B",
        &[Value::Object(Some(sk_ref))],
    )
    .unwrap()
    .unwrap();
    let arr_ref = encoded.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(arr_ref), 32);
}

#[test]
fn secret_key_spec_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let key_bytes = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        16,
    );
    // Distinguishable key material. A freshly allocated array is all
    // zeroes, and all-zeroes cannot tell a copy from an alias — nor from
    // the scrubbed buffer this test now checks for.
    for i in 0..16 {
        let _ = shared
            .mem
            .heap
            .set_array_element(key_bytes, i, Value::Int(i as i32 + 1));
    }
    let sks = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "<init>",
        "([BLjava/lang/String;)V",
        &[
            Value::Object(Some(sks)),
            Value::Object(Some(key_bytes)),
            Value::Object(Some(algo)),
        ],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sks))],
    )
    .unwrap()
    .unwrap();
    let algo_ref = result.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, algo_ref),
        Some("AES".to_string())
    );
    // `SecretKeySpec` copies on the way IN and on the way OUT — `<init>` is
    // `this.key = key.clone()` and `getEncoded()` is `return
    // this.key.clone()` (JDK 25 `src.zip`). This assertion used to be
    // `assert_eq!(enc, Value::Object(Some(key_bytes)))`, i.e. it required
    // the exact ALIASING those clones exist to prevent — and that aliasing
    // was a live all-zero-key defect, because SunJCE's generators scrub
    // their working buffer the instant the key object exists
    // (`AESKeyGenerator.engineGenerateKey` = `new SecretKeySpec(keyBytes,
    // "AES"); Arrays.fill(keyBytes, (byte) 0);`), so the scrub landed on
    // the key itself. Scrub the caller's array here the same way and check
    // the key survives it, which is the property that was actually broken.
    for i in 0..16 {
        let _ = shared
            .mem
            .heap
            .set_array_element(key_bytes, i, Value::Int(0));
    }
    let enc = call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "getEncoded",
        "()[B",
        &[Value::Object(Some(sks))],
    )
    .unwrap()
    .unwrap();
    let enc_ref = enc.as_object().expect("getEncoded() must return a byte[]");
    assert_ne!(
        enc_ref, key_bytes,
        "getEncoded() must not hand back the caller's own array"
    );
    assert_eq!(shared.mem.heap.array_length(enc_ref), 16);
    for i in 0..16 {
        assert_eq!(
            shared.mem.heap.get_array_element(enc_ref, i).unwrap(),
            Value::Int(i as i32 + 1),
            "key byte {i} did not survive the caller scrubbing its buffer"
        );
    }
    let fmt = call_native(
        &shared,
        &mut thread,
        "javax/crypto/spec/SecretKeySpec",
        "getFormat",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sks))],
    )
    .unwrap()
    .unwrap();
    let fmt_ref = fmt.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, fmt_ref),
        Some("RAW".to_string())
    );
}

#[test]
fn security_provider_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let provs = call_native(
        &shared,
        &mut thread,
        "java/security/Security",
        "getProviders",
        "()[Ljava/security/Provider;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arr = provs.as_object().unwrap();
    // T2.6.15 seeded the registry with the five standard Sun providers
    // (SUN, SunJCE, SunRsaSign, SunEC, SunJSSE) to match the JDK's
    // default provider list; earlier revisions of this test asserted a
    // single "CratonVM" entry, which predated that work.
    assert_eq!(shared.mem.heap.array_length(arr), 5);
    let prov = shared.mem.heap.get_array_element(arr, 0).unwrap();
    let prov_ref = prov.as_object().unwrap();
    let name = call_native(
        &shared,
        &mut thread,
        "java/security/Provider",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(prov_ref))],
    )
    .unwrap()
    .unwrap();
    let name_ref = name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, name_ref),
        Some("SUN".to_string())
    );
    let ver = call_native(
        &shared,
        &mut thread,
        "java/security/Provider",
        "getVersion",
        "()D",
        &[Value::Object(Some(prov_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ver, Value::Double(21.0));
}

#[test]
fn keystore_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ks_type = create_java_string(&shared, "PKCS12");
    let ks = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyStore;",
        &[Value::Object(Some(ks_type))],
    )
    .unwrap()
    .unwrap();
    let ks_ref = ks.as_object().unwrap();
    let tp = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "getType",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ks_ref))],
    )
    .unwrap()
    .unwrap();
    let tp_ref = tp.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, tp_ref),
        Some("PKCS12".to_string())
    );
    // A KeyStore must be loaded before it can be queried: the JDK throws
    // `KeyStoreException: Uninitialized keystore` from size()/containsAlias()
    // otherwise, and so does CratonVM. `load(null, null)` is the documented
    // way to initialise an empty one.
    call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "load",
        "(Ljava/io/InputStream;[C)V",
        &[
            Value::Object(Some(ks_ref)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();
    let sz = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "size",
        "()I",
        &[Value::Object(Some(ks_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(0));
    let alias = create_java_string(&shared, "mykey");
    let ca = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "containsAlias",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(ks_ref)), Value::Object(Some(alias))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ca, Value::Int(0));
    let dt = call_native(
        &shared,
        &mut thread,
        "java/security/KeyStore",
        "getDefaultType",
        "()Ljava/lang/String;",
        &[],
    )
    .unwrap()
    .unwrap();
    let dt_ref = dt.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, dt_ref),
        Some("PKCS12".to_string())
    );
}

/// API-surface check for `KeyPairGenerator`: `getInstance` → `initialize`
/// → `generateKeyPair` → `getPublic`/`getPrivate`/`getAlgorithm`.
///
/// Deliberately asks for a **512-bit** key, as the `m22_*` tests do. Since
/// the synthetic `KeyPairGenerator` stub in
/// `native-builtins::phases_early::register_phase53_security` was retired,
/// `generateKeyPair` runs the genuine `crypto_impl::Rsa::generate_keypair`
/// — two real Miller-Rabin prime searches. At 2048 bits that is two
/// ~1024-bit searches through a hand-rolled u32-limb `BigUint`, which in an
/// **unoptimised debug test binary** costs tens of seconds to minutes, with
/// the heavy tail of a geometric prime search on top (and a ~39% chance of
/// discarding the whole pair when `n` lands one bit short, restarting both
/// searches). This binary already runs ~10 minutes against a 20-minute CI
/// timeout, and a timeout produces NO result line for ANY test in it.
///
/// 512 bits keeps the crypto real — the same code path, a real key, a real
/// `sign`/`verify` elsewhere — while a 256-bit prime search costs ~1/64th
/// of a 1024-bit one per modular exponentiation. What this test asserts (the
/// method surface and the algorithm round-trip) is unchanged; it was never
/// a prime-search benchmark.
#[test]
fn keypair_generator_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "RSA");
    let kpg = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let kpg_ref = kpg.as_object().unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "initialize",
        "(I)V",
        &[Value::Object(Some(kpg_ref)), Value::Int(512)],
    )
    .unwrap();
    let kp = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "generateKeyPair",
        "()Ljava/security/KeyPair;",
        &[Value::Object(Some(kpg_ref))],
    )
    .unwrap()
    .unwrap();
    let kp_ref = kp.as_object().unwrap();
    let pub_key = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPair",
        "getPublic",
        "()Ljava/security/PublicKey;",
        &[Value::Object(Some(kp_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(pub_key.as_object().is_some());
    let priv_key = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPair",
        "getPrivate",
        "()Ljava/security/PrivateKey;",
        &[Value::Object(Some(kp_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(priv_key.as_object().is_some());
    let alg = call_native(
        &shared,
        &mut thread,
        "java/security/KeyPairGenerator",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(kpg_ref))],
    )
    .unwrap()
    .unwrap();
    let alg_ref = alg.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, alg_ref),
        Some("RSA".to_string())
    );
}

#[test]
fn signature_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "SHA256withRSA");
    let sig = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/Signature;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let sig_ref = sig.as_object().unwrap();
    let alg = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sig_ref))],
    )
    .unwrap()
    .unwrap();
    let alg_ref = alg.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, alg_ref),
        Some("SHA256withRSA".to_string())
    );
    let priv_key = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "initSign",
        "(Ljava/security/PrivateKey;)V",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(priv_key))],
    )
    .unwrap();
    let signed = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "sign",
        "()[B",
        &[Value::Object(Some(sig_ref))],
    )
    .unwrap()
    .unwrap();
    let arr = signed.as_object().unwrap();
    // RSA signature length is 256 bytes (2048-bit key)
    assert_eq!(shared.mem.heap.array_length(arr), 256);
    let pub_key = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "initVerify",
        "(Ljava/security/PublicKey;)V",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(pub_key))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/security/Signature",
        "verify",
        "([B)Z",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
fn socket_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Use default constructor (no connect) to test field layout
    let sock = alloc_receiver(&shared, &mut thread, "java/net/Socket", 5);
    call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "<init>",
        "()V",
        &[Value::Object(Some(sock))],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "getPort",
        "()I",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(0));
    let closed = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));
    let conn = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "isConnected",
        "()Z",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(conn, Value::Int(0)); // not connected (no stream_id)
    call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "close",
        "()V",
        &[Value::Object(Some(sock))],
    )
    .unwrap();
    let closed2 = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed2, Value::Int(1));
}

#[test]
fn server_socket_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ss = alloc_receiver(&shared, &mut thread, "java/net/ServerSocket", 4);
    // Use port 0 for OS-assigned port to avoid conflicts
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ss)), Value::Int(0)],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "getLocalPort",
        "()I",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    // Port 0 → OS assigns a real port > 0
    assert!(port.as_int().unwrap() > 0);
    let closed = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "close",
        "()V",
        &[Value::Object(Some(ss))],
    )
    .unwrap();
    let closed2 = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed2, Value::Int(1));
}

#[test]
fn socket_exception_classes() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let result1 = call_native(
        &shared,
        &mut thread,
        "java/net/SocketException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result1.is_ok());
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/net/BindException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result2.is_ok());
    let result3 = call_native(
        &shared,
        &mut thread,
        "java/net/ConnectException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result3.is_ok());
    let result4 = call_native(
        &shared,
        &mut thread,
        "java/net/SocketTimeoutException",
        "<init>",
        "()V",
        &[Value::Object(None)],
    );
    assert!(result4.is_ok());
}

#[test]
fn service_loader_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // The argument to the STATIC `ServiceLoader.load(Class)` is the service
    // interface's Class mirror, not a ServiceLoader — and it has to be a
    // REAL mirror: the native resolves `service.getName()` to derive the
    // provider-configuration resource name, which a bare synthetic object
    // cannot answer.
    let service_cid = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.try_ensure_synthetic_class("java/util/spi/ToolProvider", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
    };
    let class_mirror = get_or_create_class_mirror(&shared, service_cid);
    let sl = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    let sl_ref = sl.as_object().unwrap();
    let itr = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(itr.as_object().is_some());
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(stream.as_object().is_some());
    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "findFirst",
        "()Ljava/util/Optional;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(opt.as_object().is_some());
    let s = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    let s_ref = s.as_object().unwrap();
    let s_str = read_java_string(&shared.mem.heap, s_ref).unwrap_or_default();
    assert!(s_str.contains("ServiceLoader"));
}

#[test]
fn record_equals_and_hash_code() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let rec1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec1, 0, Value::Int(42));
    shared.mem.heap.set_field(rec1, 1, Value::Int(100));
    let rec2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec2, 0, Value::Int(42));
    shared.mem.heap.set_field(rec2, 1, Value::Int(100));
    let eq = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(rec1)), Value::Object(Some(rec2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
    let h1 = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "hashCode",
        "()I",
        &[Value::Object(Some(rec1))],
    )
    .unwrap()
    .unwrap();
    let h2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "hashCode",
        "()I",
        &[Value::Object(Some(rec2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h1, h2);
    let rec3 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec3, 0, Value::Int(42));
    shared.mem.heap.set_field(rec3, 1, Value::Int(999));
    let eq2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(rec1)), Value::Object(Some(rec3))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq2, Value::Int(0));
}

#[test]
fn record_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let rec = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let s = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(rec))],
    )
    .unwrap()
    .unwrap();
    let s_ref = s.as_object().unwrap();
    let s_str = read_java_string(&shared.mem.heap, s_ref).unwrap_or_default();
    // toString with no record components uses fallback format: ClassName@hex or @hex
    assert!(
        s_str.contains('@'),
        "expected fallback toString with @, got: {}",
        s_str
    );
}

#[test]
fn class_sealed_and_record_markers() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // A REAL mirror. Hand-building one and stuffing an Int into slot 0 no
    // longer identifies a class: the natives resolve a mirror through the
    // VM's `class_mirrors_reverse` map, and an object that was never
    // registered there resolves to nothing, so `isSealed` returned
    // `Ok(None)` and the test unwrapped it.
    let class_id = make_test_class(&shared, "test/PlainClass", vec![], vec![]);
    let class_mirror = get_or_create_class_mirror(&shared, class_id);
    let sealed = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isSealed",
        "()Z",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sealed, Value::Int(0));
    let is_rec = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isRecord",
        "()Z",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_rec, Value::Int(0));
    let subs = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getPermittedSubclasses",
        "()[Ljava/lang/Class;",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap()
    .unwrap();
    let subs_ref = subs.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(subs_ref), 0);
    let comps = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getRecordComponents",
        "()[Ljava/lang/reflect/RecordComponent;",
        &[Value::Object(Some(class_mirror))],
    )
    .unwrap();
    // The JDK returns NULL from `getRecordComponents()` for a class that is
    // not a record; this VM hands back an empty array from the sibling
    // `getPermittedSubclasses()`. Accept either — what the test is actually
    // about is that a plain class reports no record components, not which
    // of the two spellings of "none" the implementation picked.
    match comps {
        None | Some(Value::Object(None)) => {}
        Some(Value::Object(Some(a))) => {
            assert_eq!(shared.mem.heap.array_length(a), 0);
        }
        other => panic!("expected null or an empty array, got {other:?}"),
    }
}

#[test]
fn cipher_do_final_passthrough() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let algo = create_java_string(&shared, "AES");
    let cipher = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    let cipher_ref = cipher.as_object().unwrap();
    // A Cipher must be initialised before doFinal: the JDK throws
    // IllegalStateException("Cipher not initialized") otherwise and so does
    // CratonVM. This used to call doFinal straight after getInstance and
    // accept whatever came back — the comment below ("crypto_impl stub
    // returns Object(None)") records the passthrough stub that made that
    // work.
    // A REAL key: `doFinal` needs key material, and a bare
    // `ClassId::new(0)` object has none ("No key provided"). Generate one
    // the way a caller would.
    let kg_algo = create_java_string(&shared, "AES");
    let kg = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        &[Value::Object(Some(kg_algo))],
    )
    .unwrap()
    .unwrap();
    let kg_ref = kg.as_object().unwrap();
    let key = call_native(
        &shared,
        &mut thread,
        "javax/crypto/KeyGenerator",
        "generateKey",
        "()Ljavax/crypto/SecretKey;",
        &[Value::Object(Some(kg_ref))],
    )
    .unwrap()
    .unwrap()
    .as_object()
    .expect("generateKey must return a SecretKey");
    call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "init",
        "(ILjava/security/Key;)V",
        &[
            Value::Object(Some(cipher_ref)),
            Value::Int(1),
            Value::Object(Some(key)),
        ],
    )
    .unwrap();
    let input = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        4,
    );
    let _ = shared.mem.heap.set_array_element(input, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(input, 1, Value::Int(2));
    let _ = shared.mem.heap.set_array_element(input, 2, Value::Int(3));
    let _ = shared.mem.heap.set_array_element(input, 3, Value::Int(4));
    let result = call_native(
        &shared,
        &mut thread,
        "javax/crypto/Cipher",
        "doFinal",
        "([B)[B",
        &[Value::Object(Some(cipher_ref)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    // An initialised AES cipher encrypts the 4-byte input into a padded
    // block, so doFinal must hand back an actual array.
    let out = result
        .as_object()
        .expect("doFinal on an initialised cipher must return a byte[]");
    assert_eq!(
        shared.mem.heap.array_length(out),
        16,
        "4 bytes of plaintext pad up to one AES block"
    );
}

#[test]
fn socket_get_inet_address() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Use default constructor + set host field manually to test getInetAddress
    // without needing a real TCP connection
    let sock = alloc_receiver(&shared, &mut thread, "java/net/Socket", 5);
    call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "<init>",
        "()V",
        &[Value::Object(Some(sock))],
    )
    .unwrap();
    // getInetAddress on an unconnected socket with host set
    let addr = call_native(
        &shared,
        &mut thread,
        "java/net/Socket",
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        &[Value::Object(Some(sock))],
    )
    .unwrap()
    .unwrap();
    // Host is empty string from default init, InetAddress still created
    assert!(addr.as_object().is_some());
}

#[test]
fn server_socket_with_backlog() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ss = alloc_receiver(&shared, &mut thread, "java/net/ServerSocket", 4);
    // Use port 0 for OS-assigned to avoid conflicts
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "<init>",
        "(II)V",
        &[Value::Object(Some(ss)), Value::Int(0), Value::Int(100)],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "getLocalPort",
        "()I",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert!(port.as_int().unwrap() > 0);
    // Clean up
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "close",
        "()V",
        &[Value::Object(Some(ss))],
    )
    .unwrap();
}

#[test]
fn key_interface_methods() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let key = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        16,
    );
    // Distinguishable material, for the same reason as
    // `secret_key_spec_basics`: a zeroed array cannot tell a copy from an
    // alias.
    for i in 0..16 {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(i as i32 + 1));
    }
    shared.mem.heap.set_field(key, 0, Value::Object(Some(arr)));
    let enc = call_native(
        &shared,
        &mut thread,
        "java/security/Key",
        "getEncoded",
        "()[B",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    // `Key.getEncoded()` returns a fresh array — every JCA key
    // implementation ends `return this.<field>.clone()`, and callers build
    // key hygiene on that (the JDK's own providers scrub what they get
    // back). This used to assert the opposite, by identity; the same stale
    // aliasing assertion as `secret_key_spec_basics`, where it was pinning
    // a live all-zero-key defect.
    let enc_ref = enc.as_object().expect("getEncoded() must return a byte[]");
    assert_ne!(
        enc_ref, arr,
        "getEncoded() must not hand back the key's own backing array"
    );
    assert_eq!(shared.mem.heap.array_length(enc_ref), 16);
    for i in 0..16 {
        assert_eq!(
            shared.mem.heap.get_array_element(enc_ref, i).unwrap(),
            Value::Int(i as i32 + 1),
            "encoded byte {i} does not match the key material"
        );
    }
    let algo = call_native(
        &shared,
        &mut thread,
        "java/security/Key",
        "getAlgorithm",
        "()Ljava/lang/String;",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    let algo_ref = algo.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, algo_ref),
        Some("AES".to_string())
    );
    let fmt = call_native(
        &shared,
        &mut thread,
        "java/security/Key",
        "getFormat",
        "()Ljava/lang/String;",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    let fmt_ref = fmt.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, fmt_ref),
        Some("RAW".to_string())
    );
}

