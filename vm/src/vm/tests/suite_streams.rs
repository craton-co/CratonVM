// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// -----------------------------------------------------------------------
// Phase 9 Part 3: Stream API tests
// -----------------------------------------------------------------------

// -- Source tests --

#[test]
fn stream_of_single_element() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "hello");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(1));
}

#[test]
fn stream_of_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    for (i, s) in ["a", "b", "c"].iter().enumerate() {
        let sr = create_java_string(&shared, s);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }

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
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(3));
}

#[test]
fn stream_empty_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "empty",
        "()Ljava/util/stream/Stream;",
        &[],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(0));
}

#[test]
fn arraylist_stream_creates_stream() {
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
    for s in &["x", "y"] {
        let sr = create_java_string(&shared, s);
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(sr))],
        )
        .unwrap();
    }

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(2));
}

// -- Intermediate operation tests --

#[test]
fn stream_filter_keeps_matching() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create stream of 3 elements
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    for (i, s) in ["a", "b", "c"].iter().enumerate() {
        let sr = create_java_string(&shared, s);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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

    // Predicate always returns true: identityHashCode returns non-zero
    let predicate = make_lambda_proxy(
        &shared,
        "java/util/function/Predicate",
        "test",
        "(Ljava/lang/Object;)Z",
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );

    let filtered = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        &[stream, Value::Object(Some(predicate))],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[filtered],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(3));
}

#[test]
fn stream_map_transforms() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s1 = create_java_string(&shared, "hello");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s1))],
    )
    .unwrap()
    .unwrap();

    // Function: identityHashCode — transforms object to Int
    let function = make_lambda_proxy(
        &shared,
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );

    let mapped = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "map",
        "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
        &[stream, Value::Object(Some(function))],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[mapped],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(1));
}

#[test]
fn stream_sorted_natural_order() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    for (i, s) in ["c", "a", "b"].iter().enumerate() {
        let sr = create_java_string(&shared, s);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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
    let sorted = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "sorted",
        "()Ljava/util/stream/Stream;",
        &[sorted_stream_arg(stream)],
    )
    .unwrap()
    .unwrap();
    // Verify first element is "a"
    let first = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "findFirst",
        "()Ljava/util/Optional;",
        &[sorted],
    )
    .unwrap()
    .unwrap();
    let opt_ref = match first {
        Value::Object(Some(r)) => r,
        _ => panic!("expected optional"),
    };
    let val = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(opt_ref))],
    )
    .unwrap()
    .unwrap();
    match val {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "a")
        }
        _ => panic!("expected string"),
    }
}


#[test]
fn stream_distinct_removes_duplicates() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 4);
    let a1 = create_java_string(&shared, "a");
    let a2 = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let a3 = create_java_string(&shared, "a");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(a1)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(a2)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Object(Some(b)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 3, Value::Object(Some(a3)));

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
    let distinct = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "distinct",
        "()Ljava/util/stream/Stream;",
        &[stream],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[distinct],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(2)); // "a" and "b"
}

#[test]
fn stream_limit_truncates() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 5);
    for i in 0..5 {
        let sr = create_java_string(&shared, &format!("e{i}"));
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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
    let limited = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "limit",
        "(J)Ljava/util/stream/Stream;",
        &[stream, Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[limited],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(2));
}

#[test]
fn stream_skip_drops() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 5);
    for i in 0..5 {
        let sr = create_java_string(&shared, &format!("e{i}"));
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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
    let skipped = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "skip",
        "(J)Ljava/util/stream/Stream;",
        &[stream, Value::Long(3)],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[skipped],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(2));
}

// -- Terminal operation tests --

#[test]
fn stream_for_each_consumer() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    for (i, s) in ["hi", "world"].iter().enumerate() {
        let sr = create_java_string(&shared, s);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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

    let ps = alloc_receiver(&shared, &mut thread, "java/io/PrintStream", 0);
    let consumer = make_lambda_proxy(
        &shared,
        "java/util/function/Consumer",
        "accept",
        "(Ljava/lang/Object;)V",
        "java/io/PrintStream",
        "println",
        "(Ljava/lang/String;)V",
        MethodHandleKind::InvokeVirtual,
        vec!['L'],
        &[Value::Object(Some(ps))],
    );

    call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[stream, Value::Object(Some(consumer))],
    )
    .unwrap();

    assert_eq!(thread.printed_lines, vec!["hi", "world"]);
}

#[test]
fn stream_to_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(a)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(b)));
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

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "toArray",
        "()[Ljava/lang/Object;",
        &[stream],
    )
    .unwrap()
    .unwrap();
    let result_arr = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(result_arr), 2);
}

#[test]
fn stream_find_first_present() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "first");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();

    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "findFirst",
        "()Ljava/util/Optional;",
        &[stream],
    )
    .unwrap()
    .unwrap();
    let opt_ref = match opt {
        Value::Object(Some(r)) => r,
        _ => panic!("expected optional"),
    };
    let is_present = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "isPresent",
        "()Z",
        &[Value::Object(Some(opt_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_present, Value::Int(1));
}

#[test]
fn stream_find_first_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "empty",
        "()Ljava/util/stream/Stream;",
        &[],
    )
    .unwrap()
    .unwrap();

    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "findFirst",
        "()Ljava/util/Optional;",
        &[stream],
    )
    .unwrap()
    .unwrap();
    let opt_ref = match opt {
        Value::Object(Some(r)) => r,
        _ => panic!("expected optional"),
    };
    let is_present = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "isPresent",
        "()Z",
        &[Value::Object(Some(opt_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_present, Value::Int(0));
}

#[test]
fn stream_any_match_true() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "x");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();

    // Predicate always returns true
    let predicate = make_lambda_proxy(
        &shared,
        "java/util/function/Predicate",
        "test",
        "(Ljava/lang/Object;)Z",
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "anyMatch",
        "(Ljava/util/function/Predicate;)Z",
        &[stream, Value::Object(Some(predicate))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
fn stream_reduce_with_identity() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Stream of 3 Int values — reduce with identity using a BinaryOperator
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(10));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(20));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(30));

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

    // BinaryOperator: Math.max(int, int) as accumulator
    let operator = make_lambda_proxy(
        &shared,
        "java/util/function/BinaryOperator",
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "java/lang/Math",
        "max",
        "(II)I",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "reduce",
        "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;",
        &[stream, Value::Int(0), Value::Object(Some(operator))],
    )
    .unwrap()
    .unwrap();
    // reduce(T identity, BinaryOperator<T>) returns T (object reference).
    // coerce_return boxes the primitive result to Integer; verify it holds 30.
    match result {
        Value::Object(Some(r)) => assert_eq!(shared.mem.heap.get_field(r, 0), Value::Int(30)),
        Value::Int(n) => assert_eq!(n, 30), // tolerate already-unboxed path
        other => panic!("unexpected reduce result: {:?}", other),
    }
}

// -- Collectors tests --

#[test]
fn stream_collect_to_list() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(a)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(b)));
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

    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "toList",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();

    let list = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, collector],
    )
    .unwrap()
    .unwrap();

    let list_ref = match list {
        Value::Object(Some(r)) => r,
        _ => panic!("expected list"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

#[test]
fn stream_collect_to_set() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = create_java_string(&shared, "x");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();

    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "toSet",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();

    let set = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, collector],
    )
    .unwrap()
    .unwrap();

    let set_ref = match set {
        Value::Object(Some(r)) => r,
        _ => panic!("expected set"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "size",
        "()I",
        &[Value::Object(Some(set_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
fn stream_collect_joining() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    for (i, s) in ["a", "b", "c"].iter().enumerate() {
        let sr = create_java_string(&shared, s);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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

    let delim = create_java_string(&shared, ",");
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "joining",
        "(Ljava/lang/CharSequence;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(delim))],
    )
    .unwrap()
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, collector],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "a,b,c")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn stream_collect_to_map() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Stream of one element, collect to map using key=String.valueOf, value=String.valueOf
    // (identity-like functions that return Object, since HashMap doesn't support Int keys)
    let s = create_java_string(&shared, "x");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();

    let key_fn = make_lambda_proxy(
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
    let val_fn = make_lambda_proxy(
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

    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "toMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(key_fn)), Value::Object(Some(val_fn))],
    )
    .unwrap()
    .unwrap();

    let map = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, collector],
    )
    .unwrap()
    .unwrap();

    let map_ref = match map {
        Value::Object(Some(r)) => r,
        _ => panic!("expected map"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "size",
        "()I",
        &[Value::Object(Some(map_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

// -- IntStream tests --

#[test]
fn int_stream_range_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "range",
        "(II)Ljava/util/stream/IntStream;",
        &[Value::Int(0), Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(5));
}

#[test]
fn int_stream_sum() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "range",
        "(II)Ljava/util/stream/IntStream;",
        &[Value::Int(1), Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "sum",
        "()I",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Int(6)); // 1+2+3 = 6
}

#[test]
fn int_stream_range_closed() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "rangeClosed",
        "(II)Ljava/util/stream/IntStream;",
        &[Value::Int(1), Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(3));

    // Re-create (stream consumed)
    let stream2 = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "rangeClosed",
        "(II)Ljava/util/stream/IntStream;",
        &[Value::Int(1), Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "sum",
        "()I",
        &[stream2],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Int(6)); // 1+2+3 = 6
}

// -- Pipeline tests --

#[test]
fn stream_filter_map_collect() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Stream of ["a", "b", "c"]
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    for (i, s) in ["a", "b", "c"].iter().enumerate() {
        let sr = create_java_string(&shared, s);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(sr)));
    }
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

    // filter (always true)
    let predicate = make_lambda_proxy(
        &shared,
        "java/util/function/Predicate",
        "test",
        "(Ljava/lang/Object;)Z",
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );
    let filtered = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        &[stream, Value::Object(Some(predicate))],
    )
    .unwrap()
    .unwrap();

    // map (identityHashCode — transforms each)
    let function = make_lambda_proxy(
        &shared,
        "java/util/function/Function",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "java/lang/System",
        "identityHashCode",
        "(Ljava/lang/Object;)I",
        MethodHandleKind::InvokeStatic,
        vec![],
        &[],
    );
    let mapped = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "map",
        "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
        &[filtered, Value::Object(Some(function))],
    )
    .unwrap()
    .unwrap();

    // collect(toList())
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "toList",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[mapped, collector],
    )
    .unwrap()
    .unwrap();

    let list_ref = match list {
        Value::Object(Some(r)) => r,
        _ => panic!("expected list"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));
}

#[test]
fn stream_to_list_convenience() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(a)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(b)));
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

    let list = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "toList",
        "()Ljava/util/List;",
        &[stream],
    )
    .unwrap()
    .unwrap();

    let list_ref = match list {
        Value::Object(Some(r)) => r,
        _ => panic!("expected list"),
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

// ===================================================================
// Phase 10: Exception message propagation + Throwable + interfaces
// ===================================================================

#[test]
fn exception_message_propagated() {
    // Create an exception with a message and verify getMessage returns it
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Allocate a Throwable object (2 fields: detailMessage, cause)
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let msg = create_java_string(&shared, "test error message");
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(msg)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "getMessage",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(str_ref)) => {
            let text = read_java_string(&shared.mem.heap, str_ref).unwrap();
            assert_eq!(text, "test error message");
        }
        _ => panic!("expected string, got {:?}", result),
    }
}

