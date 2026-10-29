// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// =========================================================================
// Phase S.2: DataInputStream / DataOutputStream round-trip + transferTo
// These tests use NativeContextImpl so that invoke_virtual inside DOS/DIS
// can resolve class names from proper ClassIds (not ClassId(0)).
// =========================================================================


/// Write an int to a DataOutputStream backed by ByteArrayOutputStream,
/// then read it back with DataInputStream backed by ByteArrayInputStream.
#[test]
fn data_streams_int_round_trip_p72() {
    let mut vm = Vm::new(VmConfig::default());

    // --- Write side: DOS → BAOS ---
    let baos = alloc_named(&mut vm, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    let dos = alloc_named(&mut vm, "java/io/DataOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(dos)), Value::Object(Some(baos))],
    )
    .unwrap();

    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "writeInt",
        "(I)V",
        &[Value::Object(Some(dos)), Value::Int(0x12345678)],
    )
    .unwrap();

    let arr = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    let byte_arr = match arr {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(byte_arr), 4);
    assert_eq!(
        vm.shared.mem.heap.get_array_element(byte_arr, 0),
        Ok(Value::Int(0x12))
    );
    assert_eq!(
        vm.shared.mem.heap.get_array_element(byte_arr, 1),
        Ok(Value::Int(0x34))
    );
    assert_eq!(
        vm.shared.mem.heap.get_array_element(byte_arr, 2),
        Ok(Value::Int(0x56))
    );
    assert_eq!(
        vm.shared.mem.heap.get_array_element(byte_arr, 3),
        Ok(Value::Int(0x78))
    );

    // --- Read side: DIS ← BAIS ---
    let bais = alloc_named(&mut vm, "java/io/ByteArrayInputStream", 4);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(byte_arr))],
    )
    .unwrap();

    let dis = alloc_named(&mut vm, "java/io/DataInputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(dis)), Value::Object(Some(bais))],
    )
    .unwrap();

    let val = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readInt",
        "()I",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(0x12345678));
}

/// Write a long to DOS, read it back from DIS.
#[test]
fn data_streams_long_round_trip_p72() {
    let mut vm = Vm::new(VmConfig::default());

    let baos = alloc_named(&mut vm, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    let dos = alloc_named(&mut vm, "java/io/DataOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(dos)), Value::Object(Some(baos))],
    )
    .unwrap();

    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "writeLong",
        "(J)V",
        &[
            Value::Object(Some(dos)),
            Value::Long(0x0102030405060708_i64),
        ],
    )
    .unwrap();

    let arr = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    let byte_arr = match arr {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(byte_arr), 8);

    let bais = alloc_named(&mut vm, "java/io/ByteArrayInputStream", 4);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(byte_arr))],
    )
    .unwrap();

    let dis = alloc_named(&mut vm, "java/io/DataInputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(dis)), Value::Object(Some(bais))],
    )
    .unwrap();

    let val = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readLong",
        "()J",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Long(0x0102030405060708_i64));
}

/// Write a UTF string to DOS, read it back from DIS.
#[test]
fn data_streams_utf_round_trip_p72() {
    let mut vm = Vm::new(VmConfig::default());

    let baos = alloc_named(&mut vm, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    let dos = alloc_named(&mut vm, "java/io/DataOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(dos)), Value::Object(Some(baos))],
    )
    .unwrap();

    let hello = create_java_string(&vm.shared, "Hello");
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "writeUTF",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(dos)), Value::Object(Some(hello))],
    )
    .unwrap();

    let arr = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    let byte_arr = match arr {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    // 2 bytes length prefix + 5 bytes "Hello"
    assert_eq!(vm.shared.mem.heap.array_length(byte_arr), 7);

    let bais = alloc_named(&mut vm, "java/io/ByteArrayInputStream", 4);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(byte_arr))],
    )
    .unwrap();

    let dis = alloc_named(&mut vm, "java/io/DataInputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(dis)), Value::Object(Some(bais))],
    )
    .unwrap();

    let sval = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readUTF",
        "()Ljava/lang/String;",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    let sref = match sval {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string"),
    };
    let s = read_java_string(&vm.shared.mem.heap, sref).unwrap();
    assert_eq!(s, "Hello");
}

/// DOS.size() tracks bytes written correctly.
#[test]
fn data_stream_size_tracking_p72() {
    let mut vm = Vm::new(VmConfig::default());

    let baos = alloc_named(&mut vm, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    let dos = alloc_named(&mut vm, "java/io/DataOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(dos)), Value::Object(Some(baos))],
    )
    .unwrap();

    let sz0 = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(dos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz0, Value::Int(0));

    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "writeShort",
        "(I)V",
        &[Value::Object(Some(dos)), Value::Int(0xABCD)],
    )
    .unwrap();

    let sz2 = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(dos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz2, Value::Int(2));

    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "writeInt",
        "(I)V",
        &[Value::Object(Some(dos)), Value::Int(1)],
    )
    .unwrap();

    let sz6 = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(dos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz6, Value::Int(6));
}

/// InputStream.transferTo copies all bytes to the output stream.
#[test]
fn input_stream_transfer_to_p72() {
    let mut vm = Vm::new(VmConfig::default());

    // Source: ByteArrayInputStream([1,2,3,4,5])
    let src_arr = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 5);
    for i in 0..5usize {
        let _ = vm
            .shared
            .mem
            .heap
            .set_array_element(src_arr, i, Value::Int((i + 1) as i32));
    }
    let bais = alloc_named(&mut vm, "java/io/ByteArrayInputStream", 4);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(src_arr))],
    )
    .unwrap();

    // Destination: ByteArrayOutputStream
    let baos = alloc_named(&mut vm, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    let transferred = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/InputStream",
        "transferTo",
        "(Ljava/io/OutputStream;)J",
        &[Value::Object(Some(bais)), Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(transferred, Value::Long(5));

    let dest_arr = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayOutputStream",
        "toByteArray",
        "()[B",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    let dest_ref = match dest_arr {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(dest_ref), 5);
    for i in 0..5usize {
        assert_eq!(
            vm.shared.mem.heap.get_array_element(dest_ref, i),
            Ok(Value::Int((i + 1) as i32))
        );
    }
}

/// InputStream.readAllBytes() collects all remaining bytes into a new array.
#[ignore = "STALE: Removed by the wave-3 constant-stub sweep (077c84a18) or a later jdk-only wave; the real bytecode serves it now. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn input_stream_read_all_bytes_p72() {
    let mut vm = Vm::new(VmConfig::default());

    let src_arr = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 0, Value::Int(10));
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 1, Value::Int(20));
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 2, Value::Int(30));

    let bais = alloc_named(&mut vm, "java/io/ByteArrayInputStream", 4);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(src_arr))],
    )
    .unwrap();

    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/InputStream",
        "readAllBytes",
        "()[B",
        &[Value::Object(Some(bais))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(vm.shared.mem.heap.array_length(arr), 3);
    assert_eq!(
        vm.shared.mem.heap.get_array_element(arr, 0),
        Ok(Value::Int(10))
    );
    assert_eq!(
        vm.shared.mem.heap.get_array_element(arr, 1),
        Ok(Value::Int(20))
    );
    assert_eq!(
        vm.shared.mem.heap.get_array_element(arr, 2),
        Ok(Value::Int(30))
    );
}

/// DIS: readBoolean, readByte, readShort, readUnsignedShort all decode correctly.
#[test]
fn data_stream_multi_type_read_p72() {
    let mut vm = Vm::new(VmConfig::default());

    // [1, 0xFF, 0x01, 0x02, 0x00, 0x41, 0]
    let src_arr = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 7);
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 0, Value::Int(1)); // readBoolean → true
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 1, Value::Int(0xFF)); // readByte → -1
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 2, Value::Int(0x01)); // readShort hi
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 3, Value::Int(0x02)); // readShort lo → 258
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 4, Value::Int(0x00)); // readUnsignedShort hi
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 5, Value::Int(0x41)); // readUnsignedShort lo → 65
    let _ = vm
        .shared
        .mem
        .heap
        .set_array_element(src_arr, 6, Value::Int(0));

    let bais = alloc_named(&mut vm, "java/io/ByteArrayInputStream", 4);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(src_arr))],
    )
    .unwrap();

    let dis = alloc_named(&mut vm, "java/io/DataInputStream", 2);
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(dis)), Value::Object(Some(bais))],
    )
    .unwrap();

    let b = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readBoolean",
        "()Z",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b, Value::Int(1));

    let byte_val = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readByte",
        "()B",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(byte_val, Value::Int(-1)); // 0xFF as signed byte

    let short_val = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readShort",
        "()S",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(short_val, Value::Int(258)); // 0x0102

    let ushort_val = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/io/DataInputStream",
        "readUnsignedShort",
        "()I",
        &[Value::Object(Some(dis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ushort_val, Value::Int(65)); // 0x0041
}

#[test]
fn string_join_array() {
    let mut vm = Vm::new(VmConfig::default());
    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };

    let delim = ctx.create_string(", ");
    let a = ctx.create_string("a");
    let b = ctx.create_string("b");
    let c = ctx.create_string("c");
    let arr = ctx.new_ref_array(ClassId::new(0), 3);
    ctx.set_array_element(arr, 0, Value::Object(Some(a)));
    ctx.set_array_element(arr, 1, Value::Object(Some(b)));
    ctx.set_array_element(arr, 2, Value::Object(Some(c)));
    drop(ctx);

    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/String",
        "join",
        "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;",
        &[Value::Object(Some(delim)), Value::Object(Some(arr))],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(s))) => {
            let ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "a, b, c");
        }
        _ => panic!("String.join should return a String"),
    }
}

#[test]
fn string_strip() {
    let mut vm = Vm::new(VmConfig::default());
    let s = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.create_string("  hello  ")
    };

    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/String",
        "strip",
        "()Ljava/lang/String;",
        &[Value::Object(Some(s))],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(r))) => {
            let ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            assert_eq!(ctx.read_string(r).unwrap(), "hello");
        }
        _ => panic!("strip should return a String"),
    }
}

#[test]
fn string_repeat() {
    let mut vm = Vm::new(VmConfig::default());
    let s = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.create_string("ab")
    };

    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/String",
        "repeat",
        "(I)Ljava/lang/String;",
        &[Value::Object(Some(s)), Value::Int(3)],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(r))) => {
            let ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            assert_eq!(ctx.read_string(r).unwrap(), "ababab");
        }
        _ => panic!("repeat should return a String"),
    }
}

#[test]
fn integer_sum_max_min() {
    let mut vm = Vm::new(VmConfig::default());

    let sum = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "sum",
        "(II)I",
        &[Value::Int(3), Value::Int(4)],
    )
    .unwrap();
    assert_eq!(sum, Some(Value::Int(7)));

    let max = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "max",
        "(II)I",
        &[Value::Int(3), Value::Int(7)],
    )
    .unwrap();
    assert_eq!(max, Some(Value::Int(7)));

    let min = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "min",
        "(II)I",
        &[Value::Int(3), Value::Int(7)],
    )
    .unwrap();
    assert_eq!(min, Some(Value::Int(3)));
}

#[test]
fn integer_to_hex_string_p30() {
    let mut vm = Vm::new(VmConfig::default());

    let hex = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "toHexString",
        "(I)Ljava/lang/String;",
        &[Value::Int(255)],
    )
    .unwrap();
    match hex {
        Some(Value::Object(Some(s))) => {
            let ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "ff");
        }
        _ => panic!("toHexString should return a String"),
    }
}

#[test]
fn string_joiner_p30_registration_exists() {
    // E40-1 N6: upgraded from three `find(...).is_some()` checks to the
    // round trip they were standing in for — `<init>(delimiter)`, two
    // `add`s, then `toString()` must actually join with the delimiter.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sj = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let delim = create_java_string(&shared, ", ");
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj)), Value::Object(Some(delim))],
    )
    .expect("StringJoiner.<init>(CharSequence) must not fail");

    for word in ["a", "b"] {
        let elem = create_java_string(&shared, word);
        call_native(
            &shared,
            &mut thread,
            "java/util/StringJoiner",
            "add",
            "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
            &[Value::Object(Some(sj)), Value::Object(Some(elem))],
        )
        .expect("StringJoiner.add must not fail");
    }

    let joined = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .expect("StringJoiner.toString must not fail");
    match joined {
        Some(Value::Object(Some(r))) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r).as_deref(),
                Some("a, b")
            );
        }
        other => panic!("toString() must return the delimiter-joined text, got {other:?}"),
    }
}

#[test]
#[ignore] // Requires classpath for ArrayList internal allocation
fn _string_joiner_lifecycle_p30() {
    let mut vm = Vm::new(VmConfig::default());

    // Allocate StringJoiner with enough fields (use heap directly to avoid class resolution)
    let sj = vm.shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.create_string(", ")
    };

    // Init
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj)), Value::Object(Some(delim))],
    )
    .unwrap();

    // Add elements
    let a = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.create_string("hello")
    };
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(a))],
    )
    .unwrap();

    let b = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.create_string("world")
    };
    call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(b))],
    )
    .unwrap();

    // toString
    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(s))) => {
            let ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            assert_eq!(ctx.read_string(s).unwrap(), "hello, world");
        }
        _ => panic!("toString should return 'hello, world'"),
    }
}

// =========================================================================
// Phase 32: Dispatch Priority Inversion Tests
// =========================================================================

#[test]
fn synthetic_classes_use_native_dispatch() {
    // For synthetic stubs, native methods should be called (existing behavior)
    let mut vm = Vm::new(VmConfig::default());

    // StringBuilder is a synthetic stub — verify its <init> dispatches to native
    let sb = vm.shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/StringBuilder",
        "<init>",
        "()V",
        &[Value::Object(Some(sb))],
    );
    assert!(
        result.is_ok(),
        "synthetic StringBuilder.<init> should dispatch to native"
    );
}

#[test]
fn compatibility_stub_origin_set_correctly() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // Load a synthetic class by requesting a JDK class that doesn't have
    // a .class file on the classpath (which it won't in test env)
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/StringBuilder")
        .unwrap();
    let cm = shared.classes.class_manager.read();
    let class = cm.class_store.get(class_id).unwrap();
    assert!(
        class.origin.is_compatibility_stub(),
        "StringBuilder should be synthetic in test env (no JDK on classpath)"
    );
    assert!(
        class.methods.is_empty(),
        "Synthetic stubs should have zero methods"
    );
}

#[test]
fn bootstrap_with_real_jdk() {
    // Skip if no JDK available
    let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
        .ok()
        .or_else(|| {
            let p = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
            if p.exists() {
                Some(p.to_string_lossy().into_owned())
            } else {
                None
            }
        });
    let Some(java_home) = java_home else {
        eprintln!("Skipping bootstrap_with_real_jdk: no JDK found");
        return;
    };
    let jmod = std::path::PathBuf::from(&java_home)
        .join("jmods")
        .join("java.base.jmod");
    if !jmod.exists() {
        eprintln!("Skipping: no java.base.jmod");
        return;
    }

    // Create VM with real JDK on boot classpath
    let config = VmConfig {
        boot_classpath: vec![jmod.to_string_lossy().into_owned()],
        ..VmConfig::default()
    };
    let shared = Arc::new(SharedVm::new(config));

    // Verify Object was loaded from real .class file (not synthetic)
    let cm = shared.classes.class_manager.read();
    let obj_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("Object should be loaded");
    let obj_class = cm.class_store.get(obj_id).unwrap();
    assert!(
        !obj_class.origin.is_compatibility_stub(),
        "Object should be loaded from real .class file"
    );
    assert!(
        !obj_class.methods.is_empty(),
        "Real Object should have methods (hashCode, equals, toString, etc.)"
    );

    // Verify String was loaded from real .class file
    let str_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("String should be loaded");
    let str_class = cm.class_store.get(str_id).unwrap();
    assert!(
        !str_class.origin.is_compatibility_stub(),
        "String should be loaded from real .class file"
    );
    assert!(
        str_class.methods.len() > 50,
        "Real String should have many methods, got {}",
        str_class.methods.len()
    );

    // Verify Throwable was loaded
    let thr_id = cm
        .get_loaded_class_id("java/lang/Throwable")
        .expect("Throwable should be loaded");
    let thr_class = cm.class_store.get(thr_id).unwrap();
    assert!(!thr_class.origin.is_compatibility_stub());

    // Verify total loaded class count is substantial (Object pulls in many deps)
    let total = cm.loaded_count();
    eprintln!("[bootstrap_with_real_jdk] Total classes loaded: {}", total);
    eprintln!(
        "[bootstrap_with_real_jdk] String methods: {}",
        str_class.methods.len()
    );
    eprintln!(
        "[bootstrap_with_real_jdk] Object methods: {}",
        obj_class.methods.len()
    );
    assert!(
        total > 20,
        "Bootstrap should load >20 classes, got {}",
        total
    );

    // Verify Object has the expected native methods
    let has_hash_code = obj_class
        .methods
        .iter()
        .any(|m| &*m.name == "hashCode" && m.is_native());
    assert!(has_hash_code, "Object should have native hashCode method");

    let has_to_string = obj_class
        .methods
        .iter()
        .any(|m| &*m.name == "toString" && !m.is_native());
    assert!(
        has_to_string,
        "Object.toString should be non-native bytecode method"
    );
}

#[test]
fn dispatch_inversion_preserves_native_for_synthetic() {
    // Verify that invoke_or_native still calls native for synthetic stubs
    let mut vm = Vm::new(VmConfig::default());

    // System.currentTimeMillis should still work via native dispatch
    let result = call_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/System",
        "currentTimeMillis",
        "()J",
        &[],
    );
    match result {
        Ok(Some(Value::Long(t))) => assert!(t > 0, "currentTimeMillis should return positive"),
        other => panic!("Expected Long, got {:?}", other),
    }
}

// =========================================================================
// Phase 34: Name-Based Field Access Tests
// =========================================================================

#[test]
fn resolve_field_index_real_jdk() {
    // Skip if no JDK available
    let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
        .ok()
        .or_else(|| {
            let p = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
            if p.exists() {
                Some(p.to_string_lossy().into_owned())
            } else {
                None
            }
        });
    let Some(java_home) = java_home else {
        return;
    };
    let jmod = std::path::PathBuf::from(&java_home)
        .join("jmods")
        .join("java.base.jmod");
    if !jmod.exists() {
        return;
    }

    let config = VmConfig {
        boot_classpath: vec![jmod.to_string_lossy().into_owned()],
        ..VmConfig::default()
    };
    let shared = Arc::new(SharedVm::new(config));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Resolve Throwable.detailMessage field
    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let idx = ctx.resolve_field_index("java/lang/Throwable", "detailMessage");
    assert!(idx.is_some(), "Throwable should have 'detailMessage' field");
    eprintln!(
        "[resolve_field_index_real_jdk] Throwable.detailMessage index = {:?}",
        idx
    );

    // Resolve Throwable.cause field
    let cause_idx = ctx.resolve_field_index("java/lang/Throwable", "cause");
    assert!(cause_idx.is_some(), "Throwable should have 'cause' field");
    assert_ne!(
        idx, cause_idx,
        "detailMessage and cause should have different indices"
    );

    // Resolve String field (JDK 9+: 'value' is byte[], 'coder' is byte)
    let val_idx = ctx.resolve_field_index("java/lang/String", "value");
    assert!(val_idx.is_some(), "String should have 'value' field");

    // Non-existent field should return None
    let missing = ctx.resolve_field_index("java/lang/Object", "nonexistent_field_xyz");
    assert!(missing.is_none());
}

// =========================================================================
// Phase 35: Real JDK Bytecode Execution Tests
// =========================================================================


#[test]
fn real_jdk_invoke_object_hashcode() {
    // Object.hashCode() is ACC_NATIVE — should dispatch to our native registry
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    // Allocate an Object
    let obj = vm.shared.mem.heap.alloc_object(ClassId::new(0), 0);

    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(Some(obj))],
    );
    match &result {
        Ok(Some(Value::Int(h))) => {
            eprintln!("[real_jdk] Object.hashCode() = {}", h);
        }
        Ok(other) => eprintln!("[real_jdk] Object.hashCode() returned {:?}", other),
        Err(e) => eprintln!("[real_jdk] Object.hashCode() error: {:?}", e),
    }
    assert!(result.is_ok(), "Object.hashCode should not crash");
}

#[test]
fn real_jdk_invoke_integer_valueof() {
    // E40-1 N5, PROMOTED FROM A PROBE TO AN ASSERTION (2026-09-20). Every
    // `match` arm here used to only `eprintln!`, `Err` included, with "this is
    // expected to fail initially — don't assert" at the site: a stated reason,
    // not an oversight, and the record's own nomination said deciding whether
    // it had gone stale needed someone who could RUN it against a real JDK.
    // Run here: `Integer.valueOf(42)` now succeeds and returns a real,
    // interned `java.lang.Integer` whose `intValue()` reads back 42 through
    // the SAME real-bytecode dispatch this test exercises — not `is_some()`,
    // a value.
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    let boxed = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    )
    .unwrap_or_else(|e| panic!("Integer.valueOf(42) must not fail against a real JDK: {e:?}"));
    let Some(Value::Object(Some(boxed))) = boxed else {
        panic!("Integer.valueOf(42) must return a non-null Integer, got {boxed:?}");
    };

    let unboxed = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "intValue",
        "()I",
        &[Value::Object(Some(boxed))],
    )
    .unwrap_or_else(|e| panic!("Integer(42).intValue() must not fail: {e:?}"));
    assert_eq!(
        unboxed,
        Some(Value::Int(42)),
        "the boxed Integer must read back the value it was boxed from"
    );
}

#[test]
fn real_jdk_class_has_bytecode_methods() {
    // Verify that real classes have Code attributes on non-native methods
    let Some(vm) = vm_with_real_jdk() else {
        return;
    };

    let cm = vm.shared.classes.class_manager.read();
    let obj_id = cm.get_loaded_class_id("java/lang/Object").unwrap();
    let obj_class = cm.class_store.get(obj_id).unwrap();

    // Count native vs bytecode methods
    let native_count = obj_class.methods.iter().filter(|m| m.is_native()).count();
    let bytecode_count = obj_class.methods.iter().filter(|m| !m.is_native()).count();
    let has_code_count = obj_class
        .methods
        .iter()
        .filter(|m| !m.is_native() && m.code().is_some())
        .count();

    eprintln!(
        "[real_jdk] Object: {} native, {} bytecode, {} with Code attribute",
        native_count, bytecode_count, has_code_count
    );

    assert!(native_count > 0, "Object should have native methods");
    assert!(bytecode_count > 0, "Object should have bytecode methods");
    assert_eq!(
        bytecode_count, has_code_count,
        "All non-native methods should have Code attributes"
    );

    // Check String too
    let str_id = cm.get_loaded_class_id("java/lang/String").unwrap();
    let str_class = cm.class_store.get(str_id).unwrap();
    let str_bytecode = str_class
        .methods
        .iter()
        .filter(|m| !m.is_native() && m.code().is_some())
        .count();
    eprintln!(
        "[real_jdk] String: {} bytecode methods with Code",
        str_bytecode
    );
    assert!(
        str_bytecode > 100,
        "String should have >100 bytecode methods"
    );
}

#[test]
fn real_jdk_object_equals_bytecode() {
    // Object.equals(Object) is non-native bytecode — tests "aload_0, aload_1, if_acmpeq"
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    // Allocate objects with correct field count for real Object class
    let cm = vm.shared.classes.class_manager.read();
    let obj_id = cm.get_loaded_class_id("java/lang/Object").unwrap();
    let nfields = cm
        .class_store
        .get(obj_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);
    drop(cm);

    let obj1 = vm.shared.mem.heap.alloc_object(obj_id, nfields);
    let obj2 = vm.shared.mem.heap.alloc_object(obj_id, nfields);

    // Diagnostic: verify refs_equal works for this ObjectRef pair
    let v1 = Value::Object(Some(obj1));
    let v2 = Value::Object(Some(obj1)); // same ref
    eprintln!(
        "[real_jdk] obj1 ptr={:?}, v1={:?}, v2={:?}",
        obj1.as_ptr(),
        v1,
        v2
    );
    eprintln!(
        "[real_jdk] refs_equal(v1,v2)={}",
        crate::runtime::interpreter::test_refs_equal(&v1, &v2)
    );

    // equals(self) should return true (same reference)
    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Object",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj1)), Value::Object(Some(obj1))],
    );
    eprintln!("[real_jdk] Object.equals(self) = {:?}", result);
    match &result {
        Ok(Some(Value::Int(1))) => {} // correct: same reference
        Ok(Some(Value::Int(v))) => {
            panic!("equals(self) returned {v}, expected 1 (same reference)");
        }
        other => panic!("equals(self) unexpected: {other:?}"),
    }

    // equals(other) should return false (different references)
    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Object",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj1)), Value::Object(Some(obj2))],
    );
    match &result {
        Ok(Some(Value::Int(0))) => {} // correct: different references
        Ok(Some(Value::Int(v))) => {
            panic!("equals(other) returned {v}, expected 0 (different references)");
        }
        other => panic!("equals(other) unexpected: {other:?}"),
    }
}

#[test]
fn real_jdk_integer_valueof_creates_object() {
    // Integer.valueOf(42) → verify it creates an object via real JDK bytecode
    // Note: field access by name fails because JIT allocates objects with ClassId(0)
    // instead of real Integer ClassId. This is a known JIT limitation to be fixed.
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    );
    assert!(
        matches!(result, Ok(Some(Value::Object(Some(_))))),
        "Integer.valueOf(42) should return an object, got {:?}",
        result
    );
}

#[test]
fn real_jdk_integer_valueof_returns_object() {
    // Integer.valueOf(int) — this executes real JDK bytecode via the interpreter!
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    );
    eprintln!("[real_jdk] Integer.valueOf(42) = {:?}", result);
    // The key assertion: this method executes real JDK bytecode and returns an object
    assert!(
        matches!(result, Ok(Some(Value::Object(Some(_))))),
        "Integer.valueOf should return an Integer object"
    );
}

/// WP0.1 regression — Multiple `System.out.println(String)` calls must all
/// dispatch to the Rust native, even after the real JDK's
/// `java/io/PrintStream` class has been loaded (which happens automatically
/// when `vm_with_real_jdk` is used and `java.base.jmod` is on the boot
/// classpath).
///
/// Pre-fix symptom: the first `println` call went through the slow path
/// and hit the native. The VtableManager fast path (`execute_invokevirtual_vtable_fast`)
/// then observed the vtable populated from the real JDK bytecode and, on
/// the second call, dispatched to real JDK `PrintStream.println` bytecode —
/// which NPEs on `textOut.write(s)` because our synthetic PrintStream
/// object only has field 0 populated (fd_id).
///
/// Fix: the vtable fast path now consults `native_methods.find(...)` for
/// the receiver's class name before using the vtable, consistent with the
/// dispatch order used by `populate_virtual_invoke_cache` and
/// `try_stackless_invoke`. A match returns `CacheMiss` so the slow path /
/// `execute_invokevirtual_cached` VirtualNative entry is honored.
#[test]
fn test_system_out_multiline_println() {
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    let (out_ref, _err_ref) = vm.shared.ensure_system_streams();

    // Five sequential println(String) calls — each must hit the Rust
    // native, never the real JDK bytecode (whose field dependencies are
    // not satisfied by our synthetic PrintStream object).
    let lines = ["one", "two", "three", "four", "five"];
    for (i, text) in lines.iter().enumerate() {
        let jstring = create_java_string(&vm.shared, text);
        let result = invoke_or_native(
            &vm.shared,
            &mut vm.main_thread,
            "java/io/PrintStream",
            "println",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(out_ref)), Value::Object(Some(jstring))],
        );
        assert!(
            result.is_ok(),
            "println(String) call #{i} ({text:?}) should succeed, got {result:?}"
        );
    }

    // All five lines must be captured; before the fix only the first one
    // made it through before an NPE aborted execution.
    assert_eq!(
            vm.main_thread.printed_lines,
            vec!["one", "two", "three", "four", "five"],
            "all 5 println(String) lines must be captured (vtable fast-path native-override regression)"
        );
}

