// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ===== Phase 65 Tests =====

#[test]
fn priority_blocking_queue_basic_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pbq = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(pbq))],
    )
    .unwrap();
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(pbq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    // Put elements
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pbq)), Value::Int(30)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pbq)), Value::Int(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pbq)), Value::Int(20)],
    )
    .unwrap();
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "size",
        "()I",
        &[Value::Object(Some(pbq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(3));
}

#[test]
fn priority_blocking_queue_min_heap_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pbq = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(pbq))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pbq)), Value::Int(50)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pbq)), Value::Int(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "put",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pbq)), Value::Int(30)],
    )
    .unwrap();
    // peek = min = 10
    let peek = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "peek",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pbq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(peek, Value::Int(10));
    // poll returns min
    let p1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pbq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(p1, Value::Int(10));
    let p2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pbq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(p2, Value::Int(30));
    let p3 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/PriorityBlockingQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pbq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(p3, Value::Int(50));
}

#[test]
fn delay_queue_basic_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let dq = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/DelayQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(dq))],
    )
    .unwrap();
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/DelayQueue",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(dq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/DelayQueue",
        "put",
        "(Ljava/util/concurrent/Delayed;)V",
        &[Value::Object(Some(dq)), Value::Object(None)],
    )
    .unwrap();
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/DelayQueue",
        "size",
        "()I",
        &[Value::Object(Some(dq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(1));
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/DelayQueue",
        "clear",
        "()V",
        &[Value::Object(Some(dq))],
    )
    .unwrap();
    let size2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/DelayQueue",
        "size",
        "()I",
        &[Value::Object(Some(dq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size2, Value::Int(0));
}

#[test]
fn completion_service_basic_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ecs = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorCompletionService",
        "<init>",
        "(Ljava/util/concurrent/Executor;)V",
        &[Value::Object(Some(ecs)), Value::Object(None)],
    )
    .unwrap();
    // submit returns a future
    let fut = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorCompletionService",
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        &[Value::Object(Some(ecs)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(fut, Value::Object(Some(_))));
    // poll returns null (nothing queued)
    let polled = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorCompletionService",
        "poll",
        "()Ljava/util/concurrent/Future;",
        &[Value::Object(Some(ecs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(polled, Value::Object(None));
}

/// A `null` class or type is the factory's `NullPointerException`, as the JDK
/// bodies these bridges stand for raise it (interpreter round i1 wave 40, lane
/// L4; this test used to require a handle for it).
#[test]
fn method_handles_factories_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    for (method, descriptor) in [
        (
            "identity",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        ),
        (
            "arrayElementGetter",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        ),
        (
            "empty",
            "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        ),
    ] {
        let refused = call_native(
            &shared,
            &mut thread,
            "java/lang/invoke/MethodHandles",
            method,
            descriptor,
            &[Value::Object(None)],
        );
        assert!(
            matches!(
                refused,
                Err(crate::error::MethodCallFailed::InternalError(
                    crate::error::VmError::Runtime(
                        crate::error::RuntimeError::NullPointerException { .. }
                    )
                ))
            ),
            "{method}(null) must be a NullPointerException"
        );
    }
}

#[test]
fn datetime_formatter_builder_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let builder = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/time/format/DateTimeFormatterBuilder",
        "<init>",
        "()V",
        &[Value::Object(Some(builder))],
    )
    .unwrap();
    // Chain: appendLiteral returns self
    let b2 = call_native(
        &shared,
        &mut thread,
        "java/time/format/DateTimeFormatterBuilder",
        "appendLiteral",
        "(Ljava/lang/String;)Ljava/time/format/DateTimeFormatterBuilder;",
        &[Value::Object(Some(builder)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(b2, Value::Object(Some(_))));
    // toFormatter
    let fmt = call_native(
        &shared,
        &mut thread,
        "java/time/format/DateTimeFormatterBuilder",
        "toFormatter",
        "()Ljava/time/format/DateTimeFormatter;",
        &[Value::Object(Some(builder))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(fmt, Value::Object(Some(_))));
}

#[test]
fn format_style_enum_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let vals = call_native(
        &shared,
        &mut thread,
        "java/time/format/FormatStyle",
        "values",
        "()[Ljava/time/format/FormatStyle;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(arr)) = vals {
        let len = shared.mem.heap.array_length(arr);
        assert_eq!(len, 4);
    } else {
        panic!("expected array");
    }
}

#[test]
fn checked_collections_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let al = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    // checkedList returns the list itself
    let checked = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "checkedList",
        "(Ljava/util/List;Ljava/lang/Class;)Ljava/util/List;",
        &[Value::Object(Some(al)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(checked, Value::Object(Some(al)));
}

#[test]
fn unmodifiable_sequenced_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let al = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "unmodifiableSequencedCollection",
        "(Ljava/util/SequencedCollection;)Ljava/util/SequencedCollection;",
        &[Value::Object(Some(al))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Object(Some(al)));
}

#[test]
fn pattern_split_as_stream_p65() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Create a Pattern object (2-field: source, flags)
    let pat = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let source = create_java_string(&shared, ",");
    shared
        .mem
        .heap
        .set_field(pat, 0, Value::Object(Some(source)));
    shared.mem.heap.set_field(pat, 1, Value::Int(0));
    let input = create_java_string(&shared, "a,b,c");
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/regex/Pattern",
        "splitAsStream",
        "(Ljava/lang/CharSequence;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(pat)), Value::Object(Some(input))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(stream, Value::Object(Some(_))));
    // Convert to list and check size
    if let Value::Object(Some(s)) = stream {
        let list = call_native(
            &shared,
            &mut thread,
            "java/util/stream/Stream",
            "toList",
            "()Ljava/util/List;",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(al)) = list {
            let size = call_native(
                &shared,
                &mut thread,
                "java/util/ArrayList",
                "size",
                "()I",
                &[Value::Object(Some(al))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(size, Value::Int(3));
        }
    }
}

// -------------------------------------------------------
// Phase 66 tests
// -------------------------------------------------------

#[test]
fn collator_basics_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let c = "java/text/Collator";

    // getInstance
    let coll = call_native(
        &shared,
        &mut thread,
        c,
        "getInstance",
        "()Ljava/text/Collator;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(coll, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = coll {
        // Default strength is SECONDARY (2)
        let s = call_native(
            &shared,
            &mut thread,
            c,
            "getStrength",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(s, Value::Int(2));

        // setStrength
        call_native(
            &shared,
            &mut thread,
            c,
            "setStrength",
            "(I)V",
            &[Value::Object(Some(obj)), Value::Int(0)],
        )
        .unwrap();
        let s2 = call_native(
            &shared,
            &mut thread,
            c,
            "getStrength",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(s2, Value::Int(0));

        // compare
        let a = create_java_string(&shared, "apple");
        let b = create_java_string(&shared, "banana");
        let cmp = call_native(
            &shared,
            &mut thread,
            c,
            "compare",
            "(Ljava/lang/String;Ljava/lang/String;)I",
            &[
                Value::Object(Some(obj)),
                Value::Object(Some(a)),
                Value::Object(Some(b)),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(cmp, Value::Int(-1));

        // equals
        let eq = call_native(
            &shared,
            &mut thread,
            c,
            "equals",
            "(Ljava/lang/String;Ljava/lang/String;)Z",
            &[
                Value::Object(Some(obj)),
                Value::Object(Some(a)),
                Value::Object(Some(a)),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(eq, Value::Int(1));
    }
}

#[test]
fn break_iterator_basics_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let bi = "java/text/BreakIterator";

    // getWordInstance
    let iter = call_native(
        &shared,
        &mut thread,
        bi,
        "getWordInstance",
        "()Ljava/text/BreakIterator;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(iter, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = iter {
        // setText
        let text = create_java_string(&shared, "hello world test");
        call_native(
            &shared,
            &mut thread,
            bi,
            "setText",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(obj)), Value::Object(Some(text))],
        )
        .unwrap();

        // first
        let f = call_native(
            &shared,
            &mut thread,
            bi,
            "first",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(f, Value::Int(0));

        // next — finds next boundary
        let n = call_native(
            &shared,
            &mut thread,
            bi,
            "next",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Int(v) = n {
            assert!(v > 0);
        }

        // current
        let cur = call_native(
            &shared,
            &mut thread,
            bi,
            "current",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(cur, n);

        // last
        let l = call_native(
            &shared,
            &mut thread,
            bi,
            "last",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(l, Value::Int(16)); // "hello world test".len()
    }
}

#[test]
fn file_visit_result_enum_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let fvr = "java/nio/file/FileVisitResult";

    // values()
    let arr = call_native(
        &shared,
        &mut thread,
        fvr,
        "values",
        "()[Ljava/nio/file/FileVisitResult;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(arr, Value::Object(Some(_))));

    // valueOf
    let name = create_java_string(&shared, "TERMINATE");
    let v = call_native(
        &shared,
        &mut thread,
        fvr,
        "valueOf",
        "(Ljava/lang/String;)Ljava/nio/file/FileVisitResult;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(v, Value::Object(Some(_))));
}

#[test]
fn simple_file_visitor_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sfv = "java/nio/file/SimpleFileVisitor";

    // preVisitDirectory returns CONTINUE
    let result = call_native(&shared, &mut thread, sfv, "preVisitDirectory",
            "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
            &[Value::Object(None), Value::Object(None)]).unwrap().unwrap();
    assert!(matches!(result, Value::Object(Some(_))));

    // visitFile returns CONTINUE
    let result2 = call_native(&shared, &mut thread, sfv, "visitFile",
            "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
            &[Value::Object(None), Value::Object(None)]).unwrap().unwrap();
    assert!(matches!(result2, Value::Object(Some(_))));
}

#[test]
fn watch_service_stubs_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ws = "java/nio/file/WatchService";

    // Real WatchService layout is 3 fields: [0]=regs, [1]=count, [2]=open
    // (see WS_NUM_FIELDS in native-io/src/lib.rs). poll() rejects the
    // request with "WatchService is closed" when slot 2 (open) isn't
    // Int(1), so we must initialize the object to match the native
    // layout even though we never actually register a directory.
    let ws_obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(ws_obj, 0, Value::Object(None)); // regs
    shared.mem.heap.set_field(ws_obj, 1, Value::Int(0)); // count
    shared.mem.heap.set_field(ws_obj, 2, Value::Int(1)); // open = true

    // poll returns None (no events — no directory registered)
    let key = call_native(
        &shared,
        &mut thread,
        ws,
        "poll",
        "()Ljava/nio/file/WatchKey;",
        &[Value::Object(Some(ws_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(key, Value::Object(None));

    // close is a no-op (takes the WatchService as this)
    call_native(
        &shared,
        &mut thread,
        ws,
        "close",
        "()V",
        &[Value::Object(Some(ws_obj))],
    )
    .unwrap();
}

#[test]
fn class_desc_basics_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let cd = "java/lang/constant/ClassDesc";

    // ClassDesc.of(name)
    let name = create_java_string(&shared, "java.lang.String");
    let desc = call_native(
        &shared,
        &mut thread,
        cd,
        "of",
        "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(desc, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = desc {
        // displayName
        let dn = call_native(
            &shared,
            &mut thread,
            cd,
            "displayName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = dn {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "java.lang.String");
        }

        // isClassOrInterface
        let is_class = call_native(
            &shared,
            &mut thread,
            cd,
            "isClassOrInterface",
            "()Z",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(is_class, Value::Int(1));

        // isPrimitive
        let is_prim = call_native(
            &shared,
            &mut thread,
            cd,
            "isPrimitive",
            "()Z",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(is_prim, Value::Int(0));
    }
}

#[test]
fn class_desc_of_package_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let cd = "java/lang/constant/ClassDesc";

    let pkg = create_java_string(&shared, "com.example");
    let name = create_java_string(&shared, "MyClass");
    let desc = call_native(
        &shared,
        &mut thread,
        cd,
        "of",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
        &[Value::Object(Some(pkg)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(desc, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = desc {
        let dn = call_native(
            &shared,
            &mut thread,
            cd,
            "displayName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = dn {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "com.example.MyClass");
        }
    }
}

#[test]
fn thread_builder_virtual_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let t = "java/lang/Thread";

    // Thread.ofVirtual()
    let builder = call_native(
        &shared,
        &mut thread,
        t,
        "ofVirtual",
        "()Ljava/lang/Thread$Builder;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(builder, Value::Object(Some(_))));

    // Thread.ofPlatform()
    let builder2 = call_native(
        &shared,
        &mut thread,
        t,
        "ofPlatform",
        "()Ljava/lang/Thread$Builder;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(builder2, Value::Object(Some(_))));

    // Thread.threadId()
    let tid = call_native(&shared, &mut thread, t, "threadId", "()J", &[])
        .unwrap()
        .unwrap();
    assert!(matches!(tid, Value::Long(_)));

    // Thread.isVirtual()
    let virt = call_native(&shared, &mut thread, t, "isVirtual", "()Z", &[])
        .unwrap()
        .unwrap();
    assert_eq!(virt, Value::Int(0));
}

#[test]
fn thread_builder_name_start_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tb = "java/lang/Thread$Builder";

    // ofVirtual + name + start
    let builder = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "ofVirtual",
        "()Ljava/lang/Thread$Builder;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(b)) = builder {
        let name = create_java_string(&shared, "my-worker");
        let named = call_native(
            &shared,
            &mut thread,
            tb,
            "name",
            "(Ljava/lang/String;)Ljava/lang/Thread$Builder;",
            &[Value::Object(Some(b)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(named, Value::Object(Some(_))));

        // start with a null runnable — thread starts but run() finds no target
        let thr = call_native(
            &shared,
            &mut thread,
            tb,
            "start",
            "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
            &[Value::Object(Some(b)), Value::Object(None)],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(thr, Value::Object(Some(_))));
        // Wait a moment for the thread to complete
        std::thread::sleep(std::time::Duration::from_millis(50));
    } else {
        panic!(
            "Thread.ofVirtual()Ljava/lang/Thread$Builder; must answer a reference, \
                 got {builder:?}"
        );
    }
}

#[test]
fn pushback_reader_basics_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pr = "java/io/PushbackReader";

    // Create PushbackReader (3-field synthetic)
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        pr,
        "<init>",
        "(Ljava/io/Reader;)V",
        &[Value::Object(Some(obj)), Value::Object(None)],
    )
    .unwrap();

    // Initially read returns -1 (pos == buf_len, no underlying reader)
    let ch = call_native(
        &shared,
        &mut thread,
        pr,
        "read",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ch, Value::Int(-1));

    // unread a character
    call_native(
        &shared,
        &mut thread,
        pr,
        "unread",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(65)],
    )
    .unwrap(); // 'A'

    // ready after unread
    let rdy = call_native(
        &shared,
        &mut thread,
        pr,
        "ready",
        "()Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rdy, Value::Int(1));

    // read returns the unread char
    let ch2 = call_native(
        &shared,
        &mut thread,
        pr,
        "read",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ch2, Value::Int(65));

    // `close()` delegates to the wrapped Reader at slot 0, which is null
    // here — that is why it is quiet, not a discarded result. The
    // delegation propagates since W7-57-close-flush-swallow-sweep.md.
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Object(None));
    call_native(
        &shared,
        &mut thread,
        pr,
        "close",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
}

#[test]
fn pushback_reader_overflow_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pr = "java/io/PushbackReader";

    // Create with buffer size 1
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        pr,
        "<init>",
        "(Ljava/io/Reader;)V",
        &[Value::Object(Some(obj)), Value::Object(None)],
    )
    .unwrap();

    // unread one char — OK
    call_native(
        &shared,
        &mut thread,
        pr,
        "unread",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(66)],
    )
    .unwrap();

    // unread again — should fail (buffer overflow)
    let result = call_native(
        &shared,
        &mut thread,
        pr,
        "unread",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(67)],
    );
    assert!(result.is_err());
}

#[test]
fn method_type_desc_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mtd = "java/lang/constant/MethodTypeDesc";

    let desc_str = create_java_string(&shared, "(Ljava/lang/String;)I");
    let obj = call_native(
        &shared,
        &mut thread,
        mtd,
        "ofDescriptor",
        "(Ljava/lang/String;)Ljava/lang/constant/MethodTypeDesc;",
        &[Value::Object(Some(desc_str))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(obj, Value::Object(Some(_))));
}

// `watch_event_kinds_p66` WAS HERE AND IS DELETED (E40-1 §1, answering
// E36-1 N2(a)). It asserted
//
//     read_java_string(call_native(StandardWatchEventKinds, "ENTRY_CREATE",
//                                  "Ljava/nio/file/WatchEvent$Kind;")) == "ENTRY_CREATE"
//
// i.e. it did not merely keep a dead registration alive, it PINNED A WRONG
// TYPE AS CORRECT. Measured on the oracle (JDK 25.0.3+9-LTS):
//
//     ENTRY_CREATE.getClass().getName()
//       = java.nio.file.StandardWatchEventKinds$StdWatchEventKind
//     ENTRY_CREATE.name() = "ENTRY_CREATE"
//     ENTRY_CREATE.type() = interface java.nio.file.Path
//     (ENTRY_CREATE instanceof String) = false
//     javap: public static final WatchEvent$Kind<Path> ENTRY_CREATE;
//
// The four registrations it drove (`native-builtins/src/phases_late/
// nio_file.rs`, `register_p66_watch_service`) each answer
// `ctx.create_string("ENTRY_CREATE")` — a `java/lang/String` where the
// descriptor names a `WatchEvent$Kind`. They are also unreachable:
// `StandardWatchEventKinds.ENTRY_CREATE` compiles to a real `getstatic`
// (measured), and `getstatic` never consults the native registry
// (E21-1 §1). So EVERY assertion this test could make was one of
//   (a) green now and red the moment the type is fixed — what it did;
//   (b) red now, because the row answers a String today;
//   (c) vacuous — and its second half, `matches!(modify, Object(Some(_)))`,
//       already was.
// There is no fourth option, which is why the right edit is deletion and
// not a rewrite. The row deletion is nominated (E40-1 §5 N1); this file's
// `call_native` panics on an unregistered triple, so it had to go first.
//
// The behaviour worth testing is the CONSUMER, and it lives in a crate this
// test module cannot reach through the registry: `native-io`'s
// `watch_event_kind_bit` / `watch_event_kind_object`. Replacement coverage
// is `native-io/src/lib.rs`'s
// `watch_event_kind_bit_translates_all_three_kind_shapes` and
// `watch_event_kind_object_round_trips_through_the_bit`, which assert the
// oracle's `name()` and reject `OVERFLOW`.

#[test]
fn start_virtual_thread_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let thr = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "startVirtualThread",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(thr, Value::Object(Some(_))));
    std::thread::sleep(std::time::Duration::from_millis(50));
}

#[test]
fn files_walk_file_tree_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // walkFileTree returns the path argument
    let path = create_java_string(&shared, "/tmp/test");
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
        &[Value::Object(Some(path)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Object(Some(path)));
}

// -------------------------------------------------------
// Phase 67 tests
// -------------------------------------------------------

#[test]
fn structured_task_scope_basic_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sts = "jdk/incubator/concurrent/StructuredTaskScope";

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        sts,
        "<init>",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();

    // isShutdown initially false
    let shut = call_native(
        &shared,
        &mut thread,
        sts,
        "isShutdown",
        "()Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut, Value::Int(0));

    // shutdown
    call_native(
        &shared,
        &mut thread,
        sts,
        "shutdown",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    let shut2 = call_native(
        &shared,
        &mut thread,
        sts,
        "isShutdown",
        "()Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut2, Value::Int(1));

    // join returns self
    let joined = call_native(
        &shared,
        &mut thread,
        sts,
        "join",
        "()Ljdk/incubator/concurrent/StructuredTaskScope;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(joined, Value::Object(Some(obj)));

    // close is no-op
    call_native(
        &shared,
        &mut thread,
        sts,
        "close",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
}

#[test]
fn scoped_value_basic_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sv = "java/lang/ScopedValue";

    // newInstance
    let scoped = call_native(
        &shared,
        &mut thread,
        sv,
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(scoped, Value::Object(Some(_))));

    if let Value::Object(Some(s)) = scoped {
        // Not bound initially
        let bound = call_native(
            &shared,
            &mut thread,
            sv,
            "isBound",
            "()Z",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(bound, Value::Int(0));

        // get throws when not bound
        let err = call_native(
            &shared,
            &mut thread,
            sv,
            "get",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(s))],
        );
        assert!(err.is_err());

        // orElse returns default when not bound
        let val = call_native(
            &shared,
            &mut thread,
            sv,
            "orElse",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(s)), Value::Int(42)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(val, Value::Int(42));
    }
}

#[test]
fn gatherer_of_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let g = "java/util/stream/Gatherer";

    // Gatherer.of(integrator) — pass null integrator
    let gatherer = call_native(
        &shared,
        &mut thread,
        g,
        "of",
        "(Ljava/util/stream/Gatherer$Integrator;)Ljava/util/stream/Gatherer;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(gatherer, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = gatherer {
        // defaultInitializer
        let init = call_native(
            &shared,
            &mut thread,
            g,
            "defaultInitializer",
            "()Ljava/util/function/Supplier;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(init, Value::Object(None));

        // defaultFinisher
        let fin = call_native(
            &shared,
            &mut thread,
            g,
            "defaultFinisher",
            "()Ljava/util/function/BiConsumer;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(fin, Value::Object(None));
    }
}

#[test]
fn gatherers_window_fixed_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let gs = "java/util/stream/Gatherers";

    let g = call_native(
        &shared,
        &mut thread,
        gs,
        "windowFixed",
        "(I)Ljava/util/stream/Gatherer;",
        &[Value::Int(3)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(g, Value::Object(Some(_))));
}

#[test]
fn async_file_channel_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let afc = "java/nio/channels/AsynchronousFileChannel";

    // `AsynchronousFileChannel.open` is file-backed for real (it opens the
    // path through `std::fs`), and the JDK specifies a null path as a
    // NullPointerException — so the channel needs a real file. A fresh
    // empty one also makes the `size() == 0` assertion below meaningful
    // rather than a property of "no file was ever opened".
    let file_path = std::env::temp_dir().join(format!(
        "cratonvm_async_file_channel_p67_{}.tmp",
        std::process::id()
    ));
    std::fs::write(&file_path, b"").expect("create empty temp file");
    let path_text = create_java_string(&shared, &file_path.to_string_lossy());
    let path = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Paths",
        "get",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(path_text))],
    )
    .unwrap()
    .unwrap();
    let path_ref = path.as_object().unwrap();

    // Null options array => the default READ access mode.
    let ch = call_native(&shared, &mut thread, afc, "open",
            "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;",
            &[Value::Object(Some(path_ref)), Value::Object(None)]).unwrap().unwrap();
    assert!(matches!(ch, Value::Object(Some(_))));

    if let Value::Object(Some(c)) = ch {
        // isOpen
        let open = call_native(
            &shared,
            &mut thread,
            afc,
            "isOpen",
            "()Z",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(open, Value::Int(1));

        // size
        let sz = call_native(
            &shared,
            &mut thread,
            afc,
            "size",
            "()J",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(sz, Value::Long(0));

        // close
        call_native(
            &shared,
            &mut thread,
            afc,
            "close",
            "()V",
            &[Value::Object(Some(c))],
        )
        .unwrap();
        let open2 = call_native(
            &shared,
            &mut thread,
            afc,
            "isOpen",
            "()Z",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(open2, Value::Int(0));
    }
    let _ = std::fs::remove_file(&file_path);
}

#[test]
fn async_socket_channel_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let asc = "java/nio/channels/AsynchronousSocketChannel";

    let ch = call_native(
        &shared,
        &mut thread,
        asc,
        "open",
        "()Ljava/nio/channels/AsynchronousSocketChannel;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(ch, Value::Object(Some(_))));

    // isOpen — every other call in this module passes the receiver as
    // args[0] (the JVM descriptor never counts it, but `call_native`/the
    // native callback both expect it there). This call used to omit it
    // entirely (`&[]`), so `isOpen` read field 1 off whatever it fell back
    // to instead of `ch` and answered closed (`Int(0)`). Pass `ch`.
    let open = call_native(&shared, &mut thread, asc, "isOpen", "()Z", &[ch])
        .unwrap()
        .unwrap();
    assert_eq!(open, Value::Int(1));
}

#[test]
fn arena_and_memory_segment_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let arena = "java/lang/foreign/Arena";

    // ofConfined
    let a = call_native(
        &shared,
        &mut thread,
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(a, Value::Object(Some(_))));

    if let Value::Object(Some(ar)) = a {
        // allocate
        let seg = call_native(
            &shared,
            &mut thread,
            arena,
            "allocate",
            "(J)Ljava/lang/foreign/MemorySegment;",
            &[Value::Object(Some(ar)), Value::Long(1024)],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(seg, Value::Object(Some(_))));

        if let Value::Object(Some(s)) = seg {
            let ms = "java/lang/foreign/MemorySegment";
            // byteSize
            let sz = call_native(
                &shared,
                &mut thread,
                ms,
                "byteSize",
                "()J",
                &[Value::Object(Some(s))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(sz, Value::Long(1024));

            // isNative — segments from Arena are backed by native memory
            let native = call_native(
                &shared,
                &mut thread,
                ms,
                "isNative",
                "()Z",
                &[Value::Object(Some(s))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(native, Value::Int(1));
        }

        // close arena
        call_native(
            &shared,
            &mut thread,
            arena,
            "close",
            "()V",
            &[Value::Object(Some(ar))],
        )
        .unwrap();
    }
}

#[test]
fn value_layout_constants_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let byte_layout = ffm_value_layout(&shared, &mut thread, "JAVA_BYTE")
        .unwrap()
        .unwrap();
    assert!(matches!(byte_layout, Value::Object(Some(_))));

    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();
    assert!(matches!(int_layout, Value::Object(Some(_))));

    let long_layout = ffm_value_layout(&shared, &mut thread, "JAVA_LONG")
        .unwrap()
        .unwrap();
    if let Value::Object(Some(ll)) = long_layout {
        let sz = call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/ValueLayout$OfLong",
            "byteSize",
            "()J",
            &[Value::Object(Some(ll))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(sz, Value::Long(8));
    }
}

// `string_template_basics_p67` WAS HERE AND IS DELETED WHOLE (E40-1 §1,
// answering E36-1 N2(b) — and going further than that nomination did).
//
// Measured on the oracle (JDK 25.0.3+9-LTS, Microsoft build 25.0.3+9):
//
//     $ javap -p java.lang.StringTemplate
//     Error: class not found: java.lang.StringTemplate
//
// The class does not exist. String templates were a preview API (JEP 430 in
// 21, JEP 459 in 22) and were WITHDRAWN; nothing in JDK 25 declares
// `java.lang.StringTemplate`, and no javac on this host can compile a
// reference to it. E36-1 N2(b) asked only for the `STR` block to go,
// keeping `of`/`interpolate` because they "exercise real method-shaped
// registrations". Method-shaped is not the same as reachable: being an
// `invokestatic` target only helps if some classfile can name the class,
// and none can. The whole test drove one withdrawn class, so the whole test
// goes; the paired registrar deletion is nominated (E40-1 §5 N2).
//
// Two further facts found while checking, and recorded because they change
// the verdict on the half E36-1 wanted kept:
//
//  * `fragments` is registered `()Ljava/util/List;` and its body is
//    `Ok(Some(ctx.get_field(this, 0)))` — slot 0 is the String handed to
//    `of(String)`. It answers a `java/lang/String` where its own descriptor
//    names a `java/util/List`. That is the SAME defect as
//    `watch_event_kinds_p66`'s (a native answering in a form its descriptor
//    does not name), in the family E36-1 proposed to keep, and no test
//    called it — so the family was not "half tested, half dead", it was
//    half dead and half untested.
//  * this test's own `interpolate` assertion sat inside
//    `if let Value::Object(Some(s)) = result` with no `else`, so an
//    `interpolate` that returned `Value::Int(0)` or a null would have
//    passed it silently.

#[test]
fn parameterized_type_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let pt = "java/lang/reflect/ParameterizedType";

    // Create a synthetic ParameterizedType
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let raw = shared.mem.heap.alloc_object(ClassId::new(0), 0); // mock raw type
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(raw)));
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    shared.mem.heap.set_field(obj, 1, Value::Object(Some(arr)));

    let raw_type = call_native(
        &shared,
        &mut thread,
        pt,
        "getRawType",
        "()Ljava/lang/reflect/Type;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(raw_type, Value::Object(Some(raw)));

    let type_args = call_native(
        &shared,
        &mut thread,
        pt,
        "getActualTypeArguments",
        "()[Ljava/lang/reflect/Type;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(type_args, Value::Object(Some(_))));

    let owner = call_native(
        &shared,
        &mut thread,
        pt,
        "getOwnerType",
        "()Ljava/lang/reflect/Type;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(owner, Value::Object(None));
}

#[test]
fn stack_walker_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sw = "java/lang/StackWalker";

    let walker = call_native(
        &shared,
        &mut thread,
        sw,
        "getInstance",
        "()Ljava/lang/StackWalker;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(walker, Value::Object(Some(_))));

    // `getCallerClass` is no longer the null stub this assertion was
    // written against: `stack_walker::native_get_caller_class` now walks
    // the captured trace and returns the caller of the @CallerSensitive
    // frame. This test calls it with an EMPTY interpreter stack (a bare
    // `call_native` off a fresh JvmThread), which is the documented
    // "degraded" case — the native deliberately answers with a non-null
    // `java.lang.Object` mirror rather than null, because the boot-path
    // feature detectors (JBoss-Modules / Keycloak) that reach it accept
    // any non-null Class but NPE on null. Assert the contract that
    // actually holds: a non-null Class mirror.
    //
    // (Known deviation from HotSpot, which throws
    // `UnsupportedOperationException` for a walker created without
    // `RETAIN_CLASS_REFERENCE` and `IllegalCallerException` when there is
    // no caller frame. The native does not receive the walker receiver at
    // all here, so it cannot consult the option set.)
    let caller = call_native(
        &shared,
        &mut thread,
        sw,
        "getCallerClass",
        "()Ljava/lang/Class;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(caller, Value::Object(Some(_))),
        "getCallerClass must degrade to a non-null Class mirror, got {caller:?}"
    );
}

#[test]
fn system_logger_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let name = create_java_string(&shared, "test.logger");
    let logger = call_native(
        &shared,
        &mut thread,
        "java/lang/System",
        "getLogger",
        "(Ljava/lang/String;)Ljava/lang/System$Logger;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(logger, Value::Object(Some(_))));

    // isLoggable — WAS A DIVERGENCE PIN, IS NOW AN ASSERTION OF THE
    // REFUSAL (F31, 2026-08-13; the fix is F20-1 §7's
    // `system_logger_require_level` in `native-builtins/src/lib.rs`).
    //
    // The pin that stood here asserted `Value::Int(1)` and called it "a
    // fabricated unconditional `true`". Two things about that text were
    // wrong, and both are worth keeping visible because they are the shape
    // a pin goes stale in (F20-1 §7.3):
    //
    //   * It was never unconditional. `system_logger_is_loggable` does
    //     compare severities; this row read as unconditional only because
    //     the call passes a null RECEIVER *and* a null LEVEL, and a null
    //     level fell through to an `INFO` default.
    //   * "the severity comparison this VM has nowhere to do yet" was
    //     already false when it was written — the comparison is that
    //     function's last line.
    //
    // What the oracle actually answers for THIS call (JDK 25.0.3+9-LTS,
    // `System.getLogger(…)` is a
    // `sun.util.logging.internal.LoggingProviderImpl$JULWrapper`):
    //
    //     isLoggable(null) !! NullPointerException: Cannot invoke
    //       "java.util.logging.Level.intValue()" because "level" is null
    //
    // Both JDK implementations agree: `JULWrapper` reaches
    // `java.util.logging.Level.intValue()` and `SimpleConsoleLogger`
    // reaches `PlatformLogger.Level.ordinal()`. They disagree only about
    // `OFF`, which is a different row and is recorded, not asserted, at
    // `system_logger_is_loggable`.
    //
    // This asserts the REFUSAL, so a fabricated permission cannot come
    // back silently. NOTE the shape: the native now returns `Err`, so the
    // old `.unwrap().unwrap()` would PANIC here, not assert-fail.
    let loggable = call_native(
        &shared,
        &mut thread,
        "java/lang/System$Logger",
        "isLoggable",
        "(Ljava/lang/System$Logger$Level;)Z",
        &[Value::Object(None), Value::Object(None)],
    )
    .expect_err("a null Level must not be answered with a permission");
    assert!(
        matches!(
            &loggable,
            crate::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { message: Some(_) }
            ))
        ),
        "isLoggable(null) must raise NullPointerException — HotSpot NPEs on \
             a null level on both the JULWrapper and the SimpleConsoleLogger \
             road, and the permissive answer is the one a caller cannot see. \
             Got {loggable:?}"
    );

    // `System$Logger$Level` — CONVERTED (E36-1 §4c, E40-1 N3). What stood
    // here was an existence check on one of seven field-shaped rows that no
    // bytecode could reach, deliberately left unconverted until three
    // blockers closed together: `classloading/class_manager.rs` declaring
    // this class's statics and its own `severity` field,
    // `phases_late.rs::system_logger_level_clinit` reproducing the real
    // `<clinit>` (severity included, so it does not shadow the JDK's with a
    // live regression), and this test reading the STATIC rather than the
    // dead row. All three land together here.
    //
    // Reads `INFO`'s fields directly rather than through `getSeverity()`/
    // `name()` — those are real bytecode methods on this class in real-JDK
    // mode and are not separately registered natives, so `call_native`
    // (an exact registry lookup, no hierarchy walk) cannot reach them the
    // way it reaches `java/lang/Enum`'s inherited `ordinal()`. Reading the
    // fields the `<clinit>` wrote is what actually exercises it.
    const SLL: &str = "java/lang/System$Logger$Level";
    {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.ensure_class_initialized(SLL)
            .unwrap_or_else(|e| panic!("{SLL} failed to initialize: {e:?}"));
    }
    call_native(&shared, &mut thread, SLL, "<clinit>", "()V", &[])
        .unwrap_or_else(|e| panic!("{SLL}.<clinit> failed: {e:?}"));
    let info = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let cid = ctx
            .class_id_by_name(SLL)
            .unwrap_or_else(|| panic!("{SLL} not loaded after <clinit>"));
        let idx = ctx
            .static_field_index_by_name(cid, "INFO")
            .unwrap_or_else(|| panic!("{SLL}.INFO is not a modelled static field"));
        match ctx.get_static_field(cid, idx) {
            Value::Object(Some(o)) => o,
            other => panic!("{SLL}.INFO = {other:?}, expected a Level"),
        }
    };
    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    // By POSITION rather than by name — equally valid once
    // `classloading/class_manager.rs`'s `jdk_superclass` table carries
    // `System$Logger$Level -> Enum` (added alongside this test, for the
    // reason recorded at that row: without it `severity`, this class's one
    // own field, collides with slot 0 instead of landing at slot 2). Slot
    // order: 0/1 inherited from `Enum` (name/ordinal), 2 this class's own
    // `severity`.
    let info_name = match ctx.get_field(info, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        other => panic!("INFO.name = {other:?}, expected a String"),
    };
    assert_eq!(
        info_name.as_deref(),
        Some("INFO"),
        "INFO's own name must round-trip through the <clinit> it was minted by"
    );
    assert_eq!(
        ctx.get_field(info, 2),
        Value::Int(800),
        "measured on JDK 25.0.3+9-LTS: System.Logger.Level.INFO.getSeverity() == 800"
    );
}

#[test]
fn linker_and_function_descriptor_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let linker = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Linker",
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(linker, Value::Object(Some(_))));

    let fd = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/FunctionDescriptor",
        "ofVoid",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(fd, Value::Object(Some(_))));
}

#[test]
fn shutdown_on_success_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sos = "jdk/incubator/concurrent/StructuredTaskScope$ShutdownOnSuccess";

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        sos,
        "<init>",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();

    // result initially null
    let r = call_native(
        &shared,
        &mut thread,
        sos,
        "result",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Object(None));

    // join
    let joined = call_native(
        &shared,
        &mut thread,
        sos,
        "join",
        "()Ljdk/incubator/concurrent/StructuredTaskScope$ShutdownOnSuccess;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(joined, Value::Object(Some(obj)));
}

#[test]
fn async_channel_group_p67() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let acg = "java/nio/channels/AsynchronousChannelGroup";

    let group = call_native(
        &shared,
        &mut thread,
        acg,
        "withFixedThreadPool",
        "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;",
        &[Value::Int(4), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(group, Value::Object(Some(_))));

    let shut = call_native(&shared, &mut thread, acg, "isShutdown", "()Z", &[])
        .unwrap()
        .unwrap();
    assert_eq!(shut, Value::Int(0));

    let term = call_native(
        &shared,
        &mut thread,
        acg,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        &[Value::Long(1000), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(term, Value::Int(1));
}

// -------------------------------------------------------
// Phase 68 tests
// -------------------------------------------------------

#[test]
fn crypto_mac_basics_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mac = "javax/crypto/Mac";

    let algo = create_java_string(&shared, "HmacSHA256");
    let m = call_native(
        &shared,
        &mut thread,
        mac,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Mac;",
        &[Value::Object(None), Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(m, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = m {
        // T16.11 drift fix: the Mac native in phases_late::register_p68_crypto_mac
        // now correctly raises IllegalStateException when doFinal is called before
        // init(), matching the JDK 25 javax.crypto.Mac contract. The old comment
        // here documented a crypto_impl stub that returned Ok(None) regardless of
        // init state; that stub is no longer the registered implementation.
        let res = call_native(
            &shared,
            &mut thread,
            mac,
            "doFinal",
            "()[B",
            &[Value::Object(Some(obj))],
        );
        assert!(
            res.is_err(),
            "doFinal before init must throw IllegalStateException per JDK spec"
        );

        // init, with a real HMAC key. `Mac.init(null)` throws InvalidKeyException
        // "No installed provider supports this key: (null)" on JDK 25 -- the text
        // this test failed with -- so the key must be real; see `make_secret_key`.
        let hmac_key = make_secret_key(&shared, &mut thread, "HmacSHA256", 32);
        call_native(
            &shared,
            &mut thread,
            mac,
            "init",
            "(Ljava/security/Key;)V",
            &[Value::Object(Some(obj)), Value::Object(Some(hmac_key))],
        )
        .unwrap();

        // doFinal stub returns Object(None) in crypto_impl
        let result = call_native(
            &shared,
            &mut thread,
            mac,
            "doFinal",
            "()[B",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(result, Value::Object(_)));

        // getMacLength
        let len = call_native(
            &shared,
            &mut thread,
            mac,
            "getMacLength",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(len, Value::Int(32));

        // getAlgorithm
        let alg = call_native(
            &shared,
            &mut thread,
            mac,
            "getAlgorithm",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = alg {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "HmacSHA256");
        }
    }
}

#[test]
fn ssl_context_basics_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ctx_class = "javax/net/ssl/SSLContext";

    // getDefault
    let ctx_obj = call_native(
        &shared,
        &mut thread,
        ctx_class,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(ctx_obj, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = ctx_obj {
        // getProtocol
        let proto = call_native(
            &shared,
            &mut thread,
            ctx_class,
            "getProtocol",
            "()Ljava/lang/String;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = proto {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "TLSv1.3");
        }

        // getSocketFactory
        let sf = call_native(
            &shared,
            &mut thread,
            ctx_class,
            "getSocketFactory",
            "()Ljavax/net/ssl/SSLSocketFactory;",
            &[Value::Object(Some(obj))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(sf, Value::Object(Some(_))));
    }
}

#[test]
fn trust_manager_factory_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tmf = "javax/net/ssl/TrustManagerFactory";

    let alg = call_native(
        &shared,
        &mut thread,
        tmf,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = alg {
        let text = read_java_string(&shared.mem.heap, s).unwrap();
        assert_eq!(text, "PKIX");
    }

    // Ask for the algorithm `getDefaultAlgorithm()` just reported. This
    // used to pass a null algorithm name and expect a factory back, which
    // no JDK does: `TrustManagerFactory.getInstance` starts with
    // `Objects.requireNonNull(algorithm, "null algorithm name")`, and our
    // native correspondingly refuses an algorithm that no registered
    // Provider claims. The test, not the native, was wrong — a factory
    // handed out for "no algorithm" is a factory nothing can trust.
    let algo = create_java_string(&shared, "PKIX");
    let inst = call_native(
        &shared,
        &mut thread,
        tmf,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        &[Value::Object(Some(algo))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(inst, Value::Object(Some(_))));

    // ...and an algorithm no provider implements must still be rejected.
    let bogus = create_java_string(&shared, "not-a-real-tmf-algorithm");
    assert!(
        call_native(
            &shared,
            &mut thread,
            tmf,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
            &[Value::Object(Some(bogus))],
        )
        .is_err(),
        "TrustManagerFactory.getInstance must reject an unknown algorithm"
    );
}

#[test]
fn certificate_factory_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let cf = "java/security/cert/CertificateFactory";

    // The type argument is incidental here -- this is a fixture for the
    // generateCertificate assertions below -- but it must be a type the
    // provider actually serves. It used to be `Value::Object(None)`, which
    // asserted the defect: `getInstance` validated nothing and handed back
    // an X.509 parser for every name, `null` included, where HotSpot raises
    // `CertificateException: <type> not found` at getInstance. All three
    // in-tree Java callers ask for "X.509". W7-29.
    let x509 = create_java_string(&shared, "X.509");
    let factory = call_native(
        &shared,
        &mut thread,
        cf,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
        // `create_java_string` hands back an `ObjectRef`; `call_native`
        // takes `&[Value]`. Landed as a bare `&[x509]` (E0308) because this
        // module still did not compile when it went in; dev repaired it
        // independently, same fix.
        &[Value::Object(Some(x509))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(factory, Value::Object(Some(_))));

    // generateCertificate
    let cert = call_native(
        &shared,
        &mut thread,
        cf,
        "generateCertificate",
        "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
        &[Value::Object(None), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(cert, Value::Object(Some(_))));

    if let Value::Object(Some(c)) = cert {
        let x509 = "java/security/cert/X509Certificate";
        let ver = call_native(
            &shared,
            &mut thread,
            x509,
            "getVersion",
            "()I",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(ver, Value::Int(3));

        let sig = call_native(
            &shared,
            &mut thread,
            x509,
            "getSigAlgName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = sig {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "SHA256withRSA");
        }
    }
}

#[test]
fn jdbc_connection_lifecycle_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let dm = "java/sql/DriverManager";

    let url = create_java_string(&shared, "jdbc:h2:mem:test");
    let conn = call_native(
        &shared,
        &mut thread,
        dm,
        "getConnection",
        "(Ljava/lang/String;)Ljava/sql/Connection;",
        &[Value::Object(None), Value::Object(Some(url))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(conn, Value::Object(Some(_))));

    if let Value::Object(Some(c)) = conn {
        let cn = "java/sql/Connection";

        // isClosed = false
        let closed = call_native(
            &shared,
            &mut thread,
            cn,
            "isClosed",
            "()Z",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(closed, Value::Int(0));

        // isValid
        let valid = call_native(
            &shared,
            &mut thread,
            cn,
            "isValid",
            "(I)Z",
            &[Value::Object(Some(c)), Value::Int(5)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(valid, Value::Int(1));

        // createStatement
        let stmt = call_native(
            &shared,
            &mut thread,
            cn,
            "createStatement",
            "()Ljava/sql/Statement;",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(stmt, Value::Object(Some(_))));

        // prepareStatement
        let sql = create_java_string(&shared, "SELECT 1");
        let pstmt = call_native(
            &shared,
            &mut thread,
            cn,
            "prepareStatement",
            "(Ljava/lang/String;)Ljava/sql/PreparedStatement;",
            &[Value::Object(Some(c)), Value::Object(Some(sql))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(pstmt, Value::Object(Some(_))));

        // close
        call_native(
            &shared,
            &mut thread,
            cn,
            "close",
            "()V",
            &[Value::Object(Some(c))],
        )
        .unwrap();
        let closed2 = call_native(
            &shared,
            &mut thread,
            cn,
            "isClosed",
            "()Z",
            &[Value::Object(Some(c))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(closed2, Value::Int(1));
    }
}

#[test]
fn xml_document_builder_factory_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let dbf = "javax/xml/parsers/DocumentBuilderFactory";

    let factory = call_native(
        &shared,
        &mut thread,
        dbf,
        "newInstance",
        "()Ljavax/xml/parsers/DocumentBuilderFactory;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(factory, Value::Object(Some(_))));

    if let Value::Object(Some(f)) = factory {
        // setNamespaceAware
        call_native(
            &shared,
            &mut thread,
            dbf,
            "setNamespaceAware",
            "(Z)V",
            &[Value::Object(Some(f)), Value::Int(1)],
        )
        .unwrap();

        // newDocumentBuilder
        let builder = call_native(
            &shared,
            &mut thread,
            dbf,
            "newDocumentBuilder",
            "()Ljavax/xml/parsers/DocumentBuilder;",
            &[Value::Object(Some(f))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(builder, Value::Object(Some(_))));
    }
}

#[test]
fn xml_sax_parser_factory_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let spf = "javax/xml/parsers/SAXParserFactory";

    let factory = call_native(
        &shared,
        &mut thread,
        spf,
        "newInstance",
        "()Ljavax/xml/parsers/SAXParserFactory;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(factory, Value::Object(Some(_))));

    if let Value::Object(Some(f)) = factory {
        let parser = call_native(
            &shared,
            &mut thread,
            spf,
            "newSAXParser",
            "()Ljavax/xml/parsers/SAXParser;",
            &[Value::Object(Some(f))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(parser, Value::Object(Some(_))));
    }
}

#[test]
fn xml_xpath_factory_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let xpf = "javax/xml/xpath/XPathFactory";

    let factory = call_native(
        &shared,
        &mut thread,
        xpf,
        "newInstance",
        "()Ljavax/xml/xpath/XPathFactory;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(factory, Value::Object(Some(_))));

    if let Value::Object(Some(f)) = factory {
        let xpath = call_native(
            &shared,
            &mut thread,
            xpf,
            "newXPath",
            "()Ljavax/xml/xpath/XPath;",
            &[Value::Object(Some(f))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(xpath, Value::Object(Some(_))));
    }
}

// RETIRED 2026-08-11 — `sql_types_constants_p68`.
//
// This test called `call_native(class, "INTEGER", "I")`: a
// FIELD name with a FIELD descriptor, asking the native registry for a
// pseudo-method that served a static constant. `dc55e8057` deleted that
// whole family as "dead everywhere" — correctly, since no JDK image has a
// *method* by that name and nothing in the sampled workloads called it —
// and the deletion is governed by `scripts/baselines/jdk-only-dead-everywhere.tsv`
// plus `registry_contracts.rs` for the registrations that must survive.
//
// The test was not updated with it, and could not report that: this module
// is `#[cfg(all(test, feature = "synthetic-jdk"))]`, and `dc55e8057` was
// verified with `cargo check --workspace --all-targets`, which COMPILES
// tests and runs none. It has been red ever since, then invisible again
// once the `JitMICSlot::update` arity change stopped the module compiling.
//
// Retired rather than re-pointed: unlike `StandardCharsets`, which grew a
// `<clinit>` that populates its real static slots, there is no replacement
// mechanism to assert against, and re-registering the pseudo-method to keep
// a test green would reverse a landed, documented decision from inside a
// test file.

#[test]
fn method_handle_proxies_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let mhp = "java/lang/invoke/MethodHandleProxies";

    let is_wrapper = call_native(
        &shared,
        &mut thread,
        mhp,
        "isWrapperInstance",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_wrapper, Value::Int(0));
}

#[test]
fn ssl_engine_p68() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Get SSLContext and create engine
    let ctx_obj = call_native(
        &shared,
        &mut thread,
        "javax/net/ssl/SSLContext",
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(ctx)) = ctx_obj {
        let engine = call_native(
            &shared,
            &mut thread,
            "javax/net/ssl/SSLContext",
            "createSSLEngine",
            "()Ljavax/net/ssl/SSLEngine;",
            &[Value::Object(Some(ctx))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(engine, Value::Object(Some(_))));

        // Verify the engine was created with correct fields
        if let Value::Object(Some(eng)) = engine {
            // Allocate a fresh engine directly to test field access.
            // The production layout (ssleng_alloc in phases_late.rs) is:
            //   0 = client_mode, 1 = need_client_auth,
            //   2 = want_client_auth, 3 = enabled_protocols,
            //   4 = enabled_cipher_suites, 5 = handshake_status,
            //   6 = session. We allocate 8 slots to stay forward
            //   compatible with future layout growth.
            let test_eng = shared.mem.heap.alloc_object(ClassId::new(0), 8);
            shared.mem.heap.set_field(test_eng, 0, Value::Int(1)); // client_mode
            shared.mem.heap.set_field(test_eng, 5, Value::Int(0)); // handshake_status = NOT_HANDSHAKING

            // setUseClientMode on the directly-allocated engine
            call_native(
                &shared,
                &mut thread,
                "javax/net/ssl/SSLEngine",
                "setUseClientMode",
                "(Z)V",
                &[Value::Object(Some(test_eng)), Value::Int(1)],
            )
            .unwrap();

            // beginHandshake on the directly-allocated engine
            call_native(
                &shared,
                &mut thread,
                "javax/net/ssl/SSLEngine",
                "beginHandshake",
                "()V",
                &[Value::Object(Some(test_eng))],
            )
            .unwrap();

            // Verify handshake status was set to NEED_WRAP (client mode)
            let hs = shared.mem.heap.get_field(test_eng, 5); // handshake_status
            assert_eq!(hs, Value::Int(1)); // HS_NEED_WRAP
            let _ = eng; // original engine from createSSLEngine also valid
        } else {
            // Unreachable: guarded by the `assert!(matches!(engine, ...))`
            // three lines up. Present so the shape cannot rot into a real
            // skip if that assertion is ever moved or relaxed.
            panic!("SSLContext.createSSLEngine must answer a reference, got {engine:?}");
        }
    } else {
        panic!(
            "SSLContext.getDefault()Ljavax/net/ssl/SSLContext; must answer a \
                 reference, got {ctx_obj:?}"
        );
    }
}

// -------------------------------------------------------
// Phase 69 tests
// -------------------------------------------------------

#[test]
fn cleaner_basics_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let c = "java/lang/ref/Cleaner";

    let cleaner = call_native(
        &shared,
        &mut thread,
        c,
        "create",
        "()Ljava/lang/ref/Cleaner;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(cleaner, Value::Object(Some(_))));

    if let Value::Object(Some(cl)) = cleaner {
        let cleanable = call_native(
            &shared,
            &mut thread,
            c,
            "register",
            "(Ljava/lang/Object;Ljava/lang/Runnable;)Ljava/lang/ref/Cleaner$Cleanable;",
            &[
                Value::Object(Some(cl)),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(cleanable, Value::Object(Some(_))));

        if let Value::Object(Some(ca)) = cleanable {
            call_native(
                &shared,
                &mut thread,
                "java/lang/ref/Cleaner$Cleanable",
                "clean",
                "()V",
                &[Value::Object(Some(ca))],
            )
            .unwrap();
        }
    }
}

#[test]
fn spliterator_basics_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sp = "java/util/Spliterator";

    // Create spliterator with 3 elements
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        3,
    );
    for i in 0..3 {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int((i + 1) as i32));
    }

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(obj, 0, Value::Object(Some(arr)));
    shared.mem.heap.set_field(obj, 1, Value::Int(0));
    shared.mem.heap.set_field(obj, 2, Value::Int(3));

    // estimateSize
    let sz = call_native(
        &shared,
        &mut thread,
        sp,
        "estimateSize",
        "()J",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Long(3));

    // characteristics
    let chars = call_native(
        &shared,
        &mut thread,
        sp,
        "characteristics",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(chars, Value::Int(_)));

    // trySplit MUST actually split. This asserted `null` while the native
    // was a hardcoded "cannot split" — the test pinned the stub rather
    // than the contract, and a permanently unsplittable Spliterator
    // silently disables all parallel-stream decomposition.
    //
    // The receiver is the JDK's ArraySpliterator shape (0 = array,
    // 1 = cursor, 2 = fence), so 3 elements split at
    // mid = (0 + 3) >>> 1 = 1: the returned prefix covers [0, 1) and the
    // receiver advances to [1, 3).
    let split = call_native(
        &shared,
        &mut thread,
        sp,
        "trySplit",
        "()Ljava/util/Spliterator;",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    let Value::Object(Some(prefix)) = split else {
        panic!("trySplit must return a prefix spliterator, got {split:?}");
    };
    // The two halves must partition the original range exactly — nothing
    // duplicated, nothing dropped.
    assert_eq!(shared.mem.heap.get_field(prefix, 1), Value::Int(0));
    assert_eq!(shared.mem.heap.get_field(prefix, 2), Value::Int(1));
    assert_eq!(shared.mem.heap.get_field(obj, 1), Value::Int(1));
    assert_eq!(shared.mem.heap.get_field(obj, 2), Value::Int(3));

    // A single-element range still cannot split: lo >= mid.
    let tiny = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(tiny, 0, Value::Object(Some(arr)));
    shared.mem.heap.set_field(tiny, 1, Value::Int(2));
    shared.mem.heap.set_field(tiny, 2, Value::Int(3));
    let none = call_native(
        &shared,
        &mut thread,
        sp,
        "trySplit",
        "()Ljava/util/Spliterator;",
        &[Value::Object(Some(tiny))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(none, Value::Object(None));
}

#[test]
fn spliterators_utility_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sps = "java/util/Spliterators";

    let empty = call_native(
        &shared,
        &mut thread,
        sps,
        "emptySpliterator",
        "()Ljava/util/Spliterator;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(empty, Value::Object(Some(_))));

    if let Value::Object(Some(sp)) = empty {
        let sz = call_native(
            &shared,
            &mut thread,
            "java/util/Spliterator",
            "estimateSize",
            "()J",
            &[Value::Object(Some(sp))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(sz, Value::Long(0));
    }
}

#[test]
fn stream_support_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a spliterator
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let sp = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(sp, 0, Value::Object(Some(arr)));
    shared.mem.heap.set_field(sp, 1, Value::Int(0));
    shared.mem.heap.set_field(sp, 2, Value::Int(2));

    // stream from spliterator
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        &[Value::Object(Some(sp)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(stream, Value::Object(Some(_))));
}

#[test]
fn websocket_builder_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let builder = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient",
        "newWebSocketBuilder",
        "()Ljava/net/http/WebSocket$Builder;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(builder, Value::Object(Some(_))));

    if let Value::Object(Some(b)) = builder {
        // header chaining
        let key = create_java_string(&shared, "Sec-WebSocket-Protocol");
        let val = create_java_string(&shared, "chat");
        let b2 = call_native(
            &shared,
            &mut thread,
            "java/net/http/WebSocket$Builder",
            "header",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/WebSocket$Builder;",
            &[
                Value::Object(Some(b)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(b2, Value::Object(Some(_))));
    }
}

#[test]
fn compact_number_format_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let cnf = "java/text/CompactNumberFormat";

    // getCompactNumberInstance
    let nf = call_native(
        &shared,
        &mut thread,
        "java/text/NumberFormat",
        "getCompactNumberInstance",
        "()Ljava/text/NumberFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(nf, Value::Object(Some(_))));

    if let Value::Object(Some(obj)) = nf {
        // format 1500 → "2K" (rounds)
        let result = call_native(
            &shared,
            &mut thread,
            cnf,
            "format",
            "(JLjava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
            &[
                Value::Object(Some(obj)),
                Value::Long(1500),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = result {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "1K");
        }

        // format 2_500_000 → "2M"
        let result2 = call_native(
            &shared,
            &mut thread,
            cnf,
            "format",
            "(JLjava/lang/StringBuffer;Ljava/text/FieldPosition;)Ljava/lang/StringBuffer;",
            &[
                Value::Object(Some(obj)),
                Value::Long(2_500_000),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = result2 {
            let text = read_java_string(&shared.mem.heap, s).unwrap();
            assert_eq!(text, "2M");
        }
    }
}

#[test]
fn submission_publisher_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sp = "java/util/concurrent/SubmissionPublisher";

    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        sp,
        "<init>",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();

    // isClosed
    let closed = call_native(
        &shared,
        &mut thread,
        sp,
        "isClosed",
        "()Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));

    // getMaxBufferCapacity
    let cap = call_native(
        &shared,
        &mut thread,
        sp,
        "getMaxBufferCapacity",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(256));

    // close
    call_native(
        &shared,
        &mut thread,
        sp,
        "close",
        "()V",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    let closed2 = call_native(
        &shared,
        &mut thread,
        sp,
        "isClosed",
        "()Z",
        &[Value::Object(Some(obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed2, Value::Int(1));
}

#[test]
fn objects_check_index_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let obj = "java/util/Objects";

    // Valid index
    let idx = call_native(
        &shared,
        &mut thread,
        obj,
        "checkIndex",
        "(II)I",
        &[Value::Int(3), Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(idx, Value::Int(3));

    // Invalid index
    let err = call_native(
        &shared,
        &mut thread,
        obj,
        "checkIndex",
        "(II)I",
        &[Value::Int(10), Value::Int(5)],
    );
    assert!(err.is_err());

    // checkFromToIndex valid
    let from = call_native(
        &shared,
        &mut thread,
        obj,
        "checkFromToIndex",
        "(III)I",
        &[Value::Int(2), Value::Int(5), Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(from, Value::Int(2));
}

#[test]
fn map_copy_of_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // `List.copyOf` over a REAL collection. This used to pass `null` and
    // assert it came straight back — "just returns input", which was the
    // old passthrough stub's behaviour. The JDK throws NullPointerException
    // for a null argument, so probing null asserted a contract neither the
    // JDK nor this VM has; copying an actual list is what the method is
    // for.
    //
    // Under `CRATONVM_COMPAT_JDK_IMMUTABLES` (interpreter round i1 wave 49, lane
    // L6) the row is not registered at all -- the JDK's `ImmutableCollections`
    // body answers, measured by `L6W49CopyOfIsTheJdks` -- so there is no
    // native to call here.
    if TEST_NATIVE_REGISTRY
        .find(
            "java/util/List",
            "copyOf",
            "(Ljava/util/Collection;)Ljava/util/List;",
        )
        .is_none()
    {
        return;
    }
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
    for v in [7, 8] {
        let boxed = alloc_receiver(&shared, &mut thread, "java/lang/Integer", 1);
        shared.mem.heap.set_field(boxed, 0, Value::Int(v));
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(src)), Value::Object(Some(boxed))],
        )
        .unwrap();
    }
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/List",
        "copyOf",
        "(Ljava/util/Collection;)Ljava/util/List;",
        &[Value::Object(Some(src))],
    )
    .unwrap()
    .unwrap();
    let copy = result.as_object().expect("copyOf must return a list");
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/List",
        "size",
        "()I",
        &[Value::Object(Some(copy))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2), "the copy must hold both elements");
}

#[test]
fn switch_bootstraps_stub_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let sb = "java/lang/runtime/SwitchBootstraps";

    let cs = call_native(&shared, &mut thread, sb, "typeSwitch",
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
            &[Value::Object(None), Value::Object(None), Value::Object(None), Value::Object(None)]).unwrap().unwrap();
    assert_eq!(cs, Value::Object(None));
}

#[test]
fn number_format_style_enum_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let style = "java/text/NumberFormat$Style";

    let short = call_native(
        &shared,
        &mut thread,
        style,
        "SHORT",
        "Ljava/text/NumberFormat$Style;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(short, Value::Object(Some(_))));

    let long = call_native(
        &shared,
        &mut thread,
        style,
        "LONG",
        "Ljava/text/NumberFormat$Style;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(long, Value::Object(Some(_))));
}

#[test]
fn strict_math_fma_p69() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/StrictMath",
        "fma",
        "(DDD)D",
        &[Value::Double(2.0), Value::Double(3.0), Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Double(7.0)); // 2*3 + 1 = 7
}

// ===== Phase 70 Tests =====

#[test]
fn file_time_from_millis_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ft = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        &[Value::Long(1234567890)],
    )
    .unwrap()
    .unwrap();
    let millis = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "toMillis",
        "()J",
        &[ft],
    )
    .unwrap()
    .unwrap();
    assert_eq!(millis, Value::Long(1234567890));
}

#[test]
fn file_time_compare_to_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ft1 = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        &[Value::Long(100)],
    )
    .unwrap()
    .unwrap();
    let ft2 = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        &[Value::Long(200)],
    )
    .unwrap()
    .unwrap();
    let cmp = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
        &[ft1, ft2],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cmp, Value::Int(-1));

    let cmp2 = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
        &[ft2, ft1],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cmp2, Value::Int(1));
}

#[test]
fn file_time_to_string_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ft = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        &[Value::Long(42)],
    )
    .unwrap()
    .unwrap();
    let s = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "toString",
        "()Ljava/lang/String;",
        &[ft],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(sref)) = s {
        let text = read_java_string(&shared.mem.heap, sref).unwrap();
        // `FileTime.toString()` delegates to `Instant.toString()`, which is
        // ISO-8601. The old "42ms" expectation predates that delegation.
        assert_eq!(text, "1970-01-01T00:00:00.042Z");
    } else {
        panic!("Expected string");
    }
}

// RETIRED 2026-08-11 — `posix_file_permission_enum_p70`.
//
// This test called `call_native(class, "OWNER_READ", "Ljava/nio/file/attribute/PosixFilePermission;")`: a
// FIELD name with a FIELD descriptor, asking the native registry for a
// pseudo-method that served a static constant. `dc55e8057` deleted that
// whole family as "dead everywhere" — correctly, since no JDK image has a
// *method* by that name and nothing in the sampled workloads called it —
// and the deletion is governed by `scripts/baselines/jdk-only-dead-everywhere.tsv`
// plus `registry_contracts.rs` for the registrations that must survive.
//
// The test was not updated with it, and could not report that: this module
// is `#[cfg(all(test, feature = "synthetic-jdk"))]`, and `dc55e8057` was
// verified with `cargo check --workspace --all-targets`, which COMPILES
// tests and runs none. It has been red ever since, then invisible again
// once the `JitMICSlot::update` arity change stopped the module compiling.
//
// Retired rather than re-pointed: unlike `StandardCharsets`, which grew a
// `<clinit>` that populates its real static slots, there is no replacement
// mechanism to assert against, and re-registering the pseudo-method to keep
// a test green would reverse a landed, documented decision from inside a
// test file.

#[test]
fn posix_file_permissions_to_string_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // `PosixFilePermissions.toString` renders the ACTUAL membership of the
    // set it is handed — it is not a fixed-string stub any more, and a
    // `null` set states no permissions at all. Feed it the real 0755 set
    // that `fromString` builds so this is a genuine round-trip, which is
    // also the only formulation that is meaningful on Windows (where the
    // host filesystem has no POSIX mode of its own to read).
    let perms = create_java_string(&shared, "rwxr-xr-x");
    let set = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/PosixFilePermissions",
        "fromString",
        "(Ljava/lang/String;)Ljava/util/Set;",
        &[Value::Object(Some(perms))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(set, Value::Object(Some(_))), "expected a Set");

    let s = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/PosixFilePermissions",
        "toString",
        "(Ljava/util/Set;)Ljava/lang/String;",
        &[set],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(sref)) = s {
        let text = read_java_string(&shared.mem.heap, sref).unwrap();
        assert_eq!(text, "rwxr-xr-x");
    } else {
        panic!("Expected string");
    }

    // A null set is "no permissions", not "all permissions".
    let empty = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/PosixFilePermissions",
        "toString",
        "(Ljava/util/Set;)Ljava/lang/String;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(sref)) = empty {
        assert_eq!(
            read_java_string(&shared.mem.heap, sref).unwrap(),
            "---------"
        );
    } else {
        panic!("Expected string");
    }
}

#[test]
fn gzip_input_stream_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let gis = alloc_receiver(&shared, &mut thread, "java/util/zip/GZIPInputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(gis)), Value::Object(None)],
    )
    .unwrap();

    // read returns -1 (empty stream)
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPInputStream",
        "read",
        "()I",
        &[Value::Object(Some(gis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Int(-1));

    // available returns 0
    let a = call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPInputStream",
        "available",
        "()I",
        &[Value::Object(Some(gis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(a, Value::Int(0));
}

#[test]
fn gzip_output_stream_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let gos = alloc_receiver(&shared, &mut thread, "java/util/zip/GZIPOutputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(gos)), Value::Object(None)],
    )
    .unwrap();

    // write, finish and close reach no sink here: the `<init>` above was
    // handed a NULL OutputStream, so `dos_underlying` answers `None` and
    // every delegation is skipped. That is the reason they are quiet — NOT
    // that the natives discard what the sink says. `close` propagates its
    // delegated `out.close()` since W7-57-close-flush-swallow-sweep.md, and
    // this harness has no sink to hear it from; the propagation is
    // asserted in probes/CloseFlushSwallowProbe.java instead. Pinned so a
    // later edit cannot hand it a real sink and silently change what this
    // test means.
    assert_eq!(shared.mem.heap.get_field(gos, 0), Value::Object(None));
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPOutputStream",
        "write",
        "(I)V",
        &[Value::Object(Some(gos)), Value::Int(42)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPOutputStream",
        "finish",
        "()V",
        &[Value::Object(Some(gos))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPOutputStream",
        "close",
        "()V",
        &[Value::Object(Some(gos))],
    )
    .unwrap();
}

#[test]
fn object_output_stream_p70() {
    // An `ObjectOutputStream` over a REAL stream, on the JDK-valid path.
    //
    // This test used to build the stream over `null`: `<init>(oos, null)`,
    // then write, then `close()`, relying on slot 0 staying null so `close()`
    // skipped its delegations. That premise is not Java. On JDK 25
    // `new ObjectOutputStream(null)` throws NullPointerException -- the
    // constructor writes the stream header to `this.out` -- and so does
    // `writeUTF(null)`. The VM's `<init>` is more lenient than the JDK and
    // accepted the null, but left slot 0 unwritten; on a fabricated class-0
    // object an unwritten slot reads `Int(0)`, so both the old assertion and
    // `close()`'s "is slot 0 `Object(None)`?" test failed. Making `close()`
    // accept `Int(0)` would have made a native MORE lenient to prop up a
    // premise the JDK rejects, so the test moved to the valid path instead.
    //
    // It checks something stronger than before: not "a slot is null", but that
    // the stream actually received the serialised bytes.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let baos = alloc_receiver(&shared, &mut thread, "java/io/ByteArrayOutputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "<init>",
        "()V",
        &[Value::Object(Some(baos))],
    )
    .unwrap();

    // A raw `alloc_object(ClassId::new(0), 6)` receiver has no class
    // identity, so `close()`'s internal `ctx.invoke_virtual(this, "flush",
    // ...)` cannot resolve a "flush" method on it and throws
    // `NoSuchMethodError`. `alloc_receiver` gives it the real
    // `java/io/ObjectOutputStream` identity so that dispatch resolves the
    // same way a real receiver's would.
    let oos = alloc_receiver(&shared, &mut thread, "java/io/ObjectOutputStream", 6);
    call_native(
        &shared,
        &mut thread,
        "java/io/ObjectOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(oos)), Value::Object(Some(baos))],
    )
    .unwrap();

    // `writeObject(null)` IS valid Java: it serialises a null reference.
    call_native(
        &shared,
        &mut thread,
        "java/io/ObjectOutputStream",
        "writeObject",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(oos)), Value::Object(None)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/io/ObjectOutputStream",
        "writeInt",
        "(I)V",
        &[Value::Object(Some(oos)), Value::Int(42)],
    )
    .unwrap();
    // A real string: `writeUTF(null)` throws on the JDK.
    let text = create_java_string(&shared, "cratonvm");
    call_native(
        &shared,
        &mut thread,
        "java/io/ObjectOutputStream",
        "writeUTF",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(oos)), Value::Object(Some(text))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/io/ObjectOutputStream",
        "close",
        "()V",
        &[Value::Object(Some(oos))],
    )
    .unwrap();

    // Closing flushes: the stream must now hold the serialised form -- at
    // least the stream header plus the int and the UTF payload.
    let size = call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayOutputStream",
        "size",
        "()I",
        &[Value::Object(Some(baos))],
    )
    .unwrap()
    .unwrap();
    match size {
        Value::Int(n) => assert!(
            n > 0,
            "the ObjectOutputStream wrote nothing to its stream (size {n})"
        ),
        other => panic!("ByteArrayOutputStream.size() returned {other:?}"),
    }
}

#[test]
fn object_output_stream_null_stream_throws_npe() {
    // `synthetic-bigdecimal-comparison-keeps-the-f64-body-20260917.md`'s
    // sibling record named this as the one residual left open: `new
    // ObjectOutputStream(null)` used to construct successfully here, where
    // JDK 25 throws `NullPointerException` from inside the constructor
    // itself (it writes the stream header to `out` before returning). See
    // the fixed `<init>` in `native-builtins/src/serialization.rs`.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let oos = alloc_receiver(&shared, &mut thread, "java/io/ObjectOutputStream", 6);
    let err = call_native(
        &shared,
        &mut thread,
        "java/io/ObjectOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(oos)), Value::Object(None)],
    )
    .expect_err("a null OutputStream must not construct");
    assert!(
        matches!(
            err,
            crate::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }
            ))
        ),
        "expected a message-less NullPointerException, matching the JDK's own \
         `new ObjectOutputStream(out)` javadoc (\"NullPointerException - if out \
         is null\") — got {err:?}"
    );
}

#[test]
fn object_input_stream_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ois = shared.mem.heap.alloc_object(ClassId::new(0), 6);
    call_native(
        &shared,
        &mut thread,
        "java/io/ObjectInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(ois)), Value::Object(None)],
    )
    .unwrap();

    // readObject on an empty stream returns null (TC_NULL fallback)
    // via the serialization.rs implementation.
    let obj = call_native(
        &shared,
        &mut thread,
        "java/io/ObjectInputStream",
        "readObject",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ois))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(obj, Value::Object(None));

    let i = call_native(
        &shared,
        &mut thread,
        "java/io/ObjectInputStream",
        "readInt",
        "()I",
        &[Value::Object(Some(ois))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(i, Value::Int(0));

    let s = call_native(
        &shared,
        &mut thread,
        "java/io/ObjectInputStream",
        "readUTF",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ois))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(s, Value::Object(Some(_))));
}

#[test]
fn constant_bootstraps_null_constant_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(&shared, &mut thread,
            "java/lang/invoke/ConstantBootstraps", "nullConstant",
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
            &[Value::Object(None), Value::Object(None), Value::Object(None)]).unwrap().unwrap();
    assert_eq!(result, Value::Object(None));
}

#[test]
fn long_accumulator_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let acc = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAccumulator",
        "<init>",
        "(Ljava/util/function/LongBinaryOperator;J)V",
        &[
            Value::Object(Some(acc)),
            Value::Object(None),
            Value::Long(0),
        ],
    )
    .unwrap();

    // accumulate 10 then 20
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAccumulator",
        "accumulate",
        "(J)V",
        &[Value::Object(Some(acc)), Value::Long(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAccumulator",
        "accumulate",
        "(J)V",
        &[Value::Object(Some(acc)), Value::Long(20)],
    )
    .unwrap();

    let val = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAccumulator",
        "get",
        "()J",
        &[Value::Object(Some(acc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Long(30));

    // getThenReset
    let val2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAccumulator",
        "getThenReset",
        "()J",
        &[Value::Object(Some(acc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val2, Value::Long(30));
    let val3 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAccumulator",
        "get",
        "()J",
        &[Value::Object(Some(acc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val3, Value::Long(0)); // reset to identity
}

#[test]
fn double_accumulator_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let acc = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAccumulator",
        "<init>",
        "(Ljava/util/function/DoubleBinaryOperator;D)V",
        &[
            Value::Object(Some(acc)),
            Value::Object(None),
            Value::Double(0.0),
        ],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAccumulator",
        "accumulate",
        "(D)V",
        &[Value::Object(Some(acc)), Value::Double(1.5)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAccumulator",
        "accumulate",
        "(D)V",
        &[Value::Object(Some(acc)), Value::Double(2.5)],
    )
    .unwrap();

    let val = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAccumulator",
        "get",
        "()D",
        &[Value::Object(Some(acc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Double(4.0));

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAccumulator",
        "reset",
        "()V",
        &[Value::Object(Some(acc))],
    )
    .unwrap();
    let val2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAccumulator",
        "get",
        "()D",
        &[Value::Object(Some(acc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val2, Value::Double(0.0));
}

#[test]
fn double_adder_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let adder = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "<init>",
        "()V",
        &[Value::Object(Some(adder))],
    )
    .unwrap();

    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "add",
        "(D)V",
        &[Value::Object(Some(adder)), Value::Double(3.5)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "add",
        "(D)V",
        &[Value::Object(Some(adder)), Value::Double(2.5)],
    )
    .unwrap();

    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "sum",
        "()D",
        &[Value::Object(Some(adder))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Double(6.0));

    let s2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "sumThenReset",
        "()D",
        &[Value::Object(Some(adder))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(s2, Value::Double(6.0));
    let s3 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "sum",
        "()D",
        &[Value::Object(Some(adder))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(s3, Value::Double(0.0));
}

#[test]
fn enum_set_complement_of_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let set = call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "complementOf",
        "(Ljava/util/EnumSet;)Ljava/util/EnumSet;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(set, Value::Object(Some(_))));
}

#[test]
fn enum_set_range_p70() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let set = call_native(
        &shared,
        &mut thread,
        "java/util/EnumSet",
        "range",
        "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
        &[Value::Object(None), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(set, Value::Object(Some(_))));
}

// `auto_closeable_close_p70` was REMOVED 2026-08-21 (WORKER 4), together
// with the registration it was asserting.
//
// It called `call_native(.., "java/lang/AutoCloseable", "close", "()V", ..)`
// with a NULL receiver and asserted only that the call did not error --
// i.e. it asserted that a row existed, not that anything worked. That is
// exactly what made it a blocker rather than a protection: `call_native`
// `panic!`s on an unregistered triple, so the test would have gone red for
// the deletion and for nothing else.
//
// What the deleted registration was for, and why it went, is written out
// at the foot of `native-io/src/lib.rs::register_scanner_natives`:
// dispatch keys on the RECEIVER's runtime class, the one fallback walk
// follows `superclass` links and never visits an interface, a second gate
// at step 6 of `execute_invoke_kind` drops interface instance-method
// natives outright, nothing in the tree mints a `java/lang/AutoCloseable`,
// and the row measured `invocations: 0` across 15 corpus vectors plus a
// purpose-built try-with-resources probe while the user's own `close()`
// ran byte-identically to HotSpot. `docs/known-issues/jdk-only/H11-3-*.md`
// N1 is the record; it wrote this deletion out verbatim and could not make
// it because that lane did not own this file.

// ===== Phase 71 Tests =====

#[test]
fn integer_to_binary_string_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "toBinaryString",
        "(I)Ljava/lang/String;",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = r {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "101010");
    } else {
        panic!("Expected string");
    }
}

#[test]
fn integer_to_octal_string_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "toOctalString",
        "(I)Ljava/lang/String;",
        &[Value::Int(255)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = r {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "377");
    } else {
        panic!("Expected string");
    }
}

#[test]
fn integer_unsigned_ops_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // signum
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "signum",
            "(I)I",
            &[Value::Int(-5)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(-1)
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "signum",
            "(I)I",
            &[Value::Int(0)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "signum",
            "(I)I",
            &[Value::Int(7)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(1)
    );

    // highestOneBit
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "highestOneBit",
            "(I)I",
            &[Value::Int(10)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(8)
    );

    // lowestOneBit
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "lowestOneBit",
            "(I)I",
            &[Value::Int(12)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(4)
    );

    // rotateLeft
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "rotateLeft",
            "(II)I",
            &[Value::Int(1), Value::Int(3)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(8)
    );

    // compareUnsigned: -1 as unsigned > any positive
    let cmp = call_native(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "compareUnsigned",
        "(II)I",
        &[Value::Int(-1), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cmp, Value::Int(1));
}

#[test]
fn long_to_binary_string_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Long",
        "toBinaryString",
        "(J)Ljava/lang/String;",
        &[Value::Long(255)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = r {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "11111111");
    } else {
        panic!("Expected string");
    }
}

#[test]
fn long_unsigned_ops_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Long",
            "signum",
            "(J)I",
            &[Value::Long(-100)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(-1)
    );

    // divideUnsigned
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Long",
            "divideUnsigned",
            "(JJ)J",
            &[Value::Long(100), Value::Long(3)]
        )
        .unwrap()
        .unwrap(),
        Value::Long(33)
    );
}

#[test]
fn double_is_finite_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Double",
            "isFinite",
            "(D)Z",
            &[Value::Double(1.5)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Double",
            "isFinite",
            "(D)Z",
            &[Value::Double(f64::INFINITY)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(0)
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Double",
            "isFinite",
            "(D)Z",
            &[Value::Double(f64::NAN)]
        )
        .unwrap()
        .unwrap(),
        Value::Int(0)
    );
}

#[test]
fn biginteger_gcd_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let a_s = create_java_string(&shared, "12");
    shared.mem.heap.set_field(a, 0, Value::Object(Some(a_s)));
    shared.mem.heap.set_field(a, 1, Value::Int(1));

    let b = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let b_s = create_java_string(&shared, "8");
    shared.mem.heap.set_field(b, 0, Value::Object(Some(b_s)));
    shared.mem.heap.set_field(b, 1, Value::Int(1));

    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "gcd",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(r)) = result {
        let val_str = shared.mem.heap.get_field(r, 0);
        if let Value::Object(Some(s)) = val_str {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "4");
        } else {
            panic!("Expected BigInteger value string");
        }
    } else {
        panic!("Expected BigInteger");
    }
}

#[test]
fn biginteger_shift_left_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let a_s = create_java_string(&shared, "1");
    shared.mem.heap.set_field(a, 0, Value::Object(Some(a_s)));
    shared.mem.heap.set_field(a, 1, Value::Int(1));

    let result = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "shiftLeft",
        "(I)Ljava/math/BigInteger;",
        &[Value::Object(Some(a)), Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(r)) = result {
        let val_str = shared.mem.heap.get_field(r, 0);
        if let Value::Object(Some(s)) = val_str {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "1024");
        } else {
            panic!("Expected BigInteger value string");
        }
    } else {
        panic!("Expected BigInteger");
    }
}

#[test]
fn biginteger_bit_ops_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let a_s = create_java_string(&shared, "12"); // 1100
    shared.mem.heap.set_field(a, 0, Value::Object(Some(a_s)));
    shared.mem.heap.set_field(a, 1, Value::Int(1));

    // testBit(2) should be true (bit 2 of 12 = 1100 is 1)
    let bit = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "testBit",
        "(I)Z",
        &[Value::Object(Some(a)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bit, Value::Int(1));

    // testBit(0) should be false (bit 0 of 12 is 0)
    let bit0 = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "testBit",
        "(I)Z",
        &[Value::Object(Some(a)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bit0, Value::Int(0));

    // bitLength of 12 = 4
    let bl = call_native(
        &shared,
        &mut thread,
        "java/math/BigInteger",
        "bitLength",
        "()I",
        &[Value::Object(Some(a))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bl, Value::Int(4));
}

#[test]
fn thread_state_value_of_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let name = create_java_string(&shared, "RUNNABLE");
    let state = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread$State",
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/Thread$State;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(state, Value::Object(Some(_))));
    if let Value::Object(Some(obj)) = state {
        // ordinal should be 1 (RUNNABLE)
        assert_eq!(shared.mem.heap.get_field(obj, 1), Value::Int(1));
    }
}

#[ignore = "STALE: ThreadGroup's 19 natives were retired 2026-08-23 (80d60e911) as a FIX, not a removal: activeCount() was a VM-WIDE count that ignored its receiver. 17 checks, identical to HotSpot after. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn thread_group_basics_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let tg = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let name = create_java_string(&shared, "test-group");
    call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadGroup",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(tg)), Value::Object(Some(name))],
    )
    .unwrap();

    let n = call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadGroup",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(tg))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = n {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "test-group");
    } else {
        panic!("Expected name string");
    }

    let ac = call_native(
        &shared,
        &mut thread,
        "java/lang/ThreadGroup",
        "activeCount",
        "()I",
        &[Value::Object(Some(tg))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ac, Value::Int(1));
}

#[test]
fn adler32_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ad = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Adler32",
        "<init>",
        "()V",
        &[Value::Object(Some(ad))],
    )
    .unwrap();

    // Initial value = 1
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Adler32",
        "getValue",
        "()J",
        &[Value::Object(Some(ad))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Long(1));

    // Update with a byte
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Adler32",
        "update",
        "(I)V",
        &[Value::Object(Some(ad)), Value::Int(65)],
    )
    .unwrap(); // 'A' = 65

    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Adler32",
        "getValue",
        "()J",
        &[Value::Object(Some(ad))],
    )
    .unwrap()
    .unwrap();
    // s1 = (1 + 65) % 65521 = 66, s2 = (0 + 66) % 65521 = 66
    // value = (66 << 16) | 66 = 4325442
    assert_eq!(v2, Value::Long(4_325_442));

    // Reset
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Adler32",
        "reset",
        "()V",
        &[Value::Object(Some(ad))],
    )
    .unwrap();
    let v3 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Adler32",
        "getValue",
        "()J",
        &[Value::Object(Some(ad))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v3, Value::Long(1));
}

#[test]
fn deflater_lifecycle_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let defl = alloc_receiver(&shared, &mut thread, "java/util/zip/Deflater", 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "<init>",
        "()V",
        &[Value::Object(Some(defl))],
    )
    .unwrap();

    // needsInput = true initially
    let ni = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "needsInput",
        "()Z",
        &[Value::Object(Some(defl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ni, Value::Int(1));

    // finished = false initially
    let fin = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "finished",
        "()Z",
        &[Value::Object(Some(defl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(fin, Value::Int(0));

    // Feed real input. `setInput` must make the deflater stop asking for
    // more, and `deflate` must produce a genuine DEFLATE stream — the
    // previous stubs answered a constant 0 here, so every caller silently
    // produced an empty archive.
    let payload: &[u8] = b"cratonvm deflate round-trip cratonvm deflate round-trip";
    let input = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, payload.len());
    for (i, &b) in payload.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(input, i, Value::Int(b as i32));
    }
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "setInput",
        "([B)V",
        &[Value::Object(Some(defl)), Value::Object(Some(input))],
    )
    .unwrap();
    let ni2 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "needsInput",
        "()Z",
        &[Value::Object(Some(defl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        ni2,
        Value::Int(0),
        "needsInput must be false while input is pending"
    );

    // finish()
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "finish",
        "()V",
        &[Value::Object(Some(defl))],
    )
    .unwrap();

    // HotSpot sets `Deflater.finished` INSIDE `deflate()` (when zlib reports
    // Z_STREAM_END), never in `finish()` itself — `finish()` only raises the
    // "flush the final block next time" request. So drain the compressor
    // before asserting it, exactly as the canonical
    // `while (!def.finished()) out.write(buf, 0, def.deflate(buf))` loop does.
    let out_buf = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 256);
    let mut compressed: Vec<u8> = Vec::new();
    for _ in 0..64 {
        let n = call_native(
            &shared,
            &mut thread,
            "java/util/zip/Deflater",
            "deflate",
            "([BII)I",
            &[
                Value::Object(Some(defl)),
                Value::Object(Some(out_buf)),
                Value::Int(0),
                Value::Int(256),
            ],
        )
        .unwrap()
        .unwrap();
        let n = match n {
            Value::Int(n) => n,
            other => panic!("deflate returned {other:?}"),
        };
        if n <= 0 {
            break;
        }
        for i in 0..n as usize {
            match shared.mem.heap.get_array_element(out_buf, i) {
                Ok(Value::Int(b)) => compressed.push(b as u8),
                other => panic!("deflate output slot {i}: {other:?}"),
            }
        }
    }
    let fin2 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Deflater",
        "finished",
        "()Z",
        &[Value::Object(Some(defl))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(fin2, Value::Int(1));
    assert!(
        !compressed.is_empty(),
        "deflate produced no output — the compressor is a stub"
    );
    // Default (nowrap=false) Deflater emits the ZLIB wrapper.
    assert_eq!(compressed[0] & 0x0f, 0x08, "expected a ZLIB CM=8 header");

    // Round-trip through Inflater: proves the bytes are real DEFLATE and not
    // a fabricated payload.
    let infl = alloc_receiver(&shared, &mut thread, "java/util/zip/Inflater", 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Inflater",
        "<init>",
        "()V",
        &[Value::Object(Some(infl))],
    )
    .unwrap();
    let cin =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Byte, compressed.len());
    for (i, &b) in compressed.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(cin, i, Value::Int(b as i32));
    }
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/Inflater",
        "setInput",
        "([B)V",
        &[Value::Object(Some(infl)), Value::Object(Some(cin))],
    )
    .unwrap();
    let dst =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Byte, payload.len() + 16);
    let produced = call_native(
        &shared,
        &mut thread,
        "java/util/zip/Inflater",
        "inflate",
        "([B)I",
        &[Value::Object(Some(infl)), Value::Object(Some(dst))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(produced, Value::Int(payload.len() as i32));
    let mut round_tripped: Vec<u8> = Vec::with_capacity(payload.len());
    for i in 0..payload.len() {
        match shared.mem.heap.get_array_element(dst, i) {
            Ok(Value::Int(b)) => round_tripped.push(b as u8),
            other => panic!("inflate output slot {i}: {other:?}"),
        }
    }
    assert_eq!(round_tripped.as_slice(), payload);
}

#[test]
fn zip_file_basics_p71() {
    // Build an empty ZIP in a tempdir: the real-mode ZipFile native
    // (native-io/src/zip_real_jar.rs) opens the archive during <init>,
    // so the file must exist on disk. Empty archive is required because
    // the test asserts size() == 0.
    let td = tempfile::tempdir().unwrap();
    let zip_path = td.path().join("test.zip");
    {
        let file = std::fs::File::create(&zip_path).unwrap();
        let zw = zip::ZipWriter::new(file);
        // No entries — just write out the End-Of-Central-Directory record.
        zw.finish().unwrap();
    }
    let zip_path_str = zip_path.to_string_lossy().into_owned();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let zf = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let name = create_java_string(&shared, &zip_path_str);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipFile",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(zf)), Value::Object(Some(name))],
    )
    .unwrap();

    let n = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipFile",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(zf))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = n {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), zip_path_str);
    } else {
        panic!("Expected name");
    }

    let sz = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipFile",
        "size",
        "()I",
        &[Value::Object(Some(zf))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(0));

    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipFile",
        "close",
        "()V",
        &[Value::Object(Some(zf))],
    )
    .unwrap();
}