#[test]
fn throwable_get_cause_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Throwable with 2 fields: detailMessage=null, cause=null
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "getCause",
        "()Ljava/lang/Throwable;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    assert_eq!(result, Value::Object(None));
}

#[test]
fn throwable_init_cause_sets_cause() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let throwable = alloc_receiver(&shared, &mut thread, "java/lang/Throwable", 2);
    let cause = alloc_receiver(&shared, &mut thread, "java/lang/Throwable", 2);

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(throwable)), Value::Object(Some(cause))],
    )
    .unwrap()
    .unwrap();

    // Should return `this`
    match result {
        Value::Object(Some(r)) => assert!(std::ptr::eq(r.as_ptr(), throwable.as_ptr())),
        _ => panic!("expected this"),
    }

    // getCause should now return cause
    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "getCause",
        "()Ljava/lang/Throwable;",
        &[Value::Object(Some(throwable))],
    )
    .unwrap()
    .unwrap();
    match got {
        Value::Object(Some(r)) => assert!(std::ptr::eq(r.as_ptr(), cause.as_ptr())),
        _ => panic!("expected cause"),
    }
}

#[test]
fn throwable_to_string_with_message() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let msg = create_java_string(&shared, "oops");
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(msg)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(sr)) => {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            // ClassId(0) has no class name in the store, so it'll use a fallback
            // The toString should contain ": oops"
            assert!(
                text.contains("oops"),
                "toString should contain message, got: {text}"
            );
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn throwable_to_string_no_message() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    // no message set — field 0 defaults to uninitialized

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(sr)) => {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            // Should be just the class name, no ": "
            assert!(
                !text.contains(": "),
                "no-message toString shouldn't have ': ', got: {text}"
            );
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn throwable_print_stack_trace() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let msg = create_java_string(&shared, "test error");
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(msg)));

    // Should not crash
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "printStackTrace",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert!(result.is_none());

    // Check that something was printed
    assert!(
        !thread.printed_lines.is_empty(),
        "should have printed at least one line"
    );
}

#[test]
fn throwable_get_suppressed_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "getSuppressed",
        "()[Ljava/lang/Throwable;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(arr)) => {
            let len = shared.mem.heap.array_length(arr);
            assert_eq!(len, 0);
        }
        _ => panic!("expected empty array"),
    }
}

#[test]
fn throwable_get_stack_trace_array_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Throwable",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(arr)) => {
            let len = shared.mem.heap.array_length(arr);
            assert_eq!(len, 0);
        }
        _ => panic!("expected empty array"),
    }
}

// --- Collection interface method dispatch ---

#[test]
fn collection_interface_size() {
    // Calling "java/util/Collection" "size" should work on an ArrayList
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

    // Add an element
    let s = create_java_string(&shared, "hello");
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(s))],
    )
    .unwrap();

    // Call via Collection interface
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Collection",
        "size",
        "()I",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
fn collection_interface_iterator() {
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
    let s = create_java_string(&shared, "x");
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(s))],
    )
    .unwrap();

    // Iterator via Collection interface
    let iter = call_native(
        &shared,
        &mut thread,
        "java/util/Collection",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    .unwrap();
    let iter_ref = match iter {
        Value::Object(Some(r)) => r,
        _ => panic!("expected iterator"),
    };

    let has = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList$Itr",
        "hasNext",
        "()Z",
        &[Value::Object(Some(iter_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(1));
}

#[test]
fn list_interface_get() {
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
    let s = create_java_string(&shared, "val");
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(s))],
    )
    .unwrap();

    // Get via List interface
    let elem = call_native(
        &shared,
        &mut thread,
        "java/util/List",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    match elem {
        Value::Object(Some(r)) => {
            let text = read_java_string(&shared.mem.heap, r).unwrap();
            assert_eq!(text, "val");
        }
        _ => panic!("expected string"),
    }
}

// --- Copy constructors ---

#[test]
fn arraylist_copy_constructor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create source list with 2 elements
    let src = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(src))],
    )
    .unwrap();
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(b))],
    )
    .unwrap();

    // Create copy via copy constructor
    let copy = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "(Ljava/util/Collection;)V",
        &[Value::Object(Some(copy)), Value::Object(Some(src))],
    )
    .unwrap();

    // Verify size
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(copy))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));

    // Verify elements
    let e0 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(copy)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    match e0 {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "a")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn hashset_copy_constructor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create source list with elements
    let src = alloc_receiver(&shared, &mut thread, "java/util/ArrayList", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(src))],
    )
    .unwrap();
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(b))],
    )
    .unwrap();

    // Create HashSet from the list
    let set = alloc_receiver(&shared, &mut thread, "java/util/HashSet", 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "<init>",
        "(Ljava/util/Collection;)V",
        &[Value::Object(Some(set)), Value::Object(Some(src))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "size",
        "()I",
        &[Value::Object(Some(set))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

#[test]
fn hashmap_copy_constructor() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create source map with one entry
    let src = alloc_receiver(&shared, &mut thread, "java/util/HashMap", 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(src))],
    )
    .unwrap();
    let k = create_java_string(&shared, "key");
    let v = create_java_string(&shared, "val");
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(src)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    // Copy constructor
    let copy = alloc_receiver(&shared, &mut thread, "java/util/HashMap", 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "(Ljava/util/Map;)V",
        &[Value::Object(Some(copy)), Value::Object(Some(src))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "size",
        "()I",
        &[Value::Object(Some(copy))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));

    // Verify the key is present
    let k2 = create_java_string(&shared, "key");
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(copy)), Value::Object(Some(k2))],
    )
    .unwrap()
    .unwrap();
    match got {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "val")
        }
        _ => panic!("expected string value"),
    }
}

// --- System.getenv ---

#[test]
fn system_getenv_returns_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // `System.getenv` resolves through `flags::runtime_var`, and
    // `CRATONVM_TEST_VAR` is a *declared* flag — so it is answered from the
    // process-wide snapshot, not from `environ`. `set_var` here therefore
    // only worked if this test happened to be the first thing in this
    // binary to read any flag, which in a test module this size it never
    // is. Override the snapshot for this thread instead.
    let key = create_java_string(&shared, "CRATONVM_TEST_VAR");
    let result = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_TEST_VAR", Some("hello"))],
        || {
            call_native(
                &shared,
                &mut thread,
                "java/lang/System",
                "getenv",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(key))],
            )
            .unwrap()
            .unwrap()
        },
    );

    match result {
        Value::Object(Some(sr)) => {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            assert_eq!(text, "hello");
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn system_getenv_missing_returns_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let key = create_java_string(&shared, "CRATONVM_NONEXISTENT_VAR_12345");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/System",
        "getenv",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();

    assert_eq!(result, Value::Object(None));
}

// --- String.format tests ---

#[test]
fn string_format_s_d_combined() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let fmt = create_java_string(&shared, "%s=%d");

    // Create Object[] with two elements: String "x" and Integer 42
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let x_str = create_java_string(&shared, "x");
    // For %d, it needs a boxed Integer — create one via Integer.valueOf
    let boxed_42 = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(x_str)));
    let _ = shared.mem.heap.set_array_element(arr, 1, boxed_42);

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(fmt)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(sr)) => {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            assert_eq!(text, "x=42");
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_format_specials_phase10() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let fmt = create_java_string(&shared, "100%%\nhello%n");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(fmt)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(sr)) => {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            assert!(
                text.contains("100%"),
                "should contain literal %, got: {text}"
            );
            assert!(text.contains("\n"), "should contain newline, got: {text}");
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_join_with_delimiter() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let delim = create_java_string(&shared, ",");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    let c = create_java_string(&shared, "c");
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
        .set_array_element(arr, 2, Value::Object(Some(c)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "join",
        "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;",
        &[Value::Object(Some(delim)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();

    match result {
        Value::Object(Some(sr)) => {
            let text = read_java_string(&shared.mem.heap, sr).unwrap();
            assert_eq!(text, "a,b,c");
        }
        _ => panic!("expected string"),
    }
}

// --- Map interface dispatch ---

#[test]
fn map_interface_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();

    // Via Map interface
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Map",
        "size",
        "()I",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
fn map_interface_put_and_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();

    let k = create_java_string(&shared, "k");
    let v = create_java_string(&shared, "v");

    // Put via Map interface
    call_native(
        &shared,
        &mut thread,
        "java/util/Map",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    // Get via Map interface
    let k2 = create_java_string(&shared, "k");
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/Map",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(map)), Value::Object(Some(k2))],
    )
    .unwrap()
    .unwrap();

    match got {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "v")
        }
        _ => panic!("expected string"),
    }
}

// ===================================================================
// Phase 11: StringBuilder mutations + PrintStream + String + HashMap null
// ===================================================================



#[test]
fn sb_insert_string_middle() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "hello world");
    let insert = create_java_string(&shared, "beautiful ");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "insert",
        "(ILjava/lang/String;)Ljava/lang/StringBuilder;",
        &[
            Value::Object(Some(sb)),
            Value::Int(6),
            Value::Object(Some(insert)),
        ],
    )
    .unwrap();
    assert_eq!(
        sb_to_string(&shared, &mut thread, sb),
        "hello beautiful world"
    );
}

#[test]
fn sb_delete_range() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "hello world");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "delete",
        "(II)Ljava/lang/StringBuilder;",
        &[Value::Object(Some(sb)), Value::Int(5), Value::Int(11)],
    )
    .unwrap();
    assert_eq!(sb_to_string(&shared, &mut thread, sb), "hello");
}

#[test]
fn sb_delete_char_at() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "abc");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "deleteCharAt",
        "(I)Ljava/lang/StringBuilder;",
        &[Value::Object(Some(sb)), Value::Int(1)],
    )
    .unwrap();
    assert_eq!(sb_to_string(&shared, &mut thread, sb), "ac");
}

#[test]
fn sb_replace_range() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "hello world");
    let repl = create_java_string(&shared, "Rust");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "replace",
        "(IILjava/lang/String;)Ljava/lang/StringBuilder;",
        &[
            Value::Object(Some(sb)),
            Value::Int(6),
            Value::Int(11),
            Value::Object(Some(repl)),
        ],
    )
    .unwrap();
    assert_eq!(sb_to_string(&shared, &mut thread, sb), "hello Rust");
}

#[test]
fn sb_set_char_at() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "abc");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "setCharAt",
        "(IC)V",
        &[
            Value::Object(Some(sb)),
            Value::Int(1),
            Value::Int('X' as i32),
        ],
    )
    .unwrap();
    assert_eq!(sb_to_string(&shared, &mut thread, sb), "aXc");
}

#[test]
fn sb_set_length_truncate() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "hello");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "setLength",
        "(I)V",
        &[Value::Object(Some(sb)), Value::Int(0)],
    )
    .unwrap();
    let len = call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "length",
        "()I",
        &[Value::Object(Some(sb))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(0));
}

#[test]
fn sb_substring() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "hello world");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "substring",
        "(II)Ljava/lang/String;",
        &[Value::Object(Some(sb)), Value::Int(6), Value::Int(11)],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "world")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn sb_index_of() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = make_sb(&shared, &mut thread, "hello world");
    let target = create_java_string(&shared, "world");
    let idx = call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "indexOf",
        "(Ljava/lang/String;)I",
        &[Value::Object(Some(sb)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(idx, Value::Int(6));
}

// --- PrintStream tests ---

#[test]
fn print_boolean_true() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // args[0] = PrintStream (can be null for test), args[1] = boolean
    call_native(
        &shared,
        &mut thread,
        "java/io/PrintStream",
        "print",
        "(Z)V",
        &[Value::Object(None), Value::Int(1)],
    )
    .unwrap();
    assert!(thread.printed_lines.iter().any(|l| l == "true"));
}