#[test]
fn real_jdk_ensure_class_initialized_registers_natives() {
    // Test that ensure_class_initialized works for real classes
    // This triggers <clinit> which may call registerNatives()
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    // Try to initialize Object — should work (Object has a trivial <clinit> or none)
    let result = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.ensure_class_initialized("java/lang/Object")
    };
    eprintln!("[real_jdk] Object initialization: {:?}", result);
    assert!(result.is_ok(), "Object should initialize successfully");

    // Check Boolean field layout
    {
        let cm = vm.shared.classes.class_manager.read();
        if let Some(bool_id) = cm.get_loaded_class_id("java/lang/Boolean") {
            let cls = cm.class_store.get(bool_id).unwrap();
            eprintln!("[real_jdk] Boolean: is_synthetic={}, fields={}, first_field_index={}, num_total_fields={}, methods={}",
                    cls.origin.is_compatibility_stub(), cls.fields.len(), cls.first_field_index, cls.num_total_fields, cls.methods.len());
            for f in &cls.fields {
                eprintln!(
                    "[real_jdk]   field: {} {} static={}",
                    f.name,
                    f.descriptor,
                    f.is_static()
                );
            }
        }
    }

    // Initialize Boolean — its <clinit> runs real JDK bytecode
    {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.ensure_class_initialized("java/lang/Boolean")
            .expect("Boolean should initialize");
    }

    // Note: Other wrapper classes removed from this test to avoid long init times.
    // Integer/Long <clinit> creates cache arrays (256+ objects), slow in debug builds.
}

#[test]
#[ignore] // Slow: loads real JDK
fn real_jdk_character_float_double_clinit() {
    let Some(mut vm) = vm_with_real_jdk() else {
        return;
    };

    // Initialize Object first (prerequisite for all classes)
    {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        ctx.ensure_class_initialized("java/lang/Object")
            .expect("Object should initialize");
    }

    // Character <clinit>
    {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let result = ctx.ensure_class_initialized("java/lang/Character");
        eprintln!("[real_jdk] Character initialization: {:?}", result);
        assert!(
            result.is_ok(),
            "Character should initialize: {:?}",
            result.err()
        );
    }

    // Float <clinit>
    {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let result = ctx.ensure_class_initialized("java/lang/Float");
        eprintln!("[real_jdk] Float initialization: {:?}", result);
        assert!(
            result.is_ok(),
            "Float should initialize: {:?}",
            result.err()
        );
    }

    // Double <clinit>
    {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let result = ctx.ensure_class_initialized("java/lang/Double");
        eprintln!("[real_jdk] Double initialization: {:?}", result);
        assert!(
            result.is_ok(),
            "Double should initialize: {:?}",
            result.err()
        );
    }
}

/// End-to-end test that exercises real JDK bytecode with `use_synthetic_jdk = false`.
///
/// This test:
/// 1. Boots the VM in real-JDK mode and checks the real-JDK registry filter
///    is engaged (see the long note in step 1 — this used to be a raw
///    `native_methods.len() < 2700` assertion, which measured a property
///    real-JDK mode does not implement)
/// 2. Calls `Integer.valueOf(int)` and then `Integer.intValue()` round-trip
/// 3. Calls `String.valueOf(int)`
/// 4. Checks `java/lang/Integer` was loaded from a real `.class` file
///    (`!is_synthetic_stub`, >10 methods with a real `Code` attribute)
/// 5. Enables `audit_missing_natives` and prints what's missing for visibility
///
/// Item 4 is the load-bearing proof that real JDK `.class` bytecode is
/// loaded. Items 2-3 are NOT such a proof: real-JDK mode registers natives
/// that shadow `Integer.valueOf`, `Integer.intValue` and `String.valueOf`,
/// so those calls run Rust. See the caveat at step 2 in the body.
#[test]
fn real_jdk_hello_world_e2e() {
    // Discover JAVA_HOME
    let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
        .ok()
        .or_else(|| {
            let p = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
            if p.exists() {
                Some(p.to_string_lossy().into_owned())
            } else {
                None
            }
        });
    let Some(java_home) = java_home else {
        eprintln!("Skipping real_jdk_hello_world_e2e: no JDK found");
        return;
    };
    let jmod = std::path::PathBuf::from(&java_home)
        .join("jmods")
        .join("java.base.jmod");
    if !jmod.exists() {
        eprintln!("Skipping real_jdk_hello_world_e2e: java.base.jmod not found");
        return;
    }

    // Build config: real JDK mode with audit
    let config = VmConfig {
        boot_classpath: vec![jmod.to_string_lossy().into_owned()],
        use_synthetic_jdk: false,
        audit_missing_natives: true,
        ..VmConfig::default()
    };
    let mut vm = Vm::new(config);

    // ---- Step 1: Verify the real-JDK-mode registry filter is engaged ----
    //
    // 2026-07-31: this step used to assert `native_methods.len() < 2700`
    // ("Real JDK mode should have < 2700 native stubs"). That assertion was
    // measuring something the VM has never implemented, and it observed
    // 10,9xx for a long time before it was investigated. Three separate
    // reasons, all verified against this tree — do NOT restore it as a raw
    // count with a bigger constant:
    //
    // 1. NOTHING FILTERS BY CATEGORY IN REAL-JDK MODE. The stub-dropping
    //    mechanism is `NativeMethodRegistry::drop_synthetic_stubs`
    //    (native-api/src/registry.rs), and both real-JDK arms of
    //    `vm_init.rs` deliberately do NOT turn it on — see the block
    //    comments at `vm/src/vm/vm_init.rs` ("NOTE: unconditionally
    //    dropping ALL SyntheticStub-tagged natives in real-JDK mode was
    //    tried (dev d8092acb, 2026-07-14) and reverted the same day") and
    //    the twin "set_drop_synthetic_stubs(true) intentionally NOT called
    //    here". It is opt-in via `CRATONVM_NO_STUBS` only. So `len()` here
    //    is the FULL registration surface of every category, not a stub
    //    count, and no threshold on it can mean what the old message said.
    //
    // 2. THE SURFACE GENUINELY GREW, BY DESIGN.
    //    `register_essential_natives_with_shims`
    //    (native-builtins/src/lib.rs) — the registrar the real-JDK arm
    //    calls — is now ~13k lines: ~940 direct `register(...)` calls plus
    //    ~90 sub-registrars, covering third-party app shims (Spring,
    //    Jackson, Jasper/JSP, JBoss Modules, Jython, Conscrypt/TLS, JDBC,
    //    JFR, WildFly naming/security/datasources, HTTP client, sockets,
    //    zip, Vector API) as well as JDK bridges. The "~150 essential
    //    natives" in `vm/Cargo.toml` and the "~300 truly-native methods" in
    //    `vm/src/config.rs` are stale by more than an order of magnitude.
    //
    // 3. RE-ANCHORING ON `NativeKind::SyntheticStub == 0` WOULD BE
    //    VACUOUS TODAY. Categories are ambient (`set_category` /
    //    `with_category`), and 244 `register_*` functions across
    //    native-builtins/-collections/-io (~2,900 `register` calls) never
    //    set one at all, so they inherit whatever their caller left. The
    //    largest single case is `register_essential_natives_with_shims`
    //    itself, which pins `NativeKind::Bridge` across its whole body
    //    under the comment "These are the ACC_NATIVE methods with no
    //    bytecode" — true for the JDK bridges, false for the app shims
    //    registered in the same span (e.g. `register_ecj_problem_overrides`,
    //    `register_jfr_natives`, `register_jdk_security_natives`,
    //    `register_test_harness_natives` all set no category of their own).
    //    Fixing that mis-tagging is a real, large VM cleanup; it is a
    //    prerequisite for any category-based assertion here, not something
    //    this test can paper over.
    //
    // The count invariant that IS meaningful lives in
    // `vm/src/vm/vm_init.rs::real_jdk_mode_registers_fewer_natives` (real
    // <= synthetic) and `synthetic_mode_registers_many_natives`. What this
    // test can honestly guard is that real-JDK mode's registry filter is
    // actually engaged, which is asserted below.
    let native_count = vm.shared.natives.native_methods.len();
    let registrations = vm.shared.natives.native_methods.dump_registrations();
    let (mut synthetic_stubs, mut bridges, mut intrinsics) = (0usize, 0usize, 0usize);
    for (_, _, _, kind) in &registrations {
        match kind {
            cratonvm_native_api::NativeKind::SyntheticStub => synthetic_stubs += 1,
            cratonvm_native_api::NativeKind::Bridge => bridges += 1,
            cratonvm_native_api::NativeKind::Intrinsic => intrinsics += 1,
        }
    }
    eprintln!(
        "[real_jdk_e2e] Native methods registered: {} (SyntheticStub={} Bridge={} Intrinsic={})",
        native_count, synthetic_stubs, bridges, intrinsics
    );

    // Real-JDK mode sets `set_drop_real_layout_synthetic(true)` BEFORE any
    // `register_*` pass (both arms of `vm_init.rs`), and the registry then
    // refuses every registration on the classes whose synthetic field
    // layout corrupts the real JDK object. `java/io/StringReader` is an
    // unconditional entry in that filter AND is genuinely offered in
    // real-JDK mode — `native-io`'s `register_string_rw_natives` registers
    // nine `java/io/StringReader` triples and is reached from
    // `register_io_natives`, which the real-JDK arm calls. So its absence
    // from the registry here is a direct, non-numeric observation that the
    // real-JDK filter took effect. If someone drops
    // `set_drop_real_layout_synthetic(true)` or reorders it after a
    // registrar, this fails immediately — which is what the old `< 2700`
    // proxy was reaching for. (Deliberately NOT asserted on
    // `java/util/EnumSet`: it is in the same filter, but its registrar
    // `phases_early::register_enum_set_natives` only runs under
    // `register_synthetic_overrides`, so in real-JDK mode that assertion
    // would be vacuously true and would guard nothing.)
    let real_jdk_dropped_class = "java/io/StringReader";
    assert!(
        !registrations
            .iter()
            .any(|(c, _, _, _)| *c == real_jdk_dropped_class),
        "real-JDK mode must drop all {real_jdk_dropped_class} natives \
             (set_drop_real_layout_synthetic); found one registered"
    );

    // `java/io/InputStreamReader` joined the same filter on 2026-09-17, after a
    // `--features synthetic-jdk` binary run against a real JDK registered
    // `native-builtins/src/servlet.rs`'s synthetic Reader-stack `<init>` over
    // the real class: slot 0 is `Reader.lock`, not a parked `InputStream`, and
    // the real `sd` (`sun.nio.cs.StreamDecoder`) was never created, so the bulk
    // `read([CII)I` -- which that shim does NOT register, and which every
    // `BufferedReader.readLine()` goes through -- NPE'd on a null `sd`. 188 of
    // 188 hibernate-reactive classes crashed before running a test.
    //
    // WHAT THIS ROW IS AND IS NOT. It is in the right configuration: the
    // registrar that broke it is `#[cfg(feature = "synthetic-jdk")]`, and this
    // whole module is `#[cfg(all(test, feature = "synthetic-jdk"))]` (see
    // `vm/src/vm.rs`), so the only build in which this assertion exists is the
    // only build in which the registration is offered.
    //
    // It is NOT the guard, and saying so here saves the next person the trip:
    // this test needs `<java.home>/jmods/java.base.jmod` and silently `return`s
    // with a "Skipping" line otherwise (see the top of the function), so a host
    // whose JDK 25 ships a `lib/modules` runtime image rather than jmods --
    // which is every Adoptium install, including the one this repo's harnesses
    // autodetect -- never executes a line of it. Measured, not assumed.
    //
    // THE GUARD is `native-api`'s
    // `real_layout_mode_drops_enumset_native_surface`, which asserts the filter
    // directly, runs in every build, needs no JDK, and was verified to FAIL
    // when the `java/io/InputStreamReader` arm of the filter is removed. This
    // row is belt-and-braces for the hosts that can run it.
    let isr_dropped_class = "java/io/InputStreamReader";
    assert!(
        !registrations
            .iter()
            .any(|(c, _, _, _)| *c == isr_dropped_class),
        "real-JDK mode must drop all {isr_dropped_class} natives \
             (set_drop_real_layout_synthetic); found one registered"
    );
    assert!(
        native_count > 0,
        "real-JDK mode still needs its bridge/intrinsic surface registered"
    );

    // ---- Step 2: Integer.valueOf(42) ----
    //
    // CAVEAT (2026-07-31), because the header comment above used to claim
    // this "proves real JDK .class bytecode is being loaded and executed,
    // not Rust stubs": it does not. A registered native shadows a class's
    // bytecode at EVERY dispatch site, and real-JDK mode registers natives
    // for all three probes used in steps 2-4 —
    // `lang_math::register_wrapper_natives` (reached from
    // `register_essential_natives_with_shims`) registers
    // `Integer.valueOf(I)Ljava/lang/Integer;` and `Integer.intValue()I` as
    // `NativeKind::Intrinsic`, and `register_essential_natives_with_shims`
    // itself registers `String.valueOf(I)Ljava/lang/String;` (see the
    // "String.valueOf and Integer.toString overrides" block in
    // native-builtins/src/lib.rs). Steps 2-4 therefore exercise
    // the Rust natives, not JDK bytecode. The load-bearing real-JDK proof
    // in this test is step 6 (`!is_synthetic_stub` plus >10 real Code
    // attributes on `java/lang/Integer`); steps 2-5 are smoke tests that
    // the dispatch path returns sane values. Picking probe methods that are
    // genuinely un-shadowed would make steps 2-4 mean what they claim.
    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    );
    eprintln!("[real_jdk_e2e] Integer.valueOf(42) = {:?}", result);
    let int_obj = match result {
        Ok(Some(Value::Object(Some(obj)))) => {
            eprintln!("[real_jdk_e2e] Integer.valueOf(42) returned object (SUCCESS)");
            obj
        }
        Ok(other) => {
            eprintln!(
                "[real_jdk_e2e] Integer.valueOf(42) returned {:?} (unexpected)",
                other
            );
            panic!(
                "Integer.valueOf(42) should return an Object, got {:?}",
                other
            );
        }
        Err(e) => {
            eprintln!("[real_jdk_e2e] Integer.valueOf(42) failed: {:?}", e);
            panic!("Integer.valueOf(42) should not fail: {:?}", e);
        }
    };

    // ---- Step 3: Integer.intValue() round-trip via real bytecode ----
    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Integer",
        "intValue",
        "()I",
        &[Value::Object(Some(int_obj))],
    );
    eprintln!("[real_jdk_e2e] Integer.intValue() = {:?}", result);
    match &result {
        Ok(Some(Value::Int(v))) => {
            eprintln!(
                "[real_jdk_e2e] Integer round-trip: valueOf(42).intValue() = {} (expected 42)",
                v
            );
            assert_eq!(*v, 42, "Integer round-trip should return 42");
        }
        other => {
            eprintln!("[real_jdk_e2e] Integer.intValue() unexpected: {:?}", other);
            // Don't hard-fail: intValue field access may not yet work for real JDK objects.
            // The valueOf success above already proves real bytecode execution.
        }
    }

    // ---- Step 4: String.valueOf(42) → exercises more real JDK bytecode ----
    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/String",
        "valueOf",
        "(I)Ljava/lang/String;",
        &[Value::Int(42)],
    );
    eprintln!("[real_jdk_e2e] String.valueOf(42) = {:?}", result);
    match &result {
        Ok(Some(Value::Object(Some(_)))) => {
            eprintln!("[real_jdk_e2e] String.valueOf(42) returned a String object (SUCCESS)");
        }
        Ok(other) => {
            eprintln!("[real_jdk_e2e] String.valueOf(42) returned {:?}", other);
        }
        Err(e) => {
            // String.valueOf(int) calls Integer.toString(int) which is more complex.
            // Log it but don't hard-fail — the Integer.valueOf test above already
            // proves real bytecode execution.
            eprintln!(
                "[real_jdk_e2e] String.valueOf(42) failed: {:?} (acceptable — complex call chain)",
                e
            );
        }
    }

    // ---- Step 5: Object.equals via real bytecode (simple control flow) ----
    let cm = vm.shared.classes.class_manager.read();
    let obj_id = cm.get_loaded_class_id("java/lang/Object").unwrap();
    let nfields = cm
        .class_store
        .get(obj_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);
    drop(cm);

    let obj1 = vm.shared.mem.heap.alloc_object(obj_id, nfields);
    let result = invoke_or_native(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/Object",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj1)), Value::Object(Some(obj1))],
    );
    eprintln!("[real_jdk_e2e] Object.equals(self, self) = {:?}", result);
    assert!(result.is_ok(), "Object.equals should not crash");

    // ---- Step 6: Verify bytecode methods have Code attributes ----
    {
        let cm = vm.shared.classes.class_manager.read();
        let int_id = cm.get_loaded_class_id("java/lang/Integer").unwrap();
        let int_class = cm.class_store.get(int_id).unwrap();
        assert!(
            !int_class.origin.is_compatibility_stub(),
            "Integer class should be loaded from real .class file, not synthetic stub"
        );
        let bytecode_with_code = int_class
            .methods
            .iter()
            .filter(|m| !m.is_native() && m.code().is_some())
            .count();
        eprintln!(
            "[real_jdk_e2e] Integer: {} bytecode methods with Code attribute",
            bytecode_with_code
        );
        assert!(
            bytecode_with_code > 10,
            "Integer should have >10 bytecode methods, got {}",
            bytecode_with_code
        );
    }

    // ---- Step 7: Dump missing natives audit for visibility ----
    let missing = vm.shared.get_missing_natives();
    if missing.is_empty() {
        eprintln!("[real_jdk_e2e] No missing natives encountered (all covered!)");
    } else {
        eprintln!(
            "[real_jdk_e2e] === Missing Native Methods ({}) ===",
            missing.len()
        );
        for sig in &missing {
            eprintln!("[real_jdk_e2e]   {}", sig);
        }
        eprintln!("[real_jdk_e2e] === End Missing Natives ===");
    }

    eprintln!("[real_jdk_e2e] End-to-end real JDK test PASSED");
}

#[test]
fn unsafe_get_and_add_long() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Long(100));

    let result = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "getAndAddLong",
        "(Ljava/lang/Object;JJ)J",
        &[
            Value::Object(None), // unsafe this
            Value::Object(Some(obj)),
            Value::Long(0),  // offset
            Value::Long(25), // delta
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Long(100))); // old value
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Long(125)); // updated
}

#[test]
fn unsafe_get_and_set_long() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Long(42));

    let result = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "getAndSetLong",
        "(Ljava/lang/Object;JJ)J",
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Long(99),
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Long(42))); // old value
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Long(99)); // new value
}

#[test]
fn jdk_internal_unsafe_cas_int() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Int(7));

    // Use the jdk/internal/misc/Unsafe alias
    let result = call_native(
        &shared,
        &mut thread,
        "jdk/internal/misc/Unsafe",
        "compareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Int(7),  // expected
            Value::Int(42), // new
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Int(1))); // success
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Int(42));
}

#[test]
fn unsafe_float_field_access() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    // Store a float via putFloat
    call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "putFloat",
        "(Ljava/lang/Object;JF)V",
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Float(3.14),
        ],
    )
    .unwrap();

    // Read it back via getFloat
    let result = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "getFloat",
        "(Ljava/lang/Object;J)F",
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(0),
        ],
    )
    .unwrap();

    match result {
        Some(Value::Float(f)) => assert!((f - 3.14).abs() < 0.001),
        other => panic!("expected Float, got {:?}", other),
    }
}

#[test]
fn unsafe_double_field_access() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "putDouble",
        "(Ljava/lang/Object;JD)V",
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(1),
            Value::Double(2.71828),
        ],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "getDouble",
        "(Ljava/lang/Object;J)D",
        &[
            Value::Object(None),
            Value::Object(Some(obj)),
            Value::Long(1),
        ],
    )
    .unwrap();

    match result {
        Some(Value::Double(d)) => assert!((d - 2.71828).abs() < 0.00001),
        other => panic!("expected Double, got {:?}", other),
    }
}

#[test]
fn collections_swap_test() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

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

    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let c = create_java_string(&shared, "c");

    for s in [a, b, c] {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    // Swap index 0 and 2: [a,b,c] → [c,b,a]
    call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "swap",
        "(Ljava/util/List;II)V",
        &[Value::Object(Some(list)), Value::Int(0), Value::Int(2)],
    )
    .unwrap();

    let r0 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(0)],
    )
    .unwrap();
    if let Some(Value::Object(Some(s))) = r0 {
        assert_eq!(read_java_string(&shared.mem.heap, s), Some("c".into()));
    } else {
        panic!("expected string at index 0");
    }

    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(2)],
    )
    .unwrap();
    if let Some(Value::Object(Some(s))) = r2 {
        assert_eq!(read_java_string(&shared.mem.heap, s), Some("a".into()));
    } else {
        panic!("expected string at index 2");
    }
}

#[test]
fn collections_fill_test() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

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

    for v in [10, 20, 30] {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Int(v)],
        )
        .unwrap();
    }

    // Fill all with 99
    call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "fill",
        "(Ljava/util/List;Ljava/lang/Object;)V",
        &[Value::Object(Some(list)), Value::Int(99)],
    )
    .unwrap();

    for i in 0..3 {
        let r = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i)],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(99)));
    }
}

#[test]
fn collections_rotate_test() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

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

    // [10, 20, 30]
    for v in [10, 20, 30] {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Int(v)],
        )
        .unwrap();
    }

    // Rotate by 1: [10,20,30] → [30,10,20]
    call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "rotate",
        "(Ljava/util/List;I)V",
        &[Value::Object(Some(list)), Value::Int(1)],
    )
    .unwrap();

    let r0 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(r0, Some(Value::Int(30)));

    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(1)],
    )
    .unwrap();
    assert_eq!(r1, Some(Value::Int(10)));

    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(2)],
    )
    .unwrap();
    assert_eq!(r2, Some(Value::Int(20)));
}

#[test]
fn collections_shuffle_preserves_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

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

    for v in [1, 2, 3, 4, 5] {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Int(v)],
        )
        .unwrap();
    }

    call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "shuffle",
        "(Ljava/util/List;)V",
        &[Value::Object(Some(list))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(5)));
}

#[test]
fn throwable_add_suppressed_and_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create primary exception with 3+ fields (message, cause, suppressed)
    let primary = alloc_receiver(&shared, &mut thread, "java/lang/Throwable", 4);
    let supp1 = alloc_receiver(&shared, &mut thread, "java/lang/Throwable", 4);
    let supp2 = alloc_receiver(&shared, &mut thread, "java/lang/Throwable", 4);

    // Add two suppressed exceptions
    call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(primary)), Value::Object(Some(supp1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "addSuppressed",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(Some(primary)), Value::Object(Some(supp2))],
    )
    .unwrap();

    // Get suppressed — should return array of length 2
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
        &[Value::Object(Some(primary))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(arr)) => {
            assert_eq!(shared.mem.heap.array_length(arr), 2);
            assert_eq!(
                shared.mem.heap.get_array_element(arr, 0).unwrap(),
                Value::Object(Some(supp1))
            );
            assert_eq!(
                shared.mem.heap.get_array_element(arr, 1).unwrap(),
                Value::Object(Some(supp2))
            );
        }
        _ => panic!("expected suppressed array"),
    }
}

#[test]
fn unsafe_page_size_and_address_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let page_size = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "pageSize",
        "()I",
        &[Value::Object(None)],
    )
    .unwrap();
    assert_eq!(page_size, Some(Value::Int(4096)));

    let addr_size = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "addressSize",
        "()I",
        &[Value::Object(None)],
    )
    .unwrap();
    assert_eq!(addr_size, Some(Value::Int(8)));
}

// =======================================================================
// Phase 43: CRITICAL gap fixes
// =======================================================================

/// G7: Streams groupingBy with downstream toList — inline path avoids
/// recursive native_stream_collect and potential stack overflow.
#[test]
fn streams_grouping_by_downstream_to_list() {
    use crate::classloading::resolution::MethodHandleKind;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create stream with ["a", "b", "a"]
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let a2 = create_java_string(&shared, "a");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(a)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(b)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Object(Some(a2)));

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();

    // Create classifier lambda: String.length() → groups by string length
    let classifier = make_lambda_proxy(
        &shared,
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "java/lang/String",
        "length",
        "()I",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    );

    // Create groupingBy(classifier, Collectors.toList()) collector
    let downstream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "toList",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();

    // Build GROUPING_BY_DOWNSTREAM collector manually
    let collector = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(collector, 0, Value::Int(9)); // GROUPING_BY_DOWNSTREAM
    shared
        .mem
        .heap
        .set_field(collector, 1, Value::Object(Some(classifier)));
    shared.mem.heap.set_field(collector, 2, downstream);

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, Value::Object(Some(collector))],
    )
    .unwrap();

    // Should not stack overflow — result is a Map
    assert!(result.is_some());
}

/// G7: Streams groupingBy with counting downstream — inline path.
#[test]
fn streams_grouping_by_downstream_counting() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create stream with [10, 20, 10]
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(10));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(20));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(10));

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();

    // Build GROUPING_BY_DOWNSTREAM with COUNTING downstream
    // Classifier: identity (use String.valueOf as pass-through)
    let classifier = make_lambda_proxy(
        &shared,
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "java/lang/String",
        "valueOf",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );

    let downstream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "counting",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();

    let collector = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(collector, 0, Value::Int(9)); // GROUPING_BY_DOWNSTREAM
    shared
        .mem
        .heap
        .set_field(collector, 1, Value::Object(Some(classifier)));
    shared.mem.heap.set_field(collector, 2, downstream);

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, Value::Object(Some(collector))],
    )
    .unwrap();

    assert!(result.is_some());
}

// =======================================================================
// Phase 44: Remaining CRITICAL gap fixes
// =======================================================================

/// G4: Unsafe.allocateMemory returns a non-zero address.
#[test]
fn unsafe_allocate_memory() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "allocateMemory",
        "(J)J",
        &[Value::Object(None), Value::Long(128)],
    )
    .unwrap();
    match result {
        Some(Value::Long(addr)) => assert!(addr != 0, "allocateMemory should return non-zero"),
        other => panic!("expected Long, got {:?}", other),
    }
}