#[test]
fn logging_extras_p71() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Logger.entering is a no-op
    call_native(
        &shared,
        &mut thread,
        "java/util/logging/Logger",
        "entering",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();

    // LogRecord extras
    let seq = call_native(
        &shared,
        &mut thread,
        "java/util/logging/LogRecord",
        "getSequenceNumber",
        "()J",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(seq, Value::Long(0));

    // LogManager.readConfiguration
    call_native(
        &shared,
        &mut thread,
        "java/util/logging/LogManager",
        "readConfiguration",
        "()V",
        &[Value::Object(None)],
    )
    .unwrap();
}

// =====================================================================
// Phase 72 tests: Preferences, Beans, JNDI, Datagram, HttpServer, ServerSocket
// =====================================================================

#[test]
fn preferences_put_get() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Manually create a Preferences with a HashMap backing store
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

    let prefs_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared
        .mem
        .heap
        .set_field(prefs_ref, 0, Value::Object(Some(map)));
    let name_s = create_java_string(&shared, "");
    shared
        .mem
        .heap
        .set_field(prefs_ref, 1, Value::Object(Some(name_s)));

    // put + get
    let key = create_java_string(&shared, "myKey");
    let val = create_java_string(&shared, "myValue");
    call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "put",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    let def = create_java_string(&shared, "default");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "get",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(key)),
            Value::Object(Some(def)),
        ],
    )
    .unwrap()
    .unwrap();
    let s = match result {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string: got {:?}", result),
    };
    assert_eq!(s, "myValue");

    // get missing key returns default
    let missing_key = create_java_string(&shared, "noSuchKey");
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "get",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(missing_key)),
            Value::Object(Some(def)),
        ],
    )
    .unwrap()
    .unwrap();
    let s2 = match result2 {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string: got {:?}", result2),
    };
    assert_eq!(s2, "default");
}