#[test]
fn print_double_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    call_native(
        &shared,
        &mut thread,
        "java/io/PrintStream",
        "print",
        "(D)V",
        &[Value::Object(None), Value::Double(1.23)],
    )
    .unwrap();
    assert!(thread.printed_lines.iter().any(|l| l.contains("1.23")));
}

#[test]
fn println_float_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    call_native(
        &shared,
        &mut thread,
        "java/io/PrintStream",
        "println",
        "(F)V",
        &[Value::Object(None), Value::Float(2.5)],
    )
    .unwrap();
    assert!(thread.printed_lines.iter().any(|l| l.contains("2.5")));
}

#[test]
fn printf_basic_format() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let fmt = create_java_string(&shared, "Hello %s!");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    let name = create_java_string(&shared, "World");
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(name)));

    call_native(
        &shared,
        &mut thread,
        "java/io/PrintStream",
        "printf",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        &[
            Value::Object(None),
            Value::Object(Some(fmt)),
            Value::Object(Some(arr)),
        ],
    )
    .unwrap();
    assert!(thread.printed_lines.iter().any(|l| l == "Hello World!"));
}

// --- String modern method tests ---

#[test]
fn string_repeat_method() {
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
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "ababab")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_is_blank_true() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, "   ");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "isBlank",
        "()Z",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
fn string_is_blank_false() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, " a ");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "isBlank",
        "()Z",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(0));
}

#[test]
fn string_chars_stream() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s = create_java_string(&shared, "abc");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "chars",
        "()Ljava/util/stream/IntStream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    // Get the count
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "count",
        "()J",
        &[stream],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(3));
}

#[test]
fn string_region_matches_ignore_case() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let s1 = create_java_string(&shared, "Hello");
    let s2 = create_java_string(&shared, "hello");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "regionMatches",
        "(ZILjava/lang/String;II)Z",
        &[
            Value::Object(Some(s1)),
            Value::Int(1),
            Value::Int(0),
            Value::Object(Some(s2)),
            Value::Int(0),
            Value::Int(5),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

// --- HashMap null-key tests ---

#[test]
fn hashmap_null_key_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();

    let val = create_java_string(&shared, "null_value");
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(None),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    let got = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(map)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();

    match got {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "null_value")
        }
        _ => panic!("expected string, got {:?}", got),
    }
}

#[test]
fn hashmap_null_key_overwrite() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();

    let a = create_java_string(&shared, "a");
    let b = create_java_string(&shared, "b");
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(None),
            Value::Object(Some(a)),
        ],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(None),
            Value::Object(Some(b)),
        ],
    )
    .unwrap();

    let got = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(map)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();

    match got {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "b")
        }
        _ => panic!("expected string"),
    }

    // Size should be 1 (overwritten, not added)
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "size",
        "()I",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
fn hashmap_null_key_contains() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(map))],
    )
    .unwrap();

    // Not present yet
    let before = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(map)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(before, Value::Int(0));

    // Add null key
    let val = create_java_string(&shared, "x");
    call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(map)),
            Value::Object(None),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    let after = call_native(
        &shared,
        &mut thread,
        "java/util/HashMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(map)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(after, Value::Int(1));
}

// =========================================================================
// Phase 12: Number wrapper cross-type conversion tests
// =========================================================================

#[test]
fn integer_cross_type_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let int_obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(int_obj, 0, Value::Int(42));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "doubleValue",
        "()D",
        &[Value::Object(Some(int_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(42.0));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "longValue",
        "()J",
        &[Value::Object(Some(int_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(42));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "floatValue",
        "()F",
        &[Value::Object(Some(int_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Float(42.0));
}

#[test]
fn long_cross_type_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Long(100_000));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Long",
        "intValue",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(100_000));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Long",
        "doubleValue",
        "()D",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(100_000.0));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Long",
        "floatValue",
        "()F",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Float(100_000.0));
}

#[test]
fn float_cross_type_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Float(3.5));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Float",
        "intValue",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(3));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Float",
        "longValue",
        "()J",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(3));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Float",
        "doubleValue",
        "()D",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(3.5));
}

#[test]
fn double_cross_type_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Double(2.99));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Double",
        "intValue",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(2));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Double",
        "longValue",
        "()J",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(2));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Double",
        "floatValue",
        "()F",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Float(2.99));
}

#[test]
fn byte_cross_type_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Int(127));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Byte",
        "intValue",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(127));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Byte",
        "longValue",
        "()J",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(127));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Byte",
        "doubleValue",
        "()D",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(127.0));
}

#[test]
fn short_cross_type_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Int(1000));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Short",
        "intValue",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1000));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Short",
        "longValue",
        "()J",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(1000));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Short",
        "doubleValue",
        "()D",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(1000.0));
}

// =========================================================================
// Phase 12: Wrapper instance toString / hashCode / equals tests
// =========================================================================

#[test]
fn integer_instance_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Int(42));
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "42")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn long_instance_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(obj, 0, Value::Long(999_999_999_999));
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Long",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, sr).unwrap(),
                "999999999999"
            )
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn boolean_instance_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let t = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(t, 0, Value::Int(1));
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(t))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "true")
        }
        _ => panic!("expected string"),
    }

    let f = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(f, 0, Value::Int(0));
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(f))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "false")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn character_instance_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Int('A' as i32));
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "A")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn integer_hash_code_and_equals() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(a, 0, Value::Int(42));
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(b, 0, Value::Int(42));
    let c = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(c, 0, Value::Int(43));

    // hashCode = 42
    let h = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "hashCode",
        "()I",
        &[Value::Object(Some(a))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h, Value::Int(42));

    // equals(b) = true
    let eq = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));

    // equals(c) = false
    let eq = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(a)), Value::Object(Some(c))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(0));
}

#[test]
fn long_hash_code() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(obj, 0, Value::Long(42));
    let h = call_native(
        &shared,
        &mut thread,
        "java/lang/Long",
        "hashCode",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    // For 42L: high 32 bits = 0, low 32 = 42, hash = 42 ^ 0 = 42
    assert_eq!(h, Value::Int(42));
}

#[test]
fn boolean_hash_code_values() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let t = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(t, 0, Value::Int(1));
    let h = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "hashCode",
        "()I",
        &[Value::Object(Some(t))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h, Value::Int(1231));

    let f = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(f, 0, Value::Int(0));
    let h = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "hashCode",
        "()I",
        &[Value::Object(Some(f))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h, Value::Int(1237));
}

#[test]
fn double_equals_with_nan() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(a, 0, Value::Double(f64::NAN));
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(b, 0, Value::Double(f64::NAN));

    // Double.equals: NaN == NaN is true (unlike ==)
    let eq = call_native(
        &shared,
        &mut thread,
        "java/lang/Double",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
}

#[test]
fn float_equals_with_nan() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(a, 0, Value::Float(f32::NAN));
    let b = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(b, 0, Value::Float(f32::NAN));

    let eq = call_native(
        &shared,
        &mut thread,
        "java/lang/Float",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
}

// =========================================================================
// Phase 12: Character additional method tests
// =========================================================================

#[test]
fn character_digit_and_for_digit() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Character.digit('A', 16) = 10
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "digit",
        "(CI)I",
        &[Value::Int('A' as i32), Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(10));

    // Character.forDigit(10, 16) = 'a'
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "forDigit",
        "(II)C",
        &[Value::Int(10), Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('a' as i32));

    // Character.charCount(0x10000) = 2 (supplementary)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "charCount",
        "(I)I",
        &[Value::Int(0x10000)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(2));

    // Character.charCount(65) = 1 (BMP)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "charCount",
        "(I)I",
        &[Value::Int(65)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));
}

#[test]
fn character_surrogates_and_bmp() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // isHighSurrogate(0xD800) = true
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isHighSurrogate",
        "(C)Z",
        &[Value::Int(0xD800)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // isLowSurrogate(0xDC00) = true
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isLowSurrogate",
        "(C)Z",
        &[Value::Int(0xDC00)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // isBmpCodePoint(65) = true
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isBmpCodePoint",
        "(I)Z",
        &[Value::Int(65)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // isBmpCodePoint(0x10000) = false
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isBmpCodePoint",
        "(I)Z",
        &[Value::Int(0x10000)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));
}

// =========================================================================
// Phase 12: Boolean additional method tests
// =========================================================================

#[test]
fn boolean_parse_boolean() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s_true = create_java_string(&shared, "true");
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "parseBoolean",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(s_true))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    let s_true_upper = create_java_string(&shared, "True");
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "parseBoolean",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(s_true_upper))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    let s_false = create_java_string(&shared, "false");
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "parseBoolean",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(s_false))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));

    let s_other = create_java_string(&shared, "yes");
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "parseBoolean",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(s_other))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));
}

#[test]
fn boolean_static_methods() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Boolean.toString(true) = "true"
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "toString",
        "(Z)Ljava/lang/String;",
        &[Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "true")
        }
        _ => panic!("expected string"),
    }

    // Boolean.compare(true, false) = 1
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "compare",
        "(ZZ)I",
        &[Value::Int(1), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // Boolean.compare(false, true) = -1
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Boolean",
        "compare",
        "(ZZ)I",
        &[Value::Int(0), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(-1));
}

// =========================================================================
// Phase 12: Comparator factory tests
// =========================================================================

#[test]
fn comparator_natural_order_sort() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create ArrayList with ["z", "a", "m"]
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
    for s in &["z", "a", "m"] {
        let sr = create_java_string(&shared, s);
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(sr))],
        )
        .unwrap();
    }

    // Get Comparator.naturalOrder()
    let cmp = call_native(
        &shared,
        &mut thread,
        "java/util/Comparator",
        "naturalOrder",
        "()Ljava/util/Comparator;",
        &[],
    )
    .unwrap()
    .unwrap();
    let cmp_ref = match cmp {
        Value::Object(Some(r)) => r,
        _ => panic!("expected comparator"),
    };

    // Sort with natural order
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "sort",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(list)), Value::Object(Some(cmp_ref))],
    )
    .unwrap();

    // Verify: ["a", "m", "z"]
    for (i, expected) in ["a", "m", "z"].iter().enumerate() {
        let val = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i as i32)],
        )
        .unwrap()
        .unwrap();
        match val {
            Value::Object(Some(sr)) => {
                assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), *expected)
            }
            _ => panic!("expected string at index {i}"),
        }
    }
}

#[test]
fn comparator_reverse_order_sort() {
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
    for s in &["z", "a", "m"] {
        let sr = create_java_string(&shared, s);
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(sr))],
        )
        .unwrap();
    }

    let cmp = call_native(
        &shared,
        &mut thread,
        "java/util/Comparator",
        "reverseOrder",
        "()Ljava/util/Comparator;",
        &[],
    )
    .unwrap()
    .unwrap();
    let cmp_ref = match cmp {
        Value::Object(Some(r)) => r,
        _ => panic!("expected comparator"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "sort",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(list)), Value::Object(Some(cmp_ref))],
    )
    .unwrap();

    // Verify: ["z", "m", "a"]
    for (i, expected) in ["z", "m", "a"].iter().enumerate() {
        let val = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i as i32)],
        )
        .unwrap()
        .unwrap();
        match val {
            Value::Object(Some(sr)) => {
                assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), *expected)
            }
            _ => panic!("expected string at index {i}"),
        }
    }
}