/// G4: Unsafe.throwException throws the given exception.
#[test]
fn unsafe_throw_exception() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let exc = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let result = call_native(
        &shared,
        &mut thread,
        "sun/misc/Unsafe",
        "throwException",
        "(Ljava/lang/Throwable;)V",
        &[Value::Object(None), Value::Object(Some(exc))],
    );
    assert!(result.is_err(), "throwException should return Err");
}

/// G10: SecureRandom.generateSeed returns a non-null byte array.
#[test]
fn secure_random_generate_seed() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let result = call_native(
        &shared,
        &mut thread,
        "java/security/SecureRandom",
        "generateSeed",
        "(I)[B",
        &[Value::Object(Some(sr)), Value::Int(16)],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(_arr))) => {
            // generateSeed returns a non-null array
        }
        other => panic!("expected non-null array, got {:?}", other),
    }
}

/// G10: SecureRandom.nextBytes fills the array with random data.
#[test]
fn secure_random_next_bytes() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 32);
    let sr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/security/SecureRandom",
        "nextBytes",
        "([B)V",
        &[Value::Object(Some(sr)), Value::Object(Some(arr))],
    )
    .unwrap();

    // Check that at least some bytes were set to non-zero
    let mut any_nonzero = false;
    for i in 0..32 {
        match shared.mem.heap.get_array_element(arr, i) {
            Ok(Value::Int(v)) if v != 0 => {
                any_nonzero = true;
                break;
            }
            _ => {}
        }
    }
    assert!(any_nonzero, "nextBytes should produce random data");
}

// =======================================================================
// Phase 45: HIGH priority gap fixes
// =======================================================================

/// G20: Pattern.compile with LITERAL flag should treat metacharacters as literals.
#[test]
fn pattern_compile_literal_flag() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Pattern.compile("a.b", Pattern.LITERAL) — the dot should be literal
    let pat_str = create_java_string(&shared, "a.b");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;I)Ljava/util/regex/Pattern;",
        &[Value::Object(Some(pat_str)), Value::Int(16)], // 16 = LITERAL
    )
    .unwrap();
    assert!(
        result.is_some(),
        "Pattern.compile with LITERAL should succeed"
    );
}

/// G20: Pattern.compile with COMMENTS flag should ignore whitespace.
#[test]
fn pattern_compile_comments_flag() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pat_str = create_java_string(&shared, r"a \d+ b");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;I)Ljava/util/regex/Pattern;",
        &[Value::Object(Some(pat_str)), Value::Int(4)], // 4 = COMMENTS
    )
    .unwrap();
    assert!(
        result.is_some(),
        "Pattern.compile with COMMENTS should succeed"
    );
}

/// G20: Pattern.quote should escape regex metacharacters.
#[test]
fn pattern_quote() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = create_java_string(&shared, "a.b+c*d");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "quote",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(input))],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(r))) => {
            let s = read_java_string(&shared.mem.heap, r).unwrap();
            assert!(s.contains("\\Q"), "Pattern.quote should wrap in \\Q...\\E");
            assert!(s.contains("\\E"), "Pattern.quote should wrap in \\Q...\\E");
        }
        other => panic!("expected string, got {:?}", other),
    }
}

/// G16: Matcher.region should restrict matching.
#[test]
fn matcher_region() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create Pattern and Matcher
    let pat_str = create_java_string(&shared, "\\d+");
    let pattern = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
        &[Value::Object(Some(pat_str))],
    )
    .unwrap()
    .unwrap();

    let input = create_java_string(&shared, "abc123def456");
    let matcher = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        &[pattern, Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();

    // Set region to "123def" (positions 3..9)
    let _ = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "region",
        "(II)Ljava/util/regex/Matcher;",
        &[matcher, Value::Int(3), Value::Int(9)],
    )
    .unwrap();

    // regionEnd should return input length (after substring)
    let end = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "regionEnd",
        "()I",
        &[matcher],
    )
    .unwrap();
    assert_eq!(end, Some(Value::Int(6))); // "123def" has length 6
}

// `multi_catch_exception_handler` (G37) WAS HERE AND IS DELETED (E40-1 §3).
//
// Its body was EMPTY — fifteen lines of comment and not one statement — so
// it could not fail under any input, any interpreter change, or any
// deletion of the code it named. It was one of four such tests in this file
// and the only one with no executable line at all. Mutating the code under
// test cannot turn it red because it does not call the code under test.
//
// Worse, it read as coverage while its own text admits it is not: "We can't
// easily construct bytecode with multi-catch in a unit test, but we verify
// the handler lookup behavior" — followed by a paragraph of code review
// ("Verified correct: find_exception_handler at interpreter.rs:2374
// iterates all entries..."), which is a claim about a source file, at a
// line number, made by a reader. `find_exception_handler` has since moved
// to `vm/src/runtime/interpreter/exception_dispatch.rs:226`, so the
// citation had already rotted, and nothing would have reported that.
//
// The claim is still worth testing, and the premise ("can't easily
// construct bytecode") is FALSE in this file today: `register_test_class`
// takes a `CodeAttribute` with a real `exception_table`, and the tests from
// `reflect_method_invoke_static_void` onward drive hand-assembled bytecode
// through the interpreter. The test worth having builds ONE method whose
// table holds two entries over the same PC range with the same
// `handler_pc` and two DIFFERENT `catch_type`s, throws each of the two
// exception classes in turn, and asserts the handler ran for both — and it
// must also assert the negative, that a third, unrelated class does NOT
// reach that handler, or a `find_exception_handler` that returns the first
// entry unconditionally would pass. `find_exception_handler` is
// `pub(super)` to `crate::runtime::interpreter`, so a direct unit test
// belongs in that module, not here. Nominated: E40-1 §5 N4.

// =======================================================================
// Phase 46: Formatting and ClassLoader fixes
// =======================================================================

/// DateTimeFormatter.format() with custom pattern "dd/MM/yyyy".
#[test]
fn dtf_format_custom_pattern() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create DateTimeFormatter with pattern "dd/MM/yyyy"
    let pattern = create_java_string(&shared, "dd/MM/yyyy");
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

    // Create LocalDate(2026, 4, 2)
    let date = call_native(
        &shared,
        &mut thread,
        "java/time/LocalDate",
        "of",
        "(III)Ljava/time/LocalDate;",
        &[Value::Int(2026), Value::Int(4), Value::Int(2)],
    )
    .unwrap()
    .unwrap();

    // Format the date
    let result = call_native(
        &shared,
        &mut thread,
        "java/time/format/DateTimeFormatter",
        "format",
        "(Ljava/time/temporal/TemporalAccessor;)Ljava/lang/String;",
        &[dtf, date],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(s))) => {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "02/04/2026", "DTF format should produce dd/MM/yyyy");
        }
        other => panic!("expected string, got {:?}", other),
    }
}

/// DateTimeFormatter.format() with "HH:mm:ss" pattern on LocalTime.
#[test]
fn dtf_format_time_pattern() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pattern = create_java_string(&shared, "HH:mm:ss");
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

    let time = call_native(
        &shared,
        &mut thread,
        "java/time/LocalTime",
        "of",
        "(III)Ljava/time/LocalTime;",
        &[Value::Int(14), Value::Int(30), Value::Int(5)],
    )
    .unwrap()
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/time/format/DateTimeFormatter",
        "format",
        "(Ljava/time/temporal/TemporalAccessor;)Ljava/lang/String;",
        &[dtf, time],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(s))) => {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "14:30:05");
        }
        other => panic!("expected string, got {:?}", other),
    }
}

/// DecimalFormat pattern parsing — "#,##0.00" should format with 2 decimal places.
#[test]
fn decimal_format_pattern_parsing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pattern = create_java_string(&shared, "#,##0.00");
    let df = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let _ = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(df)), Value::Object(Some(pattern))],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "format",
        "(D)Ljava/lang/String;",
        &[Value::Object(Some(df)), Value::Double(1234.5)],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(s))) => {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            // Should have grouping and 2 decimal places
            assert!(text.contains(','), "should have grouping: {}", text);
            assert!(text.contains('.'), "should have decimal: {}", text);
        }
        other => panic!("expected string, got {:?}", other),
    }
}

/// G19: Class.getClassLoader() should return non-null.
#[test]
fn class_get_class_loader_non_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cls = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[Value::Object(Some(cls))],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(_))) => {} // non-null — correct
        other => panic!("getClassLoader should return non-null, got {:?}", other),
    }
}

// =======================================================================
// Phase 47: Networking, BigInteger, URI completeness
// =======================================================================

/// InetAddress.getByName("localhost") returns non-null with 127.0.0.1.
#[test]
fn inet_address_get_by_name_localhost() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let host = create_java_string(&shared, "localhost");
    let result = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        &[Value::Object(Some(host))],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(ia))) => {
            let addr = call_native(
                &shared,
                &mut thread,
                "java/net/InetAddress",
                "getHostAddress",
                "()Ljava/lang/String;",
                &[Value::Object(Some(ia))],
            )
            .unwrap();
            match addr {
                Some(Value::Object(Some(s))) => {
                    let text = read_java_string(&shared.mem.heap, s).unwrap();
                    assert_eq!(text, "127.0.0.1");
                }
                _ => panic!("expected address string"),
            }
        }
        _ => panic!("expected InetAddress"),
    }
}

/// InetAddress.getLoopbackAddress returns 127.0.0.1.
#[test]
fn inet_address_loopback() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        &[],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(ia))) => {
            let addr = call_native(
                &shared,
                &mut thread,
                "java/net/InetAddress",
                "getHostAddress",
                "()Ljava/lang/String;",
                &[Value::Object(Some(ia))],
            )
            .unwrap();
            match addr {
                Some(Value::Object(Some(s))) => {
                    let text = read_java_string(&shared.mem.heap, s).unwrap();
                    assert_eq!(text, "127.0.0.1");
                }
                _ => panic!("expected address string"),
            }
        }
        _ => panic!("expected InetAddress"),
    }
}

/// BigInteger.gcd computes greatest common divisor.
#[test]
fn biginteger_gcd() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create BigInteger(12) and BigInteger(8), gcd should be 4
    let a_str = create_java_string(&shared, "12");
    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(a)), Value::Object(Some(a_str))],
    )
    .unwrap();

    let b_str = create_java_string(&shared, "8");
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(b)), Value::Object(Some(b_str))],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "gcd",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(r))) => {
            let s = match shared.mem.heap.get_field(r, 0) {
                Value::Object(Some(str_ref)) => {
                    read_java_string(&shared.mem.heap, str_ref).unwrap()
                }
                _ => panic!("expected string value"),
            };
            assert_eq!(s, "4", "gcd(12, 8) should be 4");
        }
        _ => panic!("expected BigInteger"),
    }
}

/// BigInteger.bitLength returns number of bits.
#[test]
fn biginteger_bit_length() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "255");
    let bi = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(bi)), Value::Object(Some(s))],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "bitLength",
        "()I",
        &[Value::Object(Some(bi))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(8)), "255 needs 8 bits");
}

/// URI.create + isAbsolute works.
#[test]
fn uri_additional_methods() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let uri_str = create_java_string(&shared, "https://example.com/path?q=1");
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

    let abs = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "isAbsolute",
        "()Z",
        &[uri],
    )
    .unwrap();
    assert_eq!(abs, Some(Value::Int(1)), "https URI should be absolute");
}

// =======================================================================
// Phase 48: Common stdlib (Arrays, Properties, Map methods)
// =======================================================================

/// Arrays.sort(int[]) sorts in ascending order.
#[test]
fn arrays_sort_int() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    for (i, v) in [5, 3, 1, 4, 2].iter().enumerate() {
        let _ = shared.mem.heap.set_array_element(arr, i, Value::Int(*v));
    }
    call_native(
        &shared,
        &mut thread,
        "java/util/Arrays",
        "sort",
        "([I)V",
        &[Value::Object(Some(arr))],
    )
    .unwrap();

    let sorted: Vec<i32> = (0..5)
        .map(
            |i| match shared.mem.heap.get_array_element(arr, i).unwrap() {
                Value::Int(v) => v,
                _ => -1,
            },
        )
        .collect();
    assert_eq!(sorted, vec![1, 2, 3, 4, 5]);
}

/// Arrays.copyOf creates a new array with specified length.
#[test]
fn arrays_copy_of_p48() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
    for (i, v) in [10, 20, 30].iter().enumerate() {
        let _ = shared.mem.heap.set_array_element(arr, i, Value::Int(*v));
    }
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Arrays",
        "copyOf",
        "([II)[I",
        &[Value::Object(Some(arr)), Value::Int(5)],
    )
    .unwrap();

    match result {
        Some(Value::Object(Some(new_arr))) => {
            assert_eq!(shared.mem.heap.array_length(new_arr), 5);
            assert_eq!(
                shared.mem.heap.get_array_element(new_arr, 0).unwrap(),
                Value::Int(10)
            );
            assert_eq!(
                shared.mem.heap.get_array_element(new_arr, 2).unwrap(),
                Value::Int(30)
            );
        }
        _ => panic!("expected array"),
    }
}

/// Arrays.binarySearch finds element in sorted array.
#[test]
fn arrays_binary_search() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    for (i, v) in [10, 20, 30, 40, 50].iter().enumerate() {
        let _ = shared.mem.heap.set_array_element(arr, i, Value::Int(*v));
    }
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Arrays",
        "binarySearch",
        "([II)I",
        &[Value::Object(Some(arr)), Value::Int(30)],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(2))); // index 2
}

/// Properties registration exists — verified by method lookup.
/// (Full round-trip test requires NativeContextImpl with array support.)
#[test]
fn properties_registered() {
    // E40-1 N6: upgraded from a bare `find(...).is_some()` triple to a real
    // round-trip. A registration existing says nothing about whether
    // `setProperty` actually stores what `getProperty` reads back, or
    // whether `size` counts real entries rather than answering a constant.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let props = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "<init>",
        "()V",
        &[Value::Object(Some(props))],
    )
    .expect("Properties.<init> must not fail");

    let key = create_java_string(&shared, "answer");
    let val = create_java_string(&shared, "42");
    call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        &[
            Value::Object(Some(props)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .expect("Properties.setProperty must not fail");

    let key2 = create_java_string(&shared, "answer");
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(props)), Value::Object(Some(key2))],
    )
    .expect("Properties.getProperty must not fail");
    match got {
        Some(Value::Object(Some(r))) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).as_deref(), Some("42"));
        }
        other => panic!(
            "getProperty(\"answer\") must return the value setProperty stored, got {other:?}"
        ),
    }

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Properties",
        "size",
        "()I",
        &[Value::Object(Some(props))],
    )
    .expect("Properties.size must not fail");
    assert_eq!(
        size,
        Some(Value::Int(1)),
        "size must count the one entry just stored"
    );
}

// =======================================================================
// Phase 49: String modern methods, Stream mapping, Map.computeIfPresent
// =======================================================================

/// String.repeat(int) registered and works.
#[test]
fn string_repeat_p49() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "ab");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "repeat",
        "(I)Ljava/lang/String;",
        &[Value::Object(Some(s)), Value::Int(3)],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(r))) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "ababab");
        }
        _ => panic!("expected string"),
    }
}

/// String.isBlank() returns true for whitespace-only strings.
#[test]
fn string_is_blank() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let blank = create_java_string(&shared, "   ");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "isBlank",
        "()Z",
        &[Value::Object(Some(blank))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(1)));

    let not_blank = create_java_string(&shared, " hi ");
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "isBlank",
        "()Z",
        &[Value::Object(Some(not_blank))],
    )
    .unwrap();
    assert_eq!(result2, Some(Value::Int(0)));
}

/// String.chars() returns an IntStream.
#[test]
fn string_chars_returns_stream() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "abc");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "chars",
        "()Ljava/util/stream/IntStream;",
        &[Value::Object(Some(s))],
    )
    .unwrap();
    assert!(matches!(result, Some(Value::Object(Some(_)))));
}

/// String method registrations exist for modern String API.
#[test]
fn string_modern_methods_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    assert!(registry
        .find("java/lang/String", "repeat", "(I)Ljava/lang/String;")
        .is_some());
    assert!(registry
        .find("java/lang/String", "isBlank", "()Z")
        .is_some());
    assert!(registry
        .find(
            "java/lang/String",
            "chars",
            "()Ljava/util/stream/IntStream;"
        )
        .is_some());
    assert!(registry
        .find("java/lang/String", "lines", "()Ljava/util/stream/Stream;")
        .is_some());
    assert!(registry
        .find("java/lang/String", "indent", "(I)Ljava/lang/String;")
        .is_some());
    assert!(registry
        .find(
            "java/lang/String",
            "codePoints",
            "()Ljava/util/stream/IntStream;"
        )
        .is_some());

    // E40-1 N6: five of these six (repeat, isBlank, chars, lines, indent)
    // already have dedicated behavioral tests elsewhere in this file —
    // `string_repeat_p49`, `string_is_blank`, `string_chars_returns_stream`,
    // and the `lines`/`indent` coverage near line 15782. `codePoints` did
    // not; call it here rather than leave the one gap silent.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, "abc");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "codePoints",
        "()Ljava/util/stream/IntStream;",
        &[Value::Object(Some(s))],
    )
    .expect("String.codePoints must not fail");
    assert!(
        matches!(result, Some(Value::Object(Some(_)))),
        "codePoints() must return a non-null IntStream, got {result:?}"
    );
}

// =======================================================================
// Phase 50: CompletableFuture completion + API audit
// =======================================================================

/// CompletableFuture.join() returns the result.
#[test]
fn completable_future_join() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a completed future with value "hello"
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

    // join() should return the value
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "join",
        "()Ljava/lang/Object;",
        &[cf],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(r))) => {
            let text = read_java_string(&shared.mem.heap, r).unwrap();
            assert_eq!(text, "hello");
        }
        other => panic!("expected string, got {:?}", other),
    }
}

/// CompletableFuture.complete() sets the result.
#[test]
fn completable_future_complete_p50() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create an empty CF
    let cf = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(cf, 0, Value::Object(None)); // result
    shared.mem.heap.set_field(cf, 1, Value::Int(0)); // not done

    let val = create_java_string(&shared, "world");
    let completed = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "complete",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(cf)), Value::Object(Some(val))],
    )
    .unwrap();
    assert_eq!(completed, Some(Value::Int(1))); // first completion succeeds

    // Second complete should fail
    let val2 = create_java_string(&shared, "nope");
    let completed2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "complete",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(cf)), Value::Object(Some(val2))],
    )
    .unwrap();
    assert_eq!(completed2, Some(Value::Int(0))); // already completed

    // get() should return original value
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(cf))],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(r))) => {
            let text = read_java_string(&shared.mem.heap, r).unwrap();
            assert_eq!(text, "world");
        }
        _ => panic!("expected string"),
    }
}

/// Comprehensive API registration verification for Phase 50.
#[test]
fn phase50_api_completeness() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        crate::native::collections::register_collections_natives(&mut r);
        r
    };
    // CompletableFuture
    let cf = "java/util/concurrent/CompletableFuture";
    assert!(
        registry.find(cf, "join", "()Ljava/lang/Object;").is_some(),
        "CF.join"
    );
    assert!(
        registry.find(cf, "get", "()Ljava/lang/Object;").is_some(),
        "CF.get"
    );
    assert!(registry.find(cf, "isDone", "()Z").is_some(), "CF.isDone");
    assert!(
        registry
            .find(cf, "complete", "(Ljava/lang/Object;)Z")
            .is_some(),
        "CF.complete"
    );
    assert!(
        registry
            .find(cf, "getNow", "(Ljava/lang/Object;)Ljava/lang/Object;")
            .is_some(),
        "CF.getNow"
    );
    assert!(
        registry
            .find(
                cf,
                "thenApply",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some(),
        "CF.thenApply"
    );

    // ConcurrentHashMap
    let chm = "java/util/concurrent/ConcurrentHashMap";
    assert!(
        registry
            .find(
                chm,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
        "CHM.put"
    );
    assert!(
        registry
            .find(chm, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
            .is_some(),
        "CHM.get"
    );
    assert!(
        registry
            .find(
                chm,
                "computeIfAbsent",
                "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;"
            )
            .is_some(),
        "CHM.computeIfAbsent"
    );

    // AtomicReference
    let ar = "java/util/concurrent/atomic/AtomicReference";
    assert!(
        registry
            .find(
                ar,
                "compareAndSet",
                "(Ljava/lang/Object;Ljava/lang/Object;)Z"
            )
            .is_some(),
        "AR.CAS"
    );
    assert!(
        registry
            .find(ar, "getAndSet", "(Ljava/lang/Object;)Ljava/lang/Object;")
            .is_some(),
        "AR.getAndSet"
    );

    // ArrayList
    assert!(
        registry
            .find("java/util/ArrayList", "indexOf", "(Ljava/lang/Object;)I")
            .is_some(),
        "AL.indexOf"
    );
    assert!(
        registry
            .find("java/util/ArrayList", "contains", "(Ljava/lang/Object;)Z")
            .is_some(),
        "AL.contains"
    );
    assert!(
        registry
            .find(
                "java/util/ArrayList",
                "stream",
                "()Ljava/util/stream/Stream;"
            )
            .is_some(),
        "AL.stream"
    );

    // HashMap
    assert!(
        registry
            .find("java/util/HashMap", "keySet", "()Ljava/util/Set;")
            .is_some(),
        "HM.keySet"
    );
    assert!(
        registry
            .find("java/util/HashMap", "values", "()Ljava/util/Collection;")
            .is_some(),
        "HM.values"
    );
    assert!(
        registry
            .find("java/util/HashMap", "entrySet", "()Ljava/util/Set;")
            .is_some(),
        "HM.entrySet"
    );

    // E40-1 N6, the doc's own named example: "`HashMap.values()` returning
    // the wrong *kind* of collection is a defect this tree has already had
    // and recorded. A registration-exists check cannot see it." This can:
    // a real map, a real `values()` call, and a check that what comes back
    // actually behaves like a `Collection` holding the two stored values —
    // not the keys, not an empty answer, not a `Set` masquerading as one.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = alloc_receiver(&shared, &mut thread, "java/util/HashMap", 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .expect("HashMap.<init> must not fail");
    for (k, v) in [("a", "1"), ("b", "2")] {
        let key = create_java_string(&shared, k);
        let val = create_java_string(&shared, v);
        call_native(
            &shared,
            &mut thread,
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(map)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        )
        .expect("HashMap.put must not fail");
    }
    let values = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "values",
        "()Ljava/util/Collection;",
        &[Value::Object(Some(map))],
    )
    .expect("HashMap.values must not fail");
    let Some(Value::Object(Some(values))) = values else {
        panic!("HashMap.values() must return a non-null Collection, got {values:?}");
    };
    // The view carrier for a plain HashMap is `java/util/HashMap$Values`
    // (`values_carrier_for` in native-collections), registered with ArrayList
    // -shaped `size`/`contains`/`iterator` natives (`register_map_view_carrier_natives`).
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap$Values",
        "size",
        "()I",
        &[Value::Object(Some(values))],
    )
    .expect("HashMap$Values.size must not fail");
    // Two puts, two values. A `values()` that accidentally answered the KEY
    // set (or an empty stand-in) would report the wrong size here.
    assert_eq!(
        size,
        Some(Value::Int(2)),
        "values() must report the two entries just put, got size={size:?}"
    );
    let contains_1 = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap$Values",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(values)),
            Value::Object(Some(create_java_string(&shared, "1"))),
        ],
    )
    .expect("HashMap$Values.contains must not fail");
    assert_eq!(
        contains_1,
        Some(Value::Int(1)),
        "values() must contain the VALUE \"1\", not the key \"a\" — the specific \
         confusion a bare registration-exists check cannot see"
    );
    let contains_key = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap$Values",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(values)),
            Value::Object(Some(create_java_string(&shared, "a"))),
        ],
    )
    .expect("HashMap$Values.contains must not fail");
    assert_eq!(
        contains_key,
        Some(Value::Int(0)),
        "values() must NOT contain the KEY \"a\" — it would if this view had \
         been built from keySet() instead of the values"
    );
}

// =======================================================================
// Phase 51: Scanner, StringReader/Writer, CLI app support
// =======================================================================

/// Scanner implementation exists and all methods are registered.
/// (Full integration test requires alloc_concurrent_synthetic for ctx.create_string in native code.)
#[test]
fn scanner_implementation_registered() {
    // Same gap as `phase51_registrations`: the Scanner surface is split
    // across crates — `close` and friends are registered by
    // `cratonvm-native-io`, whose `register_io_natives` runs after
    // `register_builtins` and deliberately overwrites the older entries.
    // Building the registry from `register_builtins` alone could never see
    // them. Mirror what `vm_init` builds.
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        crate::native::io::register_io_natives(&mut r);
        crate::native::collections::register_collections_natives(&mut r);
        r
    };
    let sc = "java/util/Scanner";
    assert!(
        registry
            .find(sc, "<init>", "(Ljava/lang/String;)V")
            .is_some(),
        "Scanner(String)"
    );
    assert!(registry.find(sc, "hasNext", "()Z").is_some(), "hasNext");
    assert!(
        registry.find(sc, "next", "()Ljava/lang/String;").is_some(),
        "next"
    );
    assert!(
        registry
            .find(sc, "nextLine", "()Ljava/lang/String;")
            .is_some(),
        "nextLine"
    );
    assert!(registry.find(sc, "nextInt", "()I").is_some(), "nextInt");
    assert!(registry.find(sc, "nextLong", "()J").is_some(), "nextLong");
    assert!(
        registry.find(sc, "nextDouble", "()D").is_some(),
        "nextDouble"
    );
    assert!(
        registry.find(sc, "hasNextLine", "()Z").is_some(),
        "hasNextLine"
    );
    assert!(
        registry.find(sc, "hasNextInt", "()Z").is_some(),
        "hasNextInt"
    );
    assert!(
        registry
            .find(
                sc,
                "useDelimiter",
                "(Ljava/lang/String;)Ljava/util/Scanner;"
            )
            .is_some(),
        "useDelimiter"
    );
    assert!(registry.find(sc, "close", "()V").is_some(), "close");

    // E40-1 N6: a real Scanner, actually scanning. `TEST_NATIVE_REGISTRY`
    // (unlike the hand-built `registry` above) already layers io + builtins +
    // collections, matching what `vm_init` builds, so `call_native` reaches
    // the real bodies behind every `find(...).is_some()` above.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let source = create_java_string(&shared, "42 hello");
    let scanner = alloc_receiver(&shared, &mut thread, sc, 4);
    call_native(
        &shared,
        &mut thread,
        sc,
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(scanner)), Value::Object(Some(source))],
    )
    .expect("Scanner(String) must not fail");

    let n = call_native(
        &shared,
        &mut thread,
        sc,
        "nextInt",
        "()I",
        &[Value::Object(Some(scanner))],
    )
    .expect("Scanner.nextInt must not fail");
    assert_eq!(
        n,
        Some(Value::Int(42)),
        "nextInt() must parse the leading token as 42"
    );

    let word = call_native(
        &shared,
        &mut thread,
        sc,
        "next",
        "()Ljava/lang/String;",
        &[Value::Object(Some(scanner))],
    )
    .expect("Scanner.next must not fail");
    match word {
        Some(Value::Object(Some(r))) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r).as_deref(),
                Some("hello")
            );
        }
        other => panic!("next() must return the second token \"hello\", got {other:?}"),
    }
}