#[test]
fn preferences_int_boolean() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let prefs_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "<init>",
        "()V",
        &[Value::Object(Some(prefs_ref))],
    )
    .unwrap();

    // putInt + getInt
    let key = create_java_string(&shared, "count");
    call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "putInt",
        "(Ljava/lang/String;I)V",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(key)),
            Value::Int(42),
        ],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "getInt",
        "(Ljava/lang/String;I)I",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(key)),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(42));

    // putBoolean + getBoolean
    let bkey = create_java_string(&shared, "flag");
    call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "putBoolean",
        "(Ljava/lang/String;Z)V",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(bkey)),
            Value::Int(1),
        ],
    )
    .unwrap();
    let bresult = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "getBoolean",
        "(Ljava/lang/String;Z)Z",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(bkey)),
            Value::Int(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bresult, Value::Int(1));
}

#[test]
fn preferences_remove_clear() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Manually create Preferences with HashMap
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
    let prefs_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared
        .mem
        .heap
        .set_field(prefs_ref, 0, Value::Object(Some(map)));
    let name_s = create_java_string(&shared, "");
    shared
        .mem
        .heap
        .set_field(prefs_ref, 1, Value::Object(Some(name_s)));

    let key = create_java_string(&shared, "k1");
    let val = create_java_string(&shared, "v1");
    call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "put",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    // remove
    call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "remove",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(prefs_ref)), Value::Object(Some(key))],
    )
    .unwrap();

    let def = create_java_string(&shared, "gone");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "get",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(key)),
            Value::Object(Some(def)),
        ],
    )
    .unwrap()
    .unwrap();
    let s = match result {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string: got {:?}", result),
    };
    assert_eq!(s, "gone");
}