#[test]
fn comparator_reversed_method() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Get naturalOrder then reversed
    let nat = call_native(
        &shared,
        &mut thread,
        "java/util/Comparator",
        "naturalOrder",
        "()Ljava/util/Comparator;",
        &[],
    )
    .unwrap()
    .unwrap();
    let nat_ref = match nat {
        Value::Object(Some(r)) => r,
        _ => panic!("expected comparator"),
    };
    let rev = call_native(
        &shared,
        &mut thread,
        "java/util/Comparator",
        "reversed",
        "()Ljava/util/Comparator;",
        &[Value::Object(Some(nat_ref))],
    )
    .unwrap()
    .unwrap();
    let rev_ref = match rev {
        Value::Object(Some(r)) => r,
        _ => panic!("expected comparator"),
    };

    // Create list and sort
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
    for s in &["b", "a", "c"] {
        let sr = create_java_string(&shared, s);
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(sr))],
        )
        .unwrap();
    }

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "sort",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(list)), Value::Object(Some(rev_ref))],
    )
    .unwrap();

    // Reversed natural order: ["c", "b", "a"]
    for (i, expected) in ["c", "b", "a"].iter().enumerate() {
        let val = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i as i32)],
        )
        .unwrap()
        .unwrap();
        match val {
            Value::Object(Some(sr)) => {
                assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), *expected)
            }
            _ => panic!("expected string at index {i}"),
        }
    }
}

#[test]
fn comparator_natural_order_integers() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create ArrayList with Integer wrappers [3, 1, 2]
    let list = alloc_receiver(&shared, &mut thread, "java/util/ArrayList", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    )
    .unwrap();
    for v in [3, 1, 2] {
        // Real `java.lang.Integer`s. `ClassId::new(0)` is no longer
        // `java/lang/Object` — `SharedVm::new` pre-registers a bootstrap
        // stub there — so these elements came out typed as
        // `Enumeration$Impl` and the natural-order comparator's
        // `Comparable` cast failed on them.
        let w = alloc_receiver(&shared, &mut thread, "java/lang/Integer", 1);
        shared.mem.heap.set_field(w, 0, Value::Int(v));
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(w))],
        )
        .unwrap();
    }

    let cmp = call_native(
        &shared,
        &mut thread,
        "java/util/Comparator",
        "naturalOrder",
        "()Ljava/util/Comparator;",
        &[],
    )
    .unwrap()
    .unwrap();
    let cmp_ref = match cmp {
        Value::Object(Some(r)) => r,
        _ => panic!("expected comparator"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "sort",
        "(Ljava/util/Comparator;)V",
        &[Value::Object(Some(list)), Value::Object(Some(cmp_ref))],
    )
    .unwrap();

    // Verify [1, 2, 3]
    for (i, expected) in [1, 2, 3].iter().enumerate() {
        let val = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i as i32)],
        )
        .unwrap()
        .unwrap();
        match val {
            Value::Object(Some(wr)) => {
                let inner = shared.mem.heap.get_field(wr, 0);
                assert_eq!(inner, Value::Int(*expected));
            }
            _ => panic!("expected wrapper at index {i}"),
        }
    }
}

// =========================================================================
// Phase 12: Collection forEach tests
// =========================================================================

#[test]
fn iterable_for_each() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create ArrayList with [10, 20, 30]
    let list = alloc_receiver(&shared, &mut thread, "java/util/ArrayList", 2);
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
        let w = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        shared.mem.heap.set_field(w, 0, Value::Int(v));
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(w))],
        )
        .unwrap();
    }

    // Create a Consumer lambda that calls println(Object) to record output.
    // The captured PrintStream is a real receiver, not `Object(None)`: the
    // proxy dispatches `accept` straight into `PrintStream.println`, and a
    // null receiver there is an NPE rather than an exercise of `forEach`.
    // Same shape as `arraylist_for_each_consumer` above.
    let ps = alloc_receiver(&shared, &mut thread, "java/io/PrintStream", 0);
    let consumer = make_lambda_proxy(
        &shared,
        "java/util/function/Consumer",
        "accept",
        "(Ljava/lang/Object;)V",
        "java/io/PrintStream",
        "println",
        "(Ljava/lang/Object;)V",
        MethodHandleKind::InvokeVirtual,
        vec!['L'],
        &[Value::Object(Some(ps))],
    );

    // Dispatch `forEach` on the CONCRETE collection class, not on the
    // `java/lang/Iterable` interface this test used to name.
    //
    // `call_native` resolves an exact (class, method, descriptor) triple,
    // so naming the interface was really asserting "a native is registered
    // on `java/lang/Iterable.forEach`" — and that registration must not
    // exist. In this VM a single registered native shadows a class's real
    // bytecode at EVERY interpreter dispatch site, so a native on a
    // supertype as broad as `Iterable` would intercept `forEach` for every
    // implementing class in the program, including user classes that
    // override it (the documented "natives on java.util.Abstract* classes
    // intercept user subclasses" defect family). There is no
    // "only if the receiver declares no override" admission gate in the
    // dispatcher to make it safe. `java/util/ArrayList.forEach` is the
    // registration that legitimately exists (`native_al_for_each`), and
    // exercising it covers the same behaviour: a `Consumer` SAM invoked
    // once per element with the list left intact.
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        &[Value::Object(Some(list)), Value::Object(Some(consumer))],
    )
    .unwrap();

    // forEach must not disturb the list: still 3 elements, still [10,20,30].
    assert_eq!(shared.mem.heap.get_field(list, 1), Value::Int(3));
    for (i, expected) in [10, 20, 30].iter().enumerate() {
        let val = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i as i32)],
        )
        .unwrap()
        .unwrap();
        match val {
            Value::Object(Some(w)) => {
                assert_eq!(shared.mem.heap.get_field(w, 0), Value::Int(*expected));
            }
            other => panic!("expected wrapper at index {i}, got {other:?}"),
        }
    }
}

#[test]
fn character_get_numeric_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Character.getNumericValue('A') = 10
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "getNumericValue",
        "(C)I",
        &[Value::Int('A' as i32)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(10));

    // Character.getNumericValue('5') = 5
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "getNumericValue",
        "(C)I",
        &[Value::Int('5' as i32)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(5));

    // Character.toString('X') = "X"
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "toString",
        "(C)Ljava/lang/String;",
        &[Value::Int('X' as i32)],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "X")
        }
        _ => panic!("expected string"),
    }
}

// ===================================================================
// Phase 13: Math completeness + Runtime + StringJoiner + Map.Entry
// ===================================================================

#[test]
fn math_add_exact_overflow() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Normal case
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "addExact",
        "(II)I",
        &[Value::Int(100), Value::Int(200)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(300));

    // Overflow
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "addExact",
        "(II)I",
        &[Value::Int(i32::MAX), Value::Int(1)],
    );
    assert!(r.is_err());

    // Long normal
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "addExact",
        "(JJ)J",
        &[Value::Long(100), Value::Long(200)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(300));

    // Long overflow
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "addExact",
        "(JJ)J",
        &[Value::Long(i64::MAX), Value::Long(1)],
    );
    assert!(r.is_err());
}

#[test]
fn math_subtract_exact_overflow() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "subtractExact",
        "(II)I",
        &[Value::Int(100), Value::Int(30)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(70));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "subtractExact",
        "(II)I",
        &[Value::Int(i32::MIN), Value::Int(1)],
    );
    assert!(r.is_err());
}

#[test]
fn math_multiply_exact_overflow() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "multiplyExact",
        "(II)I",
        &[Value::Int(100), Value::Int(200)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(20000));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "multiplyExact",
        "(II)I",
        &[Value::Int(i32::MAX), Value::Int(2)],
    );
    assert!(r.is_err());

    // Long
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "multiplyExact",
        "(JJ)J",
        &[Value::Long(i64::MAX), Value::Long(2)],
    );
    assert!(r.is_err());
}

#[test]
fn math_increment_decrement_negate_exact() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // incrementExact
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "incrementExact",
        "(I)I",
        &[Value::Int(41)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(42));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "incrementExact",
        "(I)I",
        &[Value::Int(i32::MAX)],
    );
    assert!(r.is_err());

    // decrementExact
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "decrementExact",
        "(I)I",
        &[Value::Int(43)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(42));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "decrementExact",
        "(I)I",
        &[Value::Int(i32::MIN)],
    );
    assert!(r.is_err());

    // negateExact
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "negateExact",
        "(I)I",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(-42));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "negateExact",
        "(I)I",
        &[Value::Int(i32::MIN)],
    );
    assert!(r.is_err());

    // Long variants
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "incrementExact",
        "(J)J",
        &[Value::Long(i64::MAX)],
    );
    assert!(r.is_err());

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "decrementExact",
        "(J)J",
        &[Value::Long(i64::MIN)],
    );
    assert!(r.is_err());

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "negateExact",
        "(J)J",
        &[Value::Long(i64::MIN)],
    );
    assert!(r.is_err());
}

#[test]
fn math_floor_div_and_mod() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // floorDiv(7, 2) = 3
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorDiv",
        "(II)I",
        &[Value::Int(7), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(3));

    // floorDiv(-7, 2) = -4 (rounds toward negative infinity)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorDiv",
        "(II)I",
        &[Value::Int(-7), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(-4));

    // floorDiv(7, -2) = -4
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorDiv",
        "(II)I",
        &[Value::Int(7), Value::Int(-2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(-4));

    // floorMod(7, 2) = 1
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorMod",
        "(II)I",
        &[Value::Int(7), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // floorMod(-7, 2) = 1 (always same sign as divisor)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorMod",
        "(II)I",
        &[Value::Int(-7), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // Long variants
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorDiv",
        "(JJ)J",
        &[Value::Long(-7), Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(-4));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "floorMod",
        "(JJ)J",
        &[Value::Long(-7), Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(1));
}

#[test]
fn math_to_int_exact() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "toIntExact",
        "(J)I",
        &[Value::Long(42)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(42));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "toIntExact",
        "(J)I",
        &[Value::Long(i64::MAX)],
    );
    assert!(r.is_err());
}

#[test]
fn math_multiply_high() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // multiplyHigh(2, 3) = 0 (result fits in 64 bits)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "multiplyHigh",
        "(JJ)J",
        &[Value::Long(2), Value::Long(3)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Long(0));

    // multiplyHigh(i64::MAX, 2) should give the high 64 bits
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "multiplyHigh",
        "(JJ)J",
        &[Value::Long(i64::MAX), Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    // i64::MAX * 2 = 18446744073709551614, high 64 bits = 0
    // Actually as i128: 9223372036854775807 * 2 = 18446744073709551614
    // >> 64 = 0 (fits in 64-bit unsigned but as signed it's still 0)
    assert_eq!(r, Value::Long(0));

    // Large values
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "multiplyHigh",
        "(JJ)J",
        &[Value::Long(i64::MAX), Value::Long(i64::MAX)],
    )
    .unwrap()
    .unwrap();
    let expected = ((i64::MAX as i128) * (i64::MAX as i128)) >> 64;
    assert_eq!(r, Value::Long(expected as i64));
}

#[test]
fn math_hypot() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "hypot",
        "(DD)D",
        &[Value::Double(3.0), Value::Double(4.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact: hypot(3, 4) is 5.0 with no rounding at all. The old 1e-10
        // was ~10^6x `Math.hypot`'s own 1-ULP contract.
        Value::Double(v) => assert_eq!(v.to_bits(), 5.0f64.to_bits()),
        _ => panic!("expected double"),
    }
}

