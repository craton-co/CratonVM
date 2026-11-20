#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_multiple_weak_refs_same_referent() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_json_read_value_null_input() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_json_readtree_simple_object() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_json_readtree_empty_string() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jsonnode_astext_string_node() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jsonnode_asint_number_node() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jsonnode_boolean_and_null() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jsonnode_array_size() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m17_define_class_rejects_invalid_magic() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m17_define_class_rejects_negative_offset() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m17_define_class_rejects_out_of_bounds() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m19_finalizer_enqueued_for_dead_objects() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m19_finalizer_not_enqueued_for_live_objects() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_phantom_ref_enqueued_but_not_cleared() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_soft_ref_cleared_under_zero_memory() {
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
        // HotSpot judges this round with the previous collection's clock.
        let now = wall_clock_ms() + 60_000;
        let first = proc.process_references(&|addr| addr == soft_addr, 0, now);
        assert_eq!(
            first.stats.soft_refs_cleared, 0,
            "the first round advances the clock"
        );
        let result = proc.process_references(&|addr| addr == soft_addr, 0, now);
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_weak_ref_with_queue_enqueued_after_gc() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_soft_ref_with_queue_enqueued_under_pressure() {
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
        // HotSpot judges this round with the previous collection's clock.
        let is_marked = |addr: usize| -> bool { addr == soft_addr || addr == queue_addr };
        let now = wall_clock_ms() + 60_000;
        let first = proc.process_references(&is_marked, 0, now);
        assert_eq!(
            first.stats.soft_refs_cleared, 0,
            "the first round advances the clock"
        );
        let result = proc.process_references(&is_marked, 0, now);
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_phantom_ref_enqueued_to_queue() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_reference_queue_poll_empty_returns_null() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_reference_refers_to() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_is_enqueued_lifecycle() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_multiple_refs_same_queue() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_enqueue_without_queue_returns_false() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_soft_ref_not_enqueued_when_retained() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_weak_ref_get_returns_null_after_clear() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_phantom_ref_get_always_returns_null() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn s28_reference_queue_remove_timeout_empty() {
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