#[test]
fn property_change_event_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pce = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let source = create_java_string(&shared, "src");
    let name = create_java_string(&shared, "color");
    let old_val = create_java_string(&shared, "red");
    let new_val = create_java_string(&shared, "blue");

    call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeEvent",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        &[
            Value::Object(Some(pce)),
            Value::Object(Some(source)),
            Value::Object(Some(name)),
            Value::Object(Some(old_val)),
            Value::Object(Some(new_val)),
        ],
    )
    .unwrap();

    let gn = call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeEvent",
        "getPropertyName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(pce))],
    )
    .unwrap()
    .unwrap();
    let pname = match gn {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(pname, "color");

    let go = call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeEvent",
        "getOldValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pce))],
    )
    .unwrap()
    .unwrap();
    let oval = match go {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(oval, "red");

    let gnv = call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeEvent",
        "getNewValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(pce))],
    )
    .unwrap()
    .unwrap();
    let nval = match gnv {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(nval, "blue");
}

#[test]
fn property_change_support_listeners() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let pcs = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let source = create_java_string(&shared, "bean");
    call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeSupport",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(pcs)), Value::Object(Some(source))],
    )
    .unwrap();

    // hasListeners before adding
    let hl = call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeSupport",
        "hasListeners",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(pcs)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hl, Value::Int(0));

    // add a listener (just a dummy object)
    let listener = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeSupport",
        "addPropertyChangeListener",
        "(Ljava/beans/PropertyChangeListener;)V",
        &[Value::Object(Some(pcs)), Value::Object(Some(listener))],
    )
    .unwrap();

    let hl2 = call_native(
        &shared,
        &mut thread,
        "java/beans/PropertyChangeSupport",
        "hasListeners",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(pcs)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(hl2, Value::Int(1));
}

#[test]
fn introspector_decapitalize() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let s1 = create_java_string(&shared, "FooBar");
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/beans/Introspector",
        "decapitalize",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(s1))],
    )
    .unwrap()
    .unwrap();
    let r1s = match r1 {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(r1s, "fooBar");

    // Two uppercase letters at start → unchanged
    let s2 = create_java_string(&shared, "URL");
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/beans/Introspector",
        "decapitalize",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(s2))],
    )
    .unwrap()
    .unwrap();
    let r2s = match r2 {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(r2s, "URL");
}