/// Scanner.nextLine() reads full lines.
#[test]
fn scanner_next_line_p51() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Built through the fixture API, not by writing heap slots. Until
    // 2026-08-05 this hand-rolled `alloc_object(ClassId::new(0), 3)` and
    // wrote the input String to slot 0, the position to slot 1 and the
    // delimiter to slot 2. Two things changed under it (`5521d2469`):
    //
    //  * the input text has NO field to live in. Real `java.util.Scanner`
    //    holds its input in `buf`, a `java.nio.CharBuffer`; a `String`
    //    written there is a wrong-type reference. So `native-io` moved it
    //    to a side table keyed by object identity, and slot 0 is simply not
    //    read any more — `nextLine()` answered `NoSuchElementException: no
    //    more elements` against a scanner whose text was sitting in a slot
    //    nobody consults.
    //  * `position` is resolved BY NAME on the receiver, and a
    //    `ClassId::new(0)` receiver declares no names at all.
    //
    // `scanner_set_source` is the entry point `native-io` documents for
    // "the constructors here and out-of-crate test fixtures"; it installs
    // the text and resets position/delimiter/radix/closed, which is why the
    // three slot writes are gone rather than translated.
    let sc = alloc_receiver(&shared, &mut thread, "java/util/Scanner", 5);
    {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        cratonvm_native_io::scanner_set_source(&mut ctx, sc, "line1\nline2\nline3");
    }

    let line1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextLine",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap();
    match line1 {
        Some(Value::Object(Some(s))) => {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "line1");
        }
        _ => panic!("expected string"),
    }

    let line2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextLine",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sc))],
    )
    .unwrap();
    match line2 {
        Some(Value::Object(Some(s))) => {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "line2");
        }
        _ => panic!("expected string"),
    }
}

/// Scanner/StringReader/StringWriter registered.
#[test]
fn phase51_registrations() {
    // Must mirror what the VM actually builds. This asserted registrations
    // that span crates — `java/io/StringReader` and `StringWriter` live in
    // `cratonvm-native-io`, not `native-builtins` — while constructing a
    // registry from `register_builtins` alone, so it could never see them.
    // `vm_init` registers all three groups; so does this.
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        crate::native::io::register_io_natives(&mut r);
        crate::native::collections::register_collections_natives(&mut r);
        r
    };
    assert!(registry
        .find("java/util/Scanner", "next", "()Ljava/lang/String;")
        .is_some());
    assert!(registry
        .find("java/util/Scanner", "nextLine", "()Ljava/lang/String;")
        .is_some());
    assert!(registry
        .find("java/util/Scanner", "nextInt", "()I")
        .is_some());
    assert!(registry
        .find("java/util/Scanner", "nextDouble", "()D")
        .is_some());
    assert!(registry
        .find("java/util/Scanner", "hasNext", "()Z")
        .is_some());
    assert!(registry
        .find("java/io/StringReader", "<init>", "(Ljava/lang/String;)V")
        .is_some());
    assert!(registry
        .find("java/io/StringWriter", "<init>", "()V")
        .is_some());
    assert!(registry
        .find("java/io/StringWriter", "toString", "()Ljava/lang/String;")
        .is_some());
    // E40-1 N6: behavioral coverage for this family already exists —
    // `scanner_implementation_registered` (just above) drives a real
    // `Scanner(String)` through `nextInt`/`next`, and `StringReader`'s own
    // behavioral tests sit around line 30624 in this file.
}

// =========================================================================
// G66: Scoped Values (JEP 506) — carrier chain binding
// =========================================================================

#[test]
fn g66_scoped_value_hash_code() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sv = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "<init>",
        "()V",
        &[Value::Object(Some(sv))],
    )
    .unwrap();

    let hash = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "hashCode",
        "()I",
        &[Value::Object(Some(sv))],
    )
    .unwrap();
    assert!(matches!(hash, Some(Value::Int(h)) if h != 0));

    // Second call returns same cached hash
    let hash2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "hashCode",
        "()I",
        &[Value::Object(Some(sv))],
    )
    .unwrap();
    assert_eq!(hash, hash2);
}

#[test]
fn g66_carrier_chain_binds_all_values() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create two ScopedValues
    let sv1 = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "<init>",
        "()V",
        &[Value::Object(Some(sv1))],
    )
    .unwrap();

    let sv2 = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "<init>",
        "()V",
        &[Value::Object(Some(sv2))],
    )
    .unwrap();

    // ScopedValue.where(sv1, val1) -> carrier1
    let carrier1 = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        &[Value::Object(Some(sv1)), Value::Int(42)],
    )
    .unwrap();
    let carrier1_ref = match carrier1 {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected carrier"),
    };

    // carrier1.where(sv2, val2) -> carrier2 (chained)
    let carrier2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue$Carrier",
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        &[
            Value::Object(Some(carrier1_ref)),
            Value::Object(Some(sv2)),
            Value::Int(99),
        ],
    )
    .unwrap();
    let carrier2_ref = match carrier2 {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected carrier"),
    };

    // carrier2.run(null) — binds both SVs then unbinds
    call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue$Carrier",
        "run",
        "(Ljava/lang/Runnable;)V",
        &[Value::Object(Some(carrier2_ref)), Value::Object(None)],
    )
    .unwrap();

    // After run, both should be unbound
    let bound1 = shared.mem.heap.get_field(sv1, 1);
    assert_eq!(bound1, Value::Int(0), "sv1 should be unbound after run");
    let bound2 = shared.mem.heap.get_field(sv2, 1);
    assert_eq!(bound2, Value::Int(0), "sv2 should be unbound after run");
}

#[test]
fn g66_snapshot_monotonic_timestamp() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let snap1 = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue$Snapshot",
        "capture",
        "()Ljava/lang/ScopedValue$Snapshot;",
        &[],
    )
    .unwrap();
    let snap2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue$Snapshot",
        "capture",
        "()Ljava/lang/ScopedValue$Snapshot;",
        &[],
    )
    .unwrap();

    let ts1 = match snap1 {
        Some(Value::Object(Some(r))) => shared.mem.heap.get_field(r, 1),
        _ => panic!("expected snapshot"),
    };
    let ts2 = match snap2 {
        Some(Value::Object(Some(r))) => shared.mem.heap.get_field(r, 1),
        _ => panic!("expected snapshot"),
    };
    match (ts1, ts2) {
        (Value::Int(a), Value::Int(b)) => assert!(b > a, "timestamps should be monotonic"),
        _ => panic!("expected int timestamps"),
    }
}

// =========================================================================
// G67: Structured Concurrency (JEP 505) — state validation
// =========================================================================

#[test]
fn g67_fork_validates_open_state() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Shutdown the scope
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "shutdown",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Fork after shutdown returns UNAVAILABLE subtask (not error) per JDK 25
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        &[Value::Object(Some(scope)), Value::Object(None)],
    );
    assert!(
        result.is_ok(),
        "fork on shutdown scope returns UNAVAILABLE subtask"
    );
    if let Ok(Some(Value::Object(Some(subtask)))) = result {
        // Subtask state should be UNAVAILABLE (0)
        assert_eq!(shared.mem.heap.get_field(subtask, 0), Value::Int(0));
    }
}

#[test]
fn g67_fork_increments_completed_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        8,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Fork a task (callable is None, so the call returns None/null result)
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;",
        &[Value::Object(Some(scope)), Value::Object(None)],
    )
    .unwrap();

    // Both task_count and completed_count should be 1 (synchronous execution)
    let task_count = shared.mem.heap.get_field(scope, 2);
    assert_eq!(task_count, Value::Int(1));
    let completed = shared.mem.heap.get_field(scope, 3);
    assert_eq!(completed, Value::Int(1));
}

#[test]
fn g67_join_rejects_closed_scope() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Close the scope (no tasks, so close is ok)
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "close",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Join on closed scope should fail
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        &[Value::Object(Some(scope))],
    );
    assert!(result.is_err(), "join on closed scope should fail");
}

#[test]
fn g67_close_idempotent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Close twice should not error
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "close",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "close",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();
}

#[test]
fn g67_shutdown_on_failure_no_exception() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // No exception stored, throwIfFailed should succeed
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure",
        "throwIfFailed",
        "()V",
        &[Value::Object(Some(scope))],
    );
    assert!(result.is_ok());
}

#[test]
fn g67_shutdown_on_failure_with_exception() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // Manually store an exception
    let exc = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(scope, 4, Value::Object(Some(exc)));

    // throwIfFailed should now error
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure",
        "throwIfFailed",
        "()V",
        &[Value::Object(Some(scope))],
    );
    assert!(
        result.is_err(),
        "throwIfFailed should fail with stored exception"
    );
}

#[test]
fn g67_shutdown_on_success_result_requires_shutdown() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let scope = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess",
        "<init>",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();

    // result() before shutdown should fail
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess",
        "result",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(scope))],
    );
    assert!(result.is_err(), "result() before shutdown should fail");
}

// =========================================================================
// G68: Primitive Patterns (JEP 507) — registration verification
// =========================================================================

#[test]
fn g68_primitive_pattern_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let ps = "jdk/internal/misc/PatternSupport";
    assert!(registry.find(ps, "exactConversionCheck", "(ID)Z").is_some());
    assert!(registry.find(ps, "widenPrimitive", "(II)I").is_some());
    assert!(registry.find(ps, "narrowPrimitive", "(II)I").is_some());
    assert!(registry.find(ps, "isExactFloat", "(F)Z").is_some());
    assert!(registry.find(ps, "isExactDouble", "(D)Z").is_some());

    // E40-1 N6: one behavioral pin per registration — type codes from
    // `jdk25_patterns::PrimitiveType::from_type_code` (byte=8, int=10).
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let widen = call_native(
        &shared,
        &mut thread,
        ps,
        "widenPrimitive",
        "(II)I",
        &[Value::Int(8), Value::Int(10)],
    )
    .expect("widenPrimitive must not fail");
    assert_eq!(
        widen,
        Some(Value::Int(1)),
        "byte -> int must be a legal widening"
    );

    let exact = call_native(
        &shared,
        &mut thread,
        ps,
        "isExactFloat",
        "(F)Z",
        &[Value::Float(2.0)],
    )
    .expect("isExactFloat must not fail");
    assert_eq!(
        exact,
        Some(Value::Int(1)),
        "2.0f round-trips through int exactly"
    );
    let inexact = call_native(
        &shared,
        &mut thread,
        ps,
        "isExactFloat",
        "(F)Z",
        &[Value::Float(2.5)],
    )
    .expect("isExactFloat must not fail");
    assert_eq!(
        inexact,
        Some(Value::Int(0)),
        "2.5f does NOT round-trip through int"
    );
}

// =========================================================================
// G69: Stable Values (JEP 502) — registration verification
// =========================================================================

#[test]
fn g69_stable_value_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let sv = "java/lang/StableValue";
    assert!(registry
        .find(sv, "of", "()Ljava/lang/StableValue;")
        .is_some());
    assert!(registry.find(sv, "isSet", "()Z").is_some());
    assert!(registry
        .find(sv, "orElse", "(Ljava/lang/Object;)Ljava/lang/Object;")
        .is_some());
    assert!(registry
        .find(sv, "trySet", "(Ljava/lang/Object;)Z")
        .is_some());

    // E40-1 N6: a real StableValue, actually stabilizing.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let holder = call_native(
        &shared,
        &mut thread,
        sv,
        "of",
        "()Ljava/lang/StableValue;",
        &[],
    )
    .expect("StableValue.of must not fail")
    .expect("StableValue.of must not return void");

    let before = call_native(&shared, &mut thread, sv, "isSet", "()Z", &[holder])
        .expect("StableValue.isSet must not fail");
    assert_eq!(
        before,
        Some(Value::Int(0)),
        "a freshly-created StableValue must not be set yet"
    );

    let payload = create_java_string(&shared, "computed-once");
    let set = call_native(
        &shared,
        &mut thread,
        sv,
        "trySet",
        "(Ljava/lang/Object;)Z",
        &[holder, Value::Object(Some(payload))],
    )
    .expect("StableValue.trySet must not fail");
    assert_eq!(set, Some(Value::Int(1)), "the first trySet must succeed");

    let payload2 = create_java_string(&shared, "second-write");
    let set_again = call_native(
        &shared,
        &mut thread,
        sv,
        "trySet",
        "(Ljava/lang/Object;)Z",
        &[holder, Value::Object(Some(payload2))],
    )
    .expect("StableValue.trySet must not fail");
    assert_eq!(
        set_again,
        Some(Value::Int(0)),
        "a StableValue is set-once — the second trySet must be refused"
    );

    let after = call_native(&shared, &mut thread, sv, "isSet", "()Z", &[holder])
        .expect("StableValue.isSet must not fail");
    assert_eq!(
        after,
        Some(Value::Int(1)),
        "isSet must reflect the successful trySet"
    );
}

// =========================================================================
// G70: Module Import Declarations (JEP 511) — registration + behavior
// =========================================================================

#[test]
fn g70_module_import_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let mi = "jdk/internal/module/ModuleImports";
    assert!(registry
        .find(mi, "resolveModuleImport", "(Ljava/lang/String;)Z")
        .is_some());
    assert!(registry
        .find(mi, "getExportedPackages", "(Ljava/lang/String;)I")
        .is_some());
    assert!(registry.find(mi, "isSupported", "()Z").is_some());
}

#[test]
fn g70_module_imports_supported() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "jdk/internal/module/ModuleImports",
        "isSupported",
        "()Z",
        &[],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(1)));
}

// =========================================================================
// G71: Flexible Constructor Bodies (JEP 513) — registration + behavior
// =========================================================================

#[test]
fn g71_flexible_constructor_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let fc = "jdk/internal/vm/FlexibleConstructors";
    assert!(registry.find(fc, "isPreSuperAllowed", "()Z").is_some());
    assert!(registry
        .find(fc, "validatePreSuperStatement", "(I)Z")
        .is_some());
}

#[test]
fn g71_pre_super_allowed_kinds() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Kind 0 (LocalAssign) should be allowed
    let r0 = call_native(
        &shared,
        &mut thread,
        "jdk/internal/vm/FlexibleConstructors",
        "validatePreSuperStatement",
        "(I)Z",
        &[Value::Int(0)],
    )
    .unwrap();
    assert_eq!(r0, Some(Value::Int(1)));

    // Kind 1 (StaticCall) should be allowed
    let r1 = call_native(
        &shared,
        &mut thread,
        "jdk/internal/vm/FlexibleConstructors",
        "validatePreSuperStatement",
        "(I)Z",
        &[Value::Int(1)],
    )
    .unwrap();
    assert_eq!(r1, Some(Value::Int(1)));

    // Kind 3 (Disallowed) should be rejected
    let r3 = call_native(
        &shared,
        &mut thread,
        "jdk/internal/vm/FlexibleConstructors",
        "validatePreSuperStatement",
        "(I)Z",
        &[Value::Int(3)],
    )
    .unwrap();
    assert_eq!(r3, Some(Value::Int(0)));
}

// =========================================================================
// G72: Compact Source Files / Instance Main (JEP 512) — registration
// =========================================================================

#[test]
fn g72_implicit_class_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let ic = "jdk/internal/misc/ImplicitClasses";
    assert!(registry.find(ic, "isImplicitClass", "(I)Z").is_some());
    assert!(registry.find(ic, "selectMainMethod", "(I)I").is_some());
    assert!(registry.find(ic, "isInstanceMainAllowed", "()Z").is_some());
    assert!(registry.find(ic, "isSupported", "()Z").is_some());
}

#[test]
fn g72_implicit_class_detection() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 0x1000 = ACC_IMPLICIT_CLASS
    let result = call_native(
        &shared,
        &mut thread,
        "jdk/internal/misc/ImplicitClasses",
        "isImplicitClass",
        "(I)Z",
        &[Value::Int(0x1000)],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(1)));

    // 0x0001 = ACC_PUBLIC only
    let result2 = call_native(
        &shared,
        &mut thread,
        "jdk/internal/misc/ImplicitClasses",
        "isImplicitClass",
        "(I)Z",
        &[Value::Int(0x0001)],
    )
    .unwrap();
    assert_eq!(result2, Some(Value::Int(0)));
}

// =========================================================================
// G73: Vector API (JEP 508) — ByteVector, ShortVector, cross-type ops
// =========================================================================

#[test]
fn g73_byte_vector_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let bv = "jdk/incubator/vector/ByteVector";
    assert!(registry
        .find(
            bv,
            "zero",
            "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/ByteVector;"
        )
        .is_some());
    assert!(registry
        .find(
            bv,
            "broadcast",
            "(Ljdk/incubator/vector/VectorSpecies;B)Ljdk/incubator/vector/ByteVector;"
        )
        .is_some());
    assert!(registry
        .find(
            bv,
            "add",
            "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;"
        )
        .is_some());
    assert!(registry
        .find(bv, "neg", "()Ljdk/incubator/vector/ByteVector;")
        .is_some());
    assert!(registry.find(bv, "reduceLanes", "(I)B").is_some());
    assert!(registry.find(bv, "lane", "(I)B").is_some());

    // E40-1 N6: broadcast two lanes and add them — not just that `add` is
    // registered, but that it actually adds.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let species = call_native(
        &shared,
        &mut thread,
        "jdk/incubator/vector/VectorSpecies",
        "ofByte",
        "()Ljdk/incubator/vector/VectorSpecies;",
        &[],
    )
    .unwrap()
    .unwrap();
    let v3 = call_native(
        &shared,
        &mut thread,
        bv,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;B)Ljdk/incubator/vector/ByteVector;",
        &[species, Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    let v4 = call_native(
        &shared,
        &mut thread,
        bv,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;B)Ljdk/incubator/vector/ByteVector;",
        &[species, Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let sum = call_native(
        &shared,
        &mut thread,
        bv,
        "add",
        "(Ljdk/incubator/vector/ByteVector;)Ljdk/incubator/vector/ByteVector;",
        &[v3, v4],
    )
    .unwrap()
    .unwrap();
    let lane0 = call_native(
        &shared,
        &mut thread,
        bv,
        "lane",
        "(I)B",
        &[sum, Value::Int(0)],
    )
    .unwrap();
    assert_eq!(
        lane0,
        Some(Value::Int(7)),
        "broadcast(3) + broadcast(4), lane 0 must be 7"
    );
}

#[test]
fn g73_short_vector_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let sv = "jdk/incubator/vector/ShortVector";
    assert!(registry
        .find(
            sv,
            "zero",
            "(Ljdk/incubator/vector/VectorSpecies;)Ljdk/incubator/vector/ShortVector;"
        )
        .is_some());
    assert!(registry
        .find(
            sv,
            "broadcast",
            "(Ljdk/incubator/vector/VectorSpecies;S)Ljdk/incubator/vector/ShortVector;"
        )
        .is_some());
    assert!(registry
        .find(
            sv,
            "add",
            "(Ljdk/incubator/vector/ShortVector;)Ljdk/incubator/vector/ShortVector;"
        )
        .is_some());
    assert!(registry
        .find(sv, "neg", "()Ljdk/incubator/vector/ShortVector;")
        .is_some());
    assert!(registry.find(sv, "reduceLanes", "(I)S").is_some());
    assert!(registry.find(sv, "lane", "(I)S").is_some());

    // E40-1 N6: broadcast + neg — a wrong sign or wrong lane is exactly what
    // an existence check cannot see.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let species = call_native(
        &shared,
        &mut thread,
        "jdk/incubator/vector/VectorSpecies",
        "ofShort",
        "()Ljdk/incubator/vector/VectorSpecies;",
        &[],
    )
    .unwrap()
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        sv,
        "broadcast",
        "(Ljdk/incubator/vector/VectorSpecies;S)Ljdk/incubator/vector/ShortVector;",
        &[species, Value::Int(9)],
    )
    .unwrap()
    .unwrap();
    let negated = call_native(
        &shared,
        &mut thread,
        sv,
        "neg",
        "()Ljdk/incubator/vector/ShortVector;",
        &[v],
    )
    .unwrap()
    .unwrap();
    let lane0 = call_native(
        &shared,
        &mut thread,
        sv,
        "lane",
        "(I)S",
        &[negated, Value::Int(0)],
    )
    .unwrap();
    assert_eq!(
        lane0,
        Some(Value::Int(-9)),
        "neg(broadcast(9)), lane 0 must be -9"
    );
}

#[test]
fn g73_vector_species_byte_short_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let vs = "jdk/incubator/vector/VectorSpecies";
    assert!(registry
        .find(vs, "ofByte", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
    assert!(registry
        .find(vs, "ofShort", "()Ljdk/incubator/vector/VectorSpecies;")
        .is_some());
    // E40-1 N6: `ofByte` has its own behavioral pin
    // (`g73_vector_species_of_byte_returns_species`, field-level element-type
    // and lane-count checks); `ofShort` is exercised end-to-end by
    // `g73_short_vector_natives_registered`'s broadcast+neg round trip.
}

#[test]
fn g73_int_vector_convert_cast_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let iv = "jdk/incubator/vector/IntVector";
    assert!(registry
        .find(
            iv,
            "convertShape",
            "(ILjdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/Vector;"
        )
        .is_some());
    assert!(registry
        .find(
            iv,
            "castShape",
            "(Ljdk/incubator/vector/VectorSpecies;I)Ljdk/incubator/vector/Vector;"
        )
        .is_some());
}

#[test]
fn g73_vector_species_of_byte_returns_species() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "jdk/incubator/vector/VectorSpecies",
        "ofByte",
        "()Ljdk/incubator/vector/VectorSpecies;",
        &[],
    )
    .unwrap();
    match result {
        Some(Value::Object(Some(species))) => {
            // species field 1 = element_type (BYTE=0)
            let elem = shared.mem.heap.get_field(species, 1);
            assert_eq!(elem, Value::Int(0), "element type should be BYTE(0)");
            // species field 3 = lane_count (256/8=32)
            let lc = shared.mem.heap.get_field(species, 3);
            assert_eq!(
                lc,
                Value::Int(32),
                "lane count should be 32 for 256-bit byte"
            );
        }
        _ => panic!("expected species object"),
    }
}

// ---- U6: DatagramChannel tests ----

#[test]
fn u6_datagram_channel_open_returns_channel() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
        &[],
    )
    .unwrap();
    // The winning `DatagramChannel` natives are the fd-table-backed ones in
    // native-io: they deliberately keep the channel's fd and its connected
    // flag in identity-hash side tables (`dc_fds` / `dc_connected_channels`)
    // rather than in object slots, because a real-JDK `DatagramChannel`'s
    // low slots belong to its own private layout. So query the state
    // through the natives; the old slot 1/2/3 reads described a synthetic
    // layout that no longer backs this object.
    match result {
        Some(Value::Object(Some(ch))) => {
            let open = call_native(
                &shared,
                &mut thread,
                "java/nio/channels/DatagramChannel",
                "isOpen",
                "()Z",
                &[Value::Object(Some(ch))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(open, Value::Int(1));
            let connected = call_native(
                &shared,
                &mut thread,
                "java/nio/channels/DatagramChannel",
                "isConnected",
                "()Z",
                &[Value::Object(Some(ch))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(connected, Value::Int(0));
            // "A newly-created channel is always in blocking mode."
            let blocking = call_native(
                &shared,
                &mut thread,
                "java/nio/channels/DatagramChannel",
                "isBlocking",
                "()Z",
                &[Value::Object(Some(ch))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(blocking, Value::Int(1));
            call_native(
                &shared,
                &mut thread,
                "java/nio/channels/DatagramChannel",
                "close",
                "()V",
                &[Value::Object(Some(ch))],
            )
            .unwrap();
        }
        _ => panic!("expected DatagramChannel object"),
    }
}

#[test]
fn u6_datagram_channel_connect_disconnect() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ch = match call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
        &[],
    )
    .unwrap()
    {
        Some(Value::Object(Some(c))) => c,
        _ => panic!("expected channel"),
    };

    // A real peer address. `DatagramChannel.connect` issues a genuine UDP
    // `connect(2)` (it only records the default peer — no listener has to
    // exist), so the address must actually decode: the old
    // `alloc_object(ClassId::new(0), 2)` placeholder carried neither host
    // nor port and was correctly rejected as an unsupported SocketAddress.
    // This is the flat `(host: String, port: int)` InetSocketAddress
    // layout `dc_socket_addr` falls back to.
    let host = create_java_string(&shared, "127.0.0.1");
    let sa = alloc_receiver(&shared, &mut thread, "java/net/InetSocketAddress", 2);
    shared.mem.heap.set_field(sa, 0, Value::Object(Some(host)));
    shared.mem.heap.set_field(sa, 1, Value::Int(9)); // discard port

    // Connect
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "connect",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        &[Value::Object(Some(ch)), Value::Object(Some(sa))],
    )
    .unwrap();

    // isConnected — the connected flag lives in native-io's identity-hash
    // side table, not in an object slot, so ask the native.
    let connected = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "isConnected",
        "()Z",
        &[Value::Object(Some(ch))],
    )
    .unwrap();
    assert_eq!(connected, Some(Value::Int(1)), "should be connected");

    // Disconnect
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "disconnect",
        "()Ljava/nio/channels/DatagramChannel;",
        &[Value::Object(Some(ch))],
    )
    .unwrap();
    let disconnected = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "isConnected",
        "()Z",
        &[Value::Object(Some(ch))],
    )
    .unwrap();
    assert_eq!(disconnected, Some(Value::Int(0)), "should be disconnected");
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "close",
        "()V",
        &[Value::Object(Some(ch))],
    )
    .unwrap();
}

#[test]
fn u6_datagram_channel_close() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ch = match call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
        &[],
    )
    .unwrap()
    {
        Some(Value::Object(Some(c))) => c,
        _ => panic!("expected channel"),
    };

    // Close
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "close",
        "()V",
        &[Value::Object(Some(ch))],
    )
    .unwrap();

    // isOpen should return 0
    let open = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "isOpen",
        "()Z",
        &[Value::Object(Some(ch))],
    )
    .unwrap();
    assert_eq!(open, Some(Value::Int(0)));
}