#[test]
fn math_sinh_cosh_tanh() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "sinh",
        "(D)D",
        &[Value::Double(0.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(0.0));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "cosh",
        "(D)D",
        &[Value::Double(0.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(1.0));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "tanh",
        "(D)D",
        &[Value::Double(0.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(0.0));

    // sinh(1.0) ≈ 1.1752
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "sinh",
        "(D)D",
        &[Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact, and against FDLIBM — not `f64::sinh`. This assertion used
        // to read `1.0_f64.sinh()`, under a comment asserting that
        // `Math.sinh` "must not" be the fdlibm body. That was backwards:
        // JDK 25's `Math.sinh` is a one-line `return StrictMath.sinh(x);`
        // and HotSpot has no `sinh` intrinsic, so the oracle's `Math.sinh`
        // IS fdlibm — the old assertion froze a 77-in-5400 divergence in
        // place. See the census table above `let strict` in
        // `lang_math.rs`, and the registration-level tests beside it.
        Value::Double(v) => {
            assert_eq!(v.to_bits(), cratonvm_types::fdlibm::sinh(1.0).to_bits())
        }
        _ => panic!("expected double"),
    }
}

#[test]
fn math_copy_sign_and_next_up() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // copySign(1.0, -1.0) = -1.0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "copySign",
        "(DD)D",
        &[Value::Double(1.0), Value::Double(-1.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(-1.0));

    // copySign float
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "copySign",
        "(FF)F",
        &[Value::Float(5.0), Value::Float(-1.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Float(-5.0));

    // nextUp(1.0)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "nextUp",
        "(D)D",
        &[Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact: `nextUp` is specified bit manipulation, not an estimate.
        // The old bound admitted ~4.5e8 ULP of wrong answers above 1.0.
        Value::Double(v) => assert_eq!(v.to_bits(), 1.0f64.next_up().to_bits()),
        _ => panic!("expected double"),
    }

    // nextDown(1.0)
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "nextDown",
        "(D)D",
        &[Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact: `nextDown` is specified bit manipulation.
        Value::Double(v) => assert_eq!(v.to_bits(), 1.0f64.next_down().to_bits()),
        _ => panic!("expected double"),
    }
}

#[test]
fn math_log1p_expm1() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // log1p(0.0) = 0.0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "log1p",
        "(D)D",
        &[Value::Double(0.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(0.0));

    // expm1(0.0) = 0.0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "expm1",
        "(D)D",
        &[Value::Double(0.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(0.0));
}

#[test]
fn math_ulp_and_get_exponent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // ulp(1.0) should be very small positive number
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "ulp",
        "(D)D",
        &[Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact: `ulp(1.0)` is 2^-52 == f64::EPSILON, one specific double.
        // `v > 0.0 && v < 1e-10` passes for around a million wrong answers.
        Value::Double(v) => assert_eq!(v.to_bits(), f64::EPSILON.to_bits()),
        _ => panic!("expected double"),
    }

    // ulp float
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "ulp",
        "(F)F",
        &[Value::Float(1.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact: `ulp(1.0f)` is 2^-23 == f32::EPSILON.
        Value::Float(v) => assert_eq!(v.to_bits(), f32::EPSILON.to_bits()),
        _ => panic!("expected float"),
    }

    // getExponent(1.0) = 0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "getExponent",
        "(D)I",
        &[Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));

    // getExponent(2.0) = 1
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "getExponent",
        "(D)I",
        &[Value::Double(2.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // getExponent(0.5) = -1
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "getExponent",
        "(D)I",
        &[Value::Double(0.5)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(-1));
}

#[test]
fn math_next_after() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // nextAfter(1.0, 2.0) should be slightly > 1.0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "nextAfter",
        "(DD)D",
        &[Value::Double(1.0), Value::Double(2.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact: `nextAfter` is specified bit manipulation, so an
        // ordering-only assertion admits arbitrarily large divergence.
        Value::Double(v) => assert_eq!(v.to_bits(), 1.0f64.next_up().to_bits()),
        _ => panic!("expected double"),
    }

    // nextAfter(1.0, 0.0) should be slightly < 1.0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "nextAfter",
        "(DD)D",
        &[Value::Double(1.0), Value::Double(0.0)],
    )
    .unwrap()
    .unwrap();
    match r {
        // Exact — see the sibling above.
        Value::Double(v) => assert_eq!(v.to_bits(), 1.0f64.next_down().to_bits()),
        _ => panic!("expected double"),
    }

    // nextAfter(1.0, 1.0) = 1.0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "nextAfter",
        "(DD)D",
        &[Value::Double(1.0), Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(1.0));
}

#[test]
fn runtime_available_processors() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // getRuntime() returns a non-null object
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Runtime",
        "getRuntime",
        "()Ljava/lang/Runtime;",
        &[],
    )
    .unwrap()
    .unwrap();
    let runtime = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected runtime object"),
    };

    // availableProcessors() > 0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Runtime",
        "availableProcessors",
        "()I",
        &[Value::Object(Some(runtime))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Int(n) => assert!(n > 0),
        _ => panic!("expected int"),
    }
}

#[test]
fn runtime_memory_methods() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Runtime",
        "getRuntime",
        "()Ljava/lang/Runtime;",
        &[],
    )
    .unwrap()
    .unwrap();
    let runtime = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected runtime object"),
    };

    // maxMemory > 0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Runtime",
        "maxMemory",
        "()J",
        &[Value::Object(Some(runtime))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Long(n) => assert!(n > 0),
        _ => panic!("expected long"),
    }

    // totalMemory > 0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Runtime",
        "totalMemory",
        "()J",
        &[Value::Object(Some(runtime))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Long(n) => assert!(n > 0),
        _ => panic!("expected long"),
    }

    // freeMemory > 0
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Runtime",
        "freeMemory",
        "()J",
        &[Value::Object(Some(runtime))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Long(n) => assert!(n > 0),
        _ => panic!("expected long"),
    }
}

#[test]
fn system_line_separator() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/System",
        "lineSeparator",
        "()Ljava/lang/String;",
        &[],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            let s = read_java_string(&shared.mem.heap, sr).unwrap();
            assert!(!s.is_empty());
            // On Windows it's \r\n, on Unix it's \n
            assert!(s == "\n" || s == "\r\n");
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_joiner_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create StringJoiner with ", " delimiter
    let sj = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string(", ")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj)), Value::Object(Some(delim))],
    )
    .unwrap();

    // Add elements
    let a = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a")
    };
    let b = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let c = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(b))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(c))],
    )
    .unwrap();

    // toString
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "a, b, c")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_joiner_with_prefix_suffix() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sj = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let (delim, prefix, suffix) = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        (
            ctx.create_string(", "),
            ctx.create_string("["),
            ctx.create_string("]"),
        )
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V",
        &[
            Value::Object(Some(sj)),
            Value::Object(Some(delim)),
            Value::Object(Some(prefix)),
            Value::Object(Some(suffix)),
        ],
    )
    .unwrap();

    let x = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("x")
    };
    let y = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("y")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(x))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(y))],
    )
    .unwrap();

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "[x, y]")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_joiner_empty_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sj = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string(", ")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj)), Value::Object(Some(delim))],
    )
    .unwrap();

    // Empty without setEmptyValue: returns ""
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "")
        }
        _ => panic!("expected string"),
    }

    // setEmptyValue
    let ev = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("NONE")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "setEmptyValue",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(ev))],
    )
    .unwrap();

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "NONE")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_joiner_merge() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create two joiners
    let sj1 = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let sj2 = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string(", ")
    };
    let delim2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("-")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj1)), Value::Object(Some(delim))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj2)), Value::Object(Some(delim2))],
    )
    .unwrap();

    let a = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a")
    };
    let b = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let c = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };
    let d = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("d")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj1)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj2)), Value::Object(Some(b))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj2)), Value::Object(Some(c))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj2)), Value::Object(Some(d))],
    )
    .unwrap();

    // merge sj2 into sj1
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "merge",
        "(Ljava/util/StringJoiner;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj1)), Value::Object(Some(sj2))],
    )
    .unwrap();

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sj1))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            let s = read_java_string(&shared.mem.heap, sr).unwrap();
            // sj1 had "a", merge adds "b-c-d" as single element
            assert_eq!(s, "a, b-c-d");
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn map_entry_interface_dispatch() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a HashMap entry (2-field: key, value)
    let entry = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };
    let val = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("world")
    };
    {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.set_field(entry, 0, Value::Object(Some(key)));
        ctx.set_field(entry, 1, Value::Object(Some(val)));
    }

    // getKey via Map$Entry interface
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/Map$Entry",
        "getKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "hello")
        }
        _ => panic!("expected string key"),
    }

    // getValue via Map$Entry interface
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/Map$Entry",
        "getValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "world")
        }
        _ => panic!("expected string value"),
    }
}

#[test]
fn map_entry_abstract_map_simple_entry() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let entry = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let key = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("foo")
    };
    {
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.set_field(entry, 0, Value::Object(Some(key)));
        ctx.set_field(entry, 1, Value::Int(42));
    }

    // getKey via AbstractMap$SimpleEntry
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleEntry",
        "getKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "foo")
        }
        _ => panic!("expected string key"),
    }

    // getValue
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleEntry",
        "getValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(42));
}

#[test]
fn string_joiner_length() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sj = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("-")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        &[Value::Object(Some(sj)), Value::Object(Some(delim))],
    )
    .unwrap();

    let a = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("ab")
    };
    let b = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("cd")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        &[Value::Object(Some(sj)), Value::Object(Some(b))],
    )
    .unwrap();

    // "ab-cd" = 5 chars
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/StringJoiner",
        "length",
        "()I",
        &[Value::Object(Some(sj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(5));
}

// ===================================================================
// Phase 14: Random + UUID + String Extras + OptionalInt/Double + LinkedList
// ===================================================================

#[test]
fn random_seeded_deterministic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create two Randoms with the same seed
    let r1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let r2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "<init>",
        "(J)V",
        &[Value::Object(Some(r1)), Value::Long(42)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "<init>",
        "(J)V",
        &[Value::Object(Some(r2)), Value::Long(42)],
    )
    .unwrap();

    // They should produce the same sequence
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "nextInt",
        "()I",
        &[Value::Object(Some(r1))],
    )
    .unwrap()
    .unwrap();
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "nextInt",
        "()I",
        &[Value::Object(Some(r2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, v2);
}

#[test]
fn random_next_int_bound() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let rng = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "<init>",
        "(J)V",
        &[Value::Object(Some(rng)), Value::Long(123)],
    )
    .unwrap();

    // nextInt(10) should return values in [0, 10)
    for _ in 0..20 {
        let v = call_native(
            &shared,
            &mut thread,
            "java/util/Random",
            "nextInt",
            "(I)I",
            &[Value::Object(Some(rng)), Value::Int(10)],
        )
        .unwrap()
        .unwrap();
        match v {
            Value::Int(n) => assert!((0..10).contains(&n), "nextInt(10) returned {n}"),
            _ => panic!("expected int"),
        }
    }
}

