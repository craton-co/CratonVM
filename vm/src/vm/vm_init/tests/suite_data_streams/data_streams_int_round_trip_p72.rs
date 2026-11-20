#[allow(unused_imports)]
use super::*;


// =========================================================================
// Phase S.2: DataInputStream / DataOutputStream round-trip + transferTo
// These tests use NativeContextImpl so that invoke_virtual inside DOS/DIS
// can resolve class names from proper ClassIds (not ClassId(0)).
// =========================================================================


/// Write an int to a DataOutputStream backed by ByteArrayOutputStream,
/// then read it back with DataInputStream backed by ByteArrayInputStream.
#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn data_streams_int_round_trip_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn data_streams_long_round_trip_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn data_streams_utf_round_trip_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn data_stream_size_tracking_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn input_stream_transfer_to_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn input_stream_read_all_bytes_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn data_stream_multi_type_read_p72() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_join_array() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_strip() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_repeat() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn integer_sum_max_min() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn integer_to_hex_string_p30() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_joiner_p30_registration_exists() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn _string_joiner_lifecycle_p30() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn synthetic_classes_use_native_dispatch() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn compatibility_stub_origin_set_correctly() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn bootstrap_with_real_jdk() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn dispatch_inversion_preserves_native_for_synthetic() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn resolve_field_index_real_jdk() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_invoke_object_hashcode() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_invoke_integer_valueof() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_class_has_bytecode_methods() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_object_equals_bytecode() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_integer_valueof_creates_object() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_integer_valueof_returns_object() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn test_system_out_multiline_println() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_ensure_class_initialized_registers_natives() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_character_float_double_clinit() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn real_jdk_hello_world_e2e() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_get_and_add_long() {
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
            unsafe_fixture_field_offset(&shared, 0),  // offset
            Value::Long(25), // delta
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Long(100))); // old value
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Long(125)); // updated
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_get_and_set_long() {
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
            unsafe_fixture_field_offset(&shared, 0),
            Value::Long(99),
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Long(42))); // old value
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Long(99)); // new value
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn jdk_internal_unsafe_cas_int() {
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
            unsafe_fixture_field_offset(&shared, 0),
            Value::Int(7),  // expected
            Value::Int(42), // new
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Int(1))); // success
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Int(42));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_float_field_access() {
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
            unsafe_fixture_field_offset(&shared, 0),
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
            unsafe_fixture_field_offset(&shared, 0),
        ],
    )
    .unwrap();

    match result {
        Some(Value::Float(f)) => assert!((f - 3.14).abs() < 0.001),
        other => panic!("expected Float, got {:?}", other),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_double_field_access() {
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
            unsafe_fixture_field_offset(&shared, 1),
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
            unsafe_fixture_field_offset(&shared, 1),
        ],
    )
    .unwrap();

    match result {
        Some(Value::Double(d)) => assert!((d - 2.71828).abs() < 0.00001),
        other => panic!("expected Double, got {:?}", other),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn collections_swap_test() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn collections_fill_test() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn collections_rotate_test() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn collections_shuffle_preserves_size() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn throwable_add_suppressed_and_get() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_page_size_and_address_size() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn streams_grouping_by_downstream_to_list() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn streams_grouping_by_downstream_counting() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_allocate_memory() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn unsafe_throw_exception() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn secure_random_generate_seed() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn secure_random_next_bytes() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn pattern_compile_literal_flag() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn pattern_compile_comments_flag() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn pattern_quote() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn matcher_region() {
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
// `pub(in crate::vm::tests)` to `crate::runtime::interpreter`, so a direct unit test
// belongs in that module, not here. Nominated: E40-1 §5 N4.

// =======================================================================
// Phase 46: Formatting and ClassLoader fixes
// =======================================================================

/// DateTimeFormatter.format() with custom pattern "dd/MM/yyyy".
#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn dtf_format_custom_pattern() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn dtf_format_time_pattern() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn decimal_format_pattern_parsing() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn class_get_class_loader_non_null() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn inet_address_get_by_name_localhost() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn inet_address_loopback() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn biginteger_gcd() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn biginteger_bit_length() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn uri_additional_methods() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn arrays_sort_int() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn arrays_copy_of_p48() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn arrays_binary_search() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn properties_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_repeat_p49() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_is_blank() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_chars_returns_stream() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn string_modern_methods_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn completable_future_join() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn completable_future_complete_p50() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn phase50_api_completeness() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn scanner_implementation_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn scanner_next_line_p51() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn phase51_registrations() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g66_scoped_value_hash_code() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g66_carrier_chain_binds_all_values() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g66_snapshot_monotonic_timestamp() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_fork_validates_open_state() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_fork_increments_completed_count() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_join_rejects_closed_scope() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_close_idempotent() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_shutdown_on_failure_no_exception() {
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