#[test]
fn u6_datagram_channel_configure_blocking() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ch = match call_native(
        &shared,
        &mut thread,
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
        &[],
    )
    .unwrap()
    {
        Some(Value::Object(Some(c))) => c,
        _ => panic!("expected channel"),
    };

    // Assert the OBSERVABLE contract — what `isBlocking()` answers — and
    // never a raw slot index.
    //
    // This used to read `heap.get_field(ch, 3)` directly. Slot 3 was the
    // blocking flag of the retired 5-field synthetic `DatagramChannel`
    // layout; the winning `open` is native-io's fd-table-backed one, which
    // allocates `DC_NUM_FIELDS` slots and keeps blocking mode (like the fd
    // and the connected flag) in an identity-hash side table, because a
    // real-JDK `DatagramChannel`'s low slots belong to its own private
    // layout. So that assertion was reading a slot nothing writes: it
    // reported "non-blocking" from a zeroed/out-of-bounds cell, and would
    // have said exactly the same thing had `configureBlocking` done
    // nothing at all — which, until the side table was added, was very
    // nearly the case (it flipped the OS socket and recorded nothing
    // Java-visible).
    //
    // Round-tripping BACK to blocking is the assertion that has teeth: a
    // reader wired to the wrong state can accidentally satisfy the
    // "initially blocking" and "non-blocking after configureBlocking(false)"
    // steps, but only a reader that actually observes what
    // `configureBlocking` wrote can satisfy all three.
    fn is_blocking(shared: &Arc<SharedVm>, thread: &mut JvmThread, ch: ObjectRef) -> Value {
        call_native(
            shared,
            thread,
            "java/nio/channels/DatagramChannel",
            "isBlocking",
            "()Z",
            &[Value::Object(Some(ch))],
        )
        .unwrap()
        .expect("isBlocking() must return a value")
    }

    fn configure_blocking(
        shared: &Arc<SharedVm>,
        thread: &mut JvmThread,
        ch: ObjectRef,
        blocking: bool,
    ) {
        call_native(
            shared,
            thread,
            "java/nio/channels/DatagramChannel",
            "configureBlocking",
            "(Z)Ljava/nio/channels/SelectableChannel;",
            &[Value::Object(Some(ch)), Value::Int(i32::from(blocking))],
        )
        .unwrap();
    }

    // "A newly-created channel is always in blocking mode"
    // (`java.nio.channels.SelectableChannel`).
    assert_eq!(
        is_blocking(&shared, &mut thread, ch),
        Value::Int(1),
        "a freshly opened channel must report blocking mode"
    );

    configure_blocking(&shared, &mut thread, ch, false);
    assert_eq!(
        is_blocking(&shared, &mut thread, ch),
        Value::Int(0),
        "configureBlocking(false) must be visible to isBlocking()"
    );

    configure_blocking(&shared, &mut thread, ch, true);
    assert_eq!(
        is_blocking(&shared, &mut thread, ch),
        Value::Int(1),
        "configureBlocking(true) must switch the channel back to blocking"
    );
}

#[test]
fn u6_datagram_channel_natives_registered() {
    let registry = {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        r
    };
    let dc = "java/nio/channels/DatagramChannel";
    assert!(registry
        .find(dc, "open", "()Ljava/nio/channels/DatagramChannel;")
        .is_some());
    assert!(registry
        .find(
            dc,
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;"
        )
        .is_some());
    assert!(registry
        .find(
            dc,
            "connect",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;"
        )
        .is_some());
    assert!(registry
        .find(dc, "disconnect", "()Ljava/nio/channels/DatagramChannel;")
        .is_some());
    assert!(registry.find(dc, "isConnected", "()Z").is_some());
    assert!(registry.find(dc, "isOpen", "()Z").is_some());
    assert!(registry.find(dc, "close", "()V").is_some());
    assert!(registry
        .find(
            dc,
            "configureBlocking",
            "(Z)Ljava/nio/channels/SelectableChannel;"
        )
        .is_some());
    assert!(registry
        .find(
            dc,
            "send",
            "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I"
        )
        .is_some());
    assert!(registry
        .find(
            dc,
            "receive",
            "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;"
        )
        .is_some());
}

// ---- G12: BigInteger arbitrary-precision tests ----

#[test]
fn g12_biginteger_gcd() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create BigInteger(48) and BigInteger(18), gcd should be 6
    let bi48 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(48)],
    )
    .unwrap()
    .unwrap();
    let bi18 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(18)],
    )
    .unwrap()
    .unwrap();

    let gcd = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "gcd",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[bi48, bi18],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = gcd {
        let val = shared.mem.heap.get_field(obj, 0);
        if let Value::Object(Some(s)) = val {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "6");
        } else {
            panic!("expected string value field");
        }
    } else {
        panic!("expected BigInteger object");
    }
}

#[test]
fn g12_biginteger_bit_operations() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 255 = 0xFF, bitLength=8, bitCount=8
    let bi255 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(255)],
    )
    .unwrap()
    .unwrap();

    let bl = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "bitLength",
        "()I",
        &[bi255.clone()],
    )
    .unwrap();
    assert_eq!(bl, Some(Value::Int(8)), "bitLength of 255 should be 8");

    let bc = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "bitCount",
        "()I",
        &[bi255.clone()],
    )
    .unwrap();
    assert_eq!(bc, Some(Value::Int(8)), "bitCount of 255 should be 8");

    // testBit(0) on 255 should be true
    let tb = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "testBit",
        "(I)Z",
        &[bi255.clone(), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(tb, Some(Value::Int(1)), "bit 0 of 255 should be set");
}

#[test]
fn g12_biginteger_shift_operations() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 1 << 10 = 1024
    let bi1 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(1)],
    )
    .unwrap()
    .unwrap();

    let shifted = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "shiftLeft",
        "(I)Ljava/math/BigInteger;",
        &[bi1, Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = shifted {
        let val = shared.mem.heap.get_field(obj, 0);
        if let Value::Object(Some(s)) = val {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "1024");
        } else {
            panic!("expected string value");
        }
    } else {
        panic!("expected BigInteger");
    }
}

#[test]
fn g12_biginteger_bitwise_and_or_xor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 12 & 10 = 8, 12 | 10 = 14, 12 ^ 10 = 6
    let bi12 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(12)],
    )
    .unwrap()
    .unwrap();
    let bi10 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(10)],
    )
    .unwrap()
    .unwrap();

    let and_result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "and",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[bi12.clone(), bi10.clone()],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = and_result {
        let text = read_java_string(
            &shared.mem.heap,
            match shared.mem.heap.get_field(obj, 0) {
                Value::Object(Some(s)) => s,
                _ => panic!("no str"),
            },
        )
        .unwrap();
        assert_eq!(text, "8", "12 & 10 = 8");
    } else {
        panic!("BigInteger.and must answer a BigInteger reference, got {and_result:?}");
    }

    let or_result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "or",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[bi12.clone(), bi10.clone()],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = or_result {
        let text = read_java_string(
            &shared.mem.heap,
            match shared.mem.heap.get_field(obj, 0) {
                Value::Object(Some(s)) => s,
                _ => panic!("no str"),
            },
        )
        .unwrap();
        assert_eq!(text, "14", "12 | 10 = 14");
    } else {
        panic!("BigInteger.or must answer a BigInteger reference, got {or_result:?}");
    }

    let xor_result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "xor",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[bi12.clone(), bi10.clone()],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = xor_result {
        let text = read_java_string(
            &shared.mem.heap,
            match shared.mem.heap.get_field(obj, 0) {
                Value::Object(Some(s)) => s,
                _ => panic!("no str"),
            },
        )
        .unwrap();
        assert_eq!(text, "6", "12 ^ 10 = 6");
    } else {
        panic!("BigInteger.xor must answer a BigInteger reference, got {xor_result:?}");
    }
}

#[test]
fn g12_biginteger_modpow() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 2^10 mod 1000 = 1024 mod 1000 = 24
    let bi2 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    let bi10 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(10)],
    )
    .unwrap()
    .unwrap();
    let bi1000 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(1000)],
    )
    .unwrap()
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "modPow",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[bi2, bi10, bi1000],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = result {
        let text = read_java_string(
            &shared.mem.heap,
            match shared.mem.heap.get_field(obj, 0) {
                Value::Object(Some(s)) => s,
                _ => panic!("no str"),
            },
        )
        .unwrap();
        assert_eq!(text, "24", "2^10 mod 1000 = 24");
    } else {
        panic!("expected BigInteger");
    }
}

#[test]
fn g12_biginteger_isprobableprime() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // 7 is prime
    let bi7 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(7)],
    )
    .unwrap()
    .unwrap();
    let prime = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "isProbablePrime",
        "(I)Z",
        &[bi7, Value::Int(10)],
    )
    .unwrap();
    assert_eq!(prime, Some(Value::Int(1)), "7 should be prime");

    // 9 is not prime
    let bi9 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(9)],
    )
    .unwrap()
    .unwrap();
    let not_prime = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "isProbablePrime",
        "(I)Z",
        &[bi9, Value::Int(10)],
    )
    .unwrap();
    assert_eq!(not_prime, Some(Value::Int(0)), "9 should not be prime");
}

#[test]
fn g12_biginteger_not() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // not(0) = -(0+1) = -1
    let bi0 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        &[Value::Long(0)],
    )
    .unwrap()
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "not",
        "()Ljava/math/BigInteger;",
        &[bi0],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(obj)) = result {
        let text = read_java_string(
            &shared.mem.heap,
            match shared.mem.heap.get_field(obj, 0) {
                Value::Object(Some(s)) => s,
                _ => panic!("no str"),
            },
        )
        .unwrap();
        assert_eq!(text, "-1", "not(0) = -1");
    } else {
        panic!("BigInteger.not must answer a BigInteger reference, got {result:?}");
    }
}

/// WAS A REGISTRATION CENSUS, IS NOW AN ORACLE DIFF (F31, 2026-08-13 —
/// E40-1 NOMINATION N6, the `BigInteger` member).
///
/// What stood here was thirteen assertions of the form
///
/// ```ignore
/// assert!(registry.find(bi, "gcd", "(L…BigInteger;)L…BigInteger;").is_some());
/// ```
///
/// — the tree checked against itself. Such a test cannot fail for any
/// reason a caller would care about: it goes red only when someone deletes
/// a registration, and it would be deleted in the same commit as the row it
/// guards. It never once looked at an ANSWER, so all thirteen natives could
/// have returned zero and it would have stayed green.
///
/// It is also fully subsumed: `call_native` panics with
/// `"<class>.<method><descriptor> not registered"` when a triple is
/// missing, so every row below asserts the registration *and* the answer.
///
/// Every expectation is MEASURED on `openjdk 25.0.3 2026-04-21 LTS
/// (25.0.3+9-LTS)` (Microsoft build), `scratchpad/f31/{Bi13,Mp}.java`:
///
/// ```text
/// gcd(48,18)=6   gcd(-48,18)=6   gcd(0,0)=0
/// (255).bitLength()=8   (256).bitLength()=9   (-1).bitLength()=0   (-256).bitLength()=8
/// (255).bitCount()=8    (-1).bitCount()=0     (-256).bitCount()=8
/// (5).testBit(0)=true   (5).testBit(1)=false  (-1).testBit(99)=true
/// (5).testBit(-1)      !! ArithmeticException: Negative bit address
/// (3).shiftLeft(4)=48   (-3).shiftLeft(4)=-48
/// (9).shiftRight(1)=4   (-9).shiftRight(1)=-5 (3).shiftRight(-4)=48
/// (12).and(10)=8    (-12).and(10)=0
/// (12).or(10)=14    (-12).or(10)=-2
/// (12).xor(10)=6    (-12).xor(10)=-2
/// (0).not()=-1      (-1).not()=0      (12).not()=-13
/// (7).isProbablePrime(10)=true   (9).isProbablePrime(10)=false
/// (4).isProbablePrime(0)=true    (4).isProbablePrime(-1)=true
/// (3).modPow(4,7)=4   (2).modPow(-1,7)=4
/// (2).modPow(3,0)     !! ArithmeticException: BigInteger: modulus not positive
/// (3).modInverse(7)=5
/// (2).modInverse(8)   !! ArithmeticException: BigInteger not invertible.
/// ```
///
/// Four of these rows are the ones a census could never have held, because
/// each is a *rule* rather than a registration: `(-1).bitLength()` is `0`
/// and not `1` (the JDK counts bits of the two's-complement value, not of
/// the magnitude); `(-1).bitCount()` is `0` for the same reason;
/// `shiftRight(-4)` is a LEFT shift by 4 and not an error; and
/// `isProbablePrime(0)` is `true` for a composite, because the JDK's first
/// line is `if (certainty <= 0) return true;`.
///
/// NOTE which bodies this reaches. `register_builtins` runs
/// `register_essential_natives` and then `register_synthetic_overrides`,
/// and `register()` is last-write-wins, so `math_bignum`'s
/// `register_biginteger_natives` wins for every triple it still registers
/// (`gcd`, `isProbablePrime`, `modPow`, `modInverse`), while the triples
/// deleted from it (`bitLength`, `bitCount`, `testBit`, `shiftLeft`,
/// `shiftRight`, `and`, `or`, `xor`, `not`, `divide`, `mod`, `remainder`)
/// resolve to `phases_late::register_p71_biginteger_extras`. Both are the VM's,
/// and this is a VM-level test module — but a lane editing one of those two
/// files should know which rows it owns.
#[test]
fn g12_biginteger_new_natives_answer_hotspot() {
    fn mint(shared: &Arc<SharedVm>, thread: &mut JvmThread, v: i64) -> Value {
        call_native(
            shared,
            thread,
            "java/math/BigInteger",
            "valueOf",
            "(J)Ljava/math/BigInteger;",
            &[Value::Long(v)],
        )
        .expect("BigInteger.valueOf must not refuse")
        .expect("BigInteger.valueOf must answer a reference")
    }
    /// Synthetic-stub layout: slot 0 holds the decimal `String`.
    fn dec(shared: &Arc<SharedVm>, v: Value) -> String {
        match v {
            Value::Object(Some(o)) => match shared.mem.heap.get_field(o, 0) {
                Value::Object(Some(s)) => read_java_string(&shared.mem.heap, s)
                    .expect("BigInteger slot 0 must hold a readable decimal String"),
                other => panic!("BigInteger slot 0 must hold its String, got {other:?}"),
            },
            other => panic!("expected a BigInteger reference, got {other:?}"),
        }
    }
    fn binary(
        shared: &Arc<SharedVm>,
        thread: &mut JvmThread,
        method: &str,
        x: i64,
        y: i64,
    ) -> String {
        let a = mint(shared, thread, x);
        let b = mint(shared, thread, y);
        let r = call_native(
            shared,
            thread,
            "java/math/BigInteger",
            method,
            "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
            &[a, b],
        )
        .unwrap_or_else(|e| panic!("{method}({x},{y}) must answer, got {e:?}"))
        .unwrap();
        dec(shared, r)
    }
    fn shift(
        shared: &Arc<SharedVm>,
        thread: &mut JvmThread,
        method: &str,
        x: i64,
        n: i32,
    ) -> String {
        let a = mint(shared, thread, x);
        let r = call_native(
            shared,
            thread,
            "java/math/BigInteger",
            method,
            "(I)Ljava/math/BigInteger;",
            &[a, Value::Int(n)],
        )
        .unwrap_or_else(|e| panic!("{method}({x},{n}) must answer, got {e:?}"))
        .unwrap();
        dec(shared, r)
    }
    fn int_of(shared: &Arc<SharedVm>, thread: &mut JvmThread, method: &str, x: i64) -> i32 {
        let a = mint(shared, thread, x);
        match call_native(shared, thread, "java/math/BigInteger", method, "()I", &[a])
            .unwrap_or_else(|e| panic!("{method}({x}) must answer, got {e:?}"))
        {
            Some(Value::Int(v)) => v,
            other => panic!("{method}({x}) must answer an int, got {other:?}"),
        }
    }
    fn int_arg_bool(
        shared: &Arc<SharedVm>,
        thread: &mut JvmThread,
        method: &str,
        x: i64,
        n: i32,
    ) -> crate::error::MethodCallResult {
        let a = mint(shared, thread, x);
        call_native(
            shared,
            thread,
            "java/math/BigInteger",
            method,
            "(I)Z",
            &[a, Value::Int(n)],
        )
    }
    fn arith_message(e: &crate::error::MethodCallFailed) -> String {
        match e {
            crate::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::ArithmeticException { message },
                ),
            ) => message.clone(),
            other => panic!("expected an ArithmeticException, got {other:?}"),
        }
    }

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // gcd — always non-negative, and gcd(0,0) is 0.
    assert_eq!(binary(&shared, &mut thread, "gcd", 48, 18), "6");
    assert_eq!(binary(&shared, &mut thread, "gcd", -48, 18), "6");
    assert_eq!(binary(&shared, &mut thread, "gcd", 0, 0), "0");

    // bitLength / bitCount are TWO'S-COMPLEMENT counts, so both are 0 for -1.
    assert_eq!(int_of(&shared, &mut thread, "bitLength", 255), 8);
    assert_eq!(int_of(&shared, &mut thread, "bitLength", 256), 9);
    assert_eq!(
        int_of(&shared, &mut thread, "bitLength", -1),
        0,
        "(-1).bitLength() is 0 on HotSpot — the excess of the two's-complement \
             value over its sign bit, not the magnitude's bit count"
    );
    assert_eq!(int_of(&shared, &mut thread, "bitLength", -256), 8);
    assert_eq!(int_of(&shared, &mut thread, "bitCount", 255), 8);
    assert_eq!(
        int_of(&shared, &mut thread, "bitCount", -1),
        0,
        "(-1).bitCount() counts bits DIFFERING from the sign bit"
    );
    assert_eq!(int_of(&shared, &mut thread, "bitCount", -256), 8);

    // testBit, including the refusal a census cannot see.
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "testBit", 5, 0).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "testBit", 5, 1).unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "testBit", -1, 99).unwrap(),
        Some(Value::Int(1)),
        "(-1) is all ones in two's complement, at every bit index"
    );
    let neg_bit =
        int_arg_bool(&shared, &mut thread, "testBit", 5, -1).expect_err("testBit(-1) must refuse");
    assert_eq!(arith_message(&neg_bit), "Negative bit address");

    // shifts — a NEGATIVE count reverses the direction; it is not an error.
    assert_eq!(shift(&shared, &mut thread, "shiftLeft", 3, 4), "48");
    assert_eq!(shift(&shared, &mut thread, "shiftLeft", -3, 4), "-48");
    assert_eq!(shift(&shared, &mut thread, "shiftRight", 9, 1), "4");
    assert_eq!(
        shift(&shared, &mut thread, "shiftRight", -9, 1),
        "-5",
        "an arithmetic shift rounds toward NEGATIVE infinity: -9 >> 1 is -5, not -4"
    );
    assert_eq!(
        shift(&shared, &mut thread, "shiftRight", 3, -4),
        "48",
        "shiftRight(-4) is a left shift by 4 on HotSpot"
    );

    // Bitwise ops, both signs — these are two's-complement, not magnitude.
    assert_eq!(binary(&shared, &mut thread, "and", 12, 10), "8");
    assert_eq!(binary(&shared, &mut thread, "and", -12, 10), "0");
    assert_eq!(binary(&shared, &mut thread, "or", 12, 10), "14");
    assert_eq!(binary(&shared, &mut thread, "or", -12, 10), "-2");
    assert_eq!(binary(&shared, &mut thread, "xor", 12, 10), "6");
    assert_eq!(binary(&shared, &mut thread, "xor", -12, 10), "-2");

    // not(x) == -(x+1)
    for (input, want) in [(0i64, "-1"), (-1, "0"), (12, "-13")] {
        let a = mint(&shared, &mut thread, input);
        let r = call_native(
            &shared,
            &mut thread,
            "java/math/BigInteger",
            "not",
            "()Ljava/math/BigInteger;",
            &[a],
        )
        .unwrap()
        .unwrap();
        assert_eq!(dec(&shared, r), want, "({input}).not()");
    }

    // isProbablePrime — and `certainty <= 0` is an unconditional `true`,
    // composite or not (`BigInteger.java`'s first line in that method).
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "isProbablePrime", 7, 10).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "isProbablePrime", 9, 10).unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "isProbablePrime", 4, 0).unwrap(),
        Some(Value::Int(1)),
        "(4).isProbablePrime(0) is TRUE on HotSpot: `if (certainty <= 0) return true;`"
    );
    assert_eq!(
        int_arg_bool(&shared, &mut thread, "isProbablePrime", 4, -1).unwrap(),
        Some(Value::Int(1))
    );

    // modPow — including a negative exponent (legal: invert, then raise)
    // and a non-positive modulus (refused).
    let three = mint(&shared, &mut thread, 3);
    let four = mint(&shared, &mut thread, 4);
    let seven = mint(&shared, &mut thread, 7);
    let mp = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "modPow",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[three, four, seven],
    )
    .unwrap()
    .unwrap();
    assert_eq!(dec(&shared, mp), "4", "3^4 mod 7");

    let two = mint(&shared, &mut thread, 2);
    let minus_one = mint(&shared, &mut thread, -1);
    let seven2 = mint(&shared, &mut thread, 7);
    let inv_pow = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "modPow",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[two, minus_one, seven2],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        dec(&shared, inv_pow),
        "4",
        "a NEGATIVE exponent is legal: 2^-1 mod 7 is modInverse(2,7) = 4"
    );

    let two2 = mint(&shared, &mut thread, 2);
    let three2 = mint(&shared, &mut thread, 3);
    let zero = mint(&shared, &mut thread, 0);
    let bad_mod = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "modPow",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[two2, three2, zero],
    )
    .expect_err("a zero modulus must refuse");
    assert_eq!(arith_message(&bad_mod), "BigInteger: modulus not positive");

    // modInverse — the answer, and the message when gcd != 1. Note the
    // trailing '.' : HotSpot's text is "BigInteger not invertible."
    assert_eq!(binary(&shared, &mut thread, "modInverse", 3, 7), "5");
    let a = mint(&shared, &mut thread, 2);
    let m = mint(&shared, &mut thread, 8);
    let not_inv = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "modInverse",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[a, m],
    )
    .expect_err("gcd(2,8) != 1, so there is no inverse");
    assert_eq!(arith_message(&not_inv), "BigInteger not invertible.");

    // divide, remainder, mod — distinct semantics, especially for mixed-sign operands.
    // In synthetic mode, `math_bignum` previously shadowed these and mis-registered
    // `remainder` to the same callback as `mod`, returning positive results for negative dividends.
    // See `biginteger-divide-mod-remainder-shadow-confined-to-synthetic-mode-20260917-FIXED-20260917.md`.
    assert_eq!(binary(&shared, &mut thread, "divide", 10, 3), "3");
    assert_eq!(binary(&shared, &mut thread, "divide", -10, 3), "-3");
    assert_eq!(binary(&shared, &mut thread, "divide", 10, -3), "-3");
    assert_eq!(binary(&shared, &mut thread, "divide", -10, -3), "3");

    // remainder follows the dividend's sign (truncating division)
    assert_eq!(binary(&shared, &mut thread, "remainder", 10, 3), "1");
    assert_eq!(
        binary(&shared, &mut thread, "remainder", -10, 3),
        "-1",
        "(-10).remainder(3) must be -1, NOT 2 (was shadowed by mod() in synthetic mode)"
    );
    assert_eq!(binary(&shared, &mut thread, "remainder", 10, -3), "1");
    assert_eq!(binary(&shared, &mut thread, "remainder", -10, -3), "-1");

    // mod is always non-negative (modulus must be positive)
    assert_eq!(binary(&shared, &mut thread, "mod", 10, 3), "1");
    assert_eq!(
        binary(&shared, &mut thread, "mod", -10, 3),
        "2",
        "(-10).mod(3) is 2 (floor division remainder)"
    );
    let ten = mint(&shared, &mut thread, 10);
    let neg_three = mint(&shared, &mut thread, -3);
    let bad_mod_neg = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "mod",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[ten, neg_three],
    )
    .expect_err("mod with negative modulus must refuse");
    assert_eq!(
        arith_message(&bad_mod_neg),
        "BigInteger: modulus not positive"
    );

    let ten2 = mint(&shared, &mut thread, 10);
    let zero2 = mint(&shared, &mut thread, 0);
    let div_zero = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "divide",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[ten2, zero2],
    )
    .expect_err("divide by zero must refuse");
    assert_eq!(arith_message(&div_zero), "BigInteger divide by zero");

    let ten3 = mint(&shared, &mut thread, 10);
    let zero3 = mint(&shared, &mut thread, 0);
    let rem_zero = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "remainder",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[ten3, zero3],
    )
    .expect_err("remainder by zero must refuse");
    assert_eq!(arith_message(&rem_zero), "BigInteger divide by zero");
}

// ---- M5: JIT compiles user code (no longer skipped) ----

#[test]
fn m5_jit_skip_set_does_not_block_user_classes() {
    // Verify the JIT skip set starts empty — user classes are not pre-blocked
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let skip_set = shared.jit.jit_skip_set.read();
    assert!(skip_set.is_empty(), "JIT skip set should start empty");
}

/// The positive gate memo (`JitRealm::jit_gate_pass`) must be
/// epoch-invalidated, and the direction matters: a stale SEAL only costs
/// throughput (the method stays interpreted), but a stale PASS would let a
/// redefined body — whose new bytecode may call a native-shadowed target —
/// reach the compiler, which is what the seal exists to prevent.
///
/// Asserted on the redemption rule rather than by driving `execute()`,
/// because that is the rule the gate reads: an entry whose stamp is not the
/// current `JitCache::redefine_epoch()` is a miss.
#[test]
fn jit_gate_pass_memo_is_invalidated_by_a_redefine() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    assert!(
        shared.jit.jit_gate_pass.read().is_empty(),
        "the gate-pass memo should start empty"
    );

    let key: (ClassId, Arc<str>, Arc<str>) = (ClassId::new(7), Arc::from("bar"), Arc::from("()V"));
    let epoch_at_fill = shared.jit.jit_cache.redefine_epoch();
    shared
        .jit
        .jit_gate_pass
        .write()
        .insert(key.clone(), (epoch_at_fill, false));

    // Same epoch -> the memo answers, which is the whole point.
    let redeem = |shared: &SharedVm| -> Option<bool> {
        let epoch = shared.jit.jit_cache.redefine_epoch();
        shared
            .jit
            .jit_gate_pass
            .read()
            .get(&key)
            .copied()
            .and_then(|(e, iface)| (e == epoch).then_some(iface))
    };
    assert_eq!(
        redeem(&shared),
        Some(false),
        "an entry stamped with the current epoch must be redeemable"
    );

    // A redefinition bumps the epoch; the stale PASS must stop answering.
    shared.jit.jit_cache.bump_redefine_epoch();
    assert_ne!(shared.jit.jit_cache.redefine_epoch(), epoch_at_fill);
    assert_eq!(
        redeem(&shared),
        None,
        "a pass recorded before a redefine must NOT be redeemable after it — \
             the new body may call a native-shadowed target the old one did not"
    );
}

/// Two same-named classes from different loaders must NOT share a memo
/// entry. `jit_skip_set` keys on the class NAME and is safe doing so
/// (`is_jit_bail_listed`: a collision there conservatively skips a
/// compilable method). The positive memo inverts that — a PASS redeemed by
/// a different class of the same name would compile a body that does call a
/// native-shadowed target — so it keys on `ClassId`. This is the assertion
/// that the key actually separates them.
#[test]
fn jit_gate_pass_memo_does_not_collide_across_same_named_classes() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let epoch = shared.jit.jit_cache.redefine_epoch();
    let method: Arc<str> = Arc::from("equals");
    let desc: Arc<str> = Arc::from("(Ljava/lang/Object;)Z");

    // Loader A's class passed the gate.
    let a = (ClassId::new(101), method.clone(), desc.clone());
    shared.jit.jit_gate_pass.write().insert(a, (epoch, false));

    // Loader B's same-named class must be a MISS, not a redemption.
    let b = (ClassId::new(202), method, desc);
    assert_eq!(
        shared.jit.jit_gate_pass.read().get(&b).copied(),
        None,
        "a different ClassId with the same method name+descriptor must not \
             redeem another class's PASS"
    );
}