#[test]
fn random_next_double_range() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let rng = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "<init>",
        "(J)V",
        &[Value::Object(Some(rng)), Value::Long(999)],
    )
    .unwrap();

    for _ in 0..20 {
        let v = call_native(
            &shared,
            &mut thread,
            "java/util/Random",
            "nextDouble",
            "()D",
            &[Value::Object(Some(rng))],
        )
        .unwrap()
        .unwrap();
        match v {
            Value::Double(d) => assert!((0.0..1.0).contains(&d), "nextDouble returned {d}"),
            _ => panic!("expected double"),
        }
    }
}

#[test]
fn random_next_boolean() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let rng = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "<init>",
        "(J)V",
        &[Value::Object(Some(rng)), Value::Long(77)],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/Random",
        "nextBoolean",
        "()Z",
        &[Value::Object(Some(rng))],
    )
    .unwrap()
    .unwrap();
    match v {
        Value::Int(n) => assert!(n == 0 || n == 1),
        _ => panic!("expected int"),
    }
}

#[test]
fn uuid_random_and_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "randomUUID",
        "()Ljava/util/UUID;",
        &[],
    )
    .unwrap()
    .unwrap();
    let uuid = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected UUID object"),
    };

    let s = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uuid))],
    )
    .unwrap()
    .unwrap();
    match s {
        Value::Object(Some(sr)) => {
            let str_val = read_java_string(&shared.mem.heap, sr).unwrap();
            // UUID format: 8-4-4-4-12
            let parts: Vec<&str> = str_val.split('-').collect();
            assert_eq!(parts.len(), 5);
            assert_eq!(parts[0].len(), 8);
            assert_eq!(parts[1].len(), 4);
            assert_eq!(parts[2].len(), 4);
            assert_eq!(parts[3].len(), 4);
            assert_eq!(parts[4].len(), 12);
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn uuid_from_string_roundtrip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("550e8400-e29b-41d4-a716-446655440000")
    };

    let uuid = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "fromString",
        "(Ljava/lang/String;)Ljava/util/UUID;",
        &[Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    let uuid_obj = match uuid {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected UUID"),
    };

    let s = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uuid_obj))],
    )
    .unwrap()
    .unwrap();
    match s {
        Value::Object(Some(sr)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, sr).unwrap(),
                "550e8400-e29b-41d4-a716-446655440000"
            );
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn uuid_equals_and_hash_code() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create two UUIDs with the same bits
    let u1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let u2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "<init>",
        "(JJ)V",
        &[Value::Object(Some(u1)), Value::Long(100), Value::Long(200)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "<init>",
        "(JJ)V",
        &[Value::Object(Some(u2)), Value::Long(100), Value::Long(200)],
    )
    .unwrap();

    // equals
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(u1)), Value::Object(Some(u2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    // hashCode should be equal
    let h1 = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "hashCode",
        "()I",
        &[Value::Object(Some(u1))],
    )
    .unwrap()
    .unwrap();
    let h2 = call_native(
        &shared,
        &mut thread,
        "java/util/UUID",
        "hashCode",
        "()I",
        &[Value::Object(Some(u2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn string_code_point_at() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Hello")
    };

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "codePointAt",
        "(I)I",
        &[Value::Object(Some(s)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('H' as i32));

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "codePointAt",
        "(I)I",
        &[Value::Object(Some(s)), Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int('o' as i32));
}

#[test]
fn string_code_point_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Hello")
    };

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "codePointCount",
        "(II)I",
        &[Value::Object(Some(s)), Value::Int(0), Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(5));
}

#[test]
fn string_lines_and_indent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // lines
    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a\nb\nc")
    };
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "lines",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(r, Value::Object(Some(_))));

    // indent
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello\nworld")
    };
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "indent",
        "(I)Ljava/lang/String;",
        &[Value::Object(Some(s2)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            let indented = read_java_string(&shared.mem.heap, sr).unwrap();
            assert!(indented.starts_with("  hello"));
            assert!(indented.contains("  world"));
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn optional_int_of_and_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "of",
        "(I)Ljava/util/OptionalInt;",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    let opt = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected object"),
    };

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "getAsInt",
        "()I",
        &[Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(42));

    let present = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "isPresent",
        "()Z",
        &[Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(present, Value::Int(1));
}

#[test]
fn optional_int_empty_or_else() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "empty",
        "()Ljava/util/OptionalInt;",
        &[],
    )
    .unwrap()
    .unwrap();
    let opt = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected object"),
    };

    let present = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "isPresent",
        "()Z",
        &[Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(present, Value::Int(0));

    // getAsInt should throw
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "getAsInt",
        "()I",
        &[Value::Object(Some(opt))],
    );
    assert!(r.is_err());

    // orElse returns default
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "orElse",
        "(I)I",
        &[Value::Object(Some(opt)), Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(99));
}

#[test]
fn optional_int_is_present_true() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "of",
        "(I)Ljava/util/OptionalInt;",
        &[Value::Int(7)],
    )
    .unwrap()
    .unwrap();
    let opt = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected object"),
    };

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalInt",
        "orElse",
        "(I)I",
        &[Value::Object(Some(opt)), Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(7));
}

#[test]
fn optional_long_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalLong",
        "of",
        "(J)Ljava/util/OptionalLong;",
        &[Value::Long(100)],
    )
    .unwrap()
    .unwrap();
    let opt = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected object"),
    };

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalLong",
        "getAsLong",
        "()J",
        &[Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Long(100));
}

#[test]
fn optional_double_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalDouble",
        "of",
        "(D)Ljava/util/OptionalDouble;",
        &[Value::Double(3.125)],
    )
    .unwrap()
    .unwrap();
    let opt = match r {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected object"),
    };

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalDouble",
        "getAsDouble",
        "()D",
        &[Value::Object(Some(opt))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Double(3.125));
}

#[test]
fn linked_list_add_and_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    // add elements
    for i in 0..5 {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(&format!("item{i}"))
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ll)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    // size should be 5
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "size",
        "()I",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(5));

    // get(0) = "item0"
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(ll)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "item0")
        }
        _ => panic!("expected string"),
    }

    // get(4) = "item4"
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(ll)), Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "item4")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn linked_list_add_first_last() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    let a = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("middle")
    };
    let b = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("first")
    };
    let c = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("last")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Object(Some(a))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "addFirst",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(ll)), Value::Object(Some(b))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "addLast",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(ll)), Value::Object(Some(c))],
    )
    .unwrap();

    // getFirst = "first"
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "getFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "first")
        }
        _ => panic!("expected string"),
    }

    // getLast = "last"
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "getLast",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    match r {
        Value::Object(Some(sr)) => {
            assert_eq!(read_java_string(&shared.mem.heap, sr).unwrap(), "last")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn linked_list_remove_first_last() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    for i in 1..=3 {
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ll)), Value::Int(i * 10)],
        )
        .unwrap();
    }

    // removeFirst returns 10
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "removeFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(10));

    // removeLast returns 30
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "removeLast",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(30));

    // size should be 1
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "size",
        "()I",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));
}

#[test]
fn linked_list_size_and_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Int(1)],
    )
    .unwrap();

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));
}

#[test]
fn linked_list_iterator() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    for i in 1..=3 {
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ll)), Value::Int(i)],
        )
        .unwrap();
    }

    let itr = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    let itr_obj = match itr {
        Value::Object(Some(obj)) => obj,
        _ => panic!("expected iterator"),
    };

    let mut sum = 0;
    for _ in 0..3 {
        let has = call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList$Itr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(itr_obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(has, Value::Int(1));
        let v = call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList$Itr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(itr_obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Int(n) = v {
            sum += n;
        }
    }
    assert_eq!(sum, 6); // 1+2+3

    let has = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList$Itr",
        "hasNext",
        "()Z",
        &[Value::Object(Some(itr_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(0));
}

#[test]
fn linked_list_peek_and_poll() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    // peek on empty returns null
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Object(None));

    // poll on empty returns null
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Object(None));

    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Int(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Int(20)],
    )
    .unwrap();

    // peek returns 10 without removing
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(10));

    // poll removes and returns 10
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(10));

    // poll again returns 20
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(20));

    // Now empty
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "size",
        "()I",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));
}

#[test]
fn linked_list_contains() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    for i in 1..=3 {
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ll)), Value::Int(i)],
        )
        .unwrap();
    }

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(0));
}

// =====================================================================
// Phase 15: Regex + LongStream + DoubleStream + Collectors + LinkedHashMap
// =====================================================================

#[test]
fn pattern_compile_and_matcher_find() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pattern_str = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("\\d+")
    };

    let pat = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
        &[Value::Object(Some(pattern_str))],
    )
    .unwrap()
    .unwrap();
    let pat_obj = match pat {
        Value::Object(Some(r)) => r,
        _ => panic!("expected Pattern"),
    };

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("abc123def456")
    };

    let matcher = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        &[Value::Object(Some(pat_obj)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    let mat_obj = match matcher {
        Value::Object(Some(r)) => r,
        _ => panic!("expected Matcher"),
    };

    // First find: "123"
    let found = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "find",
        "()Z",
        &[Value::Object(Some(mat_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found, Value::Int(1));

    let group = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "group",
        "()Ljava/lang/String;",
        &[Value::Object(Some(mat_obj))],
    )
    .unwrap()
    .unwrap();
    match group {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "123")
        }
        _ => panic!("expected string"),
    }

    // Second find: "456"
    let found2 = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "find",
        "()Z",
        &[Value::Object(Some(mat_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found2, Value::Int(1));

    // Third find: no more matches
    let found3 = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "find",
        "()Z",
        &[Value::Object(Some(mat_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found3, Value::Int(0));
}

#[test]
fn matcher_group_with_captures() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pattern_str = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("(\\w+)=(\\w+)")
    };

    let pat = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
        &[Value::Object(Some(pattern_str))],
    )
    .unwrap()
    .unwrap();
    let pat_obj = match pat {
        Value::Object(Some(r)) => r,
        _ => panic!("expected Pattern"),
    };

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key=value")
    };

    let matcher = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        &[Value::Object(Some(pat_obj)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    let mat_obj = match matcher {
        Value::Object(Some(r)) => r,
        _ => panic!("expected Matcher"),
    };

    let found = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "find",
        "()Z",
        &[Value::Object(Some(mat_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found, Value::Int(1));

    // group(1) = "key"
    let g1 = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "group",
        "(I)Ljava/lang/String;",
        &[Value::Object(Some(mat_obj)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    match g1 {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "key")
        }
        _ => panic!("expected string"),
    }

    // group(2) = "value"
    let g2 = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "group",
        "(I)Ljava/lang/String;",
        &[Value::Object(Some(mat_obj)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    match g2 {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "value")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_split_with_regex() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello   world\tfoo")
    };
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("\\s+")
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "split",
        "(Ljava/lang/String;)[Ljava/lang/String;",
        &[Value::Object(Some(s)), Value::Object(Some(delim))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 3);
    let e0 = shared.mem.heap.get_array_element(arr, 0).unwrap();
    match e0 {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "hello")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_replace_all_regex() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("abc123def456")
    };
    let pattern = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("\\d+")
    };
    let replacement = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("#")
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "replaceAll",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[
            Value::Object(Some(s)),
            Value::Object(Some(pattern)),
            Value::Object(Some(replacement)),
        ],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "abc#def#")
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn string_matches_regex() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("12345")
    };
    let pattern = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("\\d+")
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "matches",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(s)), Value::Object(Some(pattern))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
fn pattern_matches_static() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pattern = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("[a-z]+")
    };
    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };

    let r = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "matches",
        "(Ljava/lang/String;Ljava/lang/CharSequence;)Z",
        &[Value::Object(Some(pattern)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(1));
}