#[test]
fn initial_context_bind_lookup() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ic = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "javax/naming/InitialContext",
        "<init>",
        "()V",
        &[Value::Object(Some(ic))],
    )
    .unwrap();

    let key = create_java_string(&shared, "java:comp/env/myDS");
    let val = create_java_string(&shared, "datasource");
    call_native(
        &shared,
        &mut thread,
        "javax/naming/InitialContext",
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        &[
            Value::Object(Some(ic)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    let result = call_native(
        &shared,
        &mut thread,
        "javax/naming/InitialContext",
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[Value::Object(Some(ic)), Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    let rs = match result {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(rs, "datasource");

    // unbind
    call_native(
        &shared,
        &mut thread,
        "javax/naming/InitialContext",
        "unbind",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(ic)), Value::Object(Some(key))],
    )
    .unwrap();

    // After unbind, lookup must throw NameNotFoundException per JDK spec.
    // The t3_impl JNDI implementation raises an IllegalStateException whose
    // message contains the NameNotFoundException class name.
    let r2 = call_native(
        &shared,
        &mut thread,
        "javax/naming/InitialContext",
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[Value::Object(Some(ic)), Value::Object(Some(key))],
    );
    assert!(r2.is_err(), "lookup after unbind must throw, got {:?}", r2);
}

#[test]
fn datagram_packet_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let dp = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let data = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        10,
    );
    call_native(
        &shared,
        &mut thread,
        "java/net/DatagramPacket",
        "<init>",
        "([BI)V",
        &[
            Value::Object(Some(dp)),
            Value::Object(Some(data)),
            Value::Int(10),
        ],
    )
    .unwrap();

    let len = call_native(
        &shared,
        &mut thread,
        "java/net/DatagramPacket",
        "getLength",
        "()I",
        &[Value::Object(Some(dp))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(len, Value::Int(10));

    call_native(
        &shared,
        &mut thread,
        "java/net/DatagramPacket",
        "setPort",
        "(I)V",
        &[Value::Object(Some(dp)), Value::Int(8080)],
    )
    .unwrap();
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/DatagramPacket",
        "getPort",
        "()I",
        &[Value::Object(Some(dp))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(8080));
}

#[test]
fn datagram_socket_lifecycle() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ds = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/net/DatagramSocket",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ds)), Value::Int(9090)],
    )
    .unwrap();

    let port = call_native(
        &shared,
        &mut thread,
        "java/net/DatagramSocket",
        "getLocalPort",
        "()I",
        &[Value::Object(Some(ds))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(9090));

    let closed = call_native(
        &shared,
        &mut thread,
        "java/net/DatagramSocket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(ds))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));

    call_native(
        &shared,
        &mut thread,
        "java/net/DatagramSocket",
        "close",
        "()V",
        &[Value::Object(Some(ds))],
    )
    .unwrap();
    let closed2 = call_native(
        &shared,
        &mut thread,
        "java/net/DatagramSocket",
        "isClosed",
        "()Z",
        &[Value::Object(Some(ds))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed2, Value::Int(1));
}

#[test]
fn http_server_create_context() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let srv = call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/HttpServer",
        "create",
        "()Lcom/sun/net/httpserver/HttpServer;",
        &[],
    )
    .unwrap()
    .unwrap();
    let srv_ref = match srv {
        Value::Object(Some(o)) => o,
        _ => panic!("expected ref"),
    };

    let path = create_java_string(&shared, "/api/test");
    let ctx = call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/HttpServer",
        "createContext",
        "(Ljava/lang/String;)Lcom/sun/net/httpserver/HttpContext;",
        &[Value::Object(Some(srv_ref)), Value::Object(Some(path))],
    )
    .unwrap()
    .unwrap();
    let ctx_ref = match ctx {
        Value::Object(Some(o)) => o,
        _ => panic!("expected ref"),
    };

    let gp = call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/HttpContext",
        "getPath",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ctx_ref))],
    )
    .unwrap()
    .unwrap();
    let ps = match gp {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(ps, "/api/test");

    // `HttpServer.create()` returns a server that is deliberately NOT bound
    // ("Creates a HttpServer instance which is initially not bound to any
    // local address/port … The server must be bound using bind() before it
    // can be used"), and `ServerImpl.start()` rejects an unbound server with
    // IllegalStateException("server in wrong state"). `start()` declares no
    // checked exception, so this must NOT surface as an IOException.
    //
    // This used to `.unwrap()` a success. It could never have been one: the
    // no-arg factory minted a carrier with no `server_registry` entry at
    // all, so `start()` failed with `IOException("server not registered")` —
    // an internal-plumbing error, not the JDK's contract. The factory now
    // shares Phase E's carrier and registry entry, so the failure that
    // remains is the real, documented one.
    let started = call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/HttpServer",
        "start",
        "()V",
        &[Value::Object(Some(srv_ref))],
    );
    let started_err = match started {
        Err(e) => e.to_string(),
        Ok(v) => panic!("start() on an unbound server must throw, got Ok({v:?})"),
    };
    assert!(
        started_err.contains("IllegalStateException"),
        "start() on an unbound server must raise IllegalStateException, got {started_err}"
    );

    // stop() on a never-started server is a no-op in the JDK, and it also
    // drops this server's registry entry.
    call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/HttpServer",
        "stop",
        "(I)V",
        &[Value::Object(Some(srv_ref)), Value::Int(0)],
    )
    .unwrap();
}

#[test]
fn server_socket_lifecycle() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ss = alloc_receiver(&shared, &mut thread, "java/net/ServerSocket", 4);
    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ss)), Value::Int(8080)],
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
    assert_eq!(port, Value::Int(8080));

    let bound = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "isBound",
        "()Z",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bound, Value::Int(1));

    call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "close",
        "()V",
        &[Value::Object(Some(ss))],
    )
    .unwrap();
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
    assert_eq!(closed, Value::Int(1));

    let ts = call_native(
        &shared,
        &mut thread,
        "java/net/ServerSocket",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ss))],
    )
    .unwrap()
    .unwrap();
    let tss = match ts {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert!(tss.contains("8080"));
}

#[test]
fn multicast_socket_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ms = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/net/MulticastSocket",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ms)), Value::Int(5000)],
    )
    .unwrap();

    let ttl = call_native(
        &shared,
        &mut thread,
        "java/net/MulticastSocket",
        "getTimeToLive",
        "()I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ttl, Value::Int(1));

    call_native(
        &shared,
        &mut thread,
        "java/net/MulticastSocket",
        "close",
        "()V",
        &[Value::Object(Some(ms))],
    )
    .unwrap();
}

#[test]
fn http_headers_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let hd = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/Headers",
        "<init>",
        "()V",
        &[Value::Object(Some(hd))],
    )
    .unwrap();

    let sz = call_native(
        &shared,
        &mut thread,
        "com/sun/net/httpserver/Headers",
        "size",
        "()I",
        &[Value::Object(Some(hd))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(0));
}

#[test]
fn naming_exception_init() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let exc = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let msg_str = create_java_string(&shared, "name not found");
    call_native(
        &shared,
        &mut thread,
        "javax/naming/NamingException",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(exc)), Value::Object(Some(msg_str))],
    )
    .unwrap();

    let msg = call_native(
        &shared,
        &mut thread,
        "javax/naming/NamingException",
        "getMessage",
        "()Ljava/lang/String;",
        &[Value::Object(Some(exc))],
    )
    .unwrap()
    .unwrap();
    let ms = match msg {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(ms, "name not found");
}

/// A child node knows its own name and renders its absolute path.
///
/// The parent comes from `Preferences.userRoot()`, the production entry
/// point, and not from a hand-rolled `alloc_object(.., 2)`. That fixture is
/// what made this test fail: `Preferences` grew from 2 slots to 6 (parent
/// link, child registry, removed flag, user/system tree), every native that
/// writes past slot 1 is guarded on `object_num_fields`, and the guards did
/// their job -- so the 2-field parent silently could not hold the back-link
/// that `absolutePath()` walks, and `toString()` rendered a path with no
/// node in it. Building the receiver the way the VM does keeps the fixture
/// from drifting behind the layout again.
#[test]
fn preferences_name_and_path() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let prefs_ref = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "userRoot",
        "()Ljava/util/prefs/Preferences;",
        &[],
    )
    .unwrap()
    .unwrap()
    .as_object()
    .expect("userRoot() must return a node");

    let child_name = create_java_string(&shared, "myNode");
    let child = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "node",
        "(Ljava/lang/String;)Ljava/util/prefs/Preferences;",
        &[
            Value::Object(Some(prefs_ref)),
            Value::Object(Some(child_name)),
        ],
    )
    .unwrap()
    .unwrap();
    let child_ref = match child {
        Value::Object(Some(o)) => o,
        _ => panic!("expected ref"),
    };

    let n = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(child_ref))],
    )
    .unwrap()
    .unwrap();
    let ns = match n {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(ns, "myNode");

    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/prefs/Preferences",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(child_ref))],
    )
    .unwrap()
    .unwrap();
    let tss = match ts {
        Value::Object(Some(o)) => read_java_string(&shared.mem.heap, o).unwrap(),
        _ => panic!("expected string"),
    };
    // `AbstractPreferences.toString()` is defined as
    // `(isUserNode() ? "User" : "System") + " Preference Node: " +
    // absolutePath()`, so this pins the name, the tree and the path
    // together -- `contains("myNode")` alone passed while the path was
    // wrong.
    assert_eq!(tss, "User Preference Node: /myNode");
}

// ---- Compact reference array tests (Phase 5 Round 20) --------------------

#[test]
fn compact_ref_array_alloc_and_read() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Allocate Object[3] and store object references
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let obj0 = shared.mem.heap.alloc_object(ClassId::new(1), 1);
    shared.mem.heap.set_field(obj0, 0, Value::Int(42));
    let obj1 = shared.mem.heap.alloc_object(ClassId::new(2), 1);
    shared.mem.heap.set_field(obj1, 0, Value::Int(99));

    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(obj0)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(None));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Object(Some(obj1)));

    // Read back
    let v0 = shared.mem.heap.get_array_element(arr, 0).unwrap();
    let v1 = shared.mem.heap.get_array_element(arr, 1).unwrap();
    let v2 = shared.mem.heap.get_array_element(arr, 2).unwrap();

    match v0 {
        Value::Object(Some(r)) => assert_eq!(shared.mem.heap.get_field(r, 0), Value::Int(42)),
        _ => panic!("expected Object(Some) for arr[0]"),
    }
    assert_eq!(v1, Value::Object(None));
    match v2 {
        Value::Object(Some(r)) => assert_eq!(shared.mem.heap.get_field(r, 0), Value::Int(99)),
        _ => panic!("expected Object(Some) for arr[2]"),
    }
}

#[test]
fn compact_ref_array_autobox_primitives() {
    // Storing non-Object values in Reference arrays auto-boxes them
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 5);

    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(10));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Long(9999999999i64));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Float(3.5));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 3, Value::Double(1.23456789012345));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 4, Value::Object(None));

    // Read back via unboxing path — auto-unboxing should return original values
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 0).unwrap(),
        Value::Int(10)
    );
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 1).unwrap(),
        Value::Long(9999999999i64)
    );
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 2).unwrap(),
        Value::Float(3.5)
    );
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 3).unwrap(),
        Value::Double(1.23456789012345)
    );
    assert_eq!(
        shared.mem.heap.get_array_element_unboxing(arr, 4).unwrap(),
        Value::Object(None)
    );
}

#[test]
fn compact_ref_array_element_size() {
    // Verify Reference arrays use 8-byte elements
    use crate::memory::heap::{element_byte_size, REF_ELEMENT_SIZE};
    assert_eq!(
        element_byte_size(ArrayElementType::Reference),
        REF_ELEMENT_SIZE
    );
    assert_eq!(REF_ELEMENT_SIZE, 8);
}

#[test]
fn compact_ref_array_2d_matrix() {
    // Allocate int[3][3] — outer Object[] with inner int[] arrays
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let outer = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);

    for row in 0..3 {
        let inner = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        for col in 0..3 {
            let _ =
                shared
                    .mem
                    .heap
                    .set_array_element(inner, col, Value::Int((row * 3 + col) as i32));
        }
        let _ = shared
            .mem
            .heap
            .set_array_element(outer, row, Value::Object(Some(inner)));
    }

    // Read back the matrix
    for row in 0..3 {
        let inner = match shared.mem.heap.get_array_element(outer, row).unwrap() {
            Value::Object(Some(r)) => r,
            _ => panic!("expected inner array at row {row}"),
        };
        for col in 0..3 {
            let v = shared.mem.heap.get_array_element(inner, col).unwrap();
            assert_eq!(v, Value::Int((row * 3 + col) as i32));
        }
    }
}

#[test]
fn compact_ref_array_large() {
    // Stress test: Object[1000] filled with references
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1000);

    let mut objs = Vec::new();
    for i in 0..1000 {
        let obj = shared.mem.heap.alloc_object(ClassId::new(1), 1);
        shared.mem.heap.set_field(obj, 0, Value::Int(i as i32));
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(obj)));
        objs.push(obj);
    }

    // Verify all 1000 elements
    for i in 0..1000 {
        let v = shared.mem.heap.get_array_element(arr, i).unwrap();
        match v {
            Value::Object(Some(r)) => {
                assert_eq!(shared.mem.heap.get_field(r, 0), Value::Int(i as i32));
            }
            _ => panic!("expected Object at index {i}"),
        }
    }
}

#[test]
fn compact_ref_array_mixed_null_and_values() {
    // Alternate null and non-null in a ref array
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 6);

    for i in 0..6 {
        if i % 2 == 0 {
            let obj = shared.mem.heap.alloc_object(ClassId::new(1), 0);
            let _ = shared
                .mem
                .heap
                .set_array_element(arr, i, Value::Object(Some(obj)));
        } else {
            let _ = shared
                .mem
                .heap
                .set_array_element(arr, i, Value::Object(None));
        }
    }

    for i in 0..6 {
        let v = shared.mem.heap.get_array_element(arr, i).unwrap();
        if i % 2 == 0 {
            assert!(
                matches!(v, Value::Object(Some(_))),
                "expected non-null at {i}"
            );
        } else {
            assert_eq!(v, Value::Object(None), "expected null at {i}");
        }
    }
}

#[test]
fn compact_ref_array_overwrite() {
    // Overwrite elements in a ref array
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);

    let obj_a = shared.mem.heap.alloc_object(ClassId::new(1), 0);
    let obj_b = shared.mem.heap.alloc_object(ClassId::new(2), 0);
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(obj_a)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(obj_b)));

    // Overwrite arr[0] with obj_b
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(obj_b)));
    // Overwrite arr[1] with null
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(None));

    match shared.mem.heap.get_array_element(arr, 0).unwrap() {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), obj_b.as_ptr()),
        _ => panic!("expected obj_b"),
    }
    assert_eq!(
        shared.mem.heap.get_array_element(arr, 1).unwrap(),
        Value::Object(None)
    );
}

#[test]
fn compact_ref_array_arraylist_roundtrip() {
    // Test ArrayList (which uses ref array backing) stores and retrieves mixed values
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

    // Add a string and an int (auto-boxed)
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
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Int(42)],
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
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(2));

    let v0 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    match v0 {
        Value::Object(Some(r)) => {
            assert_eq!(read_java_string(&shared.mem.heap, r).unwrap(), "hello")
        }
        _ => panic!("expected string"),
    }

    let v1 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v1, Value::Int(42));
}

// =========================================================================
// Java 11: Optional.ifPresentOrElse, or, stream, orElseThrow(Supplier)
// =========================================================================

#[test]
fn optional_stream_present() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let val_str = create_java_string(&shared, "hello");
    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[Value::Object(Some(val_str))],
    )
    .unwrap();
    let opt_ref = match opt {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Optional"),
    };

    // stream() on present → Stream with 1 element
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(opt_ref))],
    )
    .unwrap();
    let stream_ref = match stream {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Stream"),
    };
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[Value::Object(Some(stream_ref))],
    )
    .unwrap();
    assert_eq!(count, Some(Value::Long(1)));
}

#[test]
fn optional_stream_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "empty",
        "()Ljava/util/Optional;",
        &[],
    )
    .unwrap();
    let opt_ref = match opt {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Optional"),
    };

    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "stream",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(opt_ref))],
    )
    .unwrap();
    let stream_ref = match stream {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Stream"),
    };
    let count = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "count",
        "()J",
        &[Value::Object(Some(stream_ref))],
    )
    .unwrap();
    assert_eq!(count, Some(Value::Long(0)));
}

#[test]
fn optional_or_present_returns_self() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let val_str = create_java_string(&shared, "value");
    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[Value::Object(Some(val_str))],
    )
    .unwrap();
    let opt_ref = match opt {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Optional"),
    };

    // or() on present -> returns self (the same Optional).
    //
    // The supplier must be NON-NULL. This test used to pass null "since it
    // should not be called" -- but the JDK's `Optional.or` does
    // `Objects.requireNonNull(supplier)` BEFORE it checks presence, so real
    // Java throws NPE for `Optional.of(v).or(null)` (verified on JDK 25). The
    // VM now runs that real bytecode and threw exactly that NPE; the test was
    // encoding a lenient stub. Any non-null object will do as the supplier: on
    // a present Optional it is null-checked and never invoked.
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "or",
        "(Ljava/util/function/Supplier;)Ljava/util/Optional;",
        &[Value::Object(Some(opt_ref)), Value::Object(Some(opt_ref))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Object(Some(opt_ref))));
}

#[test]
fn optional_or_else_throw_supplier_present() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let val_str = create_java_string(&shared, "found");
    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[Value::Object(Some(val_str))],
    )
    .unwrap();
    let opt_ref = match opt {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Optional"),
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        &[Value::Object(Some(opt_ref)), Value::Object(None)],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Object(Some(val_str))));
}

#[test]
fn optional_or_else_throw_supplier_empty() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "empty",
        "()Ljava/util/Optional;",
        &[],
    )
    .unwrap();
    let opt_ref = match opt {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected Optional"),
    };

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        &[Value::Object(Some(opt_ref)), Value::Object(None)],
    );
    assert!(result.is_err(), "empty Optional.orElseThrow should throw");
}

// =========================================================================
// Java 11: Collection.toArray(IntFunction)
// =========================================================================

#[test]
fn collection_to_array_generator() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create ArrayList with 3 string elements
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
    let s1 = create_java_string(&shared, "a");
    let s2 = create_java_string(&shared, "b");
    let s3 = create_java_string(&shared, "c");
    for s in [s1, s2, s3] {
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

    // toArray(IntFunction) → should return same elements as toArray()
    let arr = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        &[Value::Object(Some(list)), Value::Object(None)],
    )
    .unwrap();
    let arr_ref = match arr {
        Some(Value::Object(Some(r))) => r,
        _ => panic!("expected array"),
    };
    assert_eq!(shared.mem.heap.array_length(arr_ref), 3);
    assert_eq!(
        shared.mem.heap.get_array_element(arr_ref, 0).unwrap(),
        Value::Object(Some(s1))
    );
    assert_eq!(
        shared.mem.heap.get_array_element(arr_ref, 1).unwrap(),
        Value::Object(Some(s2))
    );
    assert_eq!(
        shared.mem.heap.get_array_element(arr_ref, 2).unwrap(),
        Value::Object(Some(s3))
    );
}

#[test]
fn record_class_with_components_tostring() {
    use crate::classloading::{Class, ClassLoaderId, ClassState, RecordComponentInfo};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");

    // Register a class with record components in the class manager
    let class_id = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("com/example/Point"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 2,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: vec![
                RecordComponentInfo {
                    name: "x".to_string(),
                    descriptor: "I".to_string(),
                    annotations: None,
                    type_annotations: None,
                    signature: None,
                },
                RecordComponentInfo {
                    name: "y".to_string(),
                    descriptor: "I".to_string(),
                    annotations: None,
                    type_annotations: None,
                    signature: None,
                },
            ],
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
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
        cm.register_class_name(ClassLoaderId::Application, "com/example/Point", id);
        id
    };

    // Allocate a record object with this class id
    let rec = shared.mem.heap.alloc_object(class_id, 2);
    shared.mem.heap.set_field(rec, 0, Value::Int(10));
    shared.mem.heap.set_field(rec, 1, Value::Int(20));

    // Test toString produces ClassName[x=10, y=20]
    let ts = call_native(
        &shared,
        &mut thread,
        "java/lang/Record",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(rec))],
    )
    .unwrap()
    .unwrap();
    let ts_ref = ts.as_object().unwrap();
    let ts_str = read_java_string(&shared.mem.heap, ts_ref).unwrap();
    assert_eq!(ts_str, "Point[x=10, y=20]");
}

#[test]
fn record_equals_different_classes() {
    use crate::classloading::{Class, ClassLoaderId, ClassState, RecordComponentInfo};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");

    // Register two different record classes with same field count
    let (cid1, cid2) = {
        let mut cm = shared.classes.class_manager_write();
        let id1 = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: id1,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("com/example/Point2D"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 2,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: vec![
                RecordComponentInfo {
                    name: "x".to_string(),
                    descriptor: "I".to_string(),
                    annotations: None,
                    type_annotations: None,
                    signature: None,
                },
                RecordComponentInfo {
                    name: "y".to_string(),
                    descriptor: "I".to_string(),
                    annotations: None,
                    type_annotations: None,
                    signature: None,
                },
            ],
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
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
        let id2 = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: id2,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("com/example/Size"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 2,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: vec![
                RecordComponentInfo {
                    name: "w".to_string(),
                    descriptor: "I".to_string(),
                    annotations: None,
                    type_annotations: None,
                    signature: None,
                },
                RecordComponentInfo {
                    name: "h".to_string(),
                    descriptor: "I".to_string(),
                    annotations: None,
                    type_annotations: None,
                    signature: None,
                },
            ],
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
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
        (id1, id2)
    };

    // Two records, different classes, same field values
    let rec1 = shared.mem.heap.alloc_object(cid1, 2);
    shared.mem.heap.set_field(rec1, 0, Value::Int(42));
    shared.mem.heap.set_field(rec1, 1, Value::Int(99));
    let rec2 = shared.mem.heap.alloc_object(cid2, 2);
    shared.mem.heap.set_field(rec2, 0, Value::Int(42));
    shared.mem.heap.set_field(rec2, 1, Value::Int(99));

    // equals should return false because classes differ
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
    assert_eq!(
        eq,
        Value::Int(0),
        "different record classes should not be equal"
    );

    // Same class, same values → equal
    let rec3 = shared.mem.heap.alloc_object(cid1, 2);
    shared.mem.heap.set_field(rec3, 0, Value::Int(42));
    shared.mem.heap.set_field(rec3, 1, Value::Int(99));
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
    assert_eq!(
        eq2,
        Value::Int(1),
        "same record class + same values should be equal"
    );
}

#[test]
fn native_context_record_sealed_methods() {
    use crate::classloading::{Class, ClassLoaderId, ClassState, RecordComponentInfo};
    use crate::native::registry::NativeContext;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");

    let (rec_id, sealed_id, plain_id) = {
        let mut cm = shared.classes.class_manager_write();
        let rid = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: rid,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("test/Record1"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 1,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: vec![RecordComponentInfo {
                name: "val".to_string(),
                descriptor: "J".to_string(),
                annotations: None,
                type_annotations: None,
                signature: None,
            }],
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
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

        let sid = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: sid,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("test/Sealed1"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: vec!["test/Sub1".to_string(), "test/Sub2".to_string()],
            inner_classes: Vec::new(),
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

        let pid = cm.class_store.next_id();
        cm.class_store.add(Class {
            id: pid,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("test/Plain1"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
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

        (rid, sid, pid)
    };

    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    // Record
    assert!(ctx.is_record_class(rec_id));
    assert!(!ctx.is_sealed_class(rec_id));
    let comps = ctx.record_components(rec_id);
    assert_eq!(comps.len(), 1);
    assert_eq!(comps[0], ("val".to_string(), "J".to_string()));

    // Sealed
    assert!(ctx.is_sealed_class(sealed_id));
    assert!(!ctx.is_record_class(sealed_id));
    let subs = ctx.permitted_subclasses(sealed_id);
    assert_eq!(subs, vec!["test/Sub1".to_string(), "test/Sub2".to_string()]);

    // Plain
    assert!(!ctx.is_record_class(plain_id));
    assert!(!ctx.is_sealed_class(plain_id));
    assert!(ctx.record_components(plain_id).is_empty());
    assert!(ctx.permitted_subclasses(plain_id).is_empty());
}

// =========================================================================
// Phase C: Java 21 — SequencedCollection, Virtual Threads, Emoji, etc.
// =========================================================================

#[test]
fn arraylist_reversed_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create ArrayList with [10, 20, 30]
    let al = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    for v in [10, 20, 30] {
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), Value::Int(v)],
        )
        .unwrap();
    }

    // Call reversed()
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "reversed",
        "()Ljava/util/List;",
        &[Value::Object(Some(al))],
    )
    .unwrap();
    let rev = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected object"),
    };

    // Check reversed list has [30, 20, 10]
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(rev))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(3)));

    let first = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(rev)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(first, Some(Value::Int(30)));

    let last = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(rev)), Value::Int(2)],
    )
    .unwrap();
    assert_eq!(last, Some(Value::Int(10)));
}

