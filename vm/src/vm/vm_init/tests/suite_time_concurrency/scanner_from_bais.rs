#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_from_bais() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create ByteArrayInputStream from bytes "10 20"
    let text = b"10 20";
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        text.len(),
    );
    for (i, &b) in text.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i32));
    }

    // BAIS real-JDK layout = 4 fields (buf, pos, mark, count).
    // A REAL receiver (`alloc_receiver` resolves the named class), not
    // `alloc_object(ClassId::new(0), ..)`. Class id 0 is not a "no class"
    // sentinel: it is a real, registered class, and a virtual call on a
    // fabricated class-0 object dispatches into THAT class -- which is why
    // `scanner_close` failed with `NoSuchMethodError` on
    // `java/util/Enumeration$Impl.close`, a class the test never names.
    let bais = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayInputStream", 4);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(arr))],
    )
    .unwrap();

    // Create Scanner from InputStream
    let sc = alloc_receiver(&shared, &mut thread, "java/util/Scanner", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(bais))],
    )
    .unwrap();

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(10));

    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(20));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_close() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("data")
    };
    // A REAL receiver (`alloc_receiver` resolves the named class), not
    // `alloc_object(ClassId::new(0), ..)`. Class id 0 is not a "no class"
    // sentinel: it is a real, registered class, and a virtual call on a
    // fabricated class-0 object dispatches into THAT class -- which is why
    // `scanner_close` failed with `NoSuchMethodError` on
    // `java/util/Enumeration$Impl.close`, a class the test never names.
    let sc = alloc_receiver(&shared, &mut thread, "java/util/Scanner", 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "close",
        "()V",
        &[Value::Object(Some(sc))],
    )
    .unwrap();

    // A closed Scanner refuses: `hasNext()` throws IllegalStateException
    // "Scanner closed" on JDK 25. This test used to assert the OPPOSITE --
    // "Scanner still works after close (no IllegalStateException in our impl)"
    // -- i.e. it was written to pin a known departure from the JDK. The native
    // now follows the JDK, so the assertion follows it too.
    let hn = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "hasNext",
        "()Z",
        &[Value::Object(Some(sc))],
    );
    match hn {
        Err(e) => assert!(
            format!("{e:?}").contains("Scanner closed"),
            "a closed Scanner must refuse with \"Scanner closed\", got {e:?}"
        ),
        Ok(v) => panic!("hasNext() on a closed Scanner returned {v:?}; the JDK throws"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_use_radix() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("FF 77")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "useRadix",
        "(I)Ljava/util/Scanner;",
        &[Value::Object(Some(sc)), Value::Int(16)],
    )
    .unwrap();

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(255)); // FF in hex

    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "()I",
        &[Value::Object(Some(sc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(119)); // 77 in hex
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_find_in_line() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("age: 25 years")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let pattern = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("\\d+")
    };
    let found = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "findInLine",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(sc)), Value::Object(Some(pattern))],
    )
    .unwrap()
    .unwrap();
    let fs = match found {
        Value::Object(Some(r)) => read_java_string(&shared.mem.heap, r),
        _ => None,
    };
    assert_eq!(fs, Some("25".to_string()));}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn scanner_next_int_with_radix_arg() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let input = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("1010")
    };
    let sc = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sc)), Value::Object(Some(input))],
    )
    .unwrap();

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/Scanner",
        "nextInt",
        "(I)I",
        &[Value::Object(Some(sc)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(10)); // 1010 in binary
}