#[test]
fn matcher_replace_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pat_str = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("[aeiou]")
    };
    let pat = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
        &[Value::Object(Some(pat_str))],
    )
    .unwrap()
    .unwrap();
    let pat_obj = match pat {
        Value::Object(Some(r)) => r,
        _ => panic!("expected Pattern"),
    };

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello world")
    };
    let matcher = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        &[Value::Object(Some(pat_obj)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    let mat_obj = match matcher {
        Value::Object(Some(r)) => r,
        _ => panic!("expected Matcher"),
    };

    let replacement = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("*")
    };
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Matcher",
        "replaceAll",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[
            Value::Object(Some(mat_obj)),
            Value::Object(Some(replacement)),
        ],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r).unwrap(),
                "h*ll* w*rld"
            )
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn long_stream_range_sum() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "range",
        "(JJ)Ljava/util/stream/LongStream;",
        &[Value::Long(1), Value::Long(6)],
    )
    .unwrap()
    .unwrap();
    let s_obj = match stream {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "sum",
        "()J",
        &[Value::Object(Some(s_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Long(15)); // 1+2+3+4+5
}

#[test]
fn long_stream_to_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "rangeClosed",
        "(JJ)Ljava/util/stream/LongStream;",
        &[Value::Long(10), Value::Long(12)],
    )
    .unwrap()
    .unwrap();
    let s_obj = match stream {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let arr_val = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "toArray",
        "()[J",
        &[Value::Object(Some(s_obj))],
    )
    .unwrap()
    .unwrap();
    let arr = match arr_val {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 3);
    assert_eq!(
        shared.mem.heap.get_array_element(arr, 0).unwrap(),
        Value::Long(10)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(arr, 1).unwrap(),
        Value::Long(11)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(arr, 2).unwrap(),
        Value::Long(12)
    );
}

#[test]
fn long_stream_min_max() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "range",
        "(JJ)Ljava/util/stream/LongStream;",
        &[Value::Long(5), Value::Long(10)],
    )
    .unwrap()
    .unwrap();
    let s_obj = match stream {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let min_opt = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "min",
        "()Ljava/util/OptionalLong;",
        &[Value::Object(Some(s_obj))],
    )
    .unwrap()
    .unwrap();
    let opt_obj = match min_opt {
        Value::Object(Some(r)) => r,
        _ => panic!("expected optional"),
    };

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalLong",
        "getAsLong",
        "()J",
        &[Value::Object(Some(opt_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Long(5));
}

#[test]
fn double_stream_of_sum() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create stream with single element via of()
    let s1 = call_native(
        &shared,
        &mut thread,
        "java/util/stream/DoubleStream",
        "of",
        "(D)Ljava/util/stream/DoubleStream;",
        &[Value::Double(2.5)],
    )
    .unwrap()
    .unwrap();
    let s1_obj = match s1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/stream/DoubleStream",
        "sum",
        "()D",
        &[Value::Object(Some(s1_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Double(2.5));
}

#[test]
fn double_stream_to_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = call_native(
        &shared,
        &mut thread,
        "java/util/stream/DoubleStream",
        "of",
        "(D)Ljava/util/stream/DoubleStream;",
        &[Value::Double(1.5)],
    )
    .unwrap()
    .unwrap();
    let s_obj = match s {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let arr_val = call_native(
        &shared,
        &mut thread,
        "java/util/stream/DoubleStream",
        "toArray",
        "()[D",
        &[Value::Object(Some(s_obj))],
    )
    .unwrap()
    .unwrap();
    let arr = match arr_val {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 1);
    assert_eq!(
        shared.mem.heap.get_array_element(arr, 0).unwrap(),
        Value::Double(1.5)
    );
}

#[test]
fn double_stream_average() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a LongStream and convert to DoubleStream, then average
    let ls = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "rangeClosed",
        "(JJ)Ljava/util/stream/LongStream;",
        &[Value::Long(1), Value::Long(4)],
    )
    .unwrap()
    .unwrap();
    let ls_obj = match ls {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let ds = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "asDoubleStream",
        "()Ljava/util/stream/DoubleStream;",
        &[Value::Object(Some(ls_obj))],
    )
    .unwrap()
    .unwrap();
    let ds_obj = match ds {
        Value::Object(Some(r)) => r,
        _ => panic!("expected stream"),
    };

    let avg_opt = call_native(
        &shared,
        &mut thread,
        "java/util/stream/DoubleStream",
        "average",
        "()Ljava/util/OptionalDouble;",
        &[Value::Object(Some(ds_obj))],
    )
    .unwrap()
    .unwrap();
    let opt_obj = match avg_opt {
        Value::Object(Some(r)) => r,
        _ => panic!("expected optional"),
    };

    let v = call_native(
        &shared,
        &mut thread,
        "java/util/OptionalDouble",
        "getAsDouble",
        "()D",
        &[Value::Object(Some(opt_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Double(2.5)); // (1+2+3+4)/4 = 2.5
}

#[test]
fn collectors_grouping_by() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Test groupingBy collector creation (null classifier — just verifying tag)
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "groupingBy",
        "(Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();

    match collector {
        Value::Object(Some(c)) => {
            let tag = shared.mem.heap.get_field(c, 0);
            assert_eq!(tag, Value::Int(7)); // COLLECTOR_TAG_GROUPING_BY
        }
        _ => panic!("expected collector"),
    }
}

#[test]
fn collectors_partitioning_by() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "partitioningBy",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Collector;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();

    match collector {
        Value::Object(Some(c)) => {
            let tag = shared.mem.heap.get_field(c, 0);
            assert_eq!(tag, Value::Int(8)); // COLLECTOR_TAG_PARTITIONING_BY
        }
        _ => panic!("expected collector"),
    }
}

#[test]
fn linked_hashmap_insertion_order() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lhm = alloc_receiver(&shared, &mut thread, "java/util/LinkedHashMap", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();

    // Insert keys "c", "a", "b" — should iterate in this order
    for key_str in &["c", "a", "b"] {
        let k = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(key_str)
        };
        let v = Value::Int(key_str.len() as i32);
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedHashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(lhm)), Value::Object(Some(k)), v],
        )
        .unwrap();
    }

    // toString should show insertion order
    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(lhm))],
    )
    .unwrap()
    .unwrap();
    match ts {
        Value::Object(Some(r)) => {
            let s = read_java_string(&shared.mem.heap, r).unwrap();
            assert_eq!(s, "{c=1, a=1, b=1}");
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn linked_hashmap_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lhm = alloc_receiver(&shared, &mut thread, "java/util/LinkedHashMap", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let boxed_42 = box_int(&shared, &mut thread, 42);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(lhm)), Value::Object(Some(k)), boxed_42],
    )
    .unwrap();

    let k2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(lhm)), Value::Object(Some(k2))],
    )
    .unwrap();
    assert_eq!(unbox_int(&shared, &mut thread, v), Some(Value::Int(42)));
}

#[test]
fn linked_hashmap_remove_preserves_order() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lhm = alloc_receiver(&shared, &mut thread, "java/util/LinkedHashMap", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();

    // Insert "a", "b", "c"
    let mut keys = Vec::new();
    for s in &["a", "b", "c"] {
        let k = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(s)
        };
        keys.push(k);
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedHashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(lhm)),
                Value::Object(Some(k)),
                Value::Int(0),
            ],
        )
        .unwrap();
    }

    // Remove "b"
    let bkey = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(lhm)), Value::Object(Some(bkey))],
    )
    .unwrap();

    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(lhm))],
    )
    .unwrap()
    .unwrap();
    match ts {
        Value::Object(Some(r)) => {
            let s = read_java_string(&shared.mem.heap, r).unwrap();
            assert_eq!(s, "{a=0, c=0}"); // "a", "c" — "b" removed, order preserved
        }
        _ => panic!("expected string"),
    }
}

#[test]
fn linked_hashmap_put_if_absent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lhm = alloc_receiver(&shared, &mut thread, "java/util/LinkedHashMap", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("x")
    };

    // putIfAbsent on new key: should insert
    let boxed_10 = box_int(&shared, &mut thread, 10);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(lhm)), Value::Object(Some(k)), boxed_10],
    )
    .unwrap();

    // putIfAbsent on existing key: should not change
    let k2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("x")
    };
    let boxed_99 = box_int(&shared, &mut thread, 99);
    let existing = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(lhm)), Value::Object(Some(k2)), boxed_99],
    )
    .unwrap();
    assert_eq!(
        unbox_int(&shared, &mut thread, existing),
        Some(Value::Int(10))
    );
}

#[test]
fn linked_hashmap_size_and_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lhm = alloc_receiver(&shared, &mut thread, "java/util/LinkedHashMap", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(lhm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("k")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(lhm)),
            Value::Object(Some(k)),
            Value::Int(1),
        ],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "size",
        "()I",
        &[Value::Object(Some(lhm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));

    let not_empty = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(lhm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(not_empty, Value::Int(0));
}

#[test]
fn string_split_comma_regex() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a,b;c,d")
    };
    let delim = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("[,;]")
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "split",
        "(Ljava/lang/String;)[Ljava/lang/String;",
        &[Value::Object(Some(s)), Value::Object(Some(delim))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 4);
}

// ===== Phase 16: ArrayDeque, PriorityQueue, Vector/Stack, bulk ops =====

#[test]
fn array_deque_add_and_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("hello")
    };
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
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(s2))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "size",
        "()I",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

#[test]
fn array_deque_add_first_last() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("B")
    };
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };
    let s3 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("C")
    };

    // Add B, then A at front, then C at back → [A, B, C]
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "addLast",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(deque)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "addFirst",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(deque)), Value::Object(Some(s2))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "addLast",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(deque)), Value::Object(Some(s3))],
    )
    .unwrap();

    // peekFirst should be A
    let first = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "peekFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let first_obj = match first {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, first_obj),
        Some("A".to_string())
    );

    // peekLast should be C
    let last = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "peekLast",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let last_obj = match last {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, last_obj),
        Some("C".to_string())
    );
}

#[test]
fn array_deque_poll_first_last() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("X")
    };
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Y")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(s2))],
    )
    .unwrap();

    // pollFirst → X
    let polled = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "pollFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let polled_obj = match polled {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, polled_obj),
        Some("X".to_string())
    );

    // pollLast → Y
    let polled2 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "pollLast",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let polled2_obj = match polled2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, polled2_obj),
        Some("Y".to_string())
    );

    // Empty now — poll returns null
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "pollFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Object(None));
}

#[test]
fn array_deque_push_pop_stack() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    // push adds to front (stack behavior)
    for i in 1..=3 {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(&i.to_string())
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayDeque",
            "push",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(deque)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    // pop should return 3, 2, 1 (LIFO)
    let pop1 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "pop",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let pop1_obj = match pop1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, pop1_obj),
        Some("3".to_string())
    );
}

#[test]
fn array_deque_contains() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("abc")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(s1))],
    )
    .unwrap();

    let needle = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("abc")
    };
    let found = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(needle))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found, Value::Int(1));

    let missing = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("xyz")
    };
    let not_found = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(missing))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(not_found, Value::Int(0));
}

#[test]
fn array_deque_to_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    for i in 0..3 {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayDeque",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(deque)), Value::Int(i * 10)],
        )
        .unwrap();
    }

    let arr_val = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "toArray",
        "()[Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let arr = match arr_val {
        Value::Object(Some(r)) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 3);
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 0).unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 1).unwrap(),
        Value::Int(10)
    );
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 2).unwrap(),
        Value::Int(20)
    );
}