#[test]
fn linkedlist_reversed_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let ll = alloc_receiver(&shared, &mut thread, "java/util/LinkedList", 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "<init>",
        "()V",
        &[Value::Object(Some(ll))],
    )
    .unwrap();
    for v in [100, 200, 300] {
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(ll)), Value::Int(v)],
        )
        .unwrap();
    }

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedList",
        "reversed",
        "()Ljava/util/List;",
        &[Value::Object(Some(ll))],
    )
    .unwrap();
    let rev = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected object"),
    };

    // Reversed result is an ArrayList
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(rev))],
    )
    .unwrap();
    assert_eq!(size, Some(Value::Int(3)));

    let first = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(rev)), Value::Int(0)],
    )
    .unwrap();
    assert_eq!(first, Some(Value::Int(300)));
}

#[test]
fn linkedhashmap_first_last_entry_p64() {
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

    // Insert "a"->1, "b"->2, "c"->3
    for (k, v) in [("alpha", 1), ("beta", 2), ("gamma", 3)] {
        let key = create_java_string(&shared, k);
        let boxed_v = box_int(&shared, &mut thread, v);
        call_native(
            &shared,
            &mut thread,
            "java/util/LinkedHashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(lhm)), Value::Object(Some(key)), boxed_v],
        )
        .unwrap();
    }

    // firstEntry
    let fe = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();
    let entry = match fe {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected entry"),
    };
    let val = call_native(
        &shared,
        &mut thread,
        "java/util/Map$Entry",
        "getValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap();
    assert_eq!(unbox_int(&shared, &mut thread, val), Some(Value::Int(1)));

    // lastEntry
    let le = call_native(
        &shared,
        &mut thread,
        "java/util/LinkedHashMap",
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        &[Value::Object(Some(lhm))],
    )
    .unwrap();
    let entry2 = match le {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected entry"),
    };
    let val2 = call_native(
        &shared,
        &mut thread,
        "java/util/Map$Entry",
        "getValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry2))],
    )
    .unwrap();
    assert_eq!(unbox_int(&shared, &mut thread, val2), Some(Value::Int(3)));
}

#[test]
fn thread_is_virtual_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // ofVirtual() returns a builder with virtual=true (field 1 = 1)
    let builder = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "ofVirtual",
        "()Ljava/lang/Thread$Builder;",
        &[],
    )
    .unwrap();
    let builder_obj = match builder {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected builder"),
    };
    // Check the builder's virtual flag
    let vflag = shared.mem.heap.get_field(builder_obj, 1);
    assert_eq!(vflag, Value::Int(1));

    // ofPlatform() returns virtual=false
    let builder2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "ofPlatform",
        "()Ljava/lang/Thread$Builder;",
        &[],
    )
    .unwrap();
    let builder2_obj = match builder2 {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected builder"),
    };
    let vflag2 = shared.mem.heap.get_field(builder2_obj, 1);
    assert_eq!(vflag2, Value::Int(0));

    // isVirtual on a regular 3-field thread should return 0
    let regular_thread = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "isVirtual",
        "()Z",
        &[Value::Object(Some(regular_thread))],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(0)));

    // isVirtual on a 5-field virtual thread should return 1
    let vthread = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    shared.mem.heap.set_field(vthread, 4, Value::Int(1));
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "isVirtual",
        "()Z",
        &[Value::Object(Some(vthread))],
    )
    .unwrap();
    assert_eq!(result2, Some(Value::Int(1)));
}

#[test]
fn character_is_emoji_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Smiley face U+1F600 is emoji
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isEmoji",
        "(I)Z",
        &[Value::Int(0x1F600)],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(1)));

    // Letter 'A' is not emoji
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isEmoji",
        "(I)Z",
        &[Value::Int(0x41)],
    )
    .unwrap();
    assert_eq!(result2, Some(Value::Int(0)));

    // Skin tone modifier U+1F3FB is emoji modifier
    let result3 = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isEmojiModifier",
        "(I)Z",
        &[Value::Int(0x1F3FB)],
    )
    .unwrap();
    assert_eq!(result3, Some(Value::Int(1)));

    // isEmojiComponent: ZWJ (U+200D)
    let result4 = call_native(
        &shared,
        &mut thread,
        "java/lang/Character",
        "isEmojiComponent",
        "(I)Z",
        &[Value::Int(0x200D)],
    )
    .unwrap();
    assert_eq!(result4, Some(Value::Int(1)));
}

#[ignore = "STALE: Removed by the wave-3 constant-stub sweep (077c84a18) or a later jdk-only wave; the real bytecode serves it now. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn sb_repeat_string_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sb = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "<init>",
        "()V",
        &[Value::Object(Some(sb))],
    )
    .unwrap();

    // Append "Hello" first
    let hello = create_java_string(&shared, "ab");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
        &[Value::Object(Some(sb)), Value::Object(Some(hello))],
    )
    .unwrap();

    // repeat("xy", 3)
    let xy = create_java_string(&shared, "xy");
    call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "repeat",
        "(Ljava/lang/String;I)Ljava/lang/StringBuilder;",
        &[
            Value::Object(Some(sb)),
            Value::Object(Some(xy)),
            Value::Int(3),
        ],
    )
    .unwrap();

    // toString should be "abxyxyxy"
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/StringBuilder",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sb))],
    )
    .unwrap();
    let str_obj = match result {
        Some(Value::Object(Some(o))) => o,
        _ => panic!("expected string"),
    };
    let text = read_java_string(&shared.mem.heap, str_obj).unwrap();
    assert_eq!(text, "abxyxyxy");
}

#[test]
fn structured_task_scope_j25_fork_join() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create StructuredTaskScope (java.util.concurrent package)
    // Phase 82: 8 fields (name, state, task_count, completed_count, exception, policy, joined, suppressed_count)
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

    // field 0 = name_ref, field 1 = state (OPEN=0), field 2 = task_count, field 3 = completed_count
    let state = shared.mem.heap.get_field(scope, 1);
    assert_eq!(state, Value::Int(0), "state should be OPEN (0)");

    let task_count = shared.mem.heap.get_field(scope, 2);
    assert_eq!(task_count, Value::Int(0), "task_count should be 0");

    // Phase 82: verify policy field is BASE (0)
    let policy = shared.mem.heap.get_field(scope, 5);
    assert_eq!(policy, Value::Int(0), "policy should be BASE (0)");
}

#[test]
fn thread_run_delegates_to_target_p66() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a 5-field Thread with no target (field 3 = None)
    let thr = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    shared.mem.heap.set_field(thr, 3, Value::Object(None));

    // Thread.run() should succeed without error even with no target
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "run",
        "()V",
        &[Value::Object(Some(thr))],
    );
    assert!(result.is_ok());

    // Thread.run() on a 3-field thread should also succeed (no target field)
    let thr2 = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "run",
        "()V",
        &[Value::Object(Some(thr2))],
    );
    assert!(result2.is_ok());
}

// =========================================================================
// Phase C: Java 21 — Pattern Matching for Switch (JEP 441)
// =========================================================================

#[test]
fn type_switch_call_site_cache_roundtrip() {
    use crate::classloading::resolution::{ResolvedCallSite, SwitchLabel};

    let mut cache = crate::classloading::resolution::ResolutionCache::new();
    let cid = ClassId::new(99);
    let labels = vec![
        SwitchLabel::Type {
            class_name: "java/lang/Integer".into(),
            class_id: ClassId::new(99999),
        },
        SwitchLabel::Str("hello".into()),
        SwitchLabel::Int(42),
    ];
    cache.put_call_site(cid, 10, ResolvedCallSite::TypeSwitch { labels });
    let site = cache.get_call_site(cid, 10).unwrap();
    match site {
        ResolvedCallSite::TypeSwitch { labels } => {
            assert_eq!(labels.len(), 3);
            match &labels[0] {
                SwitchLabel::Type { class_name, .. } => {
                    assert_eq!(&**class_name, "java/lang/Integer")
                }
                _ => panic!("expected Type label"),
            }
            match &labels[1] {
                SwitchLabel::Str(s) => assert_eq!(&**s, "hello"),
                _ => panic!("expected Str label"),
            }
            match &labels[2] {
                SwitchLabel::Int(v) => assert_eq!(*v, 42),
                _ => panic!("expected Int label"),
            }
        }
        _ => panic!("expected TypeSwitch"),
    }
}

#[test]
fn enum_switch_call_site_cache_roundtrip() {
    use crate::classloading::resolution::ResolvedCallSite;

    let mut cache = crate::classloading::resolution::ResolutionCache::new();
    let cid = ClassId::new(100);
    let labels: Vec<std::sync::Arc<str>> = vec!["RED".into(), "GREEN".into(), "BLUE".into()];
    cache.put_call_site(cid, 20, ResolvedCallSite::EnumSwitch { labels });
    let site = cache.get_call_site(cid, 20).unwrap();
    match site {
        ResolvedCallSite::EnumSwitch { labels } => {
            assert_eq!(
                labels.iter().map(|s| &**s).collect::<Vec<_>>(),
                ["RED", "GREEN", "BLUE"]
            );
        }
        _ => panic!("expected EnumSwitch"),
    }
}

#[test]
fn type_switch_null_returns_minus_one() {
    // Test the execute_type_switch function with a null target.
    // Per the SwitchBootstraps.typeSwitch contract, null returns -1
    // (the `case null` arm).
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Push a frame with an operand stack.
    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1]; // return
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    // Push target (null) and startIndex (0)
    let _ = frame.stack.push(Value::Object(None));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);

    use crate::classloading::resolution::SwitchLabel;
    let labels = vec![
        SwitchLabel::Type {
            class_name: "java/lang/String".into(),
            class_id: ClassId::new(99999),
        },
        SwitchLabel::Int(42),
    ];

    let result =
        crate::runtime::invokedynamic::execute_type_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    // Result should be -1 for null (the `case null` arm).
    let val = thread.frames[0].stack.pop().unwrap();
    assert_eq!(val, Value::Int(-1));
}

#[test]
fn type_switch_integer_match() {
    // Test matching an Integer object against an Int label.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Register Integer class and create a boxed Integer(42)
    call_native(
        &shared,
        &mut thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(None)],
    )
    .ok(); // warm up registry

    let int_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .unwrap();
    let int_obj = shared.mem.heap.alloc_object(int_class_id, 1);
    shared.mem.heap.set_field(int_obj, 0, Value::Int(42));

    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(Some(int_obj)));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);

    use crate::classloading::resolution::SwitchLabel;
    // Use a ClassId that won't match Integer (99999 is not a real class)
    let labels = vec![
        SwitchLabel::Type {
            class_name: "java/lang/String".into(),
            class_id: ClassId::new(99999),
        },
        SwitchLabel::Int(42),
    ];

    let result =
        crate::runtime::invokedynamic::execute_type_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    let val = thread.frames[0].stack.pop().unwrap();
    // Should match label index 1 (Int(42))
    assert_eq!(val, Value::Int(1));
}

#[test]
fn type_switch_string_match() {
    // Test matching a String object against a Str label.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    call_native(
        &shared,
        &mut thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(None)],
    )
    .ok();

    let str_obj = create_java_string(&shared, "hello");

    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(Some(str_obj)));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);

    use crate::classloading::resolution::SwitchLabel;
    let labels = vec![
        SwitchLabel::Int(99),
        SwitchLabel::Str("hello".into()),
        SwitchLabel::Str("world".into()),
    ];

    let result =
        crate::runtime::invokedynamic::execute_type_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    let val = thread.frames[0].stack.pop().unwrap();
    // Should match label index 1 (Str("hello"))
    assert_eq!(val, Value::Int(1));
}

#[test]
fn type_switch_no_match_returns_labels_len() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    call_native(
        &shared,
        &mut thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(None)],
    )
    .ok();

    // Create a generic object that won't match any label
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(Some(obj)));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);

    use crate::classloading::resolution::SwitchLabel;
    let labels = vec![SwitchLabel::Str("nope".into()), SwitchLabel::Int(999)];

    let result =
        crate::runtime::invokedynamic::execute_type_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    let val = thread.frames[0].stack.pop().unwrap();
    // No label matches a non-null target → labels.len() (default arm).
    assert_eq!(val, Value::Int(2));
}

#[test]
fn type_switch_start_index_skips_earlier_labels() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    call_native(
        &shared,
        &mut thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(None)],
    )
    .ok();

    let str_obj = create_java_string(&shared, "hello");

    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(Some(str_obj)));
    // startIndex = 2, so label 1 ("hello") should be skipped
    let _ = frame.stack.push(Value::Int(2));
    thread.frames.push(frame);

    use crate::classloading::resolution::SwitchLabel;
    let labels = vec![
        SwitchLabel::Int(1),
        SwitchLabel::Str("hello".into()),
        SwitchLabel::Str("world".into()),
    ];

    let result =
        crate::runtime::invokedynamic::execute_type_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    let val = thread.frames[0].stack.pop().unwrap();
    // "hello" is at index 1 but startIndex=2 skips it → no match
    // "hello" is at index 1 but startIndex=2 skips it -> no match -> labels.len()
    assert_eq!(val, Value::Int(3));
}

#[test]
fn enum_switch_matches_name() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    call_native(
        &shared,
        &mut thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(None)],
    )
    .ok();

    // Create an enum-like object: field 0 = name (String), field 1 = ordinal (int)
    let name_obj = create_java_string(&shared, "GREEN");
    let enum_obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared
        .mem
        .heap
        .set_field(enum_obj, 0, Value::Object(Some(name_obj)));
    shared.mem.heap.set_field(enum_obj, 1, Value::Int(1));

    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(Some(enum_obj)));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);

    let labels: Vec<std::sync::Arc<str>> = vec!["RED".into(), "GREEN".into(), "BLUE".into()];

    let result =
        crate::runtime::invokedynamic::execute_enum_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    let val = thread.frames[0].stack.pop().unwrap();
    assert_eq!(val, Value::Int(1)); // GREEN is index 1
}

#[test]
fn enum_switch_null_returns_minus_one() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    use crate::runtime::frame::Frame;
    let code: Vec<u8> = vec![0xb1];
    let mut frame = Frame::new(
        ClassId::new(0),
        String::new(),
        String::new(),
        String::new(),
        None,
        code,
        vec![],
        10,
        0,
        &[],
    );
    let _ = frame.stack.push(Value::Object(None));
    let _ = frame.stack.push(Value::Int(0));
    thread.frames.push(frame);

    let labels: Vec<std::sync::Arc<str>> = vec!["A".into(), "B".into()];

    let result =
        crate::runtime::invokedynamic::execute_enum_switch(&shared, &mut thread, 0, &labels);
    assert!(result.is_ok());
    let val = thread.frames[0].stack.pop().unwrap();
    assert_eq!(val, Value::Int(-1)); // null -> -1 (case null arm)
}

// =========================================================================
// Phase C: Java 21 — Virtual Threads (JEP 444)
// =========================================================================

#[test]
fn virtual_scheduler_in_shared_vm() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Scheduler should be created with hardware parallelism count
    assert!(shared.threads.virtual_scheduler.carrier_count() > 0);
}

#[test]
fn thread_kind_defaults_to_platform() {
    let thread = JvmThread::new(ThreadId(42), "test");
    assert_eq!(thread.kind, crate::threading::ThreadKind::Platform);
}

#[test]
fn thread_start_detects_virtual_flag() {
    // Verify that thread_start reads field 4 for the virtual flag.
    // We can't easily test the full spawn path in a unit test, but we can
    // verify the flag detection logic matches the native registration.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // 5-field Thread with field 4 = 1 (virtual)
    let vt = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    shared.mem.heap.set_field(vt, 4, Value::Int(1));
    let header = shared.mem.heap.get_header(vt);
    assert!(header.num_slots() >= 5);
    assert!(matches!(shared.mem.heap.get_field(vt, 4), Value::Int(1)));

    // 5-field Thread with field 4 = 0 (platform)
    let pt = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    shared.mem.heap.set_field(pt, 4, Value::Int(0));
    assert!(matches!(shared.mem.heap.get_field(pt, 4), Value::Int(0)));

    // 3-field Thread (no field 4, platform by default)
    let st = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let h = shared.mem.heap.get_header(st);
    assert!(h.num_slots() < 5);
}

// =========================================================================
// Phase D: Java 25 — Stream Gatherers, Scoped Values, Structured Concurrency
// =========================================================================

#[test]
fn class_file_version_java_25_supported() {
    use cratonvm_reader::class_file_version::ClassFileVersion;
    assert!(ClassFileVersion::JAVA_22.is_supported());
    assert!(ClassFileVersion::JAVA_23.is_supported());
    assert!(ClassFileVersion::JAVA_24.is_supported());
    assert!(ClassFileVersion::JAVA_25.is_supported());
    assert!(!ClassFileVersion::new(70, 0).is_supported());
    assert_eq!(ClassFileVersion::JAVA_25.major, 69);
}

#[test]
fn gatherer_of_creates_gatherer_pd() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create a dummy integrator object (lambda proxy not needed for registration test)
    let integrator = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Gatherer",
        "of",
        "(Ljava/util/stream/Gatherer$Integrator;)Ljava/util/stream/Gatherer;",
        &[Value::Object(Some(integrator))],
    )
    .unwrap();

    // Should return a 4-field Gatherer with integrator in field 1
    if let Some(Value::Object(Some(g))) = result {
        let integrator_val = shared.mem.heap.get_field(g, 1);
        assert!(matches!(integrator_val, Value::Object(Some(_))));
        // field 0 (initializer) should be null
        assert!(matches!(
            shared.mem.heap.get_field(g, 0),
            Value::Object(None)
        ));
    } else {
        panic!("Expected Gatherer object");
    }
}

#[test]
fn scoped_value_new_instance_pd() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        &[],
    )
    .unwrap();
    assert!(matches!(result, Some(Value::Object(Some(_)))));
}

#[test]
fn scoped_value_not_bound_returns_false_pd() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sv = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        &[],
    )
    .unwrap()
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "isBound",
        "()Z",
        &[sv],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(0)));
}

#[test]
fn scoped_value_binding_stack_pd() {
    // Test the scoped value stack directly on JvmThread
    let mut thread = JvmThread::new(ThreadId(0), "test");
    assert!(thread.scoped_values.is_empty());

    thread.scoped_values.push((42, None, Value::Int(100)));
    thread.scoped_values.push((43, None, Value::Int(200)));

    // Search from top
    let found = thread
        .scoped_values
        .iter()
        .rev()
        .find(|(k, _, _)| *k == 42)
        .map(|(_, _, v)| *v);
    assert_eq!(found, Some(Value::Int(100)));

    thread.scoped_values.pop();
    assert_eq!(thread.scoped_values.len(), 1);
}

#[test]
fn structured_task_scope_close_pd() {
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

    // State should be OPEN (0)
    assert_eq!(shared.mem.heap.get_field(scope, 1), Value::Int(0));

    // Join
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "join",
        "()Ljava/util/concurrent/StructuredTaskScope;",
        &[Value::Object(Some(scope))],
    )
    .unwrap();
    // join does not change state in new impl — state remains OPEN(0)
    assert_eq!(shared.mem.heap.get_field(scope, 1), Value::Int(0));

    // Close
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/StructuredTaskScope",
        "close",
        "()V",
        &[Value::Object(Some(scope))],
    )
    .unwrap();
    assert_eq!(shared.mem.heap.get_field(scope, 1), Value::Int(2)); // CLOSED
}

#[test]
fn scoped_value_or_else_when_unbound_pd() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let sv = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        &[],
    )
    .unwrap()
    .unwrap();

    let fallback = Value::Int(999);
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ScopedValue",
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[sv, fallback],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(999)));
}

// =========================================================================
// Phase E: Panama FFI (JEP 454)
// =========================================================================
//
// THE TWO ENCODINGS ARE GONE; THESE TESTS NOW SPEAK THE JDK'S (F16,
// 2026-08-13). This block replaces F9's "just fix it is not available"
// note, which was correct on the day it was written and is now resolved.
//
// F9 found fourteen sites in eight tests driving `ValueLayout.JAVA_*`
// under `()Ljava/lang/foreign/ValueLayout;` — a METHOD descriptor for a
// FIELD, naming a return type the JDK never uses:
//
//   $ javap -p java.lang.foreign.ValueLayout        # 25.0.3+9-LTS
//     public static final java.lang.foreign.ValueLayout$OfInt JAVA_INT;
//   $ javap -c F16Desc   # static Object e(){ return ValueLayout.JAVA_INT; }
//     0: getstatic  Field java/lang/foreign/ValueLayout.JAVA_INT:
//                         Ljava/lang/foreign/ValueLayout$OfInt;
//
// and could not simply correct them, because the `$Of*` spelling resolved
// to a SECOND registration with an incompatible object encoding, and the
// group-layout consumers were bound to the other one by registration
// ORDER rather than by descriptor. Both halves have now been resolved in
// `native-builtins`:
//
//   * `panama.rs`'s `pe_make_layout` (3 slots, `[0]=Int(kind)`) is
//     `#[cfg(test)]` and mints nothing that ships; its nine fabricated
//     `()L…ValueLayout;` rows and its whole group-layout family are
//     DELETED.
//   * `phases_late/foreign_ffm.rs` is the only implementation left, in
//     BOTH JDK modes, on the JDK's own shape — `[0]=Long(byteSize),
//     [1]=Long(byteAlignment), [2]=payload, [3]=name`, the order a real
//     `jdk.internal.foreign.layout.AbstractLayout` declares.
//
// So all fourteen sites now carry the descriptor `javap` prints, and the
// group-layout calls carry theirs (`…)Ljava/lang/foreign/StructLayout;`,
// `UnionLayout`, `SequenceLayout`) instead of the fabricated
// `…)Ljava/lang/foreign/MemoryLayout;` that only panama ever registered.
//
// READ SLOT 0 FOR A SIZE AND SLOT 1 FOR AN ALIGNMENT. The tests below used
// to read slot 1 for the size and slot 5 for the alignment, which were the
// deleted carrier's. Prefer calling `byteSize()`/`byteAlignment()` over
// touching a slot at all — those are registered on every layout class now.
//
// Two tests below assert an EXCEPTION where they used to assert a number.
// That is not a regression: they were pinning a call HotSpot refuses. See
// `panama_struct_layout_pe2` and `struct_layout_byte_int_alignment`.