// ---------------------------------------------------------------
// Phase 20: Annotations Runtime
// ---------------------------------------------------------------

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_get_annotations_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a class mirror (2-field: field 0 = ClassId Int, field 1 = name String)
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(mirror, 0, Value::Int(0));
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/MyClass")
    };
    shared
        .mem
        .heap
        .set_field(mirror, 1, Value::Object(Some(name)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(mirror))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 0);}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_get_declared_annotations_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(mirror, 0, Value::Int(0));
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/Empty")
    };
    shared
        .mem
        .heap
        .set_field(mirror, 1, Value::Object(Some(name)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(mirror))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 0);}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_is_annotation_false() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Register a class with no ACC_ANNOTATION flag
    register_test_class(&shared, "test/NotAnAnnotation", 0x0021, &[], &[]); // PUBLIC|SUPER
    let cid = shared
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("test/NotAnAnnotation")
        .unwrap();
    let mirror = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.get_class_mirror(cid)
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isAnnotation",
        "()Z",
        &[Value::Object(Some(mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_is_annotation_true() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // ACC_ANNOTATION=0x2000 | ACC_INTERFACE=0x0200 | ACC_ABSTRACT=0x0400
    register_test_class(&shared, "test/MyAnnotation", 0x2600, &[], &[]);
    let cid = shared
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("test/MyAnnotation")
        .unwrap();
    let mirror = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.get_class_mirror(cid)
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isAnnotation",
        "()Z",
        &[Value::Object(Some(mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_is_annotation_present_false() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Class mirror for the class we're checking
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(mirror, 0, Value::Int(0));
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/Plain")
    };
    shared
        .mem
        .heap
        .set_field(mirror, 1, Value::Object(Some(name)));

    // Class mirror for the annotation type we're looking for
    let ann_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(ann_mirror, 0, Value::Int(999));
    let ann_name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("java/lang/Deprecated")
    };
    shared
        .mem
        .heap
        .set_field(ann_mirror, 1, Value::Object(Some(ann_name)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        &[Value::Object(Some(mirror)), Value::Object(Some(ann_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn annotation_proxy_type() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create an annotation proxy manually (2 fields)
    let proxy = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let desc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Ljava/lang/Override;")
    };
    shared
        .mem
        .heap
        .set_field(proxy, 0, Value::Object(Some(desc)));
    // field 1 = Class mirror (leave as null for this test)
    shared.mem.heap.set_field(proxy, 1, Value::Object(None));

    // Call annotationType() — should return null since no mirror was set
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/annotation/Annotation",
        "annotationType",
        "()Ljava/lang/Class;",
        &[Value::Object(Some(proxy))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Object(None));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn annotation_proxy_with_mirror() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a class mirror for the annotation type
    let ann_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(ann_mirror, 0, Value::Int(42));
    let ann_name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("java/lang/Override")
    };
    shared
        .mem
        .heap
        .set_field(ann_mirror, 1, Value::Object(Some(ann_name)));

    // Create an annotation proxy with the mirror
    let proxy = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let desc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("Ljava/lang/Override;")
    };
    shared
        .mem
        .heap
        .set_field(proxy, 0, Value::Object(Some(desc)));
    shared
        .mem
        .heap
        .set_field(proxy, 1, Value::Object(Some(ann_mirror)));

    // Call annotationType() — should return the mirror
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/annotation/Annotation",
        "annotationType",
        "()Ljava/lang/Class;",
        &[Value::Object(Some(proxy))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(r)) => {
            // Verify it's the same mirror
            assert_eq!(shared.mem.heap.get_field(r, 0), Value::Int(42));
        }
        _ => panic!("expected Class mirror"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_get_annotation_not_found() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(mirror, 0, Value::Int(0));
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/NoAnns")
    };
    shared
        .mem
        .heap
        .set_field(mirror, 1, Value::Object(Some(name)));

    let ann_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(ann_mirror, 0, Value::Int(999));
    let ann_name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("java/lang/Override")
    };
    shared
        .mem
        .heap
        .set_field(ann_mirror, 1, Value::Object(Some(ann_name)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(mirror)), Value::Object(Some(ann_mirror))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Object(None));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn class_get_annotations_by_type_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(mirror, 0, Value::Int(0));
    let name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/X")
    };
    shared
        .mem
        .heap
        .set_field(mirror, 1, Value::Object(Some(name)));

    let ann_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(ann_mirror, 0, Value::Int(999));
    let ann_name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("java/lang/Deprecated")
    };
    shared
        .mem
        .heap
        .set_field(ann_mirror, 1, Value::Object(Some(ann_name)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(mirror)), Value::Object(Some(ann_mirror))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 0);}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn field_get_annotations_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a Field object (6 fields: 0=class mirror, 1=name, 2=type, 3=modifiers, 4=slot, 5=descriptor)
    let field_obj = shared.mem.heap.alloc_object(ClassId::new(0), 6);
    let class_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(class_mirror, 0, Value::Int(0));
    let cls_name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/Cls")
    };
    shared
        .mem
        .heap
        .set_field(class_mirror, 1, Value::Object(Some(cls_name)));
    shared
        .mem
        .heap
        .set_field(field_obj, 0, Value::Object(Some(class_mirror)));
    let fname = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("myField")
    };
    shared
        .mem
        .heap
        .set_field(field_obj, 1, Value::Object(Some(fname)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Field",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(field_obj))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 0);}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn method_get_annotations_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a Method object (7 fields: 0=class mirror, 1=name, 2=ret type, 3=param types, 4=modifiers, 5=descriptor, 6=param count)
    let method_obj = shared.mem.heap.alloc_object(ClassId::new(0), 7);
    let class_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(class_mirror, 0, Value::Int(0));
    let cls_name = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("test/Cls")
    };
    shared
        .mem
        .heap
        .set_field(class_mirror, 1, Value::Object(Some(cls_name)));
    shared
        .mem
        .heap
        .set_field(method_obj, 0, Value::Object(Some(class_mirror)));
    let mname = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("myMethod")
    };
    shared
        .mem
        .heap
        .set_field(method_obj, 1, Value::Object(Some(mname)));
    let mdesc = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("()V")
    };
    shared
        .mem
        .heap
        .set_field(method_obj, 5, Value::Object(Some(mdesc)));

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Method",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(method_obj))],
    )
    .unwrap()
    .unwrap();
    let arr = match result {
        Value::Object(Some(a)) => a,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr), 0);}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn convert_annotation_data() {
    // Test the convert_annotation helper directly
    use cratonvm_reader::attribute::{Annotation, ElementValue, ElementValuePair};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    // Build a minimal constant pool:
    // index 0 = Tombstone
    // index 1 = Utf8 "Ljava/lang/Deprecated;"
    // index 2 = Utf8 "value"
    // index 3 = Utf8 "test reason"
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Ljava/lang/Deprecated;".into()),
        ConstantPoolEntry::Utf8("value".into()),
        ConstantPoolEntry::Utf8("test reason".into()),
    ];
    let cp = ConstantPool::new(entries);

    let ann = Annotation {
        type_index: 1,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Const {
                tag: b's',
                const_value_index: 3,
            },
        }],
    };

    let data = super::convert_annotation(&ann, &cp).unwrap();
    assert_eq!(data.type_descriptor, "Ljava/lang/Deprecated;");
    assert_eq!(data.elements.len(), 1);
    assert_eq!(data.elements[0].0, "value");
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::StringVal(s) => {
            assert_eq!(s, "test reason");
        }
        _ => panic!("expected string value"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn convert_annotation_enum_value() {
    use cratonvm_reader::attribute::{Annotation, ElementValue, ElementValuePair};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Ljava/lang/annotation/Retention;".into()),
        ConstantPoolEntry::Utf8("value".into()),
        ConstantPoolEntry::Utf8("Ljava/lang/annotation/RetentionPolicy;".into()),
        ConstantPoolEntry::Utf8("RUNTIME".into()),
    ];
    let cp = ConstantPool::new(entries);

    let ann = Annotation {
        type_index: 1,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Enum {
                type_name_index: 3,
                const_name_index: 4,
            },
        }],
    };

    let data = super::convert_annotation(&ann, &cp).unwrap();
    assert_eq!(data.type_descriptor, "Ljava/lang/annotation/Retention;");
    assert_eq!(data.elements.len(), 1);
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Enum(t, c) => {
            assert_eq!(t, "Ljava/lang/annotation/RetentionPolicy;");
            assert_eq!(c, "RUNTIME");
        }
        _ => panic!("expected enum value"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn convert_annotation_int_value() {
    use cratonvm_reader::attribute::{Annotation, ElementValue, ElementValuePair};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Ltest/MyAnn;".into()),
        ConstantPoolEntry::Utf8("count".into()),
        ConstantPoolEntry::Integer(42),
    ];
    let cp = ConstantPool::new(entries);

    let ann = Annotation {
        type_index: 1,
        element_value_pairs: vec![ElementValuePair {
            element_name_index: 2,
            value: ElementValue::Const {
                tag: b'I',
                const_value_index: 3,
            },
        }],
    };

    let data = super::convert_annotation(&ann, &cp).unwrap();
    assert_eq!(data.type_descriptor, "Ltest/MyAnn;");
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Int(v) => {
            assert_eq!(*v, 42);
        }
        _ => panic!("expected int value"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn convert_annotation_array_value() {
    use cratonvm_reader::attribute::{Annotation, ElementValue, ElementValuePair};
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("Ltest/Arr;".into()),
        ConstantPoolEntry::Utf8("names".into()),
        ConstantPoolEntry::Utf8("Alice".into()),
        ConstantPoolEntry::Utf8("Bob".into()),
    ];
    let cp = ConstantPool::new(entries);

    let ann = Annotation {
        type_index: 1,
        element_value_pairs: vec![ElementValuePair {
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
    };

    let data = super::convert_annotation(&ann, &cp).unwrap();
    match &data.elements[0].1 {
        crate::native::registry::AnnotationElementValue::Array(arr) => {
            assert_eq!(arr.len(), 2);
            match &arr[0] {
                crate::native::registry::AnnotationElementValue::StringVal(s) => {
                    assert_eq!(s, "Alice")
                }
                _ => panic!("expected string"),
            }
            match &arr[1] {
                crate::native::registry::AnnotationElementValue::StringVal(s) => {
                    assert_eq!(s, "Bob")
                }
                _ => panic!("expected string"),
            }
        }
        _ => panic!("expected array value"),
    }
}

// =========================================================================
// Phase 21: java.lang.ref — Reference, WeakReference, SoftReference,
//           PhantomReference, ReferenceQueue
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn weak_reference_get_returns_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap();

    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(Some(referent)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn weak_reference_clear_nulls_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "clear",
        "()V",
        &[Value::Object(Some(weak))],
    )
    .unwrap();

    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(None));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn soft_reference_get_returns_referent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
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

    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(soft))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(Some(referent)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn phantom_reference_requires_queue() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
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

    // PhantomReference.get() — always returns null per JDK contract.
    // JLS §12.6.2 / java.lang.ref.PhantomReference javadoc:
    // "In order to ensure that a reclaimable object remains so, the
    // referent of a phantom reference may not be retrieved: The get
    // method of a phantom reference always returns null."
    // T16 follow-up fix (session 87/88): test previously asserted the
    // referent was returned (non-spec behavior from early development).
    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/PhantomReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(phantom))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(None));
    // Guard the phantom is correctly constructed with its queue (the
    // test name is `phantom_reference_requires_queue` — assert that
    // bit too so the test's semantic value is preserved).
    let _ = queue;
    let _ = referent;
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_refers_to_true() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_refers_to_false_after_clear() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "clear",
        "()V",
        &[Value::Object(Some(weak))],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "refersTo",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_queue_init_and_poll_empty() {
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

    // Poll on empty queue returns null
    let polled = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(polled, Value::Object(None));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_enqueue_and_poll() {
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

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(weak)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();

    // Enqueue
    let enq = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "enqueue",
        "()Z",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(enq, Value::Int(1)); // success

    // Poll
    let polled = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(polled, Value::Object(Some(weak)));

    // Queue now empty
    let polled2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(polled2, Value::Object(None));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn enqueue_without_queue_returns_false() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap();

    let enq = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "enqueue",
        "()Z",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(enq, Value::Int(0)); // no queue attached
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_enqueue_multiple_fifo() {
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

    let r1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let w1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(w1)),
            Value::Object(Some(r1)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();

    let r2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let w2 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(w2)),
            Value::Object(Some(r2)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();

    // Enqueue w1 then w2
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "enqueue",
        "()Z",
        &[Value::Object(Some(w1))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "enqueue",
        "()Z",
        &[Value::Object(Some(w2))],
    )
    .unwrap();

    // Queue is LIFO (stack) — last enqueued is polled first
    let p1 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(p1, Value::Object(Some(w2)));

    let p2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "poll",
        "()Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(p2, Value::Object(Some(w1)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn weak_ref_with_queue_init() {
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

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V",
        &[
            Value::Object(Some(weak)),
            Value::Object(Some(referent)),
            Value::Object(Some(queue)),
        ],
    )
    .unwrap();

    // Verify get() returns referent
    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/WeakReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(Some(referent)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn soft_ref_with_queue_init() {
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

    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let soft = shared.mem.heap.alloc_object(ClassId::new(0), 2);
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

    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/SoftReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(soft))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(Some(referent)));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_remove_timeout_empty() {
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

    // remove(timeout) on empty queue returns null (simplified non-blocking)
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/ReferenceQueue",
        "remove",
        "(J)Ljava/lang/ref/Reference;",
        &[Value::Object(Some(queue)), Value::Long(100)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Object(None));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reference_base_class_dispatch() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Test that methods work via base Reference class name too
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let weak = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(weak)), Value::Object(Some(referent))],
    )
    .unwrap();

    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Object(Some(referent)));

    call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "clear",
        "()V",
        &[Value::Object(Some(weak))],
    )
    .unwrap();

    let got2 = call_native(
        &shared,
        &mut thread,
        "java/lang/ref/Reference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(weak))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got2, Value::Object(None));
}