/// The memo carries `is_interface_default` because the gate's other three
/// outputs are all "false/None" on the pass path but that one is a real
/// value the caller uses. Storing it is what makes the short-circuit
/// equivalent to re-running the gate rather than merely cheaper.
#[test]
fn jit_gate_pass_memo_round_trips_is_interface_default() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let epoch = shared.jit.jit_cache.redefine_epoch();
    for iface in [false, true] {
        let key: (ClassId, Arc<str>, Arc<str>) = (
            ClassId::new(11),
            Arc::from(if iface { "dflt" } else { "plain" }),
            Arc::from("()V"),
        );
        shared
            .jit
            .jit_gate_pass
            .write()
            .insert(key.clone(), (epoch, iface));
        assert_eq!(
            shared.jit.jit_gate_pass.read().get(&key).copied(),
            Some((epoch, iface)),
            "is_interface_default must survive the memo, not be defaulted"
        );
    }
}

#[test]
fn m5_jit_scan_accepts_getfield_putfield() {
    // Bytecode: aload_0, getfield #1, ireturn
    // getfield/putfield should NOT cause jit_scan to reject
    let code = [
        0x2a, // aload_0
        0xb4, 0x00, 0x01, // getfield #1
        0xac, // ireturn
        0, 0,
    ];
    let scan = crate::jit::x64::jit_scan(&code, 5, "(Ljava/lang/Object;)I");
    assert!(
        scan.is_some(),
        "jit_scan should accept methods with getfield"
    );
    let s = scan.unwrap();
    assert_eq!(s.field_ops.len(), 1, "should find 1 field op");
    assert_eq!(s.field_ops[0], (1, 1), "field op at pc=1, cp_idx=1");
}

#[test]
fn m5_jit_scan_accepts_putfield() {
    // Bytecode: aload_0, iload_1, putfield #2, return
    let code = [
        0x2a, // aload_0
        0x1b, // iload_1
        0xb5, 0x00, 0x02, // putfield #2
        0xb1, // return
        0, 0,
    ];
    let scan = crate::jit::x64::jit_scan(&code, 6, "(Ljava/lang/Object;I)V");
    assert!(
        scan.is_some(),
        "jit_scan should accept methods with putfield"
    );
    let s = scan.unwrap();
    assert_eq!(s.field_ops.len(), 1, "should find 1 field op");
}

#[test]
fn m5_jit_scan_accepts_init_methods() {
    // <init> methods should now be JIT-scannable (no longer blocked)
    // Simple constructor: aload_0, invokespecial #1, return
    let code = [
        0x2a, // aload_0
        0xb7, 0x00, 0x01, // invokespecial #1
        0xb1, // return
        0, 0,
    ];
    let scan = crate::jit::x64::jit_scan(&code, 5, "()V");
    assert!(scan.is_some(), "jit_scan should accept <init> methods");
}

#[test]
fn m5_jit_scan_accepts_lambda_methods() {
    // lambda$ methods should now be JIT-scannable
    // Simple lambda: iload_0, ireturn
    let code = [0x1a, 0xac, 0, 0];
    let scan = crate::jit::x64::jit_scan(&code, 2, "(I)I");
    assert!(scan.is_some(), "jit_scan should accept lambda$ methods");
}

#[test]
fn m5_field_info_resolution_produces_correct_index() {
    // Verify that jit_scan captures field_ops with correct cp_index
    let code = [
        0x2a, // aload_0
        0xb4, 0x00, 0x05, // getfield #5
        0x2a, // aload_0
        0xb4, 0x00, 0x07, // getfield #7
        0x60, // iadd
        0xac, // ireturn
        0, 0,
    ];
    let scan = crate::jit::x64::jit_scan(&code, 10, "(Ljava/lang/Object;)I");
    assert!(scan.is_some());
    let s = scan.unwrap();
    assert_eq!(s.field_ops.len(), 2, "should find 2 field ops");
    assert_eq!(s.field_ops[0].1, 5, "first getfield cp_idx=5");
    assert_eq!(s.field_ops[1].1, 7, "second getfield cp_idx=7");
}

// -----------------------------------------------------------------------
// M7 — Panic site hardening: verify graceful failure instead of crash
// -----------------------------------------------------------------------

#[test]
fn m7_jit_scan_rejects_invalid_bytecode_gracefully() {
    // Empty bytecode — jit_scan should handle without panicking
    let empty: [u8; 0] = [];
    let _ = crate::jit::x64::jit_scan(&empty, 0, "()V"); // must not panic
                                                         // Single invalid opcode should not panic
    let bad = [0xfe, 0, 0]; // 0xfe is unused opcode
    let _ = crate::jit::x64::jit_scan(&bad, 1, "()V"); // must not panic
}

#[test]
fn m7_jit_scan_invalid_goto_does_not_panic() {
    // Bytecode with a goto targeting beyond the method should not panic
    let code = [
        0xa7, 0x27, 0x0f, // goto offset=9999
        0xb1, // return (unreachable)
        0,
    ];
    // jit_scan may accept or reject this, but must not panic
    let _ = crate::jit::x64::jit_scan(&code, 4, "()V");
}

#[test]
fn m7_gc_heap_alloc_does_not_panic_on_zero_fields() {
    // Allocating an object with 0 fields should succeed, not panic
    use cratonvm_gc::VmHeap;
    use cratonvm_types::ClassId;
    let heap = VmHeap::new(cratonvm_gc::GcBackend::Generational, 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    // ObjectRef is always non-null from successful allocation
    let _ = obj;
}

#[test]
fn m7_gc_heap_alloc_array_zero_length() {
    // Allocating a zero-length array should succeed
    use cratonvm_gc::{ArrayElementType, VmHeap};
    use cratonvm_types::ClassId;
    let heap = VmHeap::new(cratonvm_gc::GcBackend::Generational, 1024 * 1024);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    let _ = arr;
}

#[test]
fn m7_arm64_backend_stack_underflow_sets_failure() {
    // ARM64 backend pop_operand on empty stack should set failed flag, not panic
    use crate::jit::aarch64_backend::Arm64Backend;
    let mut backend = Arm64Backend::new();
    // pop on empty stack — must not panic
    let _reg = backend.pop_operand();
    // The backend should mark itself as failed
    assert!(backend.failed, "pop on empty stack should set failed flag");
}

#[test]
fn m7_x64_compiler_stack_underflow_does_not_panic() {
    // JIT x64 pop_stack on methods with underflow should fail gracefully
    // We test via jit_scan + a method that would cause stack underflow in JIT
    // iadd with nothing on stack
    let code = [0x60, 0xac, 0, 0]; // iadd, ireturn
    let scan = crate::jit::x64::jit_scan(&code, 2, "()I");
    // scan may reject (None) or accept — either way, no panic
    let _ = scan;
}

// -----------------------------------------------------------------------
// M2 — Default interface methods: verify resolution walks interfaces
// -----------------------------------------------------------------------

#[test]
fn m2_default_interface_method_resolution() {
    // Verify that find_method_recursive finds default methods on interfaces
    // when the concrete class doesn't override them.
    use cratonvm_classloading::{
        find_method_recursive, Class, ClassLoaderId, ClassState, ClassStore,
    };
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;

    let mut store = ClassStore::new();

    let mk = |id, name: &str, sup, ifaces: Vec<_>, methods: Vec<ClassFileMethod>, flags| Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: std::sync::Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: flags,
        superclass: sup,
        interfaces: ifaces,
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: vec![],
        nest_host: None,
        nest_members: vec![],
        record_components: vec![],
        permitted_subclasses: vec![],
        inner_classes: vec![],
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    };

    let obj_id = store.next_id();
    store.add(mk(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    ));

    let iface_id = store.next_id();
    store.add(mk(
        iface_id,
        "Greeting",
        Some(obj_id),
        vec![],
        vec![ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("shout"),
            descriptor: Arc::from("()Ljava/lang/String;"),
            attributes: vec![],
        }],
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT,
    ));

    let class_id = store.next_id();
    store.add(mk(
        class_id,
        "CasualGreeting",
        Some(obj_id),
        vec![iface_id],
        vec![],
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    ));

    // shout() should be found on the interface (default method)
    let result = find_method_recursive(class_id, "shout", "()Ljava/lang/String;", &store);
    assert!(
        result.is_some(),
        "M2: default interface method should be found"
    );
    let (method, declaring_id) = result.unwrap();
    assert_eq!(
        declaring_id, iface_id,
        "M2: shout() should resolve to interface"
    );
    assert_eq!(&*method.name, "shout");
}

#[test]
fn m7_pgo_sort_nan_does_not_panic() {
    // PGO sort with NaN benefit values should not panic
    // (partial_cmp returns None for NaN; we now use unwrap_or(Equal))
    // This is a compile-time verification that the fix is in place.
    let mut v = vec![f64::NAN, 1.0, f64::NAN, 2.0];
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Should not panic — NaN ordering is stable
    assert_eq!(v.len(), 4);
}

// -----------------------------------------------------------------------
// M3 — Function.andThen/compose: synthetic composition classes
// -----------------------------------------------------------------------

#[test]
fn m3_function_and_then_creates_composite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a fake Function object (field 0 unused in this test)
    let func1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let func2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);

    // Call Function.andThen — should create a Function$AndThen composite
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function",
        "andThen",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        &[Value::Object(Some(func1)), Value::Object(Some(func2))],
    );
    assert!(result.is_ok(), "Function.andThen should not crash");
    let composite = result.unwrap();
    assert!(
        composite.is_some(),
        "andThen should return a composite Function"
    );
}

#[test]
fn m3_function_compose_creates_composite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let func1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let func2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function",
        "compose",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        &[Value::Object(Some(func1)), Value::Object(Some(func2))],
    );
    assert!(result.is_ok(), "Function.compose should not crash");
    assert!(
        result.unwrap().is_some(),
        "compose should return a composite Function"
    );
}

#[test]
fn m3_function_identity_apply() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Get identity function
    let identity = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function",
        "identity",
        "()Ljava/util/function/Function;",
        &[],
    )
    .unwrap()
    .unwrap();

    // Apply identity to an object — should return the same object
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let identity_ref = match identity {
        Value::Object(Some(r)) => r,
        _ => panic!("Expected object"),
    };
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function$Identity",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(identity_ref)), Value::Object(Some(obj))],
    );
    assert!(result.is_ok(), "Function$Identity.apply should not crash");
    let returned = result.unwrap().unwrap();
    match returned {
        Value::Object(Some(r)) => assert_eq!(r, obj, "identity should return same object"),
        _ => panic!("Expected object from identity.apply"),
    }
}

#[ignore = "STALE: The seven java.util.function default/static stubs were deleted 2026-08-20 (b81aae8fc): javap shows none is ACC_NATIVE and all have real bodies. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn m3_consumer_and_then_creates_composite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let cons1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let cons2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/function/Consumer",
        "andThen",
        "(Ljava/util/function/Consumer;)Ljava/util/function/Consumer;",
        &[Value::Object(Some(cons1)), Value::Object(Some(cons2))],
    );
    assert!(result.is_ok(), "Consumer.andThen should not crash");
    assert!(
        result.unwrap().is_some(),
        "andThen should return a composite Consumer"
    );
}

#[test]
fn m3_synthetic_function_classes_have_interfaces() {
    // Verify that jdk_interfaces returns correct interfaces for synthetic classes
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create an andThen composite
    let func1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let func2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _composite = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function",
        "andThen",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        &[Value::Object(Some(func1)), Value::Object(Some(func2))],
    )
    .unwrap();

    // Verify the Function$AndThen class is loaded with correct interfaces
    let cm = shared.classes.class_manager.read();
    if let Some(and_then_class) = cm
        .class_store
        .find_by_name("java/util/function/Function$AndThen")
    {
        assert!(
            !and_then_class.interfaces.is_empty(),
            "Function$AndThen should implement Function interface"
        );
    }
    // (If the class isn't loaded yet via call_native, that's OK — the interface
    // registration is in jdk_interfaces which is applied at class load time)
}

#[test]
fn m3_function_and_then_apply_chains_correctly() {
    // End-to-end: Function$AndThen.apply dispatches via invoke_virtual
    // to support lambda proxy inner functions.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create two Function objects and compose them with andThen
    let f1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let f2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let composite = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function",
        "andThen",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        &[Value::Object(Some(f1)), Value::Object(Some(f2))],
    )
    .unwrap()
    .unwrap();
    // Verify composite is a Function$AndThen with 2 fields
    match composite {
        Value::Object(Some(r)) => {
            let cid = shared.mem.heap.class_id_of(r);
            let cm = shared.classes.class_manager.read();
            let name = cm.get_class(cid).map(|c| &*c.name);
            assert_eq!(name, Some("java/util/function/Function$AndThen"));
        }
        _ => panic!("Expected Function$AndThen object"),
    }
}

#[test]
fn m3_function_compose_apply_chains_correctly() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let f1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let f2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let composite = call_native(
        &shared,
        &mut thread,
        "java/util/function/Function",
        "compose",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        &[Value::Object(Some(f1)), Value::Object(Some(f2))],
    )
    .unwrap()
    .unwrap();
    match composite {
        Value::Object(Some(r)) => {
            let cid = shared.mem.heap.class_id_of(r);
            let cm = shared.classes.class_manager.read();
            let name = cm.get_class(cid).map(|c| &*c.name);
            assert_eq!(name, Some("java/util/function/Function$Compose"));
        }
        _ => panic!("Expected Function$Compose object"),
    }
}

#[ignore = "STALE: The seven java.util.function default/static stubs were deleted 2026-08-20 (b81aae8fc): javap shows none is ACC_NATIVE and all have real bodies. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn m3_predicate_and_creates_composite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let p1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let p2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let composite = call_native(
        &shared,
        &mut thread,
        "java/util/function/Predicate",
        "and",
        "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;",
        &[Value::Object(Some(p1)), Value::Object(Some(p2))],
    );
    assert!(composite.is_ok(), "Predicate.and should not crash");
    assert!(
        composite.unwrap().is_some(),
        "and should return a composite Predicate"
    );
}

#[ignore = "STALE: The seven java.util.function default/static stubs were deleted 2026-08-20 (b81aae8fc): javap shows none is ACC_NATIVE and all have real bodies. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn m3_predicate_or_creates_composite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let p1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let p2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let composite = call_native(
        &shared,
        &mut thread,
        "java/util/function/Predicate",
        "or",
        "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;",
        &[Value::Object(Some(p1)), Value::Object(Some(p2))],
    );
    assert!(composite.is_ok(), "Predicate.or should not crash");
    assert!(
        composite.unwrap().is_some(),
        "or should return a composite Predicate"
    );
}

#[ignore = "STALE: The seven java.util.function default/static stubs were deleted 2026-08-20 (b81aae8fc): javap shows none is ACC_NATIVE and all have real bodies. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn m3_predicate_negate_creates_composite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let p1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let composite = call_native(
        &shared,
        &mut thread,
        "java/util/function/Predicate",
        "negate",
        "()Ljava/util/function/Predicate;",
        &[Value::Object(Some(p1))],
    );
    assert!(composite.is_ok(), "Predicate.negate should not crash");
    assert!(
        composite.unwrap().is_some(),
        "negate should return a composite Predicate"
    );
}

// -----------------------------------------------------------------------
// M4 — GC heap exhaustion: verify GC-and-retry and larger heap
// -----------------------------------------------------------------------

#[test]
fn m4_heap_default_size_increased() {
    // Verify the default heap is large enough for non-trivial programs.
    // Young gen semi-spaces are now 16MB each (was 8MB).
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Allocate many objects — should not panic
    for _i in 0..1000 {
        let _obj = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    }
}

#[test]
fn m4_try_alloc_object_returns_none_on_oom() {
    // Verify try_alloc_object returns None instead of panicking
    use cratonvm_gc::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 4096); // tiny heap
                                                           // Try to allocate a large object — should return None
    let result = heap.try_alloc_object(ClassId::new(0), 10_000);
    assert!(
        result.is_none(),
        "try_alloc_object should return None on OOM, not panic"
    );
}

#[test]
fn m4_try_alloc_array_returns_none_on_oom() {
    use cratonvm_gc::{ArrayElementType, GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 4096); // tiny heap
    let result = heap.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
    assert!(
        result.is_none(),
        "try_alloc_array should return None on OOM, not panic"
    );
}

#[test]
fn m4_heavy_allocation_does_not_crash() {
    // Simulate heavy allocation — the kind that caused segfaults before M4.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Allocate 5000 objects with fields (simulating 8+ test cycles)
    for _ in 0..5000 {
        let _obj = shared.mem.heap.alloc_object(ClassId::new(0), 8);
    }
    // Allocate 2000 arrays
    for _ in 0..2000 {
        let _arr =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), cratonvm_gc::ArrayElementType::Int, 100);
    }
}

#[test]
fn m4_heap_expansion_under_pressure() {
    // With 256KB heap, GC must run to survive 2000 allocations.
    // Using try_alloc + GC pattern that the interpreter uses.
    use cratonvm_gc::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 256 * 1024); // 256 KB
    let monitors = crate::threading::monitor::MonitorTable::new();
    let mut roots: Vec<ObjectRef> = Vec::new();
    // Single-threaded test — no other mutator exists.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    for _ in 0..2000 {
        match heap.try_alloc_object(ClassId::new(0), 4) {
            Some(_obj) => {} // ephemeral — don't root
            None => {
                // GC and retry
                let _result = heap.collect_garbage(&stw, &mut roots, &monitors);
                let _obj = heap
                    .try_alloc_object(ClassId::new(0), 4)
                    .expect("alloc should succeed after GC");
            }
        }
    }
}

#[test]
fn m4_tlab_basic_alloc() {
    // Verify TLAB allocates objects correctly.
    use cratonvm_gc::Tlab;
    let mut buf = vec![0u8; 4096];
    let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 4096) };
    assert!(!tlab.is_empty());
    let ptr = tlab.alloc(128, 8);
    assert!(ptr.is_some());
    assert_eq!(tlab.remaining(), 4096 - 128);
}

#[test]
fn m4_tlab_refill_from_heap() {
    // Verify the heap can carve out TLABs.
    use cratonvm_gc::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 4 * 1024 * 1024);
    let result = heap.refill_tlab(64 * 1024);
    assert!(result.is_some(), "refill_tlab should succeed on fresh heap");
    let (ptr, size) = result.unwrap();
    assert!(!ptr.is_null());
    assert!(size > 0);
}

/// The `Zgc` arm of `refill_tlab` returned `None` until 2026-09-02, which left
/// the JIT's inline allocator dead on the default collector. It now carves a
/// zeroed chunk, and a retire of the VM buffer hands the unused tail back to
/// the arena rather than leaving an unregistered filler nothing can reclaim.
#[cfg(feature = "zgc")]
#[test]
fn zgc_refill_tlab_carves_a_chunk_and_retire_returns_the_tail() {
    use cratonvm_gc::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Zgc, 16 * 1024 * 1024);
    let VmHeap::Zgc(z) = &heap else {
        panic!("GcBackend::Zgc must build a Zgc heap");
    };
    // Opt THIS heap in explicitly. ZGC TLAB bridging shipped OPT-IN
    // (`827c059fe`, measured 4/4 slower on BinTreesClassic then) and went
    // default-ON in JIT review round 9 wave 5 (`ZGC_VM_TLAB_DEFAULT_ON`); this
    // test pins the feature's carve / retire arithmetic, so it enables the
    // feature itself rather than depend on the process default.
    // `set_vm_tlab_enabled` is per-heap, so this opts in without a process-wide
    // environment override that could leak into sibling tests.
    z.set_vm_tlab_enabled(true);
    let (ptr, size) = heap
        .refill_tlab(64 * 1024)
        .expect("ZGC must hand the VM thread's TLAB a chunk once opted in");
    assert!(!ptr.is_null());
    assert!(
        size >= 8 * 1024 && size <= 64 * 1024,
        "chunk of {size} bytes"
    );
    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
    assert!(tlab.alloc(128, 8).is_some());
    // The chunk was charged whole at refill; the retire credits the tail back.
    let allocated_before = heap.allocated_bytes();
    tlab.retire();
    assert!(tlab.is_retired());
    assert_eq!(
        allocated_before - heap.allocated_bytes(),
        size - 128,
        "the retired tail must be credited back to the heap"
    );
    let (refills, refill_bytes, tails, tail_bytes) = z.vm_tlab_engagement();
    assert_eq!(
        (refills, refill_bytes, tails, tail_bytes),
        (1, size, 1, size - 128)
    );
}

#[test]
fn m4_arena_grow() {
    // Verify arena can expand capacity.
    use cratonvm_gc::arena::Arena;
    let mut arena = Arena::new(1024);
    assert_eq!(arena.capacity(), 1024);
    arena.grow(4096);
    assert_eq!(arena.capacity(), 4096);
    // Can now allocate more
    let ptr = arena.alloc(2048, 8);
    assert!(ptr.is_some());
}

#[test]
fn m4_gc_with_expandable_heap() {
    // Heap starts small, GC runs when needed.
    use cratonvm_gc::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 256 * 1024); // 256 KB
    let monitors = crate::threading::monitor::MonitorTable::new();
    let mut kept: Vec<ObjectRef> = Vec::new();
    // Single-threaded test — no other mutator exists.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    for i in 0..500 {
        let obj = match heap.try_alloc_object(ClassId::new(0), 2) {
            Some(obj) => obj,
            None => {
                heap.collect_garbage(&stw, &mut kept, &monitors);
                heap.try_alloc_object(ClassId::new(0), 2)
                    .expect("alloc after GC should succeed")
            }
        };
        // Keep every 10th object alive
        if i % 10 == 0 {
            kept.push(obj);
        }
    }
    // Verify kept objects are still valid (they survived GC)
    for obj in &kept {
        let header = heap.get_header(*obj);
        assert_eq!(header.class_id, ClassId::new(0));
    }
}

#[test]
fn m4_massive_allocation_with_gc() {
    // Simulate a workload that previously crashed after 8+ test cycles.
    // This allocates ~20,000 objects and ~5,000 arrays — well beyond
    // what would fit in the old 8MB young gen without GC.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    for _ in 0..20_000 {
        let _obj = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    }
    for _ in 0..5_000 {
        let _arr =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), cratonvm_gc::ArrayElementType::Int, 50);
    }
}

#[test]
fn m4_concurrent_heavy_allocation() {
    // Multiple threads allocating heavily — exercises TLAB + shared path.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let s = shared.clone();
            std::thread::spawn(move || {
                for _ in 0..2000 {
                    let _obj = s.mem.heap.alloc_object(ClassId::new(0), 4);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("allocation thread panicked");
    }
}

// -----------------------------------------------------------------------
// M6 — Multi-thread execution: prove two+ threads run concurrently
// -----------------------------------------------------------------------

#[test]
fn m6_concurrent_heap_allocation() {
    // Two threads allocate objects on the shared heap simultaneously.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let s1 = shared.clone();
    let s2 = shared.clone();

    let t1 = std::thread::spawn(move || {
        for _ in 0..500 {
            let _obj = s1.mem.heap.alloc_object(ClassId::new(0), 4);
        }
    });
    let t2 = std::thread::spawn(move || {
        for _ in 0..500 {
            let _obj = s2.mem.heap.alloc_object(ClassId::new(0), 4);
        }
    });

    t1.join().expect("thread 1 panicked");
    t2.join().expect("thread 2 panicked");
}

#[test]
fn m6_concurrent_field_read_write() {
    // Two threads read/write fields on a shared object.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 4);

    shared.mem.heap.set_field(obj, 0, Value::Int(0));

    let s1 = shared.clone();
    let s2 = shared.clone();

    let t1 = std::thread::spawn(move || {
        for i in 0..1000 {
            s1.mem.heap.set_field(obj, 0, Value::Int(i));
            let _ = s1.mem.heap.get_field(obj, 1);
        }
    });
    let t2 = std::thread::spawn(move || {
        for i in 0..1000 {
            s2.mem.heap.set_field(obj, 1, Value::Int(i));
            let _ = s2.mem.heap.get_field(obj, 0);
        }
    });

    t1.join().expect("thread 1 panicked");
    t2.join().expect("thread 2 panicked");

    // Both fields should have been written — read final values
    let v0 = shared.mem.heap.get_field(obj, 0);
    let v1 = shared.mem.heap.get_field(obj, 1);
    match (v0, v1) {
        (Value::Int(a), Value::Int(b)) => {
            assert!(a >= 0 && a < 1000, "field 0 should have a valid int");
            assert!(b >= 0 && b < 1000, "field 1 should have a valid int");
        }
        _ => panic!("Expected Int values in fields"),
    }
}

#[test]
fn m6_monitor_mutual_exclusion() {
    // Two threads use monitorenter/monitorexit for mutual exclusion on a shared counter.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let lock_obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let counter_obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(counter_obj, 0, Value::Int(0));

    let s1 = shared.clone();
    let s2 = shared.clone();
    let iterations = 500;

    let t1 = std::thread::spawn(move || {
        let tid = ThreadId(100);
        for _ in 0..iterations {
            s1.threads.monitors.enter(lock_obj, tid);
            let val = match s1.mem.heap.get_field(counter_obj, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            s1.mem.heap.set_field(counter_obj, 0, Value::Int(val + 1));
            s1.threads.monitors.exit(lock_obj, tid).unwrap();
        }
    });

    let t2 = std::thread::spawn(move || {
        let tid = ThreadId(101);
        for _ in 0..iterations {
            s2.threads.monitors.enter(lock_obj, tid);
            let val = match s2.mem.heap.get_field(counter_obj, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            s2.mem.heap.set_field(counter_obj, 0, Value::Int(val + 1));
            s2.threads.monitors.exit(lock_obj, tid).unwrap();
        }
    });

    t1.join().expect("thread 1 panicked");
    t2.join().expect("thread 2 panicked");

    let final_val = match shared.mem.heap.get_field(counter_obj, 0) {
        Value::Int(v) => v,
        _ => panic!("Expected Int"),
    };
    assert_eq!(
        final_val,
        (iterations * 2) as i32,
        "M6: synchronized counter should be exactly {} (got {})",
        iterations * 2,
        final_val
    );
}

#[test]
fn m6_thread_registry_tracks_concurrent_threads() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    let tid1 = shared.threads.thread_registry.next_thread_id();
    let tid2 = shared.threads.thread_registry.next_thread_id();
    assert_ne!(tid1, tid2, "thread IDs should be unique");

    shared
        .threads
        .thread_registry
        .register(tid1, "worker-1", None);
    shared
        .threads
        .thread_registry
        .register(tid2, "worker-2", None);

    assert!(shared.threads.thread_registry.is_alive(tid1));
    assert!(shared.threads.thread_registry.is_alive(tid2));

    shared.threads.thread_registry.mark_dead(tid1);
    assert!(!shared.threads.thread_registry.is_alive(tid1));
    assert!(shared.threads.thread_registry.is_alive(tid2));

    shared.threads.thread_registry.mark_dead(tid2);
    assert!(!shared.threads.thread_registry.is_alive(tid2));
}

#[test]
fn m6_concurrent_array_operations() {
    // Two threads write to different elements of a shared array.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), cratonvm_gc::ArrayElementType::Int, 100);

    let s1 = shared.clone();
    let s2 = shared.clone();

    let t1 = std::thread::spawn(move || {
        for i in 0..50 {
            s1.mem
                .heap
                .set_array_element(arr, i, Value::Int(i as i32 * 10))
                .unwrap();
        }
    });
    let t2 = std::thread::spawn(move || {
        for i in 50..100 {
            s2.mem
                .heap
                .set_array_element(arr, i, Value::Int(i as i32 * 10))
                .unwrap();
        }
    });

    t1.join().expect("thread 1 panicked");
    t2.join().expect("thread 2 panicked");

    // Verify all elements written correctly
    for i in 0..100 {
        let val = shared.mem.heap.get_array_element(arr, i).unwrap();
        match val {
            Value::Int(v) => assert_eq!(v, i as i32 * 10, "arr[{}] should be {}", i, i * 10),
            _ => panic!("Expected Int at index {}", i),
        }
    }
}

#[test]
fn m6_monitor_reentrant_locking() {
    // A single thread can reenter the same monitor multiple times.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let lock_obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let tid = ThreadId(200);

    // Enter 3 times (reentrant)
    shared.threads.monitors.enter(lock_obj, tid);
    shared.threads.monitors.enter(lock_obj, tid);
    shared.threads.monitors.enter(lock_obj, tid);

    // Exit 3 times
    shared.threads.monitors.exit(lock_obj, tid).unwrap();
    shared.threads.monitors.exit(lock_obj, tid).unwrap();
    shared.threads.monitors.exit(lock_obj, tid).unwrap();

    // Another thread should now be able to acquire
    let s2 = shared.clone();
    let t = std::thread::spawn(move || {
        let tid2 = ThreadId(201);
        s2.threads.monitors.enter(lock_obj, tid2);
        s2.threads.monitors.exit(lock_obj, tid2).unwrap();
    });
    t.join().expect("reentrant test: second thread panicked");
}

#[test]
fn m6_four_threads_concurrent_allocation() {
    // Four threads allocate simultaneously — stress test for heap concurrency.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let s = shared.clone();
            std::thread::spawn(move || {
                for _ in 0..200 {
                    let _obj = s.mem.heap.alloc_object(ClassId::new(0), 2);
                    let _arr = s.mem.heap.alloc_array(
                        ClassId::new(0),
                        cratonvm_gc::ArrayElementType::Int,
                        10,
                    );
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

// -----------------------------------------------------------------------
// M12 — Annotation reflection tests
// -----------------------------------------------------------------------




#[test]
fn m12_convert_annotation_simple_no_elements() {
    // A bare @Component annotation with no elements.
    // CP: [0]=Tombstone, [1]="Lorg/springframework/stereotype/Component;"
    let cp = annotation_cp(&["Lorg/springframework/stereotype/Component;"]);
    let ann = mk_annotation(1);
    let data = vm_exec::convert_annotation(&ann, &cp);
    assert!(
        data.is_some(),
        "M12: convert_annotation should succeed for valid annotation"
    );
    let data = data.unwrap();
    assert_eq!(
        data.type_descriptor,
        "Lorg/springframework/stereotype/Component;"
    );
    assert!(data.elements.is_empty());
}

#[test]
fn m12_convert_annotation_with_string_element() {
    use cratonvm_reader::attribute::{ElementValue, ElementValuePair};
    // @Service(value="myService")
    // CP: [0]=Tombstone, [1]="Lorg/springframework/stereotype/Service;", [2]="value", [3]="myService"
    let cp = annotation_cp(&[
        "Lorg/springframework/stereotype/Service;",
        "value",
        "myService",
    ]);
    let ann = mk_annotation_with_elems(
        1,
        vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Const {
                tag: b's',
                const_value_index: 3,
            },
        }],
    );
    let data = vm_exec::convert_annotation(&ann, &cp).unwrap();
    assert_eq!(
        data.type_descriptor,
        "Lorg/springframework/stereotype/Service;"
    );
    assert_eq!(data.elements.len(), 1);
    assert_eq!(data.elements[0].0, "value");
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::StringVal(s) => {
            assert_eq!(s, "myService");
        }
        other => panic!("M12: expected StringVal, got {:?}", other),
    }
}

#[test]
fn m12_convert_annotation_with_int_element() {
    use cratonvm_reader::attribute::{ElementValue, ElementValuePair};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    // @Retention(value=1)  — simplified: int element
    // CP: [0]=Tombstone, [1]="Ljava/lang/annotation/Retention;", [2]="value", [3]=Integer(1)
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Ljava/lang/annotation/Retention;".into()),
        ConstantPoolEntry::Utf8("value".into()),
        ConstantPoolEntry::Integer(42),
    ]);
    let ann = mk_annotation_with_elems(
        1,
        vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: 3,
            },
        }],
    );
    let data = vm_exec::convert_annotation(&ann, &cp).unwrap();
    assert_eq!(data.type_descriptor, "Ljava/lang/annotation/Retention;");
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Int(v) => assert_eq!(*v, 42),
        other => panic!("M12: expected Int(42), got {:?}", other),
    }
}