#[test]
fn panama_value_layout_constants_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let vl = "java/lang/foreign/ValueLayout";

    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();
    if let Value::Object(Some(layout)) = int_layout {
        let size = call_native(
            &shared,
            &mut thread,
            vl,
            "byteSize",
            "()J",
            &[Value::Object(Some(layout))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(size, Value::Long(4));
    } else {
        panic!("Expected ValueLayout object");
    }

    let long_layout = ffm_value_layout(&shared, &mut thread, "JAVA_LONG")
        .unwrap()
        .unwrap();
    if let Value::Object(Some(layout)) = long_layout {
        let size = call_native(
            &shared,
            &mut thread,
            vl,
            "byteSize",
            "()J",
            &[Value::Object(Some(layout))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(size, Value::Long(8));
    }
}

#[test]
fn panama_arena_allocate_and_close_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let arena = "java/lang/foreign/Arena";

    // Create confined arena
    let a = call_native(
        &shared,
        &mut thread,
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arena_ref = match a {
        Value::Object(Some(r)) => r,
        _ => panic!("Expected arena"),
    };

    // Allocate 256 bytes
    let seg = call_native(
        &shared,
        &mut thread,
        arena,
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(arena_ref)), Value::Long(256)],
    )
    .unwrap()
    .unwrap();
    let seg_ref = match seg {
        Value::Object(Some(r)) => r,
        _ => panic!("Expected segment"),
    };

    // Check size
    let size = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "byteSize",
        "()J",
        &[Value::Object(Some(seg_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Long(256));

    // Address should be non-zero
    let addr = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "address",
        "()J",
        &[Value::Object(Some(seg_ref))],
    )
    .unwrap()
    .unwrap();
    match addr {
        Value::Long(a) => assert!(a != 0, "Address should be non-null"),
        _ => panic!("Expected long"),
    };

    // Close arena (frees memory)
    call_native(
        &shared,
        &mut thread,
        arena,
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    )
    .unwrap();

    // Double-close should error
    let result = call_native(
        &shared,
        &mut thread,
        arena,
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    );
    assert!(result.is_err());
}

#[test]
fn panama_memory_segment_get_set_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let arena = "java/lang/foreign/Arena";
    let ms = "java/lang/foreign/MemorySegment";

    // Create arena and allocate 64 bytes
    let a = call_native(
        &shared,
        &mut thread,
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arena_ref = match a {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };
    let seg = call_native(
        &shared,
        &mut thread,
        arena,
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(arena_ref)), Value::Long(64)],
    )
    .unwrap()
    .unwrap();
    let seg_ref = match seg {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Get JAVA_INT layout
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();
    let int_layout_ref = match int_layout {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Write 42 at offset 0
    call_native(
        &shared,
        &mut thread,
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(int_layout_ref)),
            Value::Long(0),
            Value::Int(42),
        ],
    )
    .unwrap();

    // Read back
    let val = call_native(
        &shared,
        &mut thread,
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(int_layout_ref)),
            Value::Long(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(42));

    // Write 99 at offset 4
    call_native(
        &shared,
        &mut thread,
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(int_layout_ref)),
            Value::Long(4),
            Value::Int(99),
        ],
    )
    .unwrap();
    let val2 = call_native(
        &shared,
        &mut thread,
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(int_layout_ref)),
            Value::Long(4),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val2, Value::Int(99));

    // Clean up
    call_native(
        &shared,
        &mut thread,
        arena,
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    )
    .unwrap();
}

#[test]
fn panama_memory_segment_fill_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arena_ref = match a {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };
    let seg = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(arena_ref)), Value::Long(16)],
    )
    .unwrap()
    .unwrap();
    let seg_ref = match seg {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Fill with 0xFF
    call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "fill",
        "(B)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(seg_ref)), Value::Int(0xFF_u8 as i32)],
    )
    .unwrap();

    // Read byte at offset 0 — should be -1 (0xFF as signed byte)
    let byte_layout = ffm_value_layout(&shared, &mut thread, "JAVA_BYTE")
        .unwrap()
        .unwrap();
    let byte_layout_ref = match byte_layout {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };
    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(byte_layout_ref)),
            Value::Long(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(-1)); // 0xFF as i8 = -1

    call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    )
    .unwrap();
}

#[test]
fn panama_function_descriptor_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let fd = "java/lang/foreign/FunctionDescriptor";

    // Get JAVA_INT layout for return type
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();

    // Create empty param array
    let params = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let _ = shared.mem.heap.set_array_element(params, 0, int_layout);

    // Create descriptor: int(int) → int
    let desc = call_native(&shared, &mut thread, fd, "of",
            "(Ljava/lang/foreign/ValueLayout;[Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;",
            &[int_layout, Value::Object(Some(params))]).unwrap().unwrap();
    assert!(matches!(desc, Value::Object(Some(_))));
}

#[test]
fn panama_native_memory_table_pe() {
    // Test the NativeMemoryTable directly
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // Allocate via SharedVm
    let (id, ptr) = shared
        .natives
        .native_memory
        .lock()
        .allocate(128, 8)
        .unwrap();
    assert!(!ptr.is_null());

    // Write and read
    unsafe {
        *(ptr as *mut i32) = 12345;
    }
    unsafe {
        assert_eq!(*(ptr as *mut i32), 12345);
    }

    // Free
    assert!(shared.natives.native_memory.lock().free(id));
}

#[test]
fn panama_linker_creation_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let linker = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Linker",
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(linker, Value::Object(Some(_))));
}

#[test]
fn panama_global_arena_cannot_close_pe() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "global",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arena_ref = match a {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Closing global arena should error
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    );
    assert!(result.is_err());
}

// =========================================================================
// Phase E2: Panama FFI — Struct Layouts, Upcalls, String Marshaling
// =========================================================================

#[test]
fn panama_struct_layout_pe2() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ml = "java/lang/foreign/MemoryLayout";

    // Create an array of [JAVA_INT, JAVA_LONG] layouts
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();
    let long_layout = ffm_value_layout(&shared, &mut thread, "JAVA_LONG")
        .unwrap()
        .unwrap();

    let members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let _ = shared.mem.heap.set_array_element(members, 0, int_layout);
    let _ = shared.mem.heap.set_array_element(members, 1, long_layout);

    // WAS A DIVERGENCE PIN, IS NOW A REAL ASSERTION (F16, 2026-08-13).
    //
    // This test used to assert byteSize=16 / byteAlignment=8 for
    // `structLayout(JAVA_INT, JAVA_LONG)`. F9 measured that HotSpot
    // REFUSES that call and relabelled the numbers as a divergence pin
    // rather than inventing new ones, because the fix was in
    // native-builtins. It has landed, so the pin becomes the measurement:
    //
    //   $ java F16Probe                               # 25.0.3+9-LTS
    //     structLayout(JAVA_INT, JAVA_LONG)
    //       -> THREW java.lang.IllegalArgumentException:
    //          Invalid alignment constraint for member layout: j8
    //
    // The JDK never auto-pads a struct — the caller writes the padding,
    // and an under-aligned member is an error. `JAVA_LONG` needs an offset
    // divisible by 8 and lands at 4.
    //
    // The KIND is what a `catch` can rely on and is asserted exactly. The
    // MESSAGE is checked loosely, because the JDK renders the offending
    // member with `MemoryLayout::toString` (`j8`) and that rendering is a
    // diagnostic, not a contract.
    let refused = call_native(
        &shared,
        &mut thread,
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        &[Value::Object(Some(members))],
    );
    match refused {
        Err(MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::IllegalArgumentException { ref message },
        ))) => {
            assert!(
                message.contains("Invalid alignment constraint for member layout"),
                "structLayout(JAVA_INT, JAVA_LONG) must refuse with the JDK's wording, \
                     got {message:?}"
            );
        }
        other => panic!(
            "structLayout(JAVA_INT, JAVA_LONG) must throw IllegalArgumentException the way \
                 HotSpot 25.0.3+9 does (Invalid alignment constraint for member layout: j8); \
                 got {other:?}"
        ),
    }

    // The padded form is the one the JDK accepts, and it is the 16/8 this
    // test used to claim for the unpadded call. Measured:
    //   structLayout(JAVA_INT, paddingLayout(4), JAVA_LONG)
    //     -> byteSize=16 byteAlignment=8
    let padding = call_native(
        &shared,
        &mut thread,
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/PaddingLayout;",
        &[Value::Long(4)],
    )
    .unwrap()
    .unwrap();

    let padded_members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        3,
    );
    let _ = shared
        .mem
        .heap
        .set_array_element(padded_members, 0, int_layout);
    let _ = shared
        .mem
        .heap
        .set_array_element(padded_members, 1, padding);
    let _ = shared
        .mem
        .heap
        .set_array_element(padded_members, 2, long_layout);

    let struct_layout = call_native(
        &shared,
        &mut thread,
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        &[Value::Object(Some(padded_members))],
    )
    .unwrap()
    .unwrap();

    if let Value::Object(Some(sl)) = struct_layout {
        let size = call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/StructLayout",
            "byteSize",
            "()J",
            &[Value::Object(Some(sl))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            size,
            Value::Long(16),
            "struct(int, pad(4), long) is 16 bytes on HotSpot 25.0.3+9"
        );

        let align = call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/StructLayout",
            "byteAlignment",
            "()J",
            &[Value::Object(Some(sl))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            align,
            Value::Long(8),
            "alignment is 8 (max member alignment)"
        );
    } else {
        panic!("MemoryLayout.structLayout must answer a reference, got {struct_layout:?}");
    }
}

/// The JDK does not round a struct's total size up to its alignment, and
/// this VM used to (F16, 2026-08-13). Measured on 25.0.3+9-LTS:
///
/// ```text
/// structLayout(JAVA_LONG, JAVA_INT) -> byteSize=12 align=8
/// structLayout(JAVA_INT,  JAVA_BYTE) -> byteSize=5  align=4
/// structLayout()                     -> byteSize=0  align=1
/// ```
///
/// `structLayout(JAVA_LONG, JAVA_INT)` is the important one: it is a call
/// the oracle ACCEPTS (each member lands on a multiple of its own
/// alignment — long at 0, int at 8), so no alignment refusal hides the
/// arithmetic. The old body ended with
/// `total_size = ((offset + max_align - 1) / max_align) * max_align`
/// and answered 16. Nothing tested it.
#[test]
fn struct_layout_does_not_pad_the_total() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ml = "java/lang/foreign/MemoryLayout";

    let long_layout = ffm_value_layout(&shared, &mut thread, "JAVA_LONG")
        .unwrap()
        .unwrap();
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();

    let members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let _ = shared.mem.heap.set_array_element(members, 0, long_layout);
    let _ = shared.mem.heap.set_array_element(members, 1, int_layout);

    let sl = call_native(
        &shared,
        &mut thread,
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        &[Value::Object(Some(members))],
    )
    .unwrap()
    .unwrap();

    let Value::Object(Some(s)) = sl else {
        panic!("MemoryLayout.structLayout must answer a reference, got {sl:?}");
    };
    let size = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/StructLayout",
        "byteSize",
        "()J",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        size,
        Value::Long(12),
        "struct(long, int) is 12 bytes on HotSpot 25.0.3+9 — the total is NOT rounded up \
             to the 8-byte alignment"
    );

    let align = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/StructLayout",
        "byteAlignment",
        "()J",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(align, Value::Long(8), "alignment is max(8, 4) = 8");
}

/// A padding layout's alignment is 1, whatever its size — and this is what
/// made the JDK's own CORRECT idiom answer wrongly here (F16,
/// 2026-08-13). Measured on 25.0.3+9-LTS:
///
/// ```text
/// paddingLayout(3)                                    -> byteSize=3 align=1
/// structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT) -> byteSize=8 align=4
/// ```
///
/// `paddingLayout` used to mint a ONE-slot carrier, so the member decode
/// found no alignment in slot 1 and fell back to "alignment = size" — a
/// 3-byte padding claiming 3-byte alignment. The struct above then came
/// out at 12 instead of 8. The carrier is four slots now, like every other
/// layout, and `PaddingLayout` has the accessors to read it with (it had
/// none at all: `paddingLayout(3).byteSize()` raised `AbstractMethodError`).
#[test]
fn padding_layout_is_byte_aligned_and_pads_a_struct_to_eight() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ml = "java/lang/foreign/MemoryLayout";

    let padding = call_native(
        &shared,
        &mut thread,
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/PaddingLayout;",
        &[Value::Long(3)],
    )
    .unwrap()
    .unwrap();
    let Value::Object(Some(p)) = padding else {
        panic!("MemoryLayout.paddingLayout must answer a reference, got {padding:?}");
    };
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/PaddingLayout",
            "byteSize",
            "()J",
            &[Value::Object(Some(p))],
        )
        .unwrap()
        .unwrap(),
        Value::Long(3),
        "paddingLayout(3).byteSize() is 3"
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/PaddingLayout",
            "byteAlignment",
            "()J",
            &[Value::Object(Some(p))],
        )
        .unwrap()
        .unwrap(),
        Value::Long(1),
        "a padding layout's alignment is ALWAYS 1, never its size"
    );

    let byte_layout = ffm_value_layout(&shared, &mut thread, "JAVA_BYTE")
        .unwrap()
        .unwrap();
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();

    let members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        3,
    );
    let _ = shared.mem.heap.set_array_element(members, 0, byte_layout);
    let _ = shared.mem.heap.set_array_element(members, 1, padding);
    let _ = shared.mem.heap.set_array_element(members, 2, int_layout);

    let sl = call_native(
        &shared,
        &mut thread,
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        &[Value::Object(Some(members))],
    )
    .unwrap()
    .unwrap();
    let Value::Object(Some(s)) = sl else {
        panic!("MemoryLayout.structLayout must answer a reference, got {sl:?}");
    };
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/StructLayout",
            "byteSize",
            "()J",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap(),
        Value::Long(8),
        "struct(byte, pad(3), int) is 8 bytes — the idiom the JDK REQUIRES, which this VM \
             answered 12 for while its padding carried alignment 3"
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/StructLayout",
            "byteAlignment",
            "()J",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap(),
        Value::Long(4),
        "alignment is max(1, 1, 4) = 4 — padding contributes 1, not 3"
    );
}

/// `sequenceLayout` rejects a negative element count, as the JDK does.
/// Measured: `sequenceLayout(-1, JAVA_INT)` throws
/// `IllegalArgumentException: The provided elementCount is negative: -1`.
/// This VM accepted it and answered a NEGATIVE byteSize.
#[test]
fn sequence_layout_rejects_negative_element_count() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();

    let refused = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemoryLayout",
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;",
        &[Value::Long(-1), int_layout],
    );
    match refused {
        Err(MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::IllegalArgumentException { ref message },
        ))) => assert!(
            message.contains("negative"),
            "sequenceLayout(-1, JAVA_INT) must refuse with the JDK's wording, got {message:?}"
        ),
        other => panic!(
            "sequenceLayout(-1, JAVA_INT) must throw IllegalArgumentException \
                 (The provided elementCount is negative: -1); got {other:?}"
        ),
    }
}

#[test]
fn panama_sequence_layout_pe2() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ml = "java/lang/foreign/MemoryLayout";

    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();

    // sequenceLayout(10, JAVA_INT) → 40 bytes, alignment 4.
    // Measured on 25.0.3+9-LTS: byteSize=40 align=4. This test already
    // agreed with the oracle; what changed (F16, 2026-08-13) is the
    // descriptor — `…)Ljava/lang/foreign/SequenceLayout;` is what javac
    // emits, and the `…)Ljava/lang/foreign/MemoryLayout;` spelling it used
    // was registered only by the deleted panama family — and the SLOT: the
    // size is slot 0 in the authoritative carrier, not slot 1.
    let seq = call_native(
        &shared,
        &mut thread,
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;",
        &[Value::Long(10), int_layout],
    )
    .unwrap()
    .unwrap();

    if let Value::Object(Some(s)) = seq {
        // Read it the way Java does rather than by slot index, so the test
        // survives a carrier change instead of pinning one.
        let size = call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/SequenceLayout",
            "byteSize",
            "()J",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            size,
            Value::Long(40),
            "sequenceLayout(10, JAVA_INT) is 40 bytes"
        );
        let align = call_native(
            &shared,
            &mut thread,
            "java/lang/foreign/SequenceLayout",
            "byteAlignment",
            "()J",
            &[Value::Object(Some(s))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            align,
            Value::Long(4),
            "a sequence's alignment is its ELEMENT's, not its total size"
        );
    } else {
        panic!("MemoryLayout.sequenceLayout must answer a reference, got {seq:?}");
    }
}

#[test]
fn panama_string_marshaling_pe2() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create arena and allocate a UTF-8 string
    let a = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arena_ref = match a {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    let hello = create_java_string(&shared, "Hello, Panama!");
    let seg = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "allocateUtf8String",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(arena_ref)), Value::Object(Some(hello))],
    )
    .unwrap()
    .unwrap();
    let seg_ref = match seg {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Read it back
    let read_back = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "getUtf8String",
        "(J)Ljava/lang/String;",
        &[Value::Object(Some(seg_ref)), Value::Long(0)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(str_ref)) = read_back {
        let s = read_java_string(&shared.mem.heap, str_ref).unwrap();
        assert_eq!(s, "Hello, Panama!");
    } else {
        panic!("Expected String");
    }

    call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    )
    .unwrap();
}

#[test]
fn panama_upcall_table_pe2() {
    // Test UpcallTable directly — use a real heap object as target
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let target_obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);

    let mut table = crate::native::ffi::UpcallTable::new();
    let entry = crate::native::ffi::UpcallEntry {
        target: target_obj,
        method_name: "apply".to_string(),
        method_descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
        param_kinds: vec![crate::native::ffi::LAYOUT_INT],
        return_kind: crate::native::ffi::LAYOUT_INT,
    };

    let slot = table.register(entry);
    assert!(table.get(slot).is_some());
    assert_eq!(table.get(slot).unwrap().method_name, "apply");

    table.remove(slot);
    assert!(table.get(slot).is_none());
}

#[test]
fn panama_reinterpret_pe2() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let a = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap();
    let arena_ref = match a {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    let seg = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(arena_ref)), Value::Long(64)],
    )
    .unwrap()
    .unwrap();
    let seg_ref = match seg {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Reinterpret as 128 bytes
    let reinterpreted = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(seg_ref)), Value::Long(128)],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(r)) = reinterpreted {
        let size = shared.mem.heap.get_field(r, 1);
        assert_eq!(size, Value::Long(128));
    } else {
        panic!("MemorySegment.reinterpret(J) must answer a reference, got {reinterpreted:?}");
    }

    call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    )
    .unwrap();
}

// =========================================================================
// Phase H: Comprehensive Test Coverage
// =========================================================================

// --- Scoped Value Edge Cases ---

#[test]
fn scoped_value_shadowing() {
    // Test that inner binding shadows outer binding with same key
    let mut thread = JvmThread::new(ThreadId(0), "test");

    thread.scoped_values.push((100, None, Value::Int(1)));
    thread.scoped_values.push((100, None, Value::Int(2))); // shadows

    // Should find the inner (most recent) binding
    let found = thread
        .scoped_values
        .iter()
        .rev()
        .find(|(k, _, _)| *k == 100)
        .map(|(_, _, v)| *v);
    assert_eq!(found, Some(Value::Int(2)));

    // Pop inner binding
    thread.scoped_values.pop();

    // Now should find the outer binding
    let found2 = thread
        .scoped_values
        .iter()
        .rev()
        .find(|(k, _, _)| *k == 100)
        .map(|(_, _, v)| *v);
    assert_eq!(found2, Some(Value::Int(1)));

    thread.scoped_values.pop();
    assert!(thread.scoped_values.is_empty());
}

#[test]
fn scoped_value_multiple_keys() {
    let mut thread = JvmThread::new(ThreadId(0), "test");

    thread.scoped_values.push((1, None, Value::Int(10)));
    thread.scoped_values.push((2, None, Value::Int(20)));
    thread.scoped_values.push((3, None, Value::Int(30)));

    // Each key returns its own value
    for (key, expected) in [(1, 10), (2, 20), (3, 30)] {
        let found = thread
            .scoped_values
            .iter()
            .rev()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, v)| *v);
        assert_eq!(found, Some(Value::Int(expected)));
    }

    // Non-existent key returns None
    let missing = thread
        .scoped_values
        .iter()
        .rev()
        .find(|(k, _, _)| *k == 999)
        .map(|(_, _, v)| *v);
    assert_eq!(missing, None);
}

#[test]
fn scoped_value_exception_safety_bindings_popped() {
    // Verify that the Carrier.run pattern pops bindings even when
    // the inner operation fails. This is tested by checking the
    // scoped_values length before and after.
    let _shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Pre-push a binding to simulate Carrier.run setup
    thread.scoped_values.push((42, None, Value::Int(100)));
    assert_eq!(thread.scoped_values.len(), 1);

    // Simulate the pop that Carrier.run always does (even on error)
    thread.scoped_values.pop();
    assert_eq!(thread.scoped_values.len(), 0);

    // Direct push/pop cycle
    thread.scoped_values.push((77, None, Value::Int(42)));
    let found = thread
        .scoped_values
        .iter()
        .rev()
        .find(|(k, _, _)| *k == 77)
        .map(|(_, _, v)| *v);
    assert_eq!(found, Some(Value::Int(42)));
    thread.scoped_values.pop();
    let not_found = thread.scoped_values.iter().rev().find(|(k, _, _)| *k == 77);
    assert!(not_found.is_none());
}

// --- CONSTANT_Dynamic (Condy) Tests ---

#[test]
fn condy_default_for_descriptor() {
    // Test the default_for_descriptor helper used by condy resolution
    use crate::runtime::interpreter::default_for_descriptor;
    assert_eq!(default_for_descriptor("I"), Value::Int(0));
    assert_eq!(default_for_descriptor("J"), Value::Long(0));
    assert_eq!(default_for_descriptor("F"), Value::Float(0.0));
    assert_eq!(default_for_descriptor("D"), Value::Double(0.0));
    assert_eq!(
        default_for_descriptor("Ljava/lang/String;"),
        Value::Object(None)
    );
    assert_eq!(default_for_descriptor("Z"), Value::Int(0)); // boolean
    assert_eq!(default_for_descriptor("B"), Value::Int(0)); // byte
}

#[test]
fn condy_cache_roundtrip() {
    use crate::classloading::resolution::ResolutionCache;
    let mut cache = ResolutionCache::new();
    let cid = ClassId::new(50);

    assert!(cache.get_condy(cid, 10).is_none());
    cache.put_condy(cid, 10, Value::Int(42));
    assert_eq!(cache.get_condy(cid, 10), Some(&Value::Int(42)));

    // Different cp_index
    cache.put_condy(cid, 20, Value::Long(9999));
    assert_eq!(cache.get_condy(cid, 20), Some(&Value::Long(9999)));
    // Original still there
    assert_eq!(cache.get_condy(cid, 10), Some(&Value::Int(42)));
}