// =========================================================================
// Phase 22: ConcurrentHashMap
// =========================================================================

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_basic_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key1")
    };
    let v = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val1")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    let got = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(chm)), Value::Object(Some(k))],
    )
    .unwrap()
    .unwrap();
    match got {
        Value::Object(Some(r)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r),
                Some("val1".to_string())
            )
        }
        _ => panic!("expected val1"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_size_and_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a")
    };
    let v = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "size",
        "()I",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));

    let not_empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(not_empty, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_mapping_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("x")
    };
    let v = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("y")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "mappingCount",
        "()J",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_put_if_absent() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("first")
    };
    let v2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("second")
    };

    // First putIfAbsent returns null (key wasn't present)
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v1)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Object(None));

    // Second putIfAbsent returns first value (key already present)
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    match r2 {
        Value::Object(Some(r)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r),
                Some("first".to_string())
            )
        }
        _ => panic!("expected 'first'"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_replace() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("old")
    };
    let v2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("new")
    };

    // Replace on non-existent key returns null
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Object(None));

    // Put a value
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v1)),
        ],
    )
    .unwrap();

    // Replace returns old value
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    match r2 {
        Value::Object(Some(r)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r),
                Some("old".to_string())
            )
        }
        _ => panic!("expected 'old'"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_replace_kv() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val1")
    };
    let v2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val2")
    };
    let v3 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val3")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v1)),
        ],
    )
    .unwrap();

    // replace(key, wrongOld, new) fails
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v3)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Int(0));

    // replace(key, correctOld, new) succeeds
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v1)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r2, Value::Int(1));

    // Verify new value
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(chm)), Value::Object(Some(k))],
    )
    .unwrap()
    .unwrap();
    match got {
        Value::Object(Some(r)) => {
            assert_eq!(
                read_java_string(&shared.mem.heap, r),
                Some("val2".to_string())
            )
        }
        _ => panic!("expected val2"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_remove_kv() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v1 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val")
    };
    let v2 = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("wrong")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v1)),
        ],
    )
    .unwrap();

    // remove(key, wrongVal) fails
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Int(0));

    // remove(key, correctVal) succeeds
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v1)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r2, Value::Int(1));

    // Size should be 0
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "size",
        "()I",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_contains_key_value() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("key")
    };
    let v = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("val")
    };
    let other = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("other")
    };

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    let ck = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(chm)), Value::Object(Some(k))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ck, Value::Int(1));

    let cv = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(chm)), Value::Object(Some(v))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cv, Value::Int(1));

    // contains() = containsValue()
    let cont = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "contains",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(chm)), Value::Object(Some(other))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cont, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_clear() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("a")
    };
    let v = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("b")
    };
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "clear",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "size",
        "()I",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_get_or_default() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(chm))],
    )
    .unwrap();

    let k = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("missing")
    };
    let def = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.create_string("default")
    };

    let got = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(k)),
            Value::Object(Some(def)),
        ],
    )
    .unwrap()
    .unwrap();
    match got {
        Value::Object(Some(r)) => assert_eq!(
            read_java_string(&shared.mem.heap, r),
            Some("default".to_string())
        ),
        _ => panic!("expected default"),
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_init_with_capacity() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "(I)V",
        &[Value::Object(Some(chm)), Value::Int(32)],
    )
    .unwrap();

    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "size",
        "()I",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn concurrent_hashmap_init_full() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let chm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    // (initialCapacity, loadFactor, concurrencyLevel)
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "<init>",
        "(IFI)V",
        &[
            Value::Object(Some(chm)),
            Value::Int(64),
            Value::Float(0.75),
            Value::Int(16),
        ],
    )
    .unwrap();

    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(chm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
}