#[test]
fn priority_queue_natural_order() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pq = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(pq))],
    )
    .unwrap();

    // Add integers: 30, 10, 20
    for &v in &[30, 10, 20] {
        let s = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.create_string(&v.to_string())
        };
        call_native(
            &shared,
            &mut thread,
            "java/util/PriorityQueue",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(pq)), Value::Object(Some(s))],
        )
        .unwrap();
    }

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "size",
        "()I",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));

    // Poll should return "10" first (string natural order)
    let p1 = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    let p1_obj = match p1 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, p1_obj),
        Some("10".to_string())
    );

    // Next should be "20"
    let p2 = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    let p2_obj = match p2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, p2_obj),
        Some("20".to_string())
    );

    // Last should be "30"
    let p3 = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    let p3_obj = match p3 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, p3_obj),
        Some("30".to_string())
    );

    // Now empty
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Object(None));
}

#[test]
fn priority_queue_peek() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pq = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(pq))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("banana")
    };
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("apple")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Object(Some(s2))],
    )
    .unwrap();

    // peek returns min without removing
    let pk = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    let pk_obj = match pk {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, pk_obj),
        Some("apple".to_string())
    );

    // size unchanged
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "size",
        "()I",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

#[test]
fn priority_queue_contains_remove() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pq = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(pq))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("cat")
    };
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("dog")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Object(Some(s2))],
    )
    .unwrap();

    let needle = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("cat")
    };
    let found = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Object(Some(needle))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(found, Value::Int(1));

    let needle2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("cat")
    };
    let removed = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "remove",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Object(Some(needle2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(removed, Value::Int(1));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "size",
        "()I",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
}

#[test]
fn vector_basic_ops() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let vec = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "<init>",
        "()V",
        &[Value::Object(Some(vec))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a")
    };
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    let s3 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("c")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(vec)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(vec)), Value::Object(Some(s2))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(vec)), Value::Object(Some(s3))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "size",
        "()I",
        &[Value::Object(Some(vec))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));

    let first = call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "firstElement",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(vec))],
    )
    .unwrap()
    .unwrap();
    let first_obj = match first {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, first_obj),
        Some("a".to_string())
    );

    let last = call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "lastElement",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(vec))],
    )
    .unwrap()
    .unwrap();
    let last_obj = match last {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, last_obj),
        Some("c".to_string())
    );

    let elem_at = call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "elementAt",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(vec)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let elem_obj = match elem_at {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, elem_obj),
        Some("b".to_string())
    );
}

#[test]
fn vector_capacity() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let vec = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "<init>",
        "(I)V",
        &[Value::Object(Some(vec)), Value::Int(20)],
    )
    .unwrap();

    let cap = call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "capacity",
        "()I",
        &[Value::Object(Some(vec))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(20));
}

#[test]
fn stack_push_pop_peek() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stack = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "<init>",
        "()V",
        &[Value::Object(Some(stack))],
    )
    .unwrap();

    // push returns the item
    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("first")
    };
    let pushed = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stack)), Value::Object(Some(s1))],
    )
    .unwrap()
    .unwrap();
    match pushed {
        Value::Object(Some(_)) => {}
        _ => panic!("push should return the item"),
    };

    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("second")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stack)), Value::Object(Some(s2))],
    )
    .unwrap();

    // peek returns top without removing
    let pk = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(stack))],
    )
    .unwrap()
    .unwrap();
    let pk_obj = match pk {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, pk_obj),
        Some("second".to_string())
    );

    // pop returns top
    let popped = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "pop",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(stack))],
    )
    .unwrap()
    .unwrap();
    let popped_obj = match popped {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, popped_obj),
        Some("second".to_string())
    );

    // Now top is "first"
    let pk2 = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(stack))],
    )
    .unwrap()
    .unwrap();
    let pk2_obj = match pk2 {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, pk2_obj),
        Some("first".to_string())
    );
}

#[test]
fn stack_search() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stack = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "<init>",
        "()V",
        &[Value::Object(Some(stack))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };
    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("B")
    };
    let s3 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("C")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stack)), Value::Object(Some(s1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stack)), Value::Object(Some(s2))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stack)), Value::Object(Some(s3))],
    )
    .unwrap();

    // Stack: [A, B, C] — C is at top (distance 1)
    let search_c = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("C")
    };
    let pos = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "search",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(stack)), Value::Object(Some(search_c))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(1));

    let search_a = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };
    let pos2 = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "search",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(stack)), Value::Object(Some(search_a))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos2, Value::Int(3));

    let search_x = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("X")
    };
    let pos3 = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "search",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(stack)), Value::Object(Some(search_x))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos3, Value::Int(-1));
}

#[test]
fn stack_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let stack = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "<init>",
        "()V",
        &[Value::Object(Some(stack))],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "empty",
        "()Z",
        &[Value::Object(Some(stack))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));

    call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(stack)), Value::Int(42)],
    )
    .unwrap();

    let not_empty = call_native(
        &shared,
        &mut thread,
        "java/util/Stack",
        "empty",
        "()Z",
        &[Value::Object(Some(stack))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(not_empty, Value::Int(0));
}

#[test]
fn arraylist_remove_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create main list [A, B, C, A]
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

    let sa = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };
    let sb = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("B")
    };
    let sc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("C")
    };
    let sa2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(sa))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(sb))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(sc))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(sa2))],
    )
    .unwrap();

    // Create removal list [A]
    let removal = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(removal))],
    )
    .unwrap();
    let ra = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(removal)), Value::Object(Some(ra))],
    )
    .unwrap();

    // removeAll
    let modified = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(removal))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(modified, Value::Int(1));

    // Should have [B, C]
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));
}

#[test]
fn arraylist_retain_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create main list [1, 2, 3, 4, 5]
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
    for i in 1..=5 {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Int(i)],
        )
        .unwrap();
    }

    // Create retain list [2, 4]
    let retain = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(retain))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(retain)), Value::Int(2)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(retain)), Value::Int(4)],
    )
    .unwrap();

    let modified = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(list)), Value::Object(Some(retain))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(modified, Value::Int(1));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));

    // Elements should be [2, 4]
    let e0 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(e0, Value::Int(2));
    let e1 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(e1, Value::Int(4));
}

#[test]
fn linked_list_add_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create linked list
    let ll = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("X")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(ll)), Value::Object(Some(s1))],
    )
    .unwrap();

    // Create source ArrayList [Y, Z]
    let src = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(src))],
    )
    .unwrap();
    let sy = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Y")
    };
    let sz = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Z")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(sy))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(sz))],
    )
    .unwrap();

    // addAll
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "addAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(ll)), Value::Object(Some(src))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "size",
        "()I",
        &[Value::Object(Some(ll))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));
}

/// # IGNORED: this test outlived both things it tested
///
/// It drives `java/util/ArrayDeque.iterator()` and then the
/// `java/util/ArrayDeque$Itr` natives on the object that comes back. Neither
/// exists any more: the `iterator()` registration was retired on 2026-08-30
/// (the JDK's `DeqIterator` is fail-fast off a physical ring-buffer index and
/// no snapshot reproduces it, so the real bytecode runs instead), and the
/// `$Itr` natives were retired on 2026-09-02 by the iterator-carrier census,
/// which found this test was their only caller AND that it already failed at
/// its first line — `java/util/ArrayDeque.iterator() not registered`.
///
/// It had been failing since the first of those changes and nobody saw it,
/// because this whole `#[cfg(all(test, feature = "synthetic-jdk"))]` module
/// stopped compiling when `InlineSite` grew a field: 4291 tests that could not
/// be built. Fixed in the same commit as this ignore.
///
/// Kept rather than deleted for the reason the census gives: the assertions are
/// the right ones if an ArrayDeque iterator is ever minted here again.
/// `iterator-carrier-census-20260902.md`.
#[ignore = "ArrayDeque.iterator() and ArrayDeque$Itr are both deliberately \
            unregistered; see the iterator-carrier census"]
#[test]
fn array_deque_iterator() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    for i in 1..=3 {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayDeque",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(deque)), Value::Int(i * 10)],
        )
        .unwrap();
    }

    let itr_val = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    let itr = match itr_val {
        Value::Object(Some(r)) => r,
        _ => panic!("expected iterator"),
    };

    // hasNext + next three times
    for expected in [10, 20, 30] {
        let has = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayDeque$Itr",
            "hasNext",
            "()Z",
            &[Value::Object(Some(itr))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(has, Value::Int(1));

        let val = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayDeque$Itr",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(itr))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(val, Value::Int(expected));
    }

    let has_more = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque$Itr",
        "hasNext",
        "()Z",
        &[Value::Object(Some(itr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has_more, Value::Int(0));
}

#[test]
fn array_deque_clear() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Int(1)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(deque)), Value::Int(2)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "clear",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "size",
        "()I",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
}

#[test]
fn priority_queue_clear() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pq = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(pq))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Int(5)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(pq)), Value::Int(3)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "clear",
        "()V",
        &[Value::Object(Some(pq))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "size",
        "()I",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));

    let peek_empty = call_native(
        &shared,
        &mut thread,
        "java/util/PriorityQueue",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(peek_empty, Value::Object(None));
}

#[test]
fn vector_set_element_at() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let vec = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "<init>",
        "()V",
        &[Value::Object(Some(vec))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("old")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(vec)), Value::Object(Some(s1))],
    )
    .unwrap();

    let s2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("new")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "setElementAt",
        "(Ljava/lang/Object;I)V",
        &[
            Value::Object(Some(vec)),
            Value::Object(Some(s2)),
            Value::Int(0),
        ],
    )
    .unwrap();

    let elem = call_native(
        &shared,
        &mut thread,
        "java/util/Vector",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(vec)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    let elem_obj = match elem {
        Value::Object(Some(r)) => r,
        _ => panic!("expected object"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, elem_obj),
        Some("new".to_string())
    );
}

#[test]
fn hashset_add_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create target HashSet
    let set = alloc_receiver(&shared, &mut thread, "java/util/HashSet", 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "<init>",
        "()V",
        &[Value::Object(Some(set))],
    )
    .unwrap();

    let s1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("A")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(set)), Value::Object(Some(s1))],
    )
    .unwrap();

    // Create source ArrayList [B, C]
    let src = alloc_receiver(&shared, &mut thread, "java/util/ArrayList", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(src))],
    )
    .unwrap();
    let sb = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("B")
    };
    let sc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("C")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(sb))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(src)), Value::Object(Some(sc))],
    )
    .unwrap();

    // addAll
    let modified = call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "addAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(set)), Value::Object(Some(src))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(modified, Value::Int(1));

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/HashSet",
        "size",
        "()I",
        &[Value::Object(Some(set))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));
}

#[test]
fn array_deque_add_all() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let deque = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "<init>",
        "()V",
        &[Value::Object(Some(deque))],
    )
    .unwrap();

    // Create source ArrayList [10, 20, 30]
    let src = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(src))],
    )
    .unwrap();
    for v in [10, 20, 30] {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(src)), Value::Int(v)],
        )
        .unwrap();
    }

    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "addAll",
        "(Ljava/util/Collection;)Z",
        &[Value::Object(Some(deque)), Value::Object(Some(src))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "size",
        "()I",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));

    // First element should be 10
    let first = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayDeque",
        "peekFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(deque))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(first, Value::Int(10));
}