// --- Enhanced NPE Message Tests ---

#[test]
fn npe_message_on_invoke_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Calling a method on null should produce a descriptive message
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/Object",
        "hashCode",
        "()I",
        &[Value::Object(None)],
    );
    // The result should be an error (NPE)
    assert!(result.is_err());
}

#[test]
fn npe_resolve_field_name() {
    // Test the field name resolution helper directly
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // With no class loaded, resolve_field_name returns None
    let name = crate::runtime::interpreter::resolve_field_name(&shared, ClassId::new(99999), 0);
    assert!(name.is_none());
}

// --- Struct Layout Edge Cases ---

#[test]
fn struct_layout_single_field() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Create struct with just JAVA_INT
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();
    let members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        1,
    );
    let _ = shared.mem.heap.set_array_element(members, 0, int_layout);

    let sl = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemoryLayout",
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        &[Value::Object(Some(members))],
    )
    .unwrap()
    .unwrap();

    // Single int field: byteSize=4, byteAlignment=4. Measured on
    // 25.0.3+9-LTS, and unchanged by F16 — this was one of the three
    // layouts F9 confirmed already agreed with the oracle. What changed is
    // only HOW it is read: the descriptor javac actually emits, and
    // `byteSize()`/`byteAlignment()` instead of slots 1 and 5, which
    // belonged to the deleted carrier.
    if let Value::Object(Some(s)) = sl {
        assert_eq!(
            call_native(
                &shared,
                &mut thread,
                "java/lang/foreign/StructLayout",
                "byteSize",
                "()J",
                &[Value::Object(Some(s))],
            )
            .unwrap()
            .unwrap(),
            Value::Long(4)
        );
        assert_eq!(
            call_native(
                &shared,
                &mut thread,
                "java/lang/foreign/StructLayout",
                "byteAlignment",
                "()J",
                &[Value::Object(Some(s))],
            )
            .unwrap()
            .unwrap(),
            Value::Long(4)
        );
    } else {
        panic!("MemoryLayout.structLayout must answer a reference, got {sl:?}");
    }
}

#[test]
fn struct_layout_byte_int_alignment() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // struct { byte, int } → byte(1) + pad(3) + int(4) = 8 bytes
    let byte_layout = ffm_value_layout(&shared, &mut thread, "JAVA_BYTE")
        .unwrap()
        .unwrap();
    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();

    let members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let _ = shared.mem.heap.set_array_element(members, 0, byte_layout);
    let _ = shared.mem.heap.set_array_element(members, 1, int_layout);

    // WAS A DIVERGENCE PIN, IS NOW A REAL ASSERTION (F16, 2026-08-13) —
    // the same fabrication as `panama_struct_layout_pe2`, one size down.
    // Re-measured on 25.0.3+9-LTS rather than taken from F9's transcript:
    //
    //   $ java F16Probe
    //     structLayout(JAVA_BYTE, JAVA_INT)
    //       -> THREW java.lang.IllegalArgumentException:
    //          Invalid alignment constraint for member layout: i4
    //
    // The 8/4 this test used to assert is the PADDED layout's answer, and
    // this call supplies no padding. `JAVA_INT` needs an offset divisible
    // by 4 and lands at 1.
    //
    // The padded form — `structLayout(JAVA_BYTE, paddingLayout(3),
    // JAVA_INT) -> 8/4`, the idiom the JDK requires — is asserted in
    // `padding_layout_is_byte_aligned_and_pads_a_struct_to_eight`, which
    // is where the old 8/4 numbers now live, attached to the call that
    // actually produces them.
    //
    // The slot-4 "offsets array" the old body checked is gone with the
    // deleted carrier. A member's offset is now derived by the one shared
    // layout-path walk (`p67_layout_path_walk`) rather than cached in a
    // parallel array, so there is no second copy to disagree with it.
    let refused = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemoryLayout",
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        &[Value::Object(Some(members))],
    );
    match refused {
        Err(MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::IllegalArgumentException { ref message },
        ))) => assert!(
            message.contains("Invalid alignment constraint for member layout"),
            "structLayout(JAVA_BYTE, JAVA_INT) must refuse with the JDK's wording, \
                 got {message:?}"
        ),
        other => panic!(
            "structLayout(JAVA_BYTE, JAVA_INT) must throw IllegalArgumentException the way \
                 HotSpot 25.0.3+9 does (Invalid alignment constraint for member layout: i4) \
                 rather than silently padding to 8; got {other:?}"
        ),
    }
}

#[test]
fn union_layout_takes_max_size() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let int_layout = ffm_value_layout(&shared, &mut thread, "JAVA_INT")
        .unwrap()
        .unwrap();
    let long_layout = ffm_value_layout(&shared, &mut thread, "JAVA_LONG")
        .unwrap()
        .unwrap();

    let members = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    let _ = shared.mem.heap.set_array_element(members, 0, int_layout);
    let _ = shared.mem.heap.set_array_element(members, 1, long_layout);

    // union(int, long) → byteSize = max(4, 8) = 8, byteAlignment = 8.
    // Measured on 25.0.3+9-LTS; a union imposes NO alignment constraint,
    // because every member sits at offset 0.
    //
    // This test used to pass against `panama.rs`'s union and read slot 1
    // directly. Both are gone (F16, 2026-08-13), and the implementation
    // that survives had a worse bug that no test could see: its
    // `unionLayout` DISCARDED its members and answered a one-slot carrier
    // holding `Long(0)`, and `UnionLayout` had no `byteSize` registration
    // to read it back with — so `unionLayout(...).byteSize()` raised
    // `AbstractMethodError` in --jdk-only mode. Asserting through
    // `byteSize()`/`byteAlignment()` is what makes that reachable.
    let ul = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemoryLayout",
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;",
        &[Value::Object(Some(members))],
    )
    .unwrap()
    .unwrap();

    if let Value::Object(Some(u)) = ul {
        assert_eq!(
            call_native(
                &shared,
                &mut thread,
                "java/lang/foreign/UnionLayout",
                "byteSize",
                "()J",
                &[Value::Object(Some(u))],
            )
            .unwrap()
            .unwrap(),
            Value::Long(8),
            "union(int, long) is max(4, 8) = 8 bytes"
        );
        assert_eq!(
            call_native(
                &shared,
                &mut thread,
                "java/lang/foreign/UnionLayout",
                "byteAlignment",
                "()J",
                &[Value::Object(Some(u))],
            )
            .unwrap()
            .unwrap(),
            Value::Long(8),
            "union alignment is max(4, 8) = 8"
        );
    } else {
        panic!("MemoryLayout.unionLayout must answer a reference, got {ul:?}");
    }
}

// --- Virtual Thread Stress Tests ---

#[test]
fn virtual_scheduler_concurrent_acquire_release() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    let sched = crate::threading::VirtualThreadScheduler::new(4);
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Spawn 20 threads, each acquiring and releasing a permit
    for _ in 0..20 {
        let s = sched.clone();
        let c = counter.clone();
        handles.push(thread::spawn(move || {
            s.acquire();
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            s.release();
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(counter.load(Ordering::SeqCst), 20);
}

// --- Hidden Class Tests ---

#[test]
fn class_hidden_flag_default_false() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Classes loaded normally should not be hidden
    let cid = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .unwrap();
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid).unwrap();
    assert!(!class.is_hidden());
}

// --- Module Attribute Test ---

#[test]
fn class_module_name_default_none() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let cid = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .unwrap();
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(cid).unwrap();
    // Regular classes don't have Module attribute
    assert!(class.module_name.is_none());
}

// --- Memory Segment Double/Float Get/Set ---

#[test]
fn panama_segment_double_roundtrip() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let arena_ref = match call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        &[],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };
    let seg_ref = match call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        &[Value::Object(Some(arena_ref)), Value::Long(64)],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    let double_layout = match ffm_value_layout(&shared, &mut thread, "JAVA_DOUBLE")
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(r)) => r,
        _ => panic!(""),
    };

    // Write pi at offset 0
    call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "set",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(double_layout)),
            Value::Long(0),
            Value::Double(std::f64::consts::PI),
        ],
    )
    .unwrap();

    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/MemorySegment",
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        &[
            Value::Object(Some(seg_ref)),
            Value::Object(Some(double_layout)),
            Value::Long(0),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Double(std::f64::consts::PI));

    call_native(
        &shared,
        &mut thread,
        "java/lang/foreign/Arena",
        "close",
        "()V",
        &[Value::Object(Some(arena_ref))],
    )
    .unwrap();
}

// --- Ffi module tests ---

#[test]
fn ffi_align_up() {
    assert_eq!(crate::native::ffi::align_up(0, 4), 0);
    assert_eq!(crate::native::ffi::align_up(1, 4), 4);
    assert_eq!(crate::native::ffi::align_up(3, 4), 4);
    assert_eq!(crate::native::ffi::align_up(4, 4), 4);
    assert_eq!(crate::native::ffi::align_up(5, 8), 8);
    assert_eq!(crate::native::ffi::align_up(8, 8), 8);
    assert_eq!(crate::native::ffi::align_up(9, 8), 16);
}

#[test]
fn ffi_layout_alignment() {
    assert_eq!(
        crate::native::ffi::layout_alignment(crate::native::ffi::LAYOUT_BYTE),
        1
    );
    assert_eq!(
        crate::native::ffi::layout_alignment(crate::native::ffi::LAYOUT_SHORT),
        2
    );
    assert_eq!(
        crate::native::ffi::layout_alignment(crate::native::ffi::LAYOUT_INT),
        4
    );
    assert_eq!(
        crate::native::ffi::layout_alignment(crate::native::ffi::LAYOUT_LONG),
        8
    );
    assert_eq!(
        crate::native::ffi::layout_alignment(crate::native::ffi::LAYOUT_DOUBLE),
        8
    );
    assert_eq!(
        crate::native::ffi::layout_alignment(crate::native::ffi::LAYOUT_ADDRESS),
        8
    );
}

// =========================================================================
// Performance Optimization Tests
// =========================================================================

#[test]
#[allow(clippy::vec_init_then_push)]
fn jit_scanner_accepts_tableswitch() {
    // Simple method: iload_0, tableswitch(0,1 → cases), iconst_1, ireturn, iconst_2, ireturn, iconst_0, ireturn
    let mut code = Vec::new();
    code.push(0x1a); // iload_0 (pc=0)
    code.push(0xaa); // tableswitch (pc=1)
    code.push(0x00);
    code.push(0x00); // padding to align pc=1 → next 4-aligned = 4
                     // At offset 4: default=24, low=0, high=1
    code.extend_from_slice(&24i32.to_be_bytes()); // default
    code.extend_from_slice(&0i32.to_be_bytes()); // low
    code.extend_from_slice(&1i32.to_be_bytes()); // high
    code.extend_from_slice(&20i32.to_be_bytes()); // offset[0]
    code.extend_from_slice(&22i32.to_be_bytes()); // offset[1]
    code.push(0x04);
    code.push(0xac); // iconst_1, ireturn
    code.push(0x05);
    code.push(0xac); // iconst_2, ireturn
    code.push(0x03);
    code.push(0xac); // iconst_0, ireturn
    let scan = crate::jit::x64::jit_scan(&code, code.len(), "(I)I");
    assert!(scan.is_some(), "JIT scanner should accept tableswitch");
}

#[test]
#[allow(clippy::vec_init_then_push)]
fn jit_scanner_accepts_lookupswitch() {
    let mut code = Vec::new();
    code.push(0x1a); // iload_0
    code.push(0xab); // lookupswitch (pc=1)
    code.push(0x00);
    code.push(0x00); // padding
    code.extend_from_slice(&20i32.to_be_bytes()); // default
    code.extend_from_slice(&2i32.to_be_bytes()); // npairs
    code.extend_from_slice(&10i32.to_be_bytes());
    code.extend_from_slice(&16i32.to_be_bytes());
    code.extend_from_slice(&20i32.to_be_bytes());
    code.extend_from_slice(&18i32.to_be_bytes());
    code.push(0x04);
    code.push(0xac);
    code.push(0x05);
    code.push(0xac);
    code.push(0x03);
    code.push(0xac);
    let scan = crate::jit::x64::jit_scan(&code, code.len(), "(I)I");
    assert!(scan.is_some(), "JIT scanner should accept lookupswitch");
}

// --- Tests for ClassStoreHierarchy interface subtyping (R1) ---

#[test]
fn class_store_hierarchy_string_is_subtype_of_charsequence() {
    use crate::classloading::vtype::ClassHierarchy;
    // When both String and CharSequence are loaded as stubs, String should
    // be considered a subtype of CharSequence (interface subtyping).
    let config = VmConfig::default();
    let shared = Arc::new(SharedVm::new(config));
    {
        let mut cm = shared.classes.class_manager_write();
        // Loading String causes its interfaces (including CharSequence) to be loaded too.
        cm.load_class("java/lang/String").unwrap();
    }
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let hierarchy = ClassStoreHierarchy { store };

    // Core interface subtype check
    assert!(
        hierarchy.is_subclass("java/lang/String", "java/lang/CharSequence"),
        "String should be a subtype of CharSequence"
    );
    // Reflexive
    assert!(hierarchy.is_subclass("java/lang/String", "java/lang/String"));
    // Always subtype of Object
    assert!(hierarchy.is_subclass("java/lang/String", "java/lang/Object"));
    // StringBuilder also implements CharSequence (if it was loaded)
    if store.find_by_name("java/lang/StringBuilder").is_some() {
        assert!(
            hierarchy.is_subclass("java/lang/StringBuilder", "java/lang/CharSequence"),
            "StringBuilder should be a subtype of CharSequence"
        );
    }
}

#[test]
fn class_store_hierarchy_interface_subtype_not_loaded() {
    use crate::classloading::vtype::ClassHierarchy;
    // When neither String nor CharSequence is in the store, the optimistic
    // fallback should return true (compiler guarantees type safety).
    let config = VmConfig::default();
    let shared = Arc::new(SharedVm::new(config));
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let hierarchy = ClassStoreHierarchy { store };

    // Neither loaded — should be optimistic
    assert!(
        hierarchy.is_subclass("java/lang/String", "java/lang/CharSequence"),
        "Optimistic fallback: neither loaded should return true"
    );
}

#[test]
#[ignore] // requires jdk_streams classes to be extracted to a temp directory
fn verify_renaissance_java_scrabble() {
    // Try to verify the actual JavaScrabble class from the Renaissance benchmark.
    // This confirms R1 interface subtyping works with a real-world class.
    let (cp1, cp2) = if cfg!(windows) {
        (
            r"C:\Users\Admin\AppData\Local\Temp\jdk_streams".to_string(),
            r"C:\Users\Admin\AppData\Local\Temp\scrabble".to_string(),
        )
    } else {
        ("/tmp/jdk_streams".to_string(), "/tmp/scrabble".to_string())
    };
    let config = VmConfig::new().with_classpath(vec![cp1, cp2]);
    let shared = Arc::new(SharedVm::new(config));
    // Load all jdk-streams classes — none should fail to load
    let classes = [
        "org/renaissance/jdk/streams/JavaScrabble",
        "org/renaissance/jdk/streams/Scrabble",
        "org/renaissance/jdk/streams/Mnemonics",
        "org/renaissance/jdk/streams/MnemonicsCoderWithStream",
    ];
    for class_name in &classes {
        let result = shared.classes.class_manager_write().load_class(class_name);
        assert!(
            result.is_ok(),
            "Loading {class_name} should succeed: {result:?}"
        );
    }
}

#[test]
fn verify_class_with_charsequence_method_arg() {
    // Verify that a class using String as a CharSequence argument passes
    // bytecode verification. This tests R1: interface subtyping in verifier.
    // The class has a method calling Pattern.matcher(CharSequence) with a String arg.
    use crate::classloading::bytecode_verifier::verify_bytecode;
    use crate::{Class, ClassId, ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;

    // This tests the invokevirtual arg-type check:
    //   pop_typed(frame, ObjectRef("CharSequence"), hierarchy)
    //   actual = ObjectRef("String")  →  is_subclass("String", "CharSequence") must be true
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    {
        let mut cm = shared.classes.class_manager_write();
        cm.load_class("java/lang/String").unwrap();
    }
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let hierarchy = ClassStoreHierarchy { store };

    // Constant pool:
    // 0: Tombstone
    // 1: MethodReference Pattern.matcher(CharSequence)Matcher  [class=2, nat=3]
    // 2: ClassReference "java/util/regex/Pattern"              [name=4]
    // 3: NameAndType "matcher" / "(LCharSequence;)LMatcher;"  [name=5, desc=6]
    // 4: Utf8 "java/util/regex/Pattern"
    // 5: Utf8 "matcher"
    // 6: Utf8 "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;"
    let cp = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 3,
        },
        ConstantPoolEntry::ClassReference { name_index: 4 },
        ConstantPoolEntry::NameAndType {
            name_index: 5,
            descriptor_index: 6,
        },
        ConstantPoolEntry::Utf8("java/util/regex/Pattern".into()),
        ConstantPoolEntry::Utf8("matcher".into()),
        ConstantPoolEntry::Utf8("(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;".into()),
    ]);

    // Static method: (Ljava/lang/String;Ljava/util/regex/Pattern;)Ljava/util/regex/Matcher;
    // locals: 0=String s, 1=Pattern p
    // Bytecode:
    //   aload_1  (Pattern)       → push Pattern
    //   aload_0  (String)        → push String (as CharSequence arg)
    //   invokevirtual #1         → Pattern.matcher(CharSequence)
    //   areturn                  → return Matcher
    let code = CodeAttribute {
        max_stack: 2,
        max_locals: 2,
        code: cratonvm_reader::ByteView::from_vec(vec![
            0x2B, // aload_1 (Pattern, local 1)
            0x2A, // aload_0 (String, local 0)
            0xB6, 0x00, 0x01, // invokevirtual #1  Pattern.matcher(CharSequence)
            0xB0, // areturn (return Matcher)
        ]),
        exception_table: vec![],
        attributes: vec![],
    };

    let method = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from("testMethod"),
        descriptor: Arc::from(
            "(Ljava/lang/String;Ljava/util/regex/Pattern;)Ljava/util/regex/Matcher;",
        ),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(code)))],
    };

    let class = Class {
        id: ClassId::new(999),
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc("TestClass"),
        source_file: None,
        version: ClassFileVersion::JAVA_11,
        state: ClassState::Verifying,
        initializing_thread: None,
        constant_pool: cp,
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![method],
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

    // Should verify without error — String is assignable to CharSequence
    let result = verify_bytecode(&class, &hierarchy);
    assert!(result.is_ok(), "Verification should pass: {result:?}");
}

// -------------------------------------------------------------------------
// Dynamic Proxy dispatch tests (java.lang.reflect.Proxy)
// -------------------------------------------------------------------------

/// Test that Proxy.newProxyInstance returns a proxy object (a generated
/// `jdk/proxyM/$ProxyN`, or the legacy `Proxy$Instance` shim) and stores
/// the InvocationHandler in field 0, retrievable via
/// `Proxy.getInvocationHandler`.
#[test]
fn proxy_new_instance_stores_handler() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");

    // Create a dummy handler object
    let handler = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    // `interfaces` must be a real (here empty) array, not null. The JDK reads
    // its length first, so `Proxy.newProxyInstance(null, null, h)` throws
    // "Cannot read the array length because \"interfaces\" is null" (verified
    // on JDK 25) -- and that exact message is what this test failed with, since
    // the VM now runs the real `Proxy` bytecode. An empty `Class[]` is valid
    // and builds a proxy implementing no interfaces, which is all this test
    // needs to check that the handler is stored.
    let no_interfaces =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);

    // Call Proxy.newProxyInstance
    let proxy_val = call_native(
            &shared,
            &mut thread,
            "java/lang/reflect/Proxy",
            "newProxyInstance",
            "(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)Ljava/lang/Object;",
            &[
                Value::Object(None),               // ClassLoader null (bootstrap)
                Value::Object(Some(no_interfaces)), // interfaces[] -- empty, not null
                Value::Object(Some(handler)),      // InvocationHandler
            ],
        )
        .unwrap()
        .unwrap();

    let proxy_ref = proxy_val.as_object().unwrap();

    // Verify the proxy's class shape.
    //
    // This used to demand the literal `java/lang/reflect/Proxy$Instance`.
    // That is the LEGACY synthetic shim; the canonical path now emits a
    // real generated class file and names it the way HotSpot's
    // `ProxyBuilder` does — `jdk/proxy<M>/$Proxy<N>`, one `jdk/proxyM`
    // package per defining loader (`proxy_class_name` in
    // native-builtins/src/reflect_annotations.rs; the pre-JDK-9 name was
    // `com/sun/proxy/$ProxyN`). Asserting the shim name would pin the VM
    // to the degraded fallback, so accept either shape: the generated
    // `$ProxyN` (canonical, `CRATONVM_REAL_PROXY` default-on) or the shim
    // (`CRATONVM_REAL_PROXY=0`, or a generation failure in non-strict
    // mode).
    let proxy_class_id = shared.mem.heap.class_id_of(proxy_ref);
    let proxy_class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(proxy_class_id)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let generated_name = (proxy_class_name.starts_with("jdk/proxy")
        || proxy_class_name.starts_with("com/sun/proxy"))
        && proxy_class_name.contains("$Proxy");
    assert!(
        generated_name || &*proxy_class_name == "java/lang/reflect/Proxy$Instance",
        "newProxyInstance should return a generated jdk/proxyM/$ProxyN \
             (or the Proxy$Instance shim), got {proxy_class_name}"
    );

    // What this test is really for: the InvocationHandler must be STORED
    // and RETRIEVABLE. Slot 0 is the handler in both layouts — the real
    // `java.lang.reflect.Proxy` super declares exactly one field, `h`
    // (the ONE synthetic class in this VM given a NAMED field), and the
    // synthetic `Proxy$Instance` super puts the handler at slot 0 too.
    let stored_handler = shared.mem.heap.get_field(proxy_ref, 0);
    assert_eq!(
        stored_handler,
        Value::Object(Some(handler)),
        "InvocationHandler should be stored in field 0"
    );

    // ...and it must come back out through the public API, which also
    // proves the proxy's superclass chain is recognised as a proxy
    // (`Proxy.getInvocationHandler` throws IllegalArgumentException for a
    // non-proxy receiver).
    let via_api = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Proxy",
        "getInvocationHandler",
        "(Ljava/lang/Object;)Ljava/lang/reflect/InvocationHandler;",
        &[Value::Object(Some(proxy_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        via_api,
        Value::Object(Some(handler)),
        "Proxy.getInvocationHandler should return the handler that was stored"
    );
}

/// Verify proxy_count_params parses JVM descriptors correctly.
#[test]
fn proxy_count_params_basic() {
    assert_eq!(proxy_count_params("()V"), 0);
    assert_eq!(proxy_count_params("(I)V"), 1);
    assert_eq!(proxy_count_params("(ILjava/lang/String;)V"), 2);
    assert_eq!(proxy_count_params("(IJD)Z"), 3);
    assert_eq!(proxy_count_params("([I)V"), 1); // array counts as 1
}