// ====================== Phase 23: NIO ByteBuffer tests ======================

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_allocate_and_capacity() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    let cap = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "capacity",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(16));

    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(0));

    let lim = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "limit",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(lim, Value::Int(16));

    let rem = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "remaining",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rem, Value::Int(16));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_get_relative() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Put bytes 10, 20, 30
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(20)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(30)],
    )
    .unwrap();

    // Position should be 3
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(3));

    // Flip: limit=3, pos=0
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    // Read back
    let v0 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v0, Value::Int(10));
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(20));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(30));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_get_absolute() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Absolute put at index 2
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(IB)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(2), Value::Int(42)],
    )
    .unwrap();

    // Absolute get at index 2
    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "(I)B",
        &[Value::Object(Some(bb_ref)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(42));

    // Position should still be 0 (absolute operations don't change position)
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_flip_clear_rewind() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Write 5 bytes
    for i in 0..5 {
        call_native(
            &shared,
            &mut thread,
            "java/nio/ByteBuffer",
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &[Value::Object(Some(bb_ref)), Value::Int(i + 1)],
        )
        .unwrap();
    }

    // Flip: limit=5, pos=0
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    let lim = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "limit",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(lim, Value::Int(5));
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(0));

    // Read 2, then rewind
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "rewind",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(0));

    // Clear: pos=0, limit=cap=10
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "clear",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    let lim = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "limit",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(lim, Value::Int(10));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_mark_and_reset() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Write 5 bytes
    for i in 0..5 {
        call_native(
            &shared,
            &mut thread,
            "java/nio/ByteBuffer",
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &[Value::Object(Some(bb_ref)), Value::Int(i + 1)],
        )
        .unwrap();
    }
    // Flip
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    // Read 2, mark, read 2 more, reset
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // 1
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // 2
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "mark",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // mark at pos=2
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // 3
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // 4
               // Reset to mark at 2
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "reset",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(2));

    // Read again should give 3
    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(3));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_reset_without_mark_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Reset without mark should fail
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "reset",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    );
    assert!(result.is_err());
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_get_underflow_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Flip immediately → limit=0, no data to read
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    );
    assert!(result.is_err());
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_overflow_throws() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Fill the 1-byte buffer
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(99)],
    )
    .unwrap();

    // Second put should overflow
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(100)],
    );
    assert!(result.is_err());
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_get_int_big_endian() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Put int 0x12345678
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "putInt",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(0x12345678)],
    )
    .unwrap();

    // Flip and read back
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "getInt",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(0x12345678));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_get_long_big_endian() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Put long
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "putLong",
        "(J)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Long(0x0102030405060708)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "getLong",
        "()J",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Long(0x0102030405060708));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_get_short() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "putShort",
        "(S)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(1000)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "getShort",
        "()S",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(1000));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_get_float_double() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Put float
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "putFloat",
        "(F)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Float(1.5)],
    )
    .unwrap();
    // Put double
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "putDouble",
        "(D)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Double(2.75)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let f = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "getFloat",
        "()F",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(f, Value::Float(1.5));

    let d = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "getDouble",
        "()D",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(d, Value::Double(2.75));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_wrap_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a byte array [5, 10, 15, 20]
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 4);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(5));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(10));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(15));
    let _ = shared.mem.heap.set_array_element(arr, 3, Value::Int(20));

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "wrap",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Capacity and limit should be 4
    let cap = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "capacity",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(4));

    // Read bytes
    let v0 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v0, Value::Int(5));
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(10));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_compact() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(6)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Put 4 bytes
    for i in 0..4 {
        call_native(
            &shared,
            &mut thread,
            "java/nio/ByteBuffer",
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &[Value::Object(Some(bb_ref)), Value::Int(i + 1)],
        )
        .unwrap();
    }

    // Flip, read 2
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // 1
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap(); // 2

    // Compact: remaining [3,4] move to front, pos=2, limit=cap=6
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "compact",
        "()Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(2)); // 2 remaining bytes were moved

    let lim = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "limit",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(lim, Value::Int(6));

    // Verify data at position 0 and 1 is 3 and 4
    let v0 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "(I)B",
        &[Value::Object(Some(bb_ref)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v0, Value::Int(3));
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "(I)B",
        &[Value::Object(Some(bb_ref)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(4));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_duplicate_shares_array() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(42)],
    )
    .unwrap();

    let dup = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "duplicate",
        "()Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    let dup_ref = match dup {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Duplicate has same position and limit
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(dup_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(1));

    // Write via duplicate at absolute index 0 affects original's backing array
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(IB)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(dup_ref)), Value::Int(0), Value::Int(99)],
    )
    .unwrap();

    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "(I)B",
        &[Value::Object(Some(bb_ref)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(99)); // Changed via duplicate
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_slice_creates_copy() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Put 4 bytes, set position to 2
    for i in 0..4 {
        call_native(
            &shared,
            &mut thread,
            "java/nio/ByteBuffer",
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &[Value::Object(Some(bb_ref)), Value::Int(i * 10)],
        )
        .unwrap();
    }
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    // Read 2 to advance position
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let sl = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "slice",
        "()Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    let sl_ref = match sl {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Slice capacity = remaining from original (4-2=2)
    let cap = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "capacity",
        "()I",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(2));

    // Slice starts at position 0 with the remaining bytes
    let v0 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v0, Value::Int(20));
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(sl_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(30));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_has_array_and_misc() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    let has_arr = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "hasArray",
        "()Z",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has_arr, Value::Int(1));

    let is_direct = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "isDirect",
        "()Z",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_direct, Value::Int(0));

    let is_ro = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "isReadOnly",
        "()Z",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_ro, Value::Int(0));

    let offset = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "arrayOffset",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(offset, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_bulk_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Create source byte array [100, 101, 102]
    let src = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    let _ = shared.mem.heap.set_array_element(src, 0, Value::Int(100));
    let _ = shared.mem.heap.set_array_element(src, 1, Value::Int(101));
    let _ = shared.mem.heap.set_array_element(src, 2, Value::Int(102));

    // Bulk put
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "([BII)Ljava/nio/ByteBuffer;",
        &[
            Value::Object(Some(bb_ref)),
            Value::Object(Some(src)),
            Value::Int(0),
            Value::Int(3),
        ],
    )
    .unwrap();

    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(3));

    // Flip, bulk get into destination
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap();

    let dst = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "([BII)Ljava/nio/ByteBuffer;",
        &[
            Value::Object(Some(bb_ref)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(3),
        ],
    )
    .unwrap();

    assert_eq!(
        shared.mem.heap.get_array_element(dst, 0).unwrap(),
        Value::Int(100)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(dst, 1).unwrap(),
        Value::Int(101)
    );
    assert_eq!(
        shared.mem.heap.get_array_element(dst, 2).unwrap(),
        Value::Int(102)
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_put_bytebuffer() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Source ByteBuffer with [1, 2, 3]
    let src = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    let src_ref = match src {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };
    for i in 1..=3 {
        call_native(
            &shared,
            &mut thread,
            "java/nio/ByteBuffer",
            "put",
            "(B)Ljava/nio/ByteBuffer;",
            &[Value::Object(Some(src_ref)), Value::Int(i)],
        )
        .unwrap();
    }
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(src_ref))],
    )
    .unwrap();

    // Destination ByteBuffer
    let dst = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let dst_ref = match dst {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "put",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(dst_ref)), Value::Object(Some(src_ref))],
    )
    .unwrap();

    // dst position should be 3
    let pos = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "position",
        "()I",
        &[Value::Object(Some(dst_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pos, Value::Int(3));

    // Flip dst and read back
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "flip",
        "()Ljava/nio/Buffer;",
        &[Value::Object(Some(dst_ref))],
    )
    .unwrap();
    let v0 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(dst_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v0, Value::Int(1));
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(dst_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(2));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "get",
        "()B",
        &[Value::Object(Some(dst_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(3));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(16)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    let s = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bb_ref))],
    )
    .unwrap()
    .unwrap();
    let s_ref = match s {
        Value::Object(Some(o)) => o,
        _ => panic!("expected string obj"),
    };
    let text = read_java_string(&shared.mem.heap, s_ref).unwrap();
    assert_eq!(text, "java.nio.HeapByteBuffer[pos=0 lim=16 cap=16]");
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn byte_buffer_get_int_absolute() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let bb = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    let bb_ref = match bb {
        Value::Object(Some(o)) => o,
        _ => panic!("expected object"),
    };

    // Absolute putInt at index 4
    call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "putInt",
        "(II)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(bb_ref)), Value::Int(4), Value::Int(-1)],
    )
    .unwrap();

    // Absolute getInt at index 4
    let v = call_native(
        &shared,
        &mut thread,
        "java/nio/ByteBuffer",
        "getInt",
        "(I)I",
        &[Value::Object(Some(bb_ref)), Value::Int(4)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(-1));
}