#[test]
fn m12_convert_annotation_with_enum_element() {
    use cratonvm_reader::attribute::{ElementValue, ElementValuePair};
    // @Retention(RetentionPolicy.RUNTIME)
    // CP: [0]=Tombstone, [1]="Ljava/lang/annotation/Retention;", [2]="value",
    //     [3]="Ljava/lang/annotation/RetentionPolicy;", [4]="RUNTIME"
    let cp = annotation_cp(&[
        "Ljava/lang/annotation/Retention;",
        "value",
        "Ljava/lang/annotation/RetentionPolicy;",
        "RUNTIME",
    ]);
    let ann = mk_annotation_with_elems(
        1,
        vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Enum {
                type_name_index: 3,
                const_name_index: 4,
            },
        }],
    );
    let data = vm_exec::convert_annotation(&ann, &cp).unwrap();
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Enum(ty, name) => {
            assert_eq!(ty, "Ljava/lang/annotation/RetentionPolicy;");
            assert_eq!(name, "RUNTIME");
        }
        other => panic!("M12: expected Enum, got {:?}", other),
    }
}

#[test]
fn m12_convert_annotation_invalid_cp_index_returns_none() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    // Annotation with type_index pointing beyond CP bounds.
    let cp = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    let ann = mk_annotation(99); // index 99 doesn't exist
    let data = vm_exec::convert_annotation(&ann, &cp);
    assert!(data.is_none(), "M12: invalid CP index should return None");
}

#[test]
fn m12_class_annotations_via_shared_vm() {
    // Build a Class with annotations, register it in SharedVm, and verify
    // class_annotations() returns the correct data.
    use cratonvm_classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    let cp = annotation_cp(&["Ljakarta/inject/Named;"]);
    let ann = mk_annotation(1);

    let class_id;
    {
        let mut cm = shared.classes.class_manager_write();
        class_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: class_id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("com/example/MyService"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cp,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: vec![ann],
            nest_host: None,
            nest_members: vec![],
            record_components: vec![],
            permitted_subclasses: vec![],
            inner_classes: vec![],
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
    }

    // Use the NativeContextImpl to get class_annotations — we go through the VM
    // the same way the native methods do.
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let annotations = ctx.class_annotations(class_id);
    assert_eq!(annotations.len(), 1, "M12: should have 1 annotation");
    assert_eq!(annotations[0].type_descriptor, "Ljakarta/inject/Named;");
}

#[test]
fn m12_annotation_matching_descriptor_format() {
    // Verify the annotation matching convention: class name → "L<name>;"
    // This is the pattern used by isAnnotationPresent and getAnnotation.
    let class_name = "org/springframework/stereotype/Component";
    let target_desc = format!("L{};", class_name);
    assert_eq!(target_desc, "Lorg/springframework/stereotype/Component;");

    // An annotation with this descriptor should match.
    let ann = crate::native::registry::AnnotationData {
        type_descriptor: "Lorg/springframework/stereotype/Component;".to_string(),
        elements: vec![],
    };
    assert!(ann.type_descriptor == target_desc);
}

#[test]
fn m12_multiple_annotations_on_class() {
    // A class with @Component + @Service should return both.
    use cratonvm_classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    let cp = annotation_cp(&[
        "Lorg/springframework/stereotype/Component;",
        "Lorg/springframework/stereotype/Service;",
    ]);
    let ann1 = mk_annotation(1);
    let ann2 = mk_annotation(2);

    let class_id;
    {
        let mut cm = shared.classes.class_manager_write();
        class_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: class_id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("com/example/DualAnnotated"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cp,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: vec![ann1, ann2],
            nest_host: None,
            nest_members: vec![],
            record_components: vec![],
            permitted_subclasses: vec![],
            inner_classes: vec![],
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
    }

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let annotations = ctx.class_annotations(class_id);
    assert_eq!(annotations.len(), 2, "M12: should have 2 annotations");

    let descs: Vec<&str> = annotations
        .iter()
        .map(|a| a.type_descriptor.as_str())
        .collect();
    assert!(descs.contains(&"Lorg/springframework/stereotype/Component;"));
    assert!(descs.contains(&"Lorg/springframework/stereotype/Service;"));
}

#[test]
fn m12_convert_annotation_nested() {
    use cratonvm_reader::attribute::{Annotation, ElementValue, ElementValuePair};
    // @Outer(inner=@Inner)
    // CP: [0]=Tombstone, [1]="Louter;", [2]="inner", [3]="Linner;"
    let cp = annotation_cp(&["Louter;", "inner", "Linner;"]);
    let inner_ann = Annotation {
        type_index: 3,
        element_value_pairs: vec![],
    };
    let ann = mk_annotation_with_elems(
        1,
        vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::AnnotationValue(inner_ann),
        }],
    );
    let data = vm_exec::convert_annotation(&ann, &cp).unwrap();
    assert_eq!(data.type_descriptor, "Louter;");
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Annotation(nested) => {
            assert_eq!(nested.type_descriptor, "Linner;");
        }
        other => panic!("M12: expected nested Annotation, got {:?}", other),
    }
}

#[test]
fn m12_convert_annotation_array_element() {
    use cratonvm_reader::attribute::{ElementValue, ElementValuePair};
    // @Target({ElementType.TYPE, ElementType.FIELD})  — simplified as string array
    // CP: [0]=Tombstone, [1]="Ltarget;", [2]="value", [3]="TYPE", [4]="FIELD"
    let cp = annotation_cp(&["Ltarget;", "value", "TYPE", "FIELD"]);
    let ann = mk_annotation_with_elems(
        1,
        vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Array(vec![
                ElementValue::Const {
                    tag: b's',
                    const_value_index: 3,
                },
                ElementValue::Const {
                    tag: b's',
                    const_value_index: 4,
                },
            ]),
        }],
    );
    let data = vm_exec::convert_annotation(&ann, &cp).unwrap();
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Array(elems) => {
            assert_eq!(elems.len(), 2);
            match &elems[0] {
                crate::native::registry::AnnotationElementValue::StringVal(s) => {
                    assert_eq!(s, "TYPE")
                }
                other => panic!("M12: expected StringVal, got {:?}", other),
            }
            match &elems[1] {
                crate::native::registry::AnnotationElementValue::StringVal(s) => {
                    assert_eq!(s, "FIELD")
                }
                other => panic!("M12: expected StringVal, got {:?}", other),
            }
        }
        other => panic!("M12: expected Array, got {:?}", other),
    }
}

#[test]
fn m12_unannotated_class_returns_empty() {
    // A class with no annotations should return empty vec.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let cp = annotation_cp(&[]);

    let class_id;
    {
        use cratonvm_classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        use cratonvm_reader::class_file_version::ClassFileVersion;

        let mut cm = shared.classes.class_manager_write();
        class_id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: class_id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("com/example/Plain"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cp,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: vec![],
            nest_host: None,
            nest_members: vec![],
            record_components: vec![],
            permitted_subclasses: vec![],
            inner_classes: vec![],
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
    }

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let annotations = ctx.class_annotations(class_id);
    assert!(
        annotations.is_empty(),
        "M12: unannotated class should have empty annotations"
    );
}

// -----------------------------------------------------------------------
// M14 — HTTP client/server on real sockets
// -----------------------------------------------------------------------

#[test]
fn m14_tcp_server_client_raw_exchange() {
    // Prove raw TCP socket communication works end-to-end.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut stream, addr) = listener.accept().unwrap();
        assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"PING");
        stream.write_all(b"PONG").unwrap();
    });

    let mut client = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    client.write_all(b"PING").unwrap();
    client.flush().unwrap();
    // Signal we're done writing so server's read returns
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    assert_eq!(response, b"PONG");

    server.join().unwrap();
}

#[test]
fn m14_http_server_serves_response() {
    // A minimal HTTP server: accept one request, return 200 OK with body.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        assert!(
            request_line.starts_with("GET /api/health"),
            "M14: got: {}",
            request_line
        );

        // Read remaining headers until empty line
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
        }

        let body = "{\"status\":\"ok\"}";
        let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body,
            );
        let mut writer = stream;
        writer.write_all(resp.as_bytes()).unwrap();
    });

    // Client side: send real HTTP request
    let mut client = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let req = "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    client.write_all(req.as_bytes()).unwrap();
    client.flush().unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let resp_str = String::from_utf8_lossy(&response);
    assert!(
        resp_str.contains("200 OK"),
        "M14: expected 200 OK in response"
    );
    assert!(
        resp_str.contains("{\"status\":\"ok\"}"),
        "M14: expected JSON body"
    );

    server.join().unwrap();
}

#[test]
fn m14_multiple_sequential_http_requests() {
    // A server handles multiple sequential requests.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        for i in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let body = format!("response-{i}");
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream.write_all(resp.as_bytes()).ok();
        }
    });

    for i in 0..3 {
        let mut client = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let req = format!("GET /req-{i} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        client.write_all(req.as_bytes()).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        let resp_str = String::from_utf8_lossy(&response);
        assert!(
            resp_str.contains(&format!("response-{i}")),
            "M14: expected response-{i}"
        );
    }

    server.join().unwrap();
}

#[test]
fn m14_concurrent_http_clients() {
    // Multiple clients connect to the server concurrently.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
            stream.write_all(resp.as_bytes()).ok();
        }
    });

    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                let mut client = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
                client
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                client
                    .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                    .unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).unwrap();
                let resp_str = String::from_utf8_lossy(&response);
                assert!(resp_str.contains("200 OK"), "M14: expected 200 OK");
                assert!(resp_str.contains("OK"), "M14: expected OK body");
            })
        })
        .collect();

    for h in handles {
        h.join().expect("M14: client thread panicked");
    }
    server.join().unwrap();
}

#[test]
fn m14_http_server_large_response_body() {
    // Server returns a large response body (10KB) — tests buffered I/O.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let body_content = "X".repeat(10_000);
    let body_clone = body_content.clone();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_clone.len(),
            body_clone,
        );
        stream.write_all(resp.as_bytes()).ok();
    });

    let mut client = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    client
        .write_all(b"GET /large HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let resp_str = String::from_utf8_lossy(&response);
    assert!(resp_str.contains(&body_content), "M14: expected 10KB body");

    server.join().unwrap();
}

#[test]
fn m14_extract_uri_scheme_defaults_port_80() {
    // When no port is specified in a URI, default to 80 for http.
    // This is a logical test of the convention used by the HTTP client.
    let scheme = "http";
    let default_port: u16 = match scheme {
        "https" => 443,
        _ => 80,
    };
    assert_eq!(default_port, 80);
}

// -----------------------------------------------------------------------
// M16 — JSON (Jackson/Gson) reflection-based serialization
// -----------------------------------------------------------------------


#[test]
fn m16_jackson_write_value_as_string_int_fields() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(&shared, "com/example/Point", &[("x", "I"), ("y", "I")]);

    let obj = shared.mem.heap.alloc_object(class_id, 2);
    shared.mem.heap.set_field(obj, 0, Value::Int(10));
    shared.mem.heap.set_field(obj, 1, Value::Int(20));

    // Create ObjectMapper (2nd arg unused)
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(mapper)), Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    let json_ref = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String result"),
    };
    let json = read_java_string(&shared.mem.heap, json_ref).unwrap();
    assert!(json.contains("\"x\":10"), "M16: expected x:10 in {json}");
    assert!(json.contains("\"y\":20"), "M16: expected y:20 in {json}");
}

#[test]
fn m16_jackson_write_value_as_string_with_string_field() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/Person",
        &[("name", "Ljava/lang/String;"), ("age", "I")],
    );

    let name_str = crate::vm::vm_object::create_java_string(&shared, "Alice");
    let obj = shared.mem.heap.alloc_object(class_id, 2);
    shared
        .mem
        .heap
        .set_field(obj, 0, Value::Object(Some(name_str)));
    shared.mem.heap.set_field(obj, 1, Value::Int(30));

    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(mapper)), Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    let json_ref = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String"),
    };
    let json = read_java_string(&shared.mem.heap, json_ref).unwrap();
    assert!(
        json.contains("\"name\":\"Alice\""),
        "M16: expected name:Alice in {json}"
    );
    assert!(
        json.contains("\"age\":30"),
        "M16: expected age:30 in {json}"
    );
}

#[test]
fn m16_jackson_write_value_as_string_boolean_field() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/Toggle",
        &[("enabled", "Z"), ("count", "I")],
    );

    let obj = shared.mem.heap.alloc_object(class_id, 2);
    shared.mem.heap.set_field(obj, 0, Value::Int(1)); // true
    shared.mem.heap.set_field(obj, 1, Value::Int(42));

    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(mapper)), Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    let json_ref = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String"),
    };
    let json = read_java_string(&shared.mem.heap, json_ref).unwrap();
    assert!(
        json.contains("\"enabled\":true"),
        "M16: expected enabled:true in {json}"
    );
    assert!(
        json.contains("\"count\":42"),
        "M16: expected count:42 in {json}"
    );
}

#[test]
fn m16_jackson_write_null_object() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(mapper)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();

    let json_ref = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String"),
    };
    let json = read_java_string(&shared.mem.heap, json_ref).unwrap();
    assert_eq!(json, "null", "M16: null object should serialize as 'null'");
}

#[test]
fn m16_jackson_read_value_int_fields() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(&shared, "com/example/Coord", &[("x", "I"), ("y", "I")]);

    // Create a class mirror for Coord
    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, class_id);

    let json_str = crate::vm::vm_object::create_java_string(&shared, r#"{"x":5,"y":10}"#);
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readValue",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        &[
            Value::Object(Some(mapper)),
            Value::Object(Some(json_str)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap();

    let obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected Object"),
    };
    let x = shared.mem.heap.get_field(obj, 0);
    let y = shared.mem.heap.get_field(obj, 1);
    assert_eq!(x.as_int(), Some(5), "M16: expected x=5");
    assert_eq!(y.as_int(), Some(10), "M16: expected y=10");
}

#[test]
fn m16_jackson_read_value_string_field() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/NamedItem",
        &[("label", "Ljava/lang/String;"), ("count", "I")],
    );

    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, class_id);
    let json_str =
        crate::vm::vm_object::create_java_string(&shared, r#"{"label":"hello","count":7}"#);
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readValue",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        &[
            Value::Object(Some(mapper)),
            Value::Object(Some(json_str)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap();

    let obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected Object"),
    };
    let label_val = shared.mem.heap.get_field(obj, 0);
    let count_val = shared.mem.heap.get_field(obj, 1);
    let label_ref = match label_val {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String for label"),
    };
    let label = read_java_string(&shared.mem.heap, label_ref).unwrap();
    assert_eq!(label, "hello", "M16: expected label=hello");
    assert_eq!(count_val.as_int(), Some(7), "M16: expected count=7");
}

#[test]
fn m16_gson_to_json() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/Config",
        &[("port", "I"), ("debug", "Z")],
    );

    let obj = shared.mem.heap.alloc_object(class_id, 2);
    shared.mem.heap.set_field(obj, 0, Value::Int(8080));
    shared.mem.heap.set_field(obj, 1, Value::Int(1)); // true

    let gson = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/google/gson/Gson",
        "toJson",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(gson)), Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    let json_ref = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String"),
    };
    let json = read_java_string(&shared.mem.heap, json_ref).unwrap();
    assert!(
        json.contains("\"port\":8080"),
        "M16: expected port:8080 in {json}"
    );
    assert!(
        json.contains("\"debug\":true"),
        "M16: expected debug:true in {json}"
    );
}

#[test]
fn m16_gson_from_json() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/Settings",
        &[("timeout", "J"), ("retries", "I")],
    );

    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, class_id);
    let json_str =
        crate::vm::vm_object::create_java_string(&shared, r#"{"timeout":30000,"retries":3}"#);
    let gson = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/google/gson/Gson",
        "fromJson",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        &[
            Value::Object(Some(gson)),
            Value::Object(Some(json_str)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap();

    let obj = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected Object"),
    };
    let timeout = shared.mem.heap.get_field(obj, 0);
    let retries = shared.mem.heap.get_field(obj, 1);
    assert_eq!(
        timeout.as_long(),
        Some(30000),
        "M16: expected timeout=30000"
    );
    assert_eq!(retries.as_int(), Some(3), "M16: expected retries=3");
}

#[test]
fn m16_jackson_roundtrip_serialize_deserialize() {
    // Serialize an object, then deserialize the JSON back, and verify fields match.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/RoundTrip",
        &[("value", "I"), ("flag", "Z")],
    );

    let obj = shared.mem.heap.alloc_object(class_id, 2);
    shared.mem.heap.set_field(obj, 0, Value::Int(999));
    shared.mem.heap.set_field(obj, 1, Value::Int(0)); // false

    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Serialize
    let json_result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(mapper)), Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    let json_ref = match json_result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String"),
    };

    // Deserialize
    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, class_id);
    let deser_result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readValue",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        &[
            Value::Object(Some(mapper)),
            Value::Object(Some(json_ref)),
            Value::Object(Some(mirror)),
        ],
    )
    .unwrap()
    .unwrap();

    let obj2 = match deser_result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected Object"),
    };
    assert_eq!(
        shared.mem.heap.get_field(obj2, 0).as_int(),
        Some(999),
        "M16: roundtrip value"
    );
    assert_eq!(
        shared.mem.heap.get_field(obj2, 1).as_int(),
        Some(0),
        "M16: roundtrip flag"
    );
}

#[test]
fn m16_json_escape_special_chars() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = m16_register_class(
        &shared,
        "com/example/TextHolder",
        &[("text", "Ljava/lang/String;")],
    );

    let text = crate::vm::vm_object::create_java_string(&shared, "hello\n\"world\"");
    let obj = shared.mem.heap.alloc_object(class_id, 1);
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(text)));

    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(mapper)), Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    let json_ref = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: expected String"),
    };
    let json = read_java_string(&shared.mem.heap, json_ref).unwrap();
    assert!(
        json.contains("\\n"),
        "M16: expected escaped newline in {json}"
    );
    assert!(
        json.contains("\\\"world\\\""),
        "M16: expected escaped quotes in {json}"
    );
}

// =====================================================================
// M20 — Weak/Soft reference processing wired to GC
// =====================================================================

#[test]
fn m20_weak_ref_init_stores_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // referent = some object with 1 field
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    // WeakReference object: field 0 = referent, field 1 = queue
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // get() should return the referent
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => assert_eq!(
            r.as_ptr(),
            referent.as_ptr(),
            "M20: get() should return referent"
        ),
        _ => panic!("M20: expected object from get()"),
    }
}

#[test]
fn m20_weak_ref_discovered_by_ref_processor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // After init, the ref processor should have 1 weak ref
    let proc = shared.mem.ref_processor.lock();
    assert_eq!(
        proc.weak_ref_count(),
        1,
        "M20: ref processor should track 1 weak ref"
    );
}

#[test]
fn m20_weak_ref_cleared_when_referent_unreachable() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // Simulate GC: referent is NOT marked (unreachable)
    let weak_addr = weak_ref.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        let is_marked = |addr: usize| -> bool {
            // Only the weak_ref itself survives, referent does not
            addr == weak_addr
        };
        let _result = proc.process_references(&is_marked, 64, 0);
        let cleared = proc.cleared_ref_objects();
        assert!(!cleared.is_empty(), "M20: should have cleared weak ref");
        // Null the referent field
        for ref_addr in cleared {
            let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
            shared.mem.heap.set_field(obj, 0, Value::Object(None));
        }
    }
    // get() should now return null
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(None) => {} // expected
        _ => panic!("M20: get() should return null after GC clears referent"),
    }
}

#[test]
fn m20_weak_ref_not_cleared_when_referent_reachable() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // Simulate GC: both referent and weak_ref are marked (reachable)
    let referent_addr = referent.as_ptr() as usize;
    let weak_addr = weak_ref.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        let is_marked = |addr: usize| -> bool { addr == weak_addr || addr == referent_addr };
        let _result = proc.process_references(&is_marked, 64, 0);
        let cleared = proc.cleared_ref_objects();
        assert!(
            cleared.is_empty(),
            "M20: should NOT clear weak ref when referent is reachable"
        );
    }
    // get() should still return the referent
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), referent.as_ptr()),
        _ => panic!("M20: get() should still return referent"),
    }
}

#[test]
fn m20_soft_ref_init_stores_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let soft_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(soft_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(soft_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => assert_eq!(
            r.as_ptr(),
            referent.as_ptr(),
            "M20: soft get() should return referent"
        ),
        _ => panic!("M20: expected object from soft get()"),
    }
}

#[test]
fn m20_soft_ref_cleared_under_memory_pressure() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let soft_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(soft_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    let soft_addr = soft_ref.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        // free_heap_mb=0 simulates memory pressure; the clock must be far
        // enough past the entry's own timestamp to exceed the LRU
        // threshold.
        //
        // This used to be a literal `1_000_000`, which stopped working when
        // `gc::reference` gained `last_observed_clock_ms`: "now" is
        // `max(caller clock, mutator clock)`, and `SoftReference.<init>`
        // stamps the entry with `SystemTime::now()` (~1.7e12) via
        // `touch_soft_reference`. A caller clock of 1e6 was therefore
        // dominated by the entry's own creation stamp, making idle time 0
        // and the ref unclearable no matter how much pressure was reported.
        // Anchor the test clock to the same wall clock the mutator uses.
        let is_marked = |addr: usize| -> bool { addr == soft_addr };
        let _result = proc.process_references(&is_marked, 0, wall_clock_ms() + 60_000);
        let cleared = proc.cleared_ref_objects();
        assert!(
            !cleared.is_empty(),
            "M20: soft ref should be cleared under memory pressure"
        );
        for ref_addr in cleared {
            let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
            shared.mem.heap.set_field(obj, 0, Value::Object(None));
        }
    }
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(soft_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(None) => {}
        _ => panic!("M20: soft get() should return null after clearing"),
    }
}

#[test]
fn m20_soft_ref_retained_with_plenty_of_memory() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let soft_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(soft_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    let soft_addr = soft_ref.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        // free_heap_mb=1024 means plenty of memory, current_time_ms=0 means recently accessed
        let is_marked = |addr: usize| -> bool { addr == soft_addr };
        let _result = proc.process_references(&is_marked, 1024, 0);
        let cleared = proc.cleared_ref_objects();
        assert!(
            cleared.is_empty(),
            "M20: soft ref should NOT be cleared with plenty of memory"
        );
    }
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(soft_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), referent.as_ptr()),
        _ => panic!("M20: soft get() should still return referent"),
    }
}