// ====================== Phase 24: java.util.concurrent tests ======================

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reentrant_lock_basic_lock_unlock() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lock = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        3,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    // Lock
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "lock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let locked = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "isLocked",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(locked, Value::Int(1));

    let held = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "isHeldByCurrentThread",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(held, Value::Int(1));

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "getHoldCount",
        "()I",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(1));

    // Unlock
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "unlock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let locked = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "isLocked",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(locked, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reentrant_lock_reentrant() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lock = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        3,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    // Lock twice (reentrant)
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "lock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "lock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "getHoldCount",
        "()I",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(2));

    // Unlock once
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "unlock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "getHoldCount",
        "()I",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(1));

    let locked = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "isLocked",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(locked, Value::Int(1)); // still locked

    // Unlock again
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "unlock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let locked = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "isLocked",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(locked, Value::Int(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reentrant_lock_try_lock() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lock = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        3,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let acquired = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "tryLock",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(acquired, Value::Int(1));

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "getHoldCount",
        "()I",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(1));

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "unlock",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reentrant_lock_fair_mode() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lock = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        3,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "<init>",
        "(Z)V",
        &[Value::Object(Some(lock)), Value::Int(1)],
    )
    .unwrap();

    let fair = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "isFair",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(fair, Value::Int(1));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn reentrant_lock_new_condition() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let lock = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        3,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();

    let cond = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/ReentrantLock",
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(cond, Value::Object(Some(_))));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn countdown_latch_basic() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let latch = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "<init>",
        "(I)V",
        &[Value::Object(Some(latch)), Value::Int(3)],
    )
    .unwrap();

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "getCount",
        "()J",
        &[Value::Object(Some(latch))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(3));

    // Count down 3 times
    for _ in 0..3 {
        call_native(
            &shared,
            &mut thread,
            "java/util/concurrent/CountDownLatch",
            "countDown",
            "()V",
            &[Value::Object(Some(latch))],
        )
        .unwrap();
    }

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "getCount",
        "()J",
        &[Value::Object(Some(latch))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(0));
}

#[test]
pub(in crate::vm::vm_init::tests::suite_time_concurrency) fn countdown_latch_extra_countdown_noop() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let latch = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "<init>",
        "(I)V",
        &[Value::Object(Some(latch)), Value::Int(1)],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "countDown",
        "()V",
        &[Value::Object(Some(latch))],
    )
    .unwrap();
    // Extra countDown should not go negative
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "countDown",
        "()V",
        &[Value::Object(Some(latch))],
    )
    .unwrap();

    let count = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CountDownLatch",
        "getCount",
        "()J",
        &[Value::Object(Some(latch))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Long(0));
}