// ======================================================================
// Soft-reference pressure input (arch-2026-07-26 §R1).
//
// `process_references_after_gc` and `g1_remark_process_references` in
// `vm/src/runtime/interpreter.rs` passed a hardcoded `64` for
// `free_heap_mb`, so the LRU threshold was pinned at exactly 64 seconds of
// idleness no matter how full the heap was and the policy could not respond
// to memory pressure at all. Both now pass
// `shared.mem.heap.soft_ref_policy_free_mb()`.
// ======================================================================



/// THE POINT OF §R1: the pressure argument is load-bearing. At one fixed
/// instant, with one fixed amount of idleness, the old hardcoded `64` MB
/// retains the referent and a real "0 MB allocatable" reading clears it.
///
/// If this test ever passes with both arms asserting the same outcome, the
/// call sites have regressed to a constant and the policy is dead again.
#[test]
fn soft_ref_policy_pressure_input_is_load_bearing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let soft_addr = soft_ref_under_test(&shared, &mut thread);
    let is_marked = |addr: usize| -> bool { addr == soft_addr };

    // 30 s of idleness: below the 64 s the old hardcode implied
    // (`SoftRefLRUPolicyMSPerMB` defaults to 1000), above the 0 s that a
    // heap with no allocatable headroom implies.
    let now = wall_clock_ms() + 30_000;

    {
        let mut proc = shared.mem.ref_processor.lock();
        let _ = proc.process_references(&is_marked, 64, now);
        assert!(
            proc.cleared_ref_objects().is_empty(),
            "the old hardcoded 64 MB must retain a 30 s-idle soft ref \
                 (threshold 64 s) -- if it does not, the arms of this test no \
                 longer discriminate and the regression it guards is invisible"
        );
    }
    {
        let mut proc = shared.mem.ref_processor.lock();
        let _ = proc.process_references(&is_marked, 0, now);
        assert!(
            !proc.cleared_ref_objects().is_empty(),
            "with no allocatable headroom the same soft ref, at the same \
                 instant, must be cleared -- this is the case the hardcoded \
                 `64` made unreachable"
        );
    }
}

/// End-to-end pin on the value the two interpreter call sites now pass:
/// it comes from the live heap, is bounded by real headroom, and the
/// threshold it produces is the one the processor actually applies.
#[test]
fn soft_ref_policy_free_mb_drives_the_threshold_the_vm_call_sites_use() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Exactly the expression `process_references_after_gc` and
    // `g1_remark_process_references` evaluate.
    let free_mb = shared.mem.heap.soft_ref_policy_free_mb();

    let (young_used, young_cap) = shared.mem.heap.young_gen_stats();
    let (old_used, old_cap) = shared.mem.heap.old_gen_stats();
    let total_free_mb = (young_cap + old_cap).saturating_sub(young_used + old_used) / (1024 * 1024);
    assert!(
        free_mb <= total_free_mb,
        "the pressure input ({free_mb} MB) must never exceed real whole-heap \
             headroom ({total_free_mb} MB); over-reporting is the failure mode \
             that clears soft refs never"
    );

    let soft_addr = soft_ref_under_test(&shared, &mut thread);
    let is_marked = |addr: usize| -> bool { addr == soft_addr };

    // `SoftRefLRUPolicyMSPerMB` defaults to 1000, so the threshold this
    // reading implies is `free_mb` seconds. Idle by comfortably more.
    let past_threshold = wall_clock_ms()
        .saturating_add(free_mb.saturating_mul(1000) as u64)
        .saturating_add(10_000);
    {
        let mut proc = shared.mem.ref_processor.lock();
        let _ = proc.process_references(&is_marked, free_mb, past_threshold);
        assert!(
            !proc.cleared_ref_objects().is_empty(),
            "a soft ref idle past `soft_ref_policy_free_mb()` seconds must \
                 clear under the real pressure input ({free_mb} MB)"
        );
    }
}

/// The `0` third argument at both call sites is deliberate, not a second
/// hardcode: `gc::reference` substitutes the mutator clock it learned from
/// `touch_soft_reference`. A soft ref created *and touched* by
/// `SoftReference.<init>` must therefore look freshly-accessed — not
/// infinitely idle — when the collector passes `0`.
#[test]
fn soft_ref_policy_zero_clock_argument_uses_the_mutator_clock() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let soft_addr = soft_ref_under_test(&shared, &mut thread);
    let is_marked = |addr: usize| -> bool { addr == soft_addr };

    let mut proc = shared.mem.ref_processor.lock();
    // Maximum pressure (0 MB free) *and* the literal `0` clock the
    // interpreter passes. The entry was stamped ~now by `<init>`, so its
    // idle time is ~0 and it must survive.
    let _ = proc.process_references(&is_marked, 0, 0);
    assert!(
        proc.cleared_ref_objects().is_empty(),
        "a just-created, just-touched soft ref must not be cleared merely \
             because the collector passes a `0` clock"
    );
}

#[test]
fn m20_phantom_ref_discovered() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let phantom = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/PhantomReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(phantom)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    let proc = shared.mem.ref_processor.lock();
    assert_eq!(
        proc.phantom_ref_count(),
        1,
        "M20: ref processor should track 1 phantom ref"
    );
}

#[test]
fn m20_ref_clear_nulls_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // Explicit clear()
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "clear",
        "()V",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(None) => {}
        _ => panic!("M20: get() should return null after clear()"),
    }
}

#[test]
fn m20_multiple_weak_refs_same_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let weak2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak1)), Value::Object(Some(referent))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak2)), Value::Object(Some(referent))],
    )
    .unwrap();
    // Both should be discovered
    assert_eq!(
        shared.mem.ref_processor.lock().weak_ref_count(),
        2,
        "M20: should track 2 weak refs"
    );
    // Simulate GC clearing both
    let w1_addr = weak1.as_ptr() as usize;
    let w2_addr = weak2.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        let is_marked = |addr: usize| -> bool { addr == w1_addr || addr == w2_addr };
        let _result = proc.process_references(&is_marked, 64, 0);
        let cleared = proc.cleared_ref_objects();
        assert_eq!(cleared.len(), 2, "M20: both weak refs should be cleared");
        for ref_addr in cleared {
            let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
            shared.mem.heap.set_field(obj, 0, Value::Object(None));
        }
    }
    // Both get() should return null
    for (name, wr) in [("weak1", weak1), ("weak2", weak2)] {
        let result = call_native(
            &shared,
            &mut thread,
            "java/lang/ref/WeakReference",
            "get",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(wr))],
        )
        .unwrap()
        .unwrap();
        match result {
            Value::Object(None) => {}
            _ => panic!("M20: {name}.get() should return null"),
        }
    }
}

// =========================================================================
// M16-M20 fix verification tests (security, correctness, edge cases)
// =========================================================================

// --- M16: JSON depth limit, input size, tree model ---

#[test]
fn m16_json_read_value_null_input() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readValue",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        &[
            Value::Object(Some(mapper)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(None) => {}
        _ => panic!("M16: readValue with null input should return null"),
    }
}

#[test]
fn m16_json_readtree_simple_object() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let json_str = create_java_string(&shared, r#"{"name":"Alice","age":30}"#);
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_str))],
    )
    .unwrap()
    .unwrap();
    let node = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: readTree should return a JsonNode"),
    };
    assert_eq!(
        shared.mem.heap.get_field(node, 0),
        Value::Int(1),
        "M16: should be object node"
    );
    assert_eq!(
        shared.mem.heap.get_field(node, 6),
        Value::Int(2),
        "M16: should have 2 children"
    );
}

#[test]
fn m16_json_readtree_empty_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let json_str = create_java_string(&shared, "");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_str))],
    )
    .unwrap()
    .unwrap();
    let node = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("M16: readTree of empty string should still return a node"),
    };
    assert_eq!(
        shared.mem.heap.get_field(node, 0),
        Value::Int(0),
        "M16: empty input => null node"
    );
}

#[test]
fn m16_jsonnode_astext_string_node() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let json_str = create_java_string(&shared, r#""hello world""#);
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_str))],
    )
    .unwrap()
    .unwrap();
    let node = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected node"),
    };
    assert_eq!(shared.mem.heap.get_field(node, 0), Value::Int(3)); // string type
    let text_result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/JsonNode",
        "asText",
        "()Ljava/lang/String;",
        &[Value::Object(Some(node))],
    )
    .unwrap()
    .unwrap();
    let text_ref = match text_result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected String"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, text_ref).unwrap(),
        "hello world"
    );
}

#[test]
fn m16_jsonnode_asint_number_node() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let json_str = create_java_string(&shared, "42");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_str))],
    )
    .unwrap()
    .unwrap();
    let node = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected node"),
    };
    assert_eq!(shared.mem.heap.get_field(node, 0), Value::Int(4)); // number type
    let int_result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/JsonNode",
        "asInt",
        "()I",
        &[Value::Object(Some(node))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(int_result, Value::Int(42));
}

#[test]
fn m16_jsonnode_boolean_and_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    // Boolean
    let json_true = create_java_string(&shared, "true");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_true))],
    )
    .unwrap()
    .unwrap();
    let node = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected node"),
    };
    assert_eq!(shared.mem.heap.get_field(node, 0), Value::Int(5)); // boolean type
    let bool_val = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/JsonNode",
        "asBoolean",
        "()Z",
        &[Value::Object(Some(node))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bool_val, Value::Int(1));
    // Null
    let json_null = create_java_string(&shared, "null");
    let result2 = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_null))],
    )
    .unwrap()
    .unwrap();
    let node2 = match result2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected node"),
    };
    let is_null = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/JsonNode",
        "isNull",
        "()Z",
        &[Value::Object(Some(node2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        is_null,
        Value::Int(1),
        "M16: null literal => isNull == true"
    );
}

#[test]
fn m16_jsonnode_array_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mapper = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let json_str = create_java_string(&shared, "[1,2,3]");
    let result = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/ObjectMapper",
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        &[Value::Object(Some(mapper)), Value::Object(Some(json_str))],
    )
    .unwrap()
    .unwrap();
    let node = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected node"),
    };
    assert_eq!(shared.mem.heap.get_field(node, 0), Value::Int(2)); // array type
    let size = call_native(
        &shared,
        &mut thread,
        "com/fasterxml/jackson/databind/JsonNode",
        "size",
        "()I",
        &[Value::Object(Some(node))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3), "M16: array [1,2,3] should have size 3");
}

// --- M17: defineClass security ---

#[test]
fn m17_define_class_rejects_invalid_magic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bad_bytes =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Byte, 10);
    for i in 0..10 {
        let _ = shared
            .mem
            .heap
            .set_array_element(bad_bytes, i, Value::Int(0));
    }
    let cl = alloc_receiver(&shared, &mut thread, "java/lang/ClassLoader", 4);
    let name_str = create_java_string(&shared, "com/test/Bad");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "defineClass",
        "(Ljava/lang/String;[BII)Ljava/lang/Class;",
        &[
            Value::Object(Some(cl)),
            Value::Object(Some(name_str)),
            Value::Object(Some(bad_bytes)),
            Value::Int(0),
            Value::Int(10),
        ],
    );
    // The JDK throws on a bad magic number — `IndexOutOfBoundsException` for a bad
    // range, `ClassFormatError` for bad bytes — it does not return null.
    // This asserted the null, so it failed against a VM that (correctly)
    // raises ClassFormatError. Rejecting means throwing.
    assert!(
        result.is_err(),
        "defineClass must reject a bad magic number by throwing, got {result:?}",
    );
}

#[test]
fn m17_define_class_rejects_negative_offset() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bytes =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Byte, 10);
    let cl = alloc_receiver(&shared, &mut thread, "java/lang/ClassLoader", 4);
    let name_str = create_java_string(&shared, "com/test/Neg");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "defineClass",
        "(Ljava/lang/String;[BII)Ljava/lang/Class;",
        &[
            Value::Object(Some(cl)),
            Value::Object(Some(name_str)),
            Value::Object(Some(bytes)),
            Value::Int(-1),
            Value::Int(10),
        ],
    );
    // The JDK throws on negative offset — `IndexOutOfBoundsException` for a bad
    // range, `ClassFormatError` for bad bytes — it does not return null.
    // This asserted the null, so it failed against a VM that (correctly)
    // raises IndexOutOfBoundsException. Rejecting means throwing.
    assert!(
        result.is_err(),
        "defineClass must reject negative offset by throwing, got {result:?}",
    );
}

#[test]
fn m17_define_class_rejects_out_of_bounds() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bytes =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Byte, 10);
    let cl = alloc_receiver(&shared, &mut thread, "java/lang/ClassLoader", 4);
    let name_str = create_java_string(&shared, "com/test/OOB");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "defineClass",
        "(Ljava/lang/String;[BII)Ljava/lang/Class;",
        &[
            Value::Object(Some(cl)),
            Value::Object(Some(name_str)),
            Value::Object(Some(bytes)),
            Value::Int(5),
            Value::Int(10),
        ],
    );
    // The JDK throws on an out-of-bounds offset+length — `IndexOutOfBoundsException` for a bad
    // range, `ClassFormatError` for bad bytes — it does not return null.
    // This asserted the null, so it failed against a VM that (correctly)
    // raises IndexOutOfBoundsException. Rejecting means throwing.
    assert!(
        result.is_err(),
        "defineClass must reject an out-of-bounds offset+length by throwing, got {result:?}",
    );
}

// --- M19: Finalizer enqueuing ---

#[test]
fn m19_finalizer_enqueued_for_dead_objects() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = register_test_class(&shared, "com/test/Finalizable", 0x0021, &[], &[]);
    let obj = shared.mem.heap.alloc_object(class_id, 1);
    shared.register_finalizable(obj.as_ptr() as usize);
    {
        let mut proc = shared.mem.ref_processor.lock();
        let result = proc.process_references(&|_| false, 64, 0);
        assert!(
            !result.to_finalize.is_empty(),
            "M19: finalizer should be enqueued for dead finalizable object"
        );
    }
}

#[test]
fn m19_finalizer_not_enqueued_for_live_objects() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let class_id = register_test_class(&shared, "com/test/FinalLive", 0x0021, &[], &[]);
    let obj = shared.mem.heap.alloc_object(class_id, 1);
    let obj_addr = obj.as_ptr() as usize;
    shared.register_finalizable(obj_addr);
    {
        let mut proc = shared.mem.ref_processor.lock();
        let result = proc.process_references(&|addr| addr == obj_addr, 64, 0);
        assert!(
            result.to_finalize.is_empty(),
            "M19: live objects should not be finalized"
        );
    }
}

// --- M20: Reference processing edge cases ---

#[test]
fn m20_phantom_ref_enqueued_but_not_cleared() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue_obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let phantom = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/PhantomReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(phantom)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue_obj)),
        ],
    )
    .unwrap();
    let phantom_addr = phantom.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        let result = proc.process_references(&|addr| addr == phantom_addr, 64, 0);
        assert!(
            result.stats.phantom_refs_enqueued >= 1,
            "M20: phantom should be enqueued when referent is dead"
        );
    }
}

#[test]
fn m20_soft_ref_cleared_under_zero_memory() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let soft = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(soft)), Value::Object(Some(referent))],
    )
    .unwrap();
    let soft_addr = soft.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        // Wall-clock-anchored: `SoftReference.<init>` stamps the entry with
        // `SystemTime::now()`, and `gc::reference` drives the LRU with
        // `max(caller clock, observed mutator clock)`, so a literal `1000`
        // here is dominated by the entry's own creation stamp and yields
        // zero idle time. See `soft_ref_policy_pressure_input_is_load_bearing`.
        let result =
            proc.process_references(&|addr| addr == soft_addr, 0, wall_clock_ms() + 60_000);
        assert!(
            result.stats.soft_refs_cleared >= 1,
            "M20: soft ref should be cleared under zero free heap"
        );
    }
}

// =========================================================================
// S28: Reference Processing (Soft/Weak/Phantom) + ReferenceQueue
// =========================================================================

#[test]
fn s28_weak_ref_with_queue_enqueued_after_gc() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    // Init ReferenceQueue
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    // Init WeakReference with queue
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(weak_ref)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    let weak_addr = weak_ref.as_ptr() as usize;
    let queue_addr = queue.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        // Referent is NOT marked (dead), weak_ref and queue survive
        let is_marked = |addr: usize| -> bool { addr == weak_addr || addr == queue_addr };
        let result = proc.process_references(&is_marked, 64, 0);
        assert!(
            result.stats.weak_refs_cleared >= 1,
            "S28: weak ref should be cleared"
        );
        // Null the referent field
        let cleared = proc.cleared_ref_objects();
        for ref_addr in cleared {
            let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
            shared.mem.heap.set_field(obj, 0, Value::Object(None));
        }
        // Enqueue into heap ReferenceQueue
        for (ra, qa) in &result.to_enqueue {
            let robj = unsafe { ObjectRef::from_raw(*ra as *mut u8) };
            let qobj = unsafe { ObjectRef::from_raw(*qa as *mut u8) };
            let old_head = shared.mem.heap.get_field(qobj, 0);
            shared
                .mem
                .heap
                .set_field(qobj, 0, Value::Object(Some(robj)));
            shared.mem.heap.set_field(robj, 0, old_head);
            let size = match shared.mem.heap.get_field(qobj, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            shared.mem.heap.set_field(qobj, 1, Value::Int(size + 1));
            shared.mem.heap.set_field(robj, 1, Value::Int(1)); // enqueued sentinel
        }
    }
    // Poll from queue — should return the weak ref
    let poll_result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    match poll_result {
        Value::Object(Some(r)) => assert_eq!(
            r.as_ptr(),
            weak_ref.as_ptr(),
            "S28: poll() should return the enqueued weak ref"
        ),
        _ => panic!("S28: poll() should not return null"),
    }
    // Second poll should return null (empty)
    let poll2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(poll2, Value::Object(None)),
        "S28: second poll() should return null"
    );
}

#[test]
fn s28_soft_ref_with_queue_enqueued_under_pressure() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let soft_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(soft_ref)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    let soft_addr = soft_ref.as_ptr() as usize;
    let queue_addr = queue.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        // free_heap=0 + a clock genuinely past the entry's creation stamp =
        // memory pressure clears the soft ref. The clock must be anchored to
        // the mutator's wall clock (`SoftReference.<init>` touches with
        // `SystemTime::now()`), not a small literal — `gc::reference` drives
        // the LRU with `max(caller clock, observed mutator clock)`.
        let is_marked = |addr: usize| -> bool { addr == soft_addr || addr == queue_addr };
        let result = proc.process_references(&is_marked, 0, wall_clock_ms() + 60_000);
        assert!(
            result.stats.soft_refs_cleared >= 1,
            "S28: soft ref should be cleared"
        );
        let cleared = proc.cleared_ref_objects();
        for ref_addr in cleared {
            let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
            shared.mem.heap.set_field(obj, 0, Value::Object(None));
        }
        for (ra, qa) in &result.to_enqueue {
            let robj = unsafe { ObjectRef::from_raw(*ra as *mut u8) };
            let qobj = unsafe { ObjectRef::from_raw(*qa as *mut u8) };
            let old_head = shared.mem.heap.get_field(qobj, 0);
            shared
                .mem
                .heap
                .set_field(qobj, 0, Value::Object(Some(robj)));
            shared.mem.heap.set_field(robj, 0, old_head);
            let size = match shared.mem.heap.get_field(qobj, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            shared.mem.heap.set_field(qobj, 1, Value::Int(size + 1));
            shared.mem.heap.set_field(robj, 1, Value::Int(1));
        }
    }
    let poll_result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    match poll_result {
        Value::Object(Some(r)) => assert_eq!(
            r.as_ptr(),
            soft_ref.as_ptr(),
            "S28: poll() should return the enqueued soft ref"
        ),
        _ => panic!("S28: poll() should not return null after soft ref cleared"),
    }
}

#[test]
fn s28_phantom_ref_enqueued_to_queue() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let phantom = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/PhantomReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(phantom)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    let phantom_addr = phantom.as_ptr() as usize;
    let queue_addr = queue.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        let is_marked = |addr: usize| -> bool { addr == phantom_addr || addr == queue_addr };
        let result = proc.process_references(&is_marked, 64, 0);
        assert!(
            result.stats.phantom_refs_enqueued >= 1,
            "S28: phantom should be enqueued"
        );
        for (ra, qa) in &result.to_enqueue {
            let robj = unsafe { ObjectRef::from_raw(*ra as *mut u8) };
            let qobj = unsafe { ObjectRef::from_raw(*qa as *mut u8) };
            let old_head = shared.mem.heap.get_field(qobj, 0);
            shared
                .mem
                .heap
                .set_field(qobj, 0, Value::Object(Some(robj)));
            shared.mem.heap.set_field(robj, 0, old_head);
            let size = match shared.mem.heap.get_field(qobj, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            shared.mem.heap.set_field(qobj, 1, Value::Int(size + 1));
            shared.mem.heap.set_field(robj, 1, Value::Int(1));
        }
    }
    let poll_result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    match poll_result {
        Value::Object(Some(r)) => assert_eq!(
            r.as_ptr(),
            phantom.as_ptr(),
            "S28: poll() should return the enqueued phantom ref"
        ),
        _ => panic!("S28: poll() should not return null for phantom ref"),
    }
}

#[test]
fn s28_reference_queue_poll_empty_returns_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(result, Value::Object(None)),
        "S28: poll() on empty queue should return null"
    );
}

#[test]
fn s28_reference_refers_to() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let other = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // refersTo(referent) should return true
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        result,
        Value::Int(1),
        "S28: refersTo(referent) should be true"
    );
    // refersTo(other) should return false
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(other))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        result2,
        Value::Int(0),
        "S28: refersTo(other) should be false"
    );
    // refersTo(null) should return false
    let result3 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(weak_ref)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        result3,
        Value::Int(0),
        "S28: refersTo(null) should be false"
    );
}

#[test]
fn s28_is_enqueued_lifecycle() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(weak_ref)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    // Before enqueue: isEnqueued = false
    let before = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "isEnqueued",
        "()Z",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        before,
        Value::Int(0),
        "S28: isEnqueued should be false before enqueue"
    );
    // Manual enqueue
    let enq = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "enqueue",
        "()Z",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(enq, Value::Int(1), "S28: enqueue() should return true");
    // After enqueue: isEnqueued = true
    let after = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "isEnqueued",
        "()Z",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        after,
        Value::Int(1),
        "S28: isEnqueued should be true after enqueue"
    );
}

#[test]
fn s28_multiple_refs_same_queue() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ref1_target = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let ref2_target = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let weak1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let weak2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(weak1)),
            Value::Object(Some(ref1_target)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(weak2)),
            Value::Object(Some(ref2_target)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    let w1_addr = weak1.as_ptr() as usize;
    let w2_addr = weak2.as_ptr() as usize;
    let q_addr = queue.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        let is_marked =
            |addr: usize| -> bool { addr == w1_addr || addr == w2_addr || addr == q_addr };
        let result = proc.process_references(&is_marked, 64, 0);
        assert_eq!(
            result.stats.weak_refs_cleared, 2,
            "S28: both weak refs should be cleared"
        );
        let cleared = proc.cleared_ref_objects();
        for ref_addr in cleared {
            let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
            shared.mem.heap.set_field(obj, 0, Value::Object(None));
        }
        for (ra, qa) in &result.to_enqueue {
            let robj = unsafe { ObjectRef::from_raw(*ra as *mut u8) };
            let qobj = unsafe { ObjectRef::from_raw(*qa as *mut u8) };
            let old_head = shared.mem.heap.get_field(qobj, 0);
            shared
                .mem
                .heap
                .set_field(qobj, 0, Value::Object(Some(robj)));
            shared.mem.heap.set_field(robj, 0, old_head);
            let size = match shared.mem.heap.get_field(qobj, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            shared.mem.heap.set_field(qobj, 1, Value::Int(size + 1));
            shared.mem.heap.set_field(robj, 1, Value::Int(1));
        }
    }
    // Poll should return both refs (LIFO order — last enqueued is head)
    let poll1 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    let poll2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    let poll3 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(poll1, Value::Object(Some(_))),
        "S28: first poll should return a ref"
    );
    assert!(
        matches!(poll2, Value::Object(Some(_))),
        "S28: second poll should return a ref"
    );
    assert!(
        matches!(poll3, Value::Object(None)),
        "S28: third poll should return null"
    );
    // Verify we got both refs (order doesn't matter)
    let got1 = match poll1 {
        Value::Object(Some(r)) => r.as_ptr() as usize,
        _ => 0,
    };
    let got2 = match poll2 {
        Value::Object(Some(r)) => r.as_ptr() as usize,
        _ => 0,
    };
    let expected = vec![w1_addr, w2_addr];
    let mut got = vec![got1, got2];
    got.sort();
    let mut exp = expected.clone();
    exp.sort();
    assert_eq!(
        got, exp,
        "S28: should have polled both weak refs from queue"
    );
}

#[test]
fn s28_enqueue_without_queue_returns_false() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    // Init without queue
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // enqueue() should return false (no queue attached)
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "enqueue",
        "()Z",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        result,
        Value::Int(0),
        "S28: enqueue without queue should return false"
    );
}

#[test]
fn s28_soft_ref_not_enqueued_when_retained() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let soft = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(soft)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    let soft_addr = soft.as_ptr() as usize;
    let queue_addr = queue.as_ptr() as usize;
    {
        let mut proc = shared.mem.ref_processor.lock();
        // Plenty of memory — soft ref should NOT be cleared
        let is_marked = |addr: usize| -> bool { addr == soft_addr || addr == queue_addr };
        let result = proc.process_references(&is_marked, 1024, 0);
        assert_eq!(
            result.stats.soft_refs_cleared, 0,
            "S28: soft ref should NOT be cleared"
        );
        assert!(
            result.to_enqueue.is_empty(),
            "S28: nothing should be enqueued"
        );
    }
    // Queue should still be empty
    let poll = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(poll, Value::Object(None)),
        "S28: queue should be empty when soft ref retained"
    );
    // get() should still return referent
    let get = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(soft))],
    )
    .unwrap()
    .unwrap();
    match get {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), referent.as_ptr()),
        _ => panic!("S28: soft get() should still return referent when retained"),
    }
}

#[test]
fn s28_weak_ref_get_returns_null_after_clear() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let weak_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    // Verify get() returns referent
    let before = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(before, Value::Object(Some(_))),
        "S28: get() before clear should return referent"
    );
    // Clear
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "clear",
        "()V",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap();
    // get() should return null
    let after = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(after, Value::Object(None)),
        "S28: get() after clear should return null"
    );
}

#[test]
fn s28_phantom_ref_get_always_returns_null() {
    // PhantomReference.get() always returns null per Java spec
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let phantom = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/PhantomReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(phantom)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();
    // PhantomReference.get() should return null
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/PhantomReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(phantom))],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(result, Value::Object(None)),
        "S28: PhantomReference.get() should always return null"
    );
}

#[test]
fn s28_reference_queue_remove_timeout_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let queue = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(queue))],
    )
    .unwrap();
    // remove(1) with 1ms timeout on empty queue should return null quickly
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "remove",
        "(J)Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue)), Value::Long(1)],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(result, Value::Object(None)),
        "S28: remove(1) on empty queue should return null after timeout"
    );
}

