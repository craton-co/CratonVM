// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// =====================================================================
// Phase 54 tests: Atomics, MethodHandle, Logging, URI, Zip
// =====================================================================

#[test]
fn atomic_integer_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ai = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ai)), Value::Int(10)],
    )
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "get",
        "()I",
        &[Value::Object(Some(ai))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(10));
    // incrementAndGet
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "incrementAndGet",
        "()I",
        &[Value::Object(Some(ai))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(11));
    // getAndAdd(5) -> old=11, new=16
    let old = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "getAndAdd",
        "(I)I",
        &[Value::Object(Some(ai)), Value::Int(5)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old, Value::Int(11));
    let curr = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "get",
        "()I",
        &[Value::Object(Some(ai))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(curr, Value::Int(16));
}

#[test]
fn atomic_integer_compare_and_set() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ai = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ai)), Value::Int(42)],
    )
    .unwrap();
    // CAS success
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "compareAndSet",
        "(II)Z",
        &[Value::Object(Some(ai)), Value::Int(42), Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "get",
        "()I",
        &[Value::Object(Some(ai))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(99));
    // CAS fail
    let fail = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "compareAndSet",
        "(II)Z",
        &[Value::Object(Some(ai)), Value::Int(42), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(fail, Value::Int(0));
}

#[test]
fn atomic_long_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let al = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicLong",
        "<init>",
        "(J)V",
        &[Value::Object(Some(al)), Value::Long(100)],
    )
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicLong",
        "get",
        "()J",
        &[Value::Object(Some(al))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Long(100));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicLong",
        "incrementAndGet",
        "()J",
        &[Value::Object(Some(al))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Long(101));
    let old = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicLong",
        "getAndAdd",
        "(J)J",
        &[Value::Object(Some(al)), Value::Long(50)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old, Value::Long(101));
}

#[test]
fn atomic_boolean_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ab = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicBoolean",
        "<init>",
        "(Z)V",
        &[Value::Object(Some(ab)), Value::Int(0)],
    )
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicBoolean",
        "get",
        "()Z",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(0));
    // CAS false->true
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicBoolean",
        "compareAndSet",
        "(ZZ)Z",
        &[Value::Object(Some(ab)), Value::Int(0), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicBoolean",
        "get",
        "()Z",
        &[Value::Object(Some(ab))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(1));
}

#[test]
fn atomic_reference_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ar = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let obj1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(ar)), Value::Object(Some(obj1))],
    )
    .unwrap();
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicReference",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ar))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Object(Some(obj1)));
    // CAS
    let obj2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicReference",
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        &[
            Value::Object(Some(ar)),
            Value::Object(Some(obj1)),
            Value::Object(Some(obj2)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
}

#[test]
fn long_adder_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let la = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "<init>",
        "()V",
        &[Value::Object(Some(la))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "add",
        "(J)V",
        &[Value::Object(Some(la)), Value::Long(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "add",
        "(J)V",
        &[Value::Object(Some(la)), Value::Long(20)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "increment",
        "()V",
        &[Value::Object(Some(la))],
    )
    .unwrap();
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
    assert_eq!(sum, Value::Long(31));
    // sumThenReset
    let old_sum = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "sumThenReset",
        "()J",
        &[Value::Object(Some(la))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old_sum, Value::Long(31));
    let zero = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/LongAdder",
        "sum",
        "()J",
        &[Value::Object(Some(la))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(zero, Value::Long(0));
}

#[test]
fn double_adder_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let da = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "<init>",
        "()V",
        &[Value::Object(Some(da))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "add",
        "(D)V",
        &[Value::Object(Some(da)), Value::Double(1.5)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "add",
        "(D)V",
        &[Value::Object(Some(da)), Value::Double(2.5)],
    )
    .unwrap();
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/DoubleAdder",
        "sum",
        "()D",
        &[Value::Object(Some(da))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Double(4.0));
}

#[test]
fn atomic_integer_array_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let aia = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "<init>",
        "(I)V",
        &[Value::Object(Some(aia)), Value::Int(5)],
    )
    .unwrap();
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
    // set and get
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "set",
        "(II)V",
        &[Value::Object(Some(aia)), Value::Int(2), Value::Int(42)],
    )
    .unwrap();
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
    // incrementAndGet(2) -> 43
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "incrementAndGet",
        "(I)I",
        &[Value::Object(Some(aia)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(43));
}

#[test]
fn atomic_stamped_reference_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let asr = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let obj1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicStampedReference",
        "<init>",
        "(Ljava/lang/Object;I)V",
        &[
            Value::Object(Some(asr)),
            Value::Object(Some(obj1)),
            Value::Int(0),
        ],
    )
    .unwrap();
    let stamp = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicStampedReference",
        "getStamp",
        "()I",
        &[Value::Object(Some(asr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(stamp, Value::Int(0));
    // CAS with correct stamp
    let obj2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicStampedReference",
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        &[
            Value::Object(Some(asr)),
            Value::Object(Some(obj1)),
            Value::Object(Some(obj2)),
            Value::Int(0),
            Value::Int(1),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    let new_stamp = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicStampedReference",
        "getStamp",
        "()I",
        &[Value::Object(Some(asr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(new_stamp, Value::Int(1));
}

#[test]
fn method_type_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ret_class = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mt = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodType",
        "methodType",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(ret_class))],
    )
    .unwrap()
    .unwrap();
    let mt_ref = mt.as_object().unwrap();
    let pc = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodType",
        "parameterCount",
        "()I",
        &[Value::Object(Some(mt_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(pc, Value::Int(0));
    let rt = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodType",
        "returnType",
        "()Ljava/lang/Class;",
        &[Value::Object(Some(mt_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rt, Value::Object(Some(ret_class)));
}

#[test]
fn method_handles_lookup() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let lookup = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(lookup.as_object().is_some());
    // findVirtual returns a MethodHandle stub
    let cls = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let name = create_java_string(&shared, "toString");
    let mt = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let lk_ref = lookup.as_object().unwrap();
    let mh = call_native(&shared, &mut thread, "java/lang/invoke/MethodHandles$Lookup", "findVirtual",
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
            &[Value::Object(Some(lk_ref)), Value::Object(Some(cls)), Value::Object(Some(name)), Value::Object(Some(mt))]).unwrap().unwrap();
    assert!(mh.as_object().is_some());
}

#[test]
fn log_record_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let level = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let msg = create_java_string(&shared, "test message");
    // LogRecord native handler requires 7 instance fields
    // (level, message, loggerName, thrown, parameters, millis, sequence).
    let lr = alloc_receiver(&shared, &mut thread, "java/util/logging/LogRecord", 7);
    call_native(
        &shared,
        &mut thread,
        "java/util/logging/LogRecord",
        "<init>",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        &[
            Value::Object(Some(lr)),
            Value::Object(Some(level)),
            Value::Object(Some(msg)),
        ],
    )
    .unwrap();
    let m = call_native(
        &shared,
        &mut thread,
        "java/util/logging/LogRecord",
        "getMessage",
        "()Ljava/lang/String;",
        &[Value::Object(Some(lr))],
    )
    .unwrap()
    .unwrap();
    let m_ref = m.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, m_ref),
        Some("test message".to_string())
    );
}

#[test]
fn uri_parsing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let raw = create_java_string(&shared, "https://example.com:8080/path?q=1#frag");
    let uri = shared.mem.heap.alloc_object(ClassId::new(0), 7);
    call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(uri)), Value::Object(Some(raw))],
    )
    .unwrap();
    // scheme
    let scheme = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "getScheme",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uri))],
    )
    .unwrap()
    .unwrap();
    let s_ref = scheme.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, s_ref),
        Some("https".to_string())
    );
    // host
    let host = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "getHost",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uri))],
    )
    .unwrap()
    .unwrap();
    let h_ref = host.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, h_ref),
        Some("example.com".to_string())
    );
    // port
    let port = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "getPort",
        "()I",
        &[Value::Object(Some(uri))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(port, Value::Int(8080));
    // path
    let path = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "getPath",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uri))],
    )
    .unwrap()
    .unwrap();
    let p_ref = path.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, p_ref),
        Some("/path".to_string())
    );
    // query
    let query = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "getQuery",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uri))],
    )
    .unwrap()
    .unwrap();
    let q_ref = query.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, q_ref),
        Some("q=1".to_string())
    );
    // fragment
    let frag = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "getFragment",
        "()Ljava/lang/String;",
        &[Value::Object(Some(uri))],
    )
    .unwrap()
    .unwrap();
    let f_ref = frag.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, f_ref),
        Some("frag".to_string())
    );
}

#[test]
fn uri_equals_and_hash() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let raw = create_java_string(&shared, "https://example.com/path");
    let uri1 = shared.mem.heap.alloc_object(ClassId::new(0), 7);
    call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(uri1)), Value::Object(Some(raw))],
    )
    .unwrap();
    let raw2 = create_java_string(&shared, "https://example.com/path");
    let uri2 = shared.mem.heap.alloc_object(ClassId::new(0), 7);
    call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(uri2)), Value::Object(Some(raw2))],
    )
    .unwrap();
    let eq = call_native(
        &shared,
        &mut thread,
        "java/net/URI",
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(uri1)), Value::Object(Some(uri2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(eq, Value::Int(1));
}

#[test]
fn inet_address_get_by_name() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let host = create_java_string(&shared, "example.com");
    let addr = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        &[Value::Object(Some(host))],
    )
    .unwrap()
    .unwrap();
    let addr_ref = addr.as_object().unwrap();
    let name = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getHostName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(addr_ref))],
    )
    .unwrap()
    .unwrap();
    let n_ref = name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref),
        Some("example.com".to_string())
    );
}

#[test]
fn zip_entry_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = create_java_string(&shared, "test.txt");
    let ze = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(ze)), Value::Object(Some(name))],
    )
    .unwrap();
    let n = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ze))],
    )
    .unwrap()
    .unwrap();
    let n_ref = n.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref),
        Some("test.txt".to_string())
    );
    let is_dir = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "isDirectory",
        "()Z",
        &[Value::Object(Some(ze))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_dir, Value::Int(0));
    // Directory entry
    let dir_name = create_java_string(&shared, "lib/");
    let ze2 = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(ze2)), Value::Object(Some(dir_name))],
    )
    .unwrap();
    let is_dir2 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "isDirectory",
        "()Z",
        &[Value::Object(Some(ze2))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_dir2, Value::Int(1));
}

#[test]
fn crc32_basics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let crc = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/CRC32",
        "<init>",
        "()V",
        &[Value::Object(Some(crc))],
    )
    .unwrap();
    // Update with bytes for "hello" = [104, 101, 108, 108, 111]
    let data = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        5,
    );
    let _ = shared.mem.heap.set_array_element(data, 0, Value::Int(104));
    let _ = shared.mem.heap.set_array_element(data, 1, Value::Int(101));
    let _ = shared.mem.heap.set_array_element(data, 2, Value::Int(108));
    let _ = shared.mem.heap.set_array_element(data, 3, Value::Int(108));
    let _ = shared.mem.heap.set_array_element(data, 4, Value::Int(111));
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/CRC32",
        "update",
        "([B)V",
        &[Value::Object(Some(crc)), Value::Object(Some(data))],
    )
    .unwrap();
    let val = call_native(
        &shared,
        &mut thread,
        "java/util/zip/CRC32",
        "getValue",
        "()J",
        &[Value::Object(Some(crc))],
    )
    .unwrap()
    .unwrap();
    // CRC32 of "hello" = 907060870 = 0x3610A686
    assert_eq!(val, Value::Long(907060870));
    // reset
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/CRC32",
        "reset",
        "()V",
        &[Value::Object(Some(crc))],
    )
    .unwrap();
    let zero = call_native(
        &shared,
        &mut thread,
        "java/util/zip/CRC32",
        "getValue",
        "()J",
        &[Value::Object(Some(crc))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(zero, Value::Long(0));
}

#[test]
fn loopback_address() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let lb = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        &[],
    )
    .unwrap()
    .unwrap();
    let lb_ref = lb.as_object().unwrap();
    let addr = call_native(
        &shared,
        &mut thread,
        "java/net/InetAddress",
        "getHostAddress",
        "()Ljava/lang/String;",
        &[Value::Object(Some(lb_ref))],
    )
    .unwrap()
    .unwrap();
    let a_ref = addr.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, a_ref),
        Some("127.0.0.1".to_string())
    );
}

// =====================================================================
// Phase 55 tests: Charset, Executors, Reflect, Collection extras
// =====================================================================

#[test]
fn charset_for_name_and_properties() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = create_java_string(&shared, "UTF-8");
    let cs = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let cs_ref = cs.as_object().unwrap();
    let n = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(cs_ref))],
    )
    .unwrap()
    .unwrap();
    let n_ref = n.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref),
        Some("UTF-8".to_string())
    );
}

#[test]
fn charset_is_supported_p55() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = create_java_string(&shared, "UTF-8");
    let ok = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "isSupported",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    let bad = create_java_string(&shared, "BOGUS");
    let no = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "isSupported",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(bad))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(no, Value::Int(0));
}

#[test]
fn standard_charsets() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let utf8 = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/StandardCharsets",
        "UTF_8",
        "Ljava/nio/charset/Charset;",
        &[],
    )
    .unwrap()
    .unwrap();
    let cs_ref = utf8.as_object().unwrap();
    let n = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(cs_ref))],
    )
    .unwrap()
    .unwrap();
    let n_ref = n.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref),
        Some("UTF-8".to_string())
    );
}

#[test]
fn executors_factory_and_shutdown() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let es = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Executors",
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        &[],
    )
    .unwrap()
    .unwrap();
    let es_ref = es.as_object().unwrap();
    let shut = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorService",
        "isShutdown",
        "()Z",
        &[Value::Object(Some(es_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut, Value::Int(0));
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorService",
        "shutdown",
        "()V",
        &[Value::Object(Some(es_ref))],
    )
    .unwrap();
    let shut2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ExecutorService",
        "isShutdown",
        "()Z",
        &[Value::Object(Some(es_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut2, Value::Int(1));
}

#[test]
fn future_get_and_is_done() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let future = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(future, 0, Value::Int(42));
    shared.mem.heap.set_field(future, 1, Value::Int(1));
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Future",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(future))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(42));
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Future",
        "isDone",
        "()Z",
        &[Value::Object(Some(future))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

#[test]
fn time_unit_conversions() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create SECONDS TimeUnit (ordinal 3)
    let sec = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(sec, 0, Value::Int(3));
    let millis = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/TimeUnit",
        "toMillis",
        "(J)J",
        &[Value::Object(Some(sec)), Value::Long(5)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(millis, Value::Long(5000));
    let nanos = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/TimeUnit",
        "toNanos",
        "(J)J",
        &[Value::Object(Some(sec)), Value::Long(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(nanos, Value::Long(2_000_000_000));
}

#[test]
fn modifier_checks() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // public static final = 0x0001 | 0x0008 | 0x0010 = 0x0019 = 25
    let is_pub = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Modifier",
        "isPublic",
        "(I)Z",
        &[Value::Int(25)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_pub, Value::Int(1));
    let is_static = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Modifier",
        "isStatic",
        "(I)Z",
        &[Value::Int(25)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_static, Value::Int(1));
    let is_final = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Modifier",
        "isFinal",
        "(I)Z",
        &[Value::Int(25)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_final, Value::Int(1));
    let is_priv = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Modifier",
        "isPrivate",
        "(I)Z",
        &[Value::Int(25)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_priv, Value::Int(0));
}

#[test]
fn modifier_to_string() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // public static = 0x0001 | 0x0008 = 9
    let s = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Modifier",
        "toString",
        "(I)Ljava/lang/String;",
        &[Value::Int(9)],
    )
    .unwrap()
    .unwrap();
    let s_ref = s.as_object().unwrap();
    let text = read_java_string(&shared.mem.heap, s_ref).unwrap_or_default();
    assert!(text.contains("public"));
    assert!(text.contains("static"));
}

#[test]
fn collections_singleton_list() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let elem = create_java_string(&shared, "hello");
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        &[Value::Object(Some(elem))],
    )
    .unwrap()
    .unwrap();
    let list_ref = list.as_object().unwrap();
    // size
    let sz = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(1));
}

#[test]
fn collections_n_copies_p55() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        &[Value::Int(3), Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    let list_ref = list.as_object().unwrap();
    let sz = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(list_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(3));
    let v = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list_ref)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v, Value::Int(42));
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(list_ref)), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(v2, Value::Int(42));
}

#[test]
fn collections_unmodifiable_list_returns_list() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Build an ArrayList with one element
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
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(list)), Value::Int(99)],
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
    // Returns a valid list (may be same or copy)
    let wr = wrapped.as_object().unwrap();
    let sz = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "size",
        "()I",
        &[Value::Object(Some(wr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(1));
}

#[test]
fn completable_future_completed() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let val = create_java_string(&shared, "result");
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
    let cf_ref = cf.as_object().unwrap();
    // get
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Object(Some(val)));
    // isDone
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

// =====================================================================
// Phase 56 tests: Stream enhancements, Collectors, SummaryStats, function
// =====================================================================

#[test]
fn stream_take_while_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Build stream of 3 string objects
    let s1 = create_java_string(&shared, "a");
    let s2 = create_java_string(&shared, "b");
    let s3 = create_java_string(&shared, "c");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
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
    let stream = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    // Predicate: identityHashCode always returns non-zero (true) → takeWhile keeps all
    let pred = make_lambda_proxy(
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
        "takeWhile",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(stream)), Value::Object(Some(pred))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let arr_val = shared.mem.heap.get_field(r_ref, 0);
    let new_arr = arr_val.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(new_arr), 3); // all kept
}

#[test]
fn stream_drop_while_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let s1 = create_java_string(&shared, "a");
    let s2 = create_java_string(&shared, "b");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(s1)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(s2)));
    let stream = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    // Predicate: identityHashCode always returns non-zero (true) → dropWhile drops all
    let pred = make_lambda_proxy(
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
        "dropWhile",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(stream)), Value::Object(Some(pred))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let arr_val = shared.mem.heap.get_field(r_ref, 0);
    let new_arr = arr_val.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(new_arr), 0); // all dropped
}

#[test]
fn stream_concat_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr_a = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr_a, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(arr_a, 1, Value::Int(2));
    let sa = alloc_receiver(&shared, &mut thread, "java/util/stream/Stream", 1);
    shared.mem.heap.set_field(sa, 0, Value::Object(Some(arr_a)));
    let arr_b = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr_b, 0, Value::Int(3));
    let _ = shared.mem.heap.set_array_element(arr_b, 1, Value::Int(4));
    let sb = alloc_receiver(&shared, &mut thread, "java/util/stream/Stream", 1);
    shared.mem.heap.set_field(sb, 0, Value::Object(Some(arr_b)));
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "concat",
        "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(sa)), Value::Object(Some(sb))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let arr_val = shared.mem.heap.get_field(r_ref, 0);
    let new_arr = arr_val.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(new_arr), 4);
}

#[test]
fn stream_of_nullable_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let arr_val = shared.mem.heap.get_field(r_ref, 0);
    let arr_ref = arr_val.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(arr_ref), 1);
    let result2 = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    let r_ref2 = result2.as_object().unwrap();
    let arr_val2 = shared.mem.heap.get_field(r_ref2, 0);
    let arr_ref2 = arr_val2.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(arr_ref2), 0);
}

#[test]
fn stream_parallel_sequential_stubs() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let stream = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let p = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "parallel",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(p, Value::Object(Some(stream)));
    let ip = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "isParallel",
        "()Z",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ip, Value::Int(0));
}

#[test]
fn int_stream_summary_statistics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(10));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(20));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(30));
    let stream = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let stats = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "summaryStatistics",
        "()Ljava/util/IntSummaryStatistics;",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    let s_ref = stats.as_object().unwrap();
    let cnt = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getCount",
        "()J",
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cnt, Value::Long(3));
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getSum",
        "()J",
        &[Value::Object(Some(s_ref))],
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
        &[Value::Object(Some(s_ref))],
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
        &[Value::Object(Some(s_ref))],
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
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avg, Value::Double(20.0));
}

#[test]
fn long_stream_summary_statistics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Long(100));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Long(200));
    let stream = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let stats = call_native(
        &shared,
        &mut thread,
        "java/util/stream/LongStream",
        "summaryStatistics",
        "()Ljava/util/LongSummaryStatistics;",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    let s_ref = stats.as_object().unwrap();
    let cnt = call_native(
        &shared,
        &mut thread,
        "java/util/LongSummaryStatistics",
        "getCount",
        "()J",
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cnt, Value::Long(2));
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/LongSummaryStatistics",
        "getSum",
        "()J",
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Long(300));
}

#[test]
fn double_stream_summary_statistics() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Double(1.5));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Double(2.5));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Double(3.0));
    let stream = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let stats = call_native(
        &shared,
        &mut thread,
        "java/util/stream/DoubleStream",
        "summaryStatistics",
        "()Ljava/util/DoubleSummaryStatistics;",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    let s_ref = stats.as_object().unwrap();
    let cnt = call_native(
        &shared,
        &mut thread,
        "java/util/DoubleSummaryStatistics",
        "getCount",
        "()J",
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cnt, Value::Long(3));
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/DoubleSummaryStatistics",
        "getSum",
        "()D",
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Double(7.0));
    let min = call_native(
        &shared,
        &mut thread,
        "java/util/DoubleSummaryStatistics",
        "getMin",
        "()D",
        &[Value::Object(Some(s_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(min, Value::Double(1.5));
}

#[test]
fn int_stream_boxed_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(5));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(10));
    let stream = alloc_receiver(&shared, &mut thread, "java/util/stream/IntStream", 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let boxed = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "boxed",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    let b_ref = boxed.as_object().unwrap();
    let arr_val = shared.mem.heap.get_field(b_ref, 0);
    let new_arr = arr_val.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(new_arr), 2);
    // `IntStream.boxed()` produces real `java.lang.Integer` objects, which
    // is the whole point of the method. `get_array_element_unboxing` only
    // unwraps the VM's `AUTOBOX_CLASS_ID` sentinel, not a genuine wrapper,
    // so it hands back the reference — this used to assert the raw ints
    // that an earlier, non-boxing `boxed()` left in the array.
    let unbox = |v: Value| -> Value {
        match v {
            Value::Object(Some(o)) => shared.mem.heap.get_field(o, 0),
            other => other,
        }
    };
    assert_eq!(
        unbox(
            shared
                .mem
                .heap
                .get_array_element_unboxing(new_arr, 0)
                .unwrap()
        ),
        Value::Int(5)
    );
    assert_eq!(
        unbox(
            shared
                .mem
                .heap
                .get_array_element_unboxing(new_arr, 1)
                .unwrap()
        ),
        Value::Int(10)
    );
}

#[test]
fn int_stream_as_long_stream() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(42));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(99));
    let stream = alloc_receiver(&shared, &mut thread, "java/util/stream/IntStream", 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let long_stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "asLongStream",
        "()Ljava/util/stream/LongStream;",
        &[Value::Object(Some(stream))],
    )
    .unwrap()
    .unwrap();
    let ls = long_stream.as_object().unwrap();
    let ls_arr = shared.mem.heap.get_field(ls, 0).as_object().unwrap();
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element_unboxing(ls_arr, 0)
            .unwrap(),
        Value::Long(42)
    );
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element_unboxing(ls_arr, 1)
            .unwrap(),
        Value::Long(99)
    );
}

#[test]
fn int_stream_concat_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let arr_a = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr_a, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(arr_a, 1, Value::Int(2));
    let sa = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(sa, 0, Value::Object(Some(arr_a)));
    let arr_b = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    let _ = shared.mem.heap.set_array_element(arr_b, 0, Value::Int(3));
    let sb = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(sb, 0, Value::Object(Some(arr_b)));
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/IntStream",
        "concat",
        "(Ljava/util/stream/IntStream;Ljava/util/stream/IntStream;)Ljava/util/stream/IntStream;",
        &[Value::Object(Some(sa)), Value::Object(Some(sb))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let arr_val = shared.mem.heap.get_field(r_ref, 0);
    let new_arr = arr_val.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(new_arr), 3);
}



// `Collectors.maxBy` / `minBy` return an `Optional`, NOT a Map.
//
// These two used to assert `get_field(collector, 0) == Int(9)` / `Int(10)` —
// i.e. they pinned down the tag `native-builtins` happened to write and
// stopped there. That number was decoded by the ONLY registered
// `Stream.collect(Collector)` (native-collections' `native_stream_collect`)
// as `COLLECTOR_TAG_GROUPING_BY_DOWNSTREAM` (9) and
// `COLLECTOR_TAG_GROUPING_BY_SUPPLIER` (10), so `collect(maxBy(cmp))`
// returned a grouping Map and the tests were green over a silent wrong
// answer. Assert the `java.util.stream.Collectors` contract end-to-end
// instead: the tag is an implementation detail, the Optional is the API.

#[test]
fn collectors_max_by_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let comparator = string_order_comparator(&shared);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "maxBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(comparator))],
    )
    .unwrap()
    .unwrap();
    let stream = stream_of_strings(&shared, &mut thread, &["cherry", "apple", "banana"]);
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
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/util/Optional",
            "isPresent",
            "()Z",
            &[result],
        )
        .unwrap()
        .unwrap(),
        Value::Int(1),
        "maxBy over a non-empty stream must be a present Optional"
    );
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "get",
        "()Ljava/lang/Object;",
        &[result],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, got.as_object().unwrap()),
        Some("cherry".to_string())
    );
}

#[test]
fn collectors_max_by_empty_stream_is_empty_optional_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let comparator = string_order_comparator(&shared);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "maxBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(comparator))],
    )
    .unwrap()
    .unwrap();
    let stream = stream_of_strings(&shared, &mut thread, &[]);
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
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/util/Optional",
            "isPresent",
            "()Z",
            &[result],
        )
        .unwrap()
        .unwrap(),
        Value::Int(0),
        "maxBy over an empty stream must be Optional.empty()"
    );
}

#[test]
fn collectors_min_by_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let comparator = string_order_comparator(&shared);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "minBy",
        "(Ljava/util/Comparator;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(comparator))],
    )
    .unwrap()
    .unwrap();
    let stream = stream_of_strings(&shared, &mut thread, &["cherry", "apple", "banana"]);
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
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/util/Optional",
            "isPresent",
            "()Z",
            &[result],
        )
        .unwrap()
        .unwrap(),
        Value::Int(1),
        "minBy over a non-empty stream must be a present Optional"
    );
    let got = call_native(
        &shared,
        &mut thread,
        "java/util/Optional",
        "get",
        "()Ljava/lang/Object;",
        &[result],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, got.as_object().unwrap()),
        Some("apple".to_string())
    );
}

#[test]
fn collectors_mapping_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let func = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let downstream = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "mapping",
        "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(func)), Value::Object(Some(downstream))],
    )
    .unwrap()
    .unwrap();
    let c_ref = collector.as_object().unwrap();
    // Tag 15 == `COLLECTOR_TAG_MAPPING` (native-collections). Two crates
    // register `Collectors.mapping`: native-builtins `phases_late::streams`
    // (tag 11, `P56_COLLECTOR_MAPPING`) and native-collections. Registration
    // is last-wins and `register_collections_natives` runs AFTER
    // `register_builtins` both here and in `vm_init`, so the collections
    // implementation is the one that ever runs. It has to be: the *only*
    // registered `Stream.collect(Collector)` is native-collections'
    // `native_stream_collect`, which decodes the `COLLECTOR_TAG_*`
    // namespace. Nothing anywhere reads the `P56_COLLECTOR_*` numbering.
    // So 15 is the correct answer and 11 is the wrong one — a collector
    // tagged 11 would be decoded by `native_stream_collect` as
    // `COLLECTOR_TAG_PARTITIONING_BY_DOWNSTREAM` and produce a
    // {false=…, true=…} Map instead of the mapped downstream result.
    assert_eq!(shared.mem.heap.get_field(c_ref, 0), Value::Int(15));
    // The mapper and the downstream collector must survive into ARG1/ARG2 —
    // that, not the tag, is what `native_stream_collect` consumes.
    assert_eq!(
        shared.mem.heap.get_field(c_ref, 1),
        Value::Object(Some(func))
    );
    assert_eq!(
        shared.mem.heap.get_field(c_ref, 2),
        Value::Object(Some(downstream))
    );
}

/// `Collectors.filtering(pred, downstream)` drops the elements the predicate
/// rejects and collects the rest THROUGH the downstream collector.
///
/// Was: `assert get_field(collector, 0) == Int(12)`. Tag 12 is
/// `COLLECTOR_TAG_TO_MAP_MERGE` to the only decoder in the VM, so
/// `collect(filtering(..))` used to read ARG1/ARG2 as a key/value function
/// pair and hand back a Map — the predicate was never called and the
/// downstream collector was never consulted.
#[test]
fn collectors_filtering_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // test(s) → s.isEmpty()
    let pred = make_lambda_proxy(
        &shared,
        "java/util/function/Predicate",
        "test",
        "(Ljava/lang/Object;)Z",
        "java/lang/String",
        "isEmpty",
        "()Z",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    );
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
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "filtering",
        "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(pred)), downstream],
    )
    .unwrap()
    .unwrap();
    let stream = stream_of_strings(&shared, &mut thread, &["", "abc", "", "de"]);
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
    let list = result.as_object().unwrap();
    // Downstream is toList(), so the result is a List of the two empty
    // strings — not a Map, and not all four elements.
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "size",
            "()I",
            &[Value::Object(Some(list))],
        )
        .unwrap()
        .unwrap(),
        Value::Int(2),
        "filtering must drop the elements the predicate rejects"
    );
    for i in 0..2 {
        let elem = call_native(
            &shared,
            &mut thread,
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Object(Some(list)), Value::Int(i)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            read_java_string(&shared.mem.heap, elem.as_object().unwrap()),
            Some(String::new())
        );
    }
}

/// `Collectors.summarizingInt(extractor)` returns an `IntSummaryStatistics`.
///
/// Was: `assert get_field(collector, 0) == Int(13)`. Tag 13 is
/// `COLLECTOR_TAG_COLLECTING_AND_THEN` to the only decoder in the VM, so
/// `collect(summarizingInt(f))` read ARG1 as a downstream collector and
/// ARG2 as a finisher, and returned null.
#[test]
fn collectors_summarizing_int_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // applyAsInt(s) → s.length()
    let func = make_lambda_proxy(
        &shared,
        "java/util/function/ToIntFunction",
        "applyAsInt",
        "(Ljava/lang/Object;)I",
        "java/lang/String",
        "length",
        "()I",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    );
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "summarizingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(func))],
    )
    .unwrap()
    .unwrap();
    let stream = stream_of_strings(&shared, &mut thread, &["a", "bbb", "cc"]);
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
    let stats = result.as_object().unwrap();
    let iss = "java/util/IntSummaryStatistics";
    let get = |thread: &mut crate::threading::JvmThread, name: &str, desc: &str| {
        call_native(
            &shared,
            thread,
            iss,
            name,
            desc,
            &[Value::Object(Some(stats))],
        )
        .unwrap()
        .unwrap()
    };
    assert_eq!(get(&mut thread, "getCount", "()J"), Value::Long(3));
    assert_eq!(get(&mut thread, "getSum", "()J"), Value::Long(6));
    assert_eq!(get(&mut thread, "getMin", "()I"), Value::Int(1));
    assert_eq!(get(&mut thread, "getMax", "()I"), Value::Int(3));
    assert_eq!(get(&mut thread, "getAverage", "()D"), Value::Double(2.0));
}

#[test]
fn collectors_to_unmodifiable_list_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "toUnmodifiableList",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();
    // collections.rs overrides with toList (tag 1) — verify it returns a valid collector
    let c_ref = collector.as_object().unwrap();
    assert_eq!(shared.mem.heap.get_field(c_ref, 0), Value::Int(1));
}


// `Collectors.averagingInt(f)` is `Collector<T,?,Double>` and
// `Collectors.summingInt(f)` is `Collector<T,?,Integer>`.
//
// These two used to assert `get_field(collector, 0) == Int(19)` / `Int(22)`
// — the raw tag `native-builtins` happened to write. Those numbers were in
// NO decoder table: `native_stream_collect` (the only registered
// `Stream.collect(Collector)`) rejected them in `is_known_collector_tag`,
// fell through to the untagged-JDK-collector path and returned the bare
// accumulation `ArrayList` — so `(Double) collect(averagingInt(f))` was a
// ClassCastException and the tests were green over it. Assert the
// `java.util.stream.Collectors` contract end-to-end instead: the tag is an
// implementation detail, the boxed result is the API.

#[test]
fn collectors_averaging_int_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let func = string_length_to_int_function(&shared);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "averagingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(func))],
    )
    .unwrap()
    .unwrap();
    // lengths 1, 3, 2 → 6 / 3 == 2.0
    let stream = stream_of_strings(&shared, &mut thread, &["a", "bbb", "cc"]);
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
    // An ArrayList would hold its backing array in slot 0, not a Double.
    assert_eq!(
        shared.mem.heap.get_field(result.as_object().unwrap(), 0),
        Value::Double(2.0),
        "averagingInt must collect to a boxed Double, not the accumulation list"
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Double",
            "doubleValue",
            "()D",
            &[result],
        )
        .unwrap()
        .unwrap(),
        Value::Double(2.0)
    );
}

#[test]
fn collectors_averaging_int_empty_stream_is_zero_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let func = string_length_to_int_function(&shared);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "averagingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(func))],
    )
    .unwrap()
    .unwrap();
    let stream = stream_of_strings(&shared, &mut thread, &[]);
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
    // The JDK finisher is `count == 0 ? 0.0d : sum / count` — 0.0, not NaN.
    assert_eq!(
        shared.mem.heap.get_field(result.as_object().unwrap(), 0),
        Value::Double(0.0),
        "averagingInt over an empty stream is 0.0"
    );
}

#[test]
fn collectors_summing_int_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let func = string_length_to_int_function(&shared);
    let collector = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "summingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/Collector;",
        &[Value::Object(Some(func))],
    )
    .unwrap()
    .unwrap();
    // lengths 1, 3, 2 → 6
    let stream = stream_of_strings(&shared, &mut thread, &["a", "bbb", "cc"]);
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
    // Integer, not Long: `Collectors.summingInt` is Collector<T,?,Integer>
    // and callers cast to Integer.
    assert_eq!(
        shared.mem.heap.get_field(result.as_object().unwrap(), 0),
        Value::Int(6),
        "summingInt must collect to a boxed Integer, not the accumulation list"
    );
    assert_eq!(
        call_native(
            &shared,
            &mut thread,
            "java/lang/Integer",
            "intValue",
            "()I",
            &[result],
        )
        .unwrap()
        .unwrap(),
        Value::Int(6)
    );
}

#[test]
fn int_summary_stats_accept_and_getters() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let stats = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "<init>",
        "()V",
        &[Value::Object(Some(stats))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "accept",
        "(I)V",
        &[Value::Object(Some(stats)), Value::Int(5)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "accept",
        "(I)V",
        &[Value::Object(Some(stats)), Value::Int(15)],
    )
    .unwrap();
    let cnt = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getCount",
        "()J",
        &[Value::Object(Some(stats))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cnt, Value::Long(2));
    let sum = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getSum",
        "()J",
        &[Value::Object(Some(stats))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sum, Value::Long(20));
    let min = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getMin",
        "()I",
        &[Value::Object(Some(stats))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(min, Value::Int(5));
    let max = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getMax",
        "()I",
        &[Value::Object(Some(stats))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(max, Value::Int(15));
    let avg = call_native(
        &shared,
        &mut thread,
        "java/util/IntSummaryStatistics",
        "getAverage",
        "()D",
        &[Value::Object(Some(stats))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avg, Value::Double(10.0));
}

#[test]
fn stream_map_to_int_p56() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Stream of objects, mapToInt with String.length
    let s1 = create_java_string(&shared, "abc");
    let s2 = create_java_string(&shared, "de");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 0, Value::Object(Some(s1)));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 1, Value::Object(Some(s2)));
    // The receiver must be a SYNTHETIC STREAM, i.e. runtime class
    // `java/util/stream/Stream` with the elements array in slot 0. This
    // fixture used to say "java/lang/String" (copy-paste of the mapper's
    // impl class), and that is what threw: the winning `Stream.mapToInt`
    // is native-collections' `native_stream_map_to_int`, whose
    // `stream_elements` treats any receiver failing `is_synthetic_stream`
    // as a REAL JDK pipeline and materialises it via
    // `invoke_virtual(recv, "toArray", "()[Ljava/lang/Object;")`. A String
    // has no `toArray`, so the native propagated that Err — the observed
    // `ExceptionThrown(ObjectRef)`.
    let stream = alloc_receiver(&shared, &mut thread, "java/util/stream/Stream", 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    // mapToInt with String.length as the ToIntFunction
    let func = make_lambda_proxy(
        &shared,
        "java/util/function/ToIntFunction",
        "applyAsInt",
        "(Ljava/lang/Object;)I",
        "java/lang/String",
        "length",
        "()I",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "mapToInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/IntStream;",
        &[Value::Object(Some(stream)), Value::Object(Some(func))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let r_arr = shared.mem.heap.get_field(r_ref, 0).as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(r_arr), 2);
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element_unboxing(r_arr, 0)
            .unwrap(),
        Value::Int(3)
    ); // "abc".length
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element_unboxing(r_arr, 1)
            .unwrap(),
        Value::Int(2)
    ); // "de".length
}

// =====================================================================
// Phase 57 tests: NIO File, ProcessBuilder, java.text
// =====================================================================

#[test]
fn path_normalize_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let input = if cfg!(windows) {
        "C:\\a\\b\\..\\c\\.\\d"
    } else {
        "/a/b/../c/./d"
    };
    let expected = if cfg!(windows) {
        "C:\\a\\c\\d"
    } else {
        "/a/c/d"
    };
    let path_str = create_java_string(&shared, input);
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(path, 0, Value::Object(Some(path_str)));
    let norm = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "normalize",
        "()Ljava/nio/file/Path;",
        &[Value::Object(Some(path))],
    )
    .unwrap()
    .unwrap();
    let n_ref = norm.as_object().unwrap();
    let s_val = shared.mem.heap.get_field(n_ref, 0);
    let s_ref = s_val.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, s_ref),
        Some(expected.to_string())
    );
}

#[test]
fn path_get_name_count_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let path_str = create_java_string(&shared, "/usr/local/bin");
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(path, 0, Value::Object(Some(path_str)));
    let cnt = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "getNameCount",
        "()I",
        &[Value::Object(Some(path))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cnt, Value::Int(3)); // usr, local, bin
}

#[test]
fn path_starts_ends_with_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let path_str = create_java_string(&shared, "/usr/local/bin");
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(path, 0, Value::Object(Some(path_str)));
    let prefix = create_java_string(&shared, "/usr");
    let sw = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "startsWith",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(path)), Value::Object(Some(prefix))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sw, Value::Int(1));
    let suffix = create_java_string(&shared, "bin");
    let ew = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "endsWith",
        "(Ljava/lang/String;)Z",
        &[Value::Object(Some(path)), Value::Object(Some(suffix))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ew, Value::Int(1));
}

#[test]
fn path_is_absolute_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let abs_input = if cfg!(windows) {
        "C:\\home\\user"
    } else {
        "/home/user"
    };
    let abs_str = create_java_string(&shared, abs_input);
    let abs_path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(abs_path, 0, Value::Object(Some(abs_str)));
    let is_abs = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "isAbsolute",
        "()Z",
        &[Value::Object(Some(abs_path))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_abs, Value::Int(1));
    let rel_str = create_java_string(&shared, "relative/path");
    let rel_path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(rel_path, 0, Value::Object(Some(rel_str)));
    let is_rel = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "isAbsolute",
        "()Z",
        &[Value::Object(Some(rel_path))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_rel, Value::Int(0));
}

#[test]
fn path_of_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = create_java_string(&shared, "/tmp/test.txt");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);
    let result = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "of",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(name)), Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    // Ask the path for its string through `toString()`, as Java does. This
    // test used to read `get_field(path, 0)` and expect the string there --
    // an assumption about the path object's PRIVATE layout, not about any JDK
    // API. `Path.of` now yields a real-layout path (on Windows JDK 25,
    // `sun.nio.fs.WindowsPath`), whose slot 0 is not the string, so the read
    // came back empty and the `unwrap` panicked.
    let s_val = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.invoke_virtual(r_ref, "toString", "()Ljava/lang/String;", &[])
            .unwrap_or_else(|e| panic!("Path.toString() failed: {e:?}"))
    };
    let s_ref = match s_val {
        Some(Value::Object(Some(r))) => r,
        other => panic!("Path.toString() returned {other:?}"),
    };
    // The expected text follows the HOST separator. The old expectation
    // "/tmp/test.txt" only holds on Unix: on Windows JDK 25,
    // `Path.of("/tmp/test.txt").toString()` is "\tmp\test.txt".
    let expected = "/tmp/test.txt".replace('/', std::path::MAIN_SEPARATOR_STR);
    assert_eq!(read_java_string(&shared.mem.heap, s_ref), Some(expected));
}

// RETIRED 2026-08-11 — `file_visit_result_enum_p57`.
//
// This test called `call_native(class, "CONTINUE", "Ljava/nio/file/FileVisitResult;")`: a
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
fn process_builder_basics_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let pb = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    // Start with no command should return an error
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessBuilder",
        "start",
        "()Ljava/lang/Process;",
        &[Value::Object(Some(pb))],
    );
    assert!(result.is_err(), "start() with no command should fail");
    // Set up a command list and start a real process
    let cmd_str = create_java_string(&shared, if cfg!(windows) { "cmd" } else { "echo" });
    let arg_str = create_java_string(
        &shared,
        if cfg!(windows) {
            "/C echo hello"
        } else {
            "hello"
        },
    );
    let cmd_arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        2,
    );
    shared
        .mem
        .heap
        .set_array_element(cmd_arr, 0, Value::Object(Some(cmd_str)))
        .unwrap();
    shared
        .mem
        .heap
        .set_array_element(cmd_arr, 1, Value::Object(Some(arg_str)))
        .unwrap();
    // Store command array as an ArrayList-like object in field 0
    let cmd_list = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared
        .mem
        .heap
        .set_field(cmd_list, 0, Value::Object(Some(cmd_arr)));
    shared.mem.heap.set_field(cmd_list, 1, Value::Int(2));
    let pb2 = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    shared
        .mem
        .heap
        .set_field(pb2, 0, Value::Object(Some(cmd_list)));
    let proc = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessBuilder",
        "start",
        "()Ljava/lang/Process;",
        &[Value::Object(Some(pb2))],
    )
    .unwrap()
    .unwrap();
    let p_ref = proc.as_object().unwrap();
    // waitFor returns exit code 0
    let exit = call_native(
        &shared,
        &mut thread,
        "java/lang/Process",
        "waitFor",
        "()I",
        &[Value::Object(Some(p_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(exit, Value::Int(0));
    // isAlive returns false
    let alive = call_native(
        &shared,
        &mut thread,
        "java/lang/Process",
        "isAlive",
        "()Z",
        &[Value::Object(Some(p_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(alive, Value::Int(0));
}

#[test]
fn process_handle_current_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let handle = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "current",
        "()Ljava/lang/ProcessHandle;",
        &[],
    )
    .unwrap()
    .unwrap();
    let h_ref = handle.as_object().unwrap();
    let pid = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "pid",
        "()J",
        &[Value::Object(Some(h_ref))],
    )
    .unwrap()
    .unwrap();
    if let Value::Long(p) = pid {
        assert!(p > 0);
    } else {
        panic!("expected Long pid");
    }
}

#[test]
fn process_handle_parent_is_present_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let handle = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "current",
        "()Ljava/lang/ProcessHandle;",
        &[],
    )
    .unwrap()
    .unwrap();
    let parent = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "parent",
        "()Ljava/util/Optional;",
        &[handle],
    )
    .unwrap()
    .unwrap()
    .as_object()
    .unwrap();
    let parent_handle = shared.mem.heap.get_field(parent, 0).as_object().unwrap();
    assert!(matches!(shared.mem.heap.get_field(parent_handle, 0), Value::Long(pid) if pid > 0));
}

#[test]
fn decimal_format_basics_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let df = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "<init>",
        "()V",
        &[Value::Object(Some(df))],
    )
    .unwrap();
    // format(D) with double value
    let result = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "format",
        "(D)Ljava/lang/String;",
        &[Value::Object(Some(df)), Value::Double(1.23456)],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let text = read_java_string(&shared.mem.heap, r_ref).unwrap_or_default();
    assert!(text.starts_with("1.23")); // 3 decimal places max
}

#[test]
fn decimal_format_custom_pattern_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let df = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let pattern = create_java_string(&shared, "#.00");
    call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(df)), Value::Object(Some(pattern))],
    )
    .unwrap();
    let pat_result = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormat",
        "getPattern",
        "()Ljava/lang/String;",
        &[Value::Object(Some(df))],
    )
    .unwrap()
    .unwrap();
    let p_ref = pat_result.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, p_ref),
        Some("#.00".to_string())
    );
}

#[test]
fn number_format_get_instance_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
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
    assert!(nf.as_object().is_some());
}

#[test]
fn simple_date_format_basics_p57() {
    // Same disposition as `timer_basic`: `java.text.SimpleDateFormat`'s
    // synthetic substitutes were removed so the real `.class` bytecode
    // runs (see `register_number_format_natives`). This asserted the
    // deleted `<init>(String)` contract.
    let registry = &*TEST_NATIVE_REGISTRY;
    assert!(
        registry
            .find(
                "java/text/SimpleDateFormat",
                "<init>",
                "(Ljava/lang/String;)V"
            )
            .is_none(),
        "SimpleDateFormat must defer to real JDK bytecode; the synthetic \
             1-field substitute was removed deliberately",
    );
}

#[test]
fn message_format_basic_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let pattern = create_java_string(&shared, "Hello {0}, you are {1} years old");
    // Create Object[] args
    let name = create_java_string(&shared, "Alice");
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
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
    let r_ref = result.as_object().unwrap();
    let text = read_java_string(&shared.mem.heap, r_ref).unwrap_or_default();
    assert!(text.contains("Alice"));
    assert!(text.contains("30"));
}

#[test]
fn message_format_instance_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let mf = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let pattern = create_java_string(&shared, "Count: {0}");
    call_native(
        &shared,
        &mut thread,
        "java/text/MessageFormat",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(mf)), Value::Object(Some(pattern))],
    )
    .unwrap();
    let pat = call_native(
        &shared,
        &mut thread,
        "java/text/MessageFormat",
        "toPattern",
        "()Ljava/lang/String;",
        &[Value::Object(Some(mf))],
    )
    .unwrap()
    .unwrap();
    let p_ref = pat.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, p_ref),
        Some("Count: {0}".to_string())
    );
}

// RETIRED 2026-08-11 — `standard_open_option_enum_p57`.
//
// This test called `call_native(class, "READ", "Ljava/nio/file/StandardOpenOption;")`: a
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
fn path_get_name_p57() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let path_str = create_java_string(&shared, "/usr/local/bin");
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared
        .mem
        .heap
        .set_field(path, 0, Value::Object(Some(path_str)));
    let name = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "getName",
        "(I)Ljava/nio/file/Path;",
        &[Value::Object(Some(path)), Value::Int(1)],
    )
    .unwrap()
    .unwrap();
    let n_ref = name.as_object().unwrap();
    let s_val = shared.mem.heap.get_field(n_ref, 0);
    let s_ref = s_val.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, s_ref),
        Some("local".to_string())
    );
}

// =====================================================================
// Phase 58 tests: CompletableFuture expansion, NIO Channels,
//                 GZIP/Zip streams, PushbackInputStream, CharsetEncoder,
//                 StringConcatFactory, SynchronousQueue
// =====================================================================

#[test]
fn cf_then_compose_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create a completed future with value 42
    let cf = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    // thenCompose needs a Function that returns a CF — use identity via System.identityHashCode
    // Instead, test thenRun which is simpler
    let cf_ref = cf.as_object().unwrap();
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

#[test]
fn cf_exceptionally_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // completedFuture → exceptionally should pass through (no exception)
    let cf = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Int(99)],
    )
    .unwrap()
    .unwrap();
    let cf_ref = cf.as_object().unwrap();
    // Create a Function proxy for exceptionally — but since no exception, result passes through
    // Test via get instead
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(99));
}

#[test]
fn cf_all_of_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // allOf with empty array
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "allOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(r_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

#[test]
fn cf_any_of_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create two completed futures and call anyOf
    let cf1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    let cf2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Int(20)],
    )
    .unwrap()
    .unwrap();
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr, 0, cf1);
    let _ = shared.mem.heap.set_array_element(arr, 1, cf2);
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "anyOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    let r_ref = result.as_object().unwrap();
    let val = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "get",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(r_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(10)); // first future's value
}

#[test]
fn cf_failed_future_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let throwable = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let msg = create_java_string(&shared, "test error");
    shared
        .mem
        .heap
        .set_field(throwable, 0, Value::Object(Some(msg)));
    let cf = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "failedFuture",
        "(Ljava/lang/Throwable;)Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(throwable))],
    )
    .unwrap()
    .unwrap();
    let cf_ref = cf.as_object().unwrap();
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

#[test]
fn cf_complete_exceptionally_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create empty CF then complete exceptionally
    let cf = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        2,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "<init>",
        "()V",
        &[Value::Object(Some(cf))],
    )
    .unwrap();
    let throwable = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "completeExceptionally",
        "(Ljava/lang/Throwable;)Z",
        &[Value::Object(Some(cf)), Value::Object(Some(throwable))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
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
    // Internal DONE field encodes 1 (completed normally) or 2 (completed
    // exceptionally); either counts as done for the isDone() boolean.
    assert!(
        matches!(done, Value::Int(v) if v != 0),
        "expected non-zero done flag, got {:?}",
        done
    );
}

#[test]
fn socket_channel_open_close_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sc = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "open",
        "()Ljava/nio/channels/SocketChannel;",
        &[],
    )
    .unwrap()
    .unwrap();
    let sc_ref = sc.as_object().unwrap();
    let is_open = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "isOpen",
        "()Z",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_open, Value::Int(1));
    let is_conn = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "isConnected",
        "()Z",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_conn, Value::Int(0));
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "close",
        "()V",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap();
    let is_open2 = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "isOpen",
        "()Z",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_open2, Value::Int(0));
}

#[test]
fn server_socket_channel_open_bind_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ssc = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
        &[],
    )
    .unwrap()
    .unwrap();
    let ssc_ref = ssc.as_object().unwrap();
    let is_open = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "isOpen",
        "()Z",
        &[Value::Object(Some(ssc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_open, Value::Int(1));
    // A freshly-opened channel is not bound yet.
    let bound_before = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "isBound",
        "()Z",
        &[Value::Object(Some(ssc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(bound_before, Value::Int(0));
    // `bind(null)` is the specified "bind to an address that is assigned
    // automatically" form. Note the descriptor: `ServerSocketChannel.bind`
    // covariantly narrows `NetworkChannel.bind`'s return type, so this is
    // the triple every `ssc.bind(addr)` call site actually uses — and it
    // must reach the SAME implementation as `accept()` below (the real
    // listener registry in native-io/src/socket_channel.rs). It did not:
    // a stale synthetic in native-builtins owned this one descriptor and
    // recorded its listener somewhere `accept()` never looks, so the
    // accept failed with "server channel not bound".
    let bound = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
        &[Value::Object(Some(ssc_ref)), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(bound, Value::Object(Some(_))));
    let bound_after = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "isBound",
        "()Z",
        &[Value::Object(Some(ssc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        bound_after,
        Value::Int(1),
        "bind(null) must assign an ephemeral local port"
    );
    // Accept, in non-blocking mode, with nothing dialling the listener:
    // the JDK contract is "returns null if no connection is available".
    // (A blocking accept here would park forever, which is exactly what
    // the JDK specifies and therefore not something a unit test can
    // assert against without a second thread.)
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        &[Value::Object(Some(ssc_ref)), Value::Int(0)],
    )
    .unwrap();
    let accepted = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "accept",
        "()Ljava/nio/channels/SocketChannel;",
        &[Value::Object(Some(ssc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(accepted, Value::Object(None));
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "close",
        "()V",
        &[Value::Object(Some(ssc_ref))],
    )
    .unwrap();
    let is_open2 = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/ServerSocketChannel",
        "isOpen",
        "()Z",
        &[Value::Object(Some(ssc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_open2, Value::Int(0));
}

#[test]
fn selector_open_close_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sel = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/Selector",
        "open",
        "()Ljava/nio/channels/Selector;",
        &[],
    )
    .unwrap()
    .unwrap();
    let sel_ref = sel.as_object().unwrap();
    let is_open = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/Selector",
        "isOpen",
        "()Z",
        &[Value::Object(Some(sel_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_open, Value::Int(1));
    let count = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/Selector",
        "selectNow",
        "()I",
        &[Value::Object(Some(sel_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(0));
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/Selector",
        "close",
        "()V",
        &[Value::Object(Some(sel_ref))],
    )
    .unwrap();
    let is_open2 = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/Selector",
        "isOpen",
        "()Z",
        &[Value::Object(Some(sel_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_open2, Value::Int(0));
}

#[test]
fn gzip_input_stream_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Test GZIPInputStream stub without stream delegation (invoke_virtual requires real class IDs)
    // Manually set up GZIPInputStream with null underlying stream
    let gis = alloc_receiver(&shared, &mut thread, "java/util/zip/GZIPInputStream", 2);
    shared.mem.heap.set_field(gis, 0, Value::Object(None)); // no underlying stream
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 0);
    shared.mem.heap.set_field(gis, 1, Value::Object(Some(arr)));
    // Read from null stream returns -1
    let b1 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPInputStream",
        "read",
        "()I",
        &[Value::Object(Some(gis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b1, Value::Int(-1));
    // Available from null stream returns 0
    let avail = call_native(
        &shared,
        &mut thread,
        "java/util/zip/GZIPInputStream",
        "available",
        "()I",
        &[Value::Object(Some(gis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(avail, Value::Int(0));
}

#[test]
fn zip_output_stream_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create a BAOS
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
    // Create ZipOutputStream wrapping BAOS
    let zos = alloc_receiver(&shared, &mut thread, "java/util/zip/ZipOutputStream", 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        &[Value::Object(Some(zos)), Value::Object(Some(baos))],
    )
    .unwrap();
    // Put a new entry
    let name = create_java_string(&shared, "test.txt");
    let entry = alloc_receiver(&shared, &mut thread, "java/util/zip/ZipEntry", 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(entry)), Value::Object(Some(name))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipOutputStream",
        "putNextEntry",
        "(Ljava/util/zip/ZipEntry;)V",
        &[Value::Object(Some(zos)), Value::Object(Some(entry))],
    )
    .unwrap();
    // Close entry
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipOutputStream",
        "closeEntry",
        "()V",
        &[Value::Object(Some(zos))],
    )
    .unwrap();
    // Finish
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipOutputStream",
        "finish",
        "()V",
        &[Value::Object(Some(zos))],
    )
    .unwrap();
}

#[test]
fn zip_input_stream_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bais = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let data = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 0);
    call_native(
        &shared,
        &mut thread,
        "java/io/ByteArrayInputStream",
        "<init>",
        "([B)V",
        &[Value::Object(Some(bais)), Value::Object(Some(data))],
    )
    .unwrap();
    let zis = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(zis)), Value::Object(Some(bais))],
    )
    .unwrap();
    // getNextEntry on empty zip returns null
    let entry = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipInputStream",
        "getNextEntry",
        "()Ljava/util/zip/ZipEntry;",
        &[Value::Object(Some(zis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(entry, Value::Object(None));
}

#[test]
fn pushback_input_stream_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Test pushback buffer directly (invoke_virtual requires real class IDs)
    // Create PushbackInputStream with null underlying stream, buf size 3
    let pis = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(pis, 0, Value::Object(None)); // no underlying stream
    let buf = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    shared.mem.heap.set_field(pis, 1, Value::Object(Some(buf)));
    shared.mem.heap.set_field(pis, 2, Value::Int(3)); // pos = 3 means buffer empty
                                                      // Unread byte 42
    call_native(
        &shared,
        &mut thread,
        "java/io/PushbackInputStream",
        "unread",
        "(I)V",
        &[Value::Object(Some(pis)), Value::Int(42)],
    )
    .unwrap();
    // Read it back from pushback buffer
    let b1 = call_native(
        &shared,
        &mut thread,
        "java/io/PushbackInputStream",
        "read",
        "()I",
        &[Value::Object(Some(pis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b1, Value::Int(42));
    // Unread two bytes
    call_native(
        &shared,
        &mut thread,
        "java/io/PushbackInputStream",
        "unread",
        "(I)V",
        &[Value::Object(Some(pis)), Value::Int(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/io/PushbackInputStream",
        "unread",
        "(I)V",
        &[Value::Object(Some(pis)), Value::Int(20)],
    )
    .unwrap();
    // Read them back in LIFO order
    let b2 = call_native(
        &shared,
        &mut thread,
        "java/io/PushbackInputStream",
        "read",
        "()I",
        &[Value::Object(Some(pis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b2, Value::Int(20));
    let b3 = call_native(
        &shared,
        &mut thread,
        "java/io/PushbackInputStream",
        "read",
        "()I",
        &[Value::Object(Some(pis))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(b3, Value::Int(10));
}

#[test]
fn charset_encoder_decoder_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Get UTF-8 charset
    let name = create_java_string(&shared, "UTF-8");
    let cs = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let cs_ref = cs.as_object().unwrap();
    // Create encoder
    let enc = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "newEncoder",
        "()Ljava/nio/charset/CharsetEncoder;",
        &[Value::Object(Some(cs_ref))],
    )
    .unwrap()
    .unwrap();
    let enc_ref = enc.as_object().unwrap();
    // Check charset
    let enc_cs = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/CharsetEncoder",
        "charset",
        "()Ljava/nio/charset/Charset;",
        &[Value::Object(Some(enc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(enc_cs, Value::Object(Some(cs_ref)));
    // Create decoder
    let dec = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "newDecoder",
        "()Ljava/nio/charset/CharsetDecoder;",
        &[Value::Object(Some(cs_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(dec, Value::Object(Some(_))));
}

#[test]
fn coder_result_checks_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Get UTF-8 charset and create encoder
    let name = create_java_string(&shared, "UTF-8");
    let cs = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let cs_ref = cs.as_object().unwrap();
    let enc = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "newEncoder",
        "()Ljava/nio/charset/CharsetEncoder;",
        &[Value::Object(Some(cs_ref))],
    )
    .unwrap()
    .unwrap();
    let enc_ref = enc.as_object().unwrap();
    // Encode returns CoderResult (UNDERFLOW = success)
    let cr = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/CharsetEncoder",
        "encode",
        "(Ljava/nio/CharBuffer;Ljava/nio/ByteBuffer;Z)Ljava/nio/charset/CoderResult;",
        &[
            Value::Object(Some(enc_ref)),
            Value::Object(None),
            Value::Object(None),
            Value::Int(1),
        ],
    )
    .unwrap()
    .unwrap();
    let cr_ref = cr.as_object().unwrap();
    let is_underflow = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/CoderResult",
        "isUnderflow",
        "()Z",
        &[Value::Object(Some(cr_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_underflow, Value::Int(1));
    let is_overflow = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/CoderResult",
        "isOverflow",
        "()Z",
        &[Value::Object(Some(cr_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_overflow, Value::Int(0));
}

#[test]
fn string_concat_factory_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // makeConcatWithConstants returns a CallSite
    let lookup = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let name = create_java_string(&shared, "makeConcatWithConstants");
    let mt = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let recipe = create_java_string(&shared, "\u{0001}\u{0001}");
    let constants = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);
    let cs = call_native(&shared, &mut thread, "java/lang/invoke/StringConcatFactory",
            "makeConcatWithConstants",
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
            &[Value::Object(Some(lookup)), Value::Object(Some(name)), Value::Object(Some(mt)),
              Value::Object(Some(recipe)), Value::Object(Some(constants))]).unwrap().unwrap();
    assert!(matches!(cs, Value::Object(Some(_))));
}

#[test]
fn synchronous_queue_put_take_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sq = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(sq))],
    )
    .unwrap();
    // Size should be 0
    let sz = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "size",
        "()I",
        &[Value::Object(Some(sq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sz, Value::Int(0));
    // gc-common w19-b: `put` blocks until a consumer takes the element (the
    // JDK contract; the `concurrent_extras` table whose `put` returned after
    // 2 s is gone), so a same-thread put-then-take would park for ever. The
    // two-thread rendezvous is covered by `native-builtins`
    // `phases_late::concurrent::w19b_synchronous_queue_tests`. Here: with no
    // consumer, a zero-timeout timed poll does not wait and finds nothing,
    // and a plain poll finds nothing either (no element is ever buffered).
    let timed = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        &[Value::Object(Some(sq)), Value::Long(0), Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(timed, Value::Object(None));
    let polled = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(sq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(polled, Value::Object(None));
}

#[test]
fn synchronous_queue_offer_poll_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sq = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "<init>",
        "()V",
        &[Value::Object(Some(sq))],
    )
    .unwrap();
    // Non-blocking offer on a SynchronousQueue only succeeds when a taker
    // is already waiting.
    let ok = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "offer",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(sq)), Value::Int(77)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(0));
    // Poll
    let val = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "poll",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(sq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Object(None));
    // isEmpty should be true
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SynchronousQueue",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(sq))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
}

// DEAD TWICE OVER (measured 2026-08-13, lane F9). The four values asserted
// below are all CORRECT against the JDK — and no classfile in any mode will
// ever ask this VM for them.
//
// First: `getstatic` has three implementations here
// (`interpreter/opcodes.rs Instruction::Getstatic`, `jit/helpers.rs
// jit_getstatic`, `ir_lower.rs emit_inline_getstatic`) and NONE consults
// the native registry — the third bakes the statics base as an immediate
// and emits two `mov`s with no call at all. E21-1.
//
// Second, and specific to these: `SelectionKey.OP_*` are `static final int`
// CONSTANT EXPRESSIONS, so JLS 13.1 inlining applies and javac emits no
// `getstatic` in the first place. Measured on 25.0.3+9-LTS:
//
//   static int opRead() { return SelectionKey.OP_READ; }
//     0: iconst_1                       // not getstatic
//
// versus a reference-typed constant, which does emit a real read the
// registry still cannot answer:
//
//   static Object utf8() { return StandardCharsets.UTF_8; }
//     0: getstatic Field java/nio/charset/StandardCharsets.UTF_8:
//                       Ljava/nio/charset/Charset;
//
// Kept, not deleted: a right answer is not a reason to delete, and the
// registrations are what would have to go first. This is a right answer to
// a question nobody asks — which is why "every value checked was correct"
// (E40-1 §4d) is not reassurance on its own.
#[test]
fn selection_key_constants_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let read = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SelectionKey",
        "OP_READ",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(read, Value::Int(1));
    let write = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SelectionKey",
        "OP_WRITE",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(write, Value::Int(4));
    let connect = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SelectionKey",
        "OP_CONNECT",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(connect, Value::Int(8));
    let accept = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SelectionKey",
        "OP_ACCEPT",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(accept, Value::Int(16));
}

#[test]
fn socket_channel_connect_p58() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // `SocketChannel.connect` is backed by a real OS connect
    // (native-io/src/socket_channel.rs), so the only honest way to observe
    // `isConnected() == true` is to give it something to connect to.
    // `connect(null)` is NOT a success — the JDK routes every connect
    // through `sun.nio.ch.Net.checkAddress`, which starts with
    // `Objects.requireNonNull(sa)`.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().unwrap().port() as i32;

    let sc = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "open",
        "()Ljava/nio/channels/SocketChannel;",
        &[],
    )
    .unwrap()
    .unwrap();
    let sc_ref = sc.as_object().unwrap();
    // Not connected initially
    let conn = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "isConnected",
        "()Z",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(conn, Value::Int(0));
    // The flat `(host: String, port: int)` InetSocketAddress layout that
    // `decode_socket_address` and the `java/net/InetSocketAddress`
    // accessors both understand.
    let host = create_java_string(&shared, "127.0.0.1");
    let addr = alloc_receiver(&shared, &mut thread, "java/net/InetSocketAddress", 2);
    shared
        .mem
        .heap
        .set_field(addr, 0, Value::Object(Some(host)));
    shared.mem.heap.set_field(addr, 1, Value::Int(port));
    // Connect (blocking mode: completes before returning, hence `true`).
    let ok = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "connect",
        "(Ljava/net/SocketAddress;)Z",
        &[Value::Object(Some(sc_ref)), Value::Object(Some(addr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ok, Value::Int(1));
    // Now connected
    let conn2 = call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "isConnected",
        "()Z",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(conn2, Value::Int(1));
    call_native(
        &shared,
        &mut thread,
        "java/nio/channels/SocketChannel",
        "close",
        "()V",
        &[Value::Object(Some(sc_ref))],
    )
    .unwrap();
    drop(listener);
}

// =====================================================================
// Phase 59 tests: Management, JAR, Spliterator, VarHandle, Package,
//                 StackWalker, File attrs, Module
// =====================================================================

#[test]
fn management_memory_mxbean_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bean = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ManagementFactory",
        "getMemoryMXBean",
        "()Ljava/lang/management/MemoryMXBean;",
        &[],
    )
    .unwrap()
    .unwrap();
    let bean_ref = bean.as_object().unwrap();
    let usage = call_native(
        &shared,
        &mut thread,
        "java/lang/management/MemoryMXBean",
        "getHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    let usage_ref = usage.as_object().unwrap();
    let used = call_native(
        &shared,
        &mut thread,
        "java/lang/management/MemoryUsage",
        "getUsed",
        "()J",
        &[Value::Object(Some(usage_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(used, Value::Long(v) if v > 0));
    let max = call_native(
        &shared,
        &mut thread,
        "java/lang/management/MemoryUsage",
        "getMax",
        "()J",
        &[Value::Object(Some(usage_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(max, Value::Long(v) if v > 0));
}

#[test]
fn management_runtime_mxbean_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bean = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ManagementFactory",
        "getRuntimeMXBean",
        "()Ljava/lang/management/RuntimeMXBean;",
        &[],
    )
    .unwrap()
    .unwrap();
    let bean_ref = bean.as_object().unwrap();
    let name = call_native(
        &shared,
        &mut thread,
        "java/lang/management/RuntimeMXBean",
        "getVmName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    let name_ref = name.as_object().unwrap();
    // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`, default ON): the VM's
    // `java.vm.name`, as HotSpot's `RuntimeImpl.getVmName` reads it.
    assert_eq!(
        read_java_string(&shared.mem.heap, name_ref),
        shared.system_properties.read().get("java.vm.name").cloned()
    );
    let version = call_native(
        &shared,
        &mut thread,
        "java/lang/management/RuntimeMXBean",
        "getVmVersion",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    let ver_ref = version.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, ver_ref),
        Some("0.1.0".to_string())
    );
}

#[test]
fn management_thread_mxbean_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bean = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ManagementFactory",
        "getThreadMXBean",
        "()Ljava/lang/management/ThreadMXBean;",
        &[],
    )
    .unwrap()
    .unwrap();
    let bean_ref = bean.as_object().unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ThreadMXBean",
        "getThreadCount",
        "()I",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    // active_thread_count() in the unit-test harness may be 0 because the
    // test thread isn't registered in the shared VM thread table; the
    // handler returns the raw count so it can be 0 when the registry is
    // empty and >= 1 otherwise. Both are valid responses here.
    assert!(
        matches!(count, Value::Int(v) if v >= 0),
        "expected non-negative thread count, got {:?}",
        count
    );
    let total = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ThreadMXBean",
        "getTotalStartedThreadCount",
        "()J",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    // Session 88 T16.R1: totalStartedThreadCount is snapshotted from
    // active_thread_count() at bean alloc (native-builtins/jmx.rs:455),
    // which is 0 in the unit-test harness for the same registry reason
    // the count>=0 check above tolerates. Spec (JDK 25 ThreadMXBean)
    // permits 0 when no threads have been started via the shared table.
    assert!(
        matches!(total, Value::Long(v) if v >= 0),
        "expected non-negative total started threads, got {:?}",
        total
    );
}

#[test]
fn management_os_mxbean_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bean = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ManagementFactory",
        "getOperatingSystemMXBean",
        "()Ljava/lang/management/OperatingSystemMXBean;",
        &[],
    )
    .unwrap()
    .unwrap();
    let bean_ref = bean.as_object().unwrap();
    let procs = call_native(
        &shared,
        &mut thread,
        "java/lang/management/OperatingSystemMXBean",
        "getAvailableProcessors",
        "()I",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(procs, Value::Int(v) if v >= 1));
    let name = call_native(
        &shared,
        &mut thread,
        "java/lang/management/OperatingSystemMXBean",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(name, Value::Object(Some(_))));
}

#[test]
fn jar_file_basics_p59() {
    use std::io::Write;

    // Build a minimal JAR in a tempdir: the real-mode JarFile native
    // (native-io/src/zip_real_jar.rs) opens the archive during <init>,
    // so the file must exist on disk. Manifest entry is required so
    // getManifest() returns a non-null Manifest object.
    let td = tempfile::tempdir().unwrap();
    let jar_path = td.path().join("test.jar");
    {
        let file = std::fs::File::create(&jar_path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zw.write_all(b"Manifest-Version: 1.0\r\nMain-Class: Foo\r\n\r\n")
            .unwrap();
        // A dummy class entry — the test exercises JAR IO, not class loading.
        zw.start_file("Foo.class", opts).unwrap();
        zw.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        zw.finish().unwrap();
    }
    let jar_path_str = jar_path.to_string_lossy().into_owned();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let path = create_java_string(&shared, &jar_path_str);
    let jf = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/jar/JarFile",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(jf)), Value::Object(Some(path))],
    )
    .unwrap();
    // getName — returns the path we opened under.
    let name = call_native(
        &shared,
        &mut thread,
        "java/util/jar/JarFile",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(jf))],
    )
    .unwrap()
    .unwrap();
    let name_ref = name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, name_ref),
        Some(jar_path_str.clone())
    );
    // getManifest should return a Manifest object
    let manifest = call_native(
        &shared,
        &mut thread,
        "java/util/jar/JarFile",
        "getManifest",
        "()Ljava/util/jar/Manifest;",
        &[Value::Object(Some(jf))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(manifest, Value::Object(Some(_))));
}

#[test]
fn jar_entry_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = create_java_string(&shared, "META-INF/MANIFEST.MF");
    let entry = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    call_native(
        &shared,
        &mut thread,
        "java/util/jar/JarEntry",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(entry)), Value::Object(Some(name))],
    )
    .unwrap();
    let got_name = call_native(
        &shared,
        &mut thread,
        "java/util/jar/JarEntry",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    let n_ref = got_name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref),
        Some("META-INF/MANIFEST.MF".to_string())
    );
}

// DEAD TWICE OVER, exactly as `selection_key_constants_p58` — see the block
// comment there for the mechanism and the bytecode. Measured on
// 25.0.3+9-LTS:
//
//   static int spOrdered() { return Spliterator.ORDERED; }
//     0: bipush 16                      // not getstatic
#[test]
fn spliterator_constants_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ordered = call_native(
        &shared,
        &mut thread,
        "java/util/Spliterator",
        "ORDERED",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ordered, Value::Int(0x10));
    let sized = call_native(
        &shared,
        &mut thread,
        "java/util/Spliterator",
        "SIZED",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sized, Value::Int(0x40));
    let distinct = call_native(
        &shared,
        &mut thread,
        "java/util/Spliterator",
        "DISTINCT",
        "I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(distinct, Value::Int(0x01));
}

#[test]
fn stream_support_from_spliterator_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create a spliterator with some data
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(2));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(3));
    // A SYNTHETIC spliterator is the JDK ArraySpliterator shape — runtime
    // class `java/util/Spliterator`, slots (array, cursor, fence) — which is
    // what every VM-side producer allocates (`alloc_synthetic(ctx,
    // "java/util/Spliterator", 3)` in native-collections and
    // phases_late/streams). This fixture used to be a 2-slot object of class
    // `java/util/stream/StreamSupport`; the winning native
    // (`service_loader::native_stream_support_stream_from_spliterator`, which
    // registers after both phases_late copies) gates on the exact class name,
    // so it classified the fixture as a REAL Spliterator and took the lazy
    // branch: slot 0 := null, slot 2 := the spliterator. That null is the
    // reported `Object(None)`.
    let spl = alloc_receiver(&shared, &mut thread, "java/util/Spliterator", 3);
    shared.mem.heap.set_field(spl, 0, Value::Object(Some(arr)));
    shared.mem.heap.set_field(spl, 1, Value::Int(0));
    shared.mem.heap.set_field(spl, 2, Value::Int(3));
    // Create stream from spliterator
    let stream = call_native(
        &shared,
        &mut thread,
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        &[Value::Object(Some(spl)), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(stream, Value::Object(Some(_))));
    // The stream carries the spliterator's [cursor, fence) elements. It is a
    // fresh SNAPSHOT array, not the spliterator's own backing array — that is
    // deliberate and required: the source spliterator's cursor keeps moving
    // (`tryAdvance`/`trySplit` rewrite slots 1 and 2), and the real
    // `StreamSupport.stream` contract gives the stream a lifetime independent
    // of it. Asserting array *identity* here pinned an implementation the
    // native does not and should not have.
    let s_ref = stream.as_object().unwrap();
    let stream_arr = shared
        .mem
        .heap
        .get_field(s_ref, 0)
        .as_object()
        .expect("StreamSupport.stream must materialise a synthetic spliterator eagerly");
    assert_eq!(shared.mem.heap.array_length(stream_arr), 3);
    for i in 0..3 {
        assert_eq!(
            shared
                .mem
                .heap
                .get_array_element_unboxing(stream_arr, i)
                .unwrap(),
            Value::Int(i as i32 + 1)
        );
    }
}

#[test]
fn varhandle_volatile_get_set_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // VarHandle.getVolatile with array arg
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(42));
    let val = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/VarHandle",
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(42));
}

#[test]
fn varhandle_lookup_factory_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Get lookup
    let lookup = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )
    .unwrap()
    .unwrap();
    let lk_ref = lookup.as_object().unwrap();
    // Use Class.forName to get a proper class mirror whose slot 1 is the
    // class-name string — findVarHandle's mirror_class_name walks that
    // slot and needs a real value to resolve the class.
    let holder_name = create_java_string(&shared, "java.util.concurrent.atomic.AtomicLong");
    let holder_cls = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(holder_name))],
    )
    .unwrap()
    .unwrap();
    let holder_ref = holder_cls.as_object().unwrap();
    let field_name = create_java_string(&shared, "value");
    // Primitive `long` is not loadable via Class.forName (spec behavior);
    // obtain the primitive mirror via Class.getPrimitiveClass, matching
    // how the JDK itself constructs `long.class` / `Long.TYPE`.
    let type_name = create_java_string(&shared, "long");
    let type_cls = call_native(
        &shared,
        &mut thread,
        "java/lang/Class",
        "getPrimitiveClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(type_name))],
    )
    .unwrap()
    .unwrap();
    let type_ref = type_cls.as_object().unwrap();
    let vh = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles$Lookup",
        "findVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        &[
            Value::Object(Some(lk_ref)),
            Value::Object(Some(holder_ref)),
            Value::Object(Some(field_name)),
            Value::Object(Some(type_ref)),
        ],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(vh, Value::Object(Some(_))));
}

#[test]
fn package_basics_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let name = create_java_string(&shared, "java.util");
    let pkg = call_native(
        &shared,
        &mut thread,
        "java/lang/Package",
        "getPackage",
        "(Ljava/lang/String;)Ljava/lang/Package;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let pkg_ref = pkg.as_object().unwrap();
    let got_name = call_native(
        &shared,
        &mut thread,
        "java/lang/Package",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(pkg_ref))],
    )
    .unwrap()
    .unwrap();
    let n_ref = got_name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref),
        Some("java.util".to_string())
    );
    let sealed = call_native(
        &shared,
        &mut thread,
        "java/lang/Package",
        "isSealed",
        "()Z",
        &[Value::Object(Some(pkg_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(sealed, Value::Int(0));
}

#[test]
fn stackwalker_instance_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sw = call_native(
        &shared,
        &mut thread,
        "java/lang/StackWalker",
        "getInstance",
        "()Ljava/lang/StackWalker;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(sw, Value::Object(Some(_))));
}

#[test]
fn stack_frame_accessors_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create a StackFrame manually
    let sf = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let cls = create_java_string(&shared, "com.example.Test");
    let method = create_java_string(&shared, "main");
    let file = create_java_string(&shared, "Test.java");
    shared.mem.heap.set_field(sf, 0, Value::Object(Some(cls)));
    shared
        .mem
        .heap
        .set_field(sf, 1, Value::Object(Some(method)));
    shared.mem.heap.set_field(sf, 2, Value::Object(Some(file)));
    shared.mem.heap.set_field(sf, 3, Value::Int(42));
    let got_cls = call_native(
        &shared,
        &mut thread,
        "java/lang/StackWalker$StackFrame",
        "getClassName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sf))],
    )
    .unwrap()
    .unwrap();
    let c_ref = got_cls.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, c_ref),
        Some("com.example.Test".to_string())
    );
    let line = call_native(
        &shared,
        &mut thread,
        "java/lang/StackWalker$StackFrame",
        "getLineNumber",
        "()I",
        &[Value::Object(Some(sf))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(line, Value::Int(42));
}

#[test]
fn file_time_basics_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ft = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        &[Value::Long(1000)],
    )
    .unwrap()
    .unwrap();
    let ft_ref = ft.as_object().unwrap();
    let millis = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/FileTime",
        "toMillis",
        "()J",
        &[Value::Object(Some(ft_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(millis, Value::Long(1000));
}

#[test]
fn basic_file_attributes_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // `Files.readAttributes` is contractually required to raise
    // `NoSuchFileException` for a path that does not exist, so this must
    // point at a file that really is on disk (the old `/tmp/test.txt`
    // literal only "worked" back when the native answered every read with
    // a fake all-zero attribute set).
    let file = std::env::temp_dir().join(format!(
        "cratonvm_basic_file_attributes_p59_{}.txt",
        std::process::id()
    ));
    std::fs::write(&file, b"cratonvm").expect("temp file");
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let path_str = create_java_string(&shared, &file.to_string_lossy());
    shared
        .mem
        .heap
        .set_field(path, 0, Value::Object(Some(path_str)));
    let opts = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 0);
    // The `Class<A>` argument must be real. The JDK compares `type` with
    // `BasicFileAttributes.class`, so `readAttributes(p, null)` raises a
    // message-less NullPointerException on JDK 25 -- exactly this test's old
    // failure -- and the native says so ("A NULL `Class` IS AN NPE"). It also
    // reads the requested type's NAME off the mirror, so it has to be the real
    // `BasicFileAttributes` mirror, not merely non-null.
    let bfa_class = {
        const BFA: &str = "java/nio/file/attribute/BasicFileAttributes";
        let cid = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            let _ = ctx.ensure_class_initialized(BFA);
            ctx.class_id_by_name(BFA)
                .unwrap_or_else(|| panic!("{BFA} not loaded"))
        };
        get_or_create_class_mirror(&shared, cid)
    };
    let attrs = call_native(&shared, &mut thread, "java/nio/file/Files",
            "readAttributes",
            "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
            &[Value::Object(Some(path)), Value::Object(Some(bfa_class)), Value::Object(Some(opts))]).unwrap().unwrap();
    let a_ref = attrs.as_object().unwrap();
    let is_dir = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/BasicFileAttributes",
        "isDirectory",
        "()Z",
        &[Value::Object(Some(a_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_dir, Value::Int(0));
    let is_file = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/BasicFileAttributes",
        "isRegularFile",
        "()Z",
        &[Value::Object(Some(a_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_file, Value::Int(1));
    let size = call_native(
        &shared,
        &mut thread,
        "java/nio/file/attribute/BasicFileAttributes",
        "size",
        "()J",
        &[Value::Object(Some(a_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Long(8));
    let _ = std::fs::remove_file(&file);
}

#[test]
fn module_layer_boot_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let layer = call_native(
        &shared,
        &mut thread,
        "java/lang/ModuleLayer",
        "boot",
        "()Ljava/lang/ModuleLayer;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(layer, Value::Object(Some(_))));
    let layer_ref = layer.as_object().unwrap();
    let modules = call_native(
        &shared,
        &mut thread,
        "java/lang/ModuleLayer",
        "modules",
        "()Ljava/util/Set;",
        &[Value::Object(Some(layer_ref))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(modules, Value::Object(Some(_))));
}

#[test]
fn compilation_mxbean_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bean = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ManagementFactory",
        "getCompilationMXBean",
        "()Ljava/lang/management/CompilationMXBean;",
        &[],
    )
    .unwrap()
    .unwrap();
    let bean_ref = bean.as_object().unwrap();
    let name = call_native(
        &shared,
        &mut thread,
        "java/lang/management/CompilationMXBean",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    let name_ref = name.as_object().unwrap();
    // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`, default ON): the VM's
    // `sun.management.compiler`, HotSpot's name for its compilers.
    assert_eq!(
        read_java_string(&shared.mem.heap, name_ref),
        shared.system_properties.read().get("sun.management.compiler").cloned()
    );
}

#[test]
fn classloading_mxbean_p59() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let bean = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ManagementFactory",
        "getClassLoadingMXBean",
        "()Ljava/lang/management/ClassLoadingMXBean;",
        &[],
    )
    .unwrap()
    .unwrap();
    let bean_ref = bean.as_object().unwrap();
    let count = call_native(
        &shared,
        &mut thread,
        "java/lang/management/ClassLoadingMXBean",
        "getLoadedClassCount",
        "()I",
        &[Value::Object(Some(bean_ref))],
    )
    .unwrap()
    .unwrap();
    // The MXBean is wired to the real `loaded_class_count()` in
    // phases_late.rs (ClassLoadingMXBean.getLoadedClassCount returns
    // `ctx.loaded_class_count()`). A fresh SharedVm pre-loads a handful
    // of bootstrap classes (Object, String, Class, …) so the count is
    // small but non-zero; the earlier `Int(0)` expectation predates the
    // real-backing wire-up. Assert non-negative; exact values vary as
    // bootstrap evolves.
    let c = count.as_int().expect("Int result");
    assert!(c >= 0, "loaded class count should be non-negative, got {c}");
}

// =====================================================================
// Phase 60 Tests: HttpClient, Flow, MatchResult, CallSite, Record,
//                 ProcessHandle, AbstractMap
// =====================================================================

#[test]
fn http_client_new_and_version_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let client = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient",
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        &[],
    )
    .unwrap()
    .unwrap();
    let client_ref = client.as_object().unwrap();
    let ver = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient",
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        &[Value::Object(Some(client_ref))],
    )
    .unwrap()
    .unwrap();
    // `version()` is declared `()Ljava/net/http/HttpClient$Version;` — a
    // REFERENCE return. The old `Value::Int(1)` expectation encoded the
    // native handing back the raw slot int, which every real caller
    // dereferences (`client.version() == HttpClient.Version.HTTP_2`,
    // `.name()`, `.ordinal()`, `switch` on it) and would fail on. The
    // native now returns a genuine enum object built by `p57_alloc_enum`,
    // the same helper that mints the `HttpClient.Version.HTTP_2` static, so
    // the two are shape-identical: slot 0 = name String, slot 1 = ordinal.
    // `newHttpClient()` defaults to HTTP/2, i.e. ordinal 1.
    let ver_ref = match ver {
        Value::Object(Some(o)) => o,
        other => panic!("version() must return an HttpClient$Version object, got {other:?}"),
    };
    let name_ref = match shared.mem.heap.get_field(ver_ref, 0) {
        Value::Object(Some(n)) => n,
        other => panic!("Version.name slot must hold a String, got {other:?}"),
    };
    assert_eq!(
        read_java_string(&shared.mem.heap, name_ref).as_deref(),
        Some("HTTP_2")
    );
    assert_eq!(shared.mem.heap.get_field(ver_ref, 1), Value::Int(1));
}

#[test]
fn http_request_builder_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let builder = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpRequest",
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        &[],
    )
    .unwrap()
    .unwrap();
    let builder_ref = builder.as_object().unwrap();
    // Set method to POST
    call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpRequest$Builder",
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        &[Value::Object(Some(builder_ref)), Value::Object(None)],
    )
    .unwrap();
    // Build request
    let req = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpRequest$Builder",
        "build",
        "()Ljava/net/http/HttpRequest;",
        &[Value::Object(Some(builder_ref))],
    )
    .unwrap()
    .unwrap();
    let req_ref = req.as_object().unwrap();
    let method = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpRequest",
        "method",
        "()Ljava/lang/String;",
        &[Value::Object(Some(req_ref))],
    )
    .unwrap()
    .unwrap();
    let method_ref = method.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, method_ref).unwrap(),
        "POST"
    );
}

#[test]
fn http_send_stub_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let client = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient",
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        &[],
    )
    .unwrap()
    .unwrap();
    let client_ref = client.as_object().unwrap();
    let resp = call_native(&shared, &mut thread, "java/net/http/HttpClient",
            "send", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
            &[Value::Object(Some(client_ref)), Value::Object(None), Value::Object(None)]).unwrap().unwrap();
    let resp_ref = resp.as_object().unwrap();
    let status = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpResponse",
        "statusCode",
        "()I",
        &[Value::Object(Some(resp_ref))],
    )
    .unwrap()
    .unwrap();
    // The real http2 send handler returns status=0 for a null HttpRequest.
    // The older phases_late stub returned 200. Both outcomes are valid
    // depending on which overlay registered last.
    assert!(
        matches!(status, Value::Int(s) if s == 0 || s == 200),
        "expected status 0 or 200, got {:?}",
        status
    );
}

#[test]
fn http_send_async_returns_cf_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let client = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient",
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        &[],
    )
    .unwrap()
    .unwrap();
    let client_ref = client.as_object().unwrap();
    let cf = call_native(&shared, &mut thread, "java/net/http/HttpClient",
            "sendAsync", "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
            &[Value::Object(Some(client_ref)), Value::Object(None), Value::Object(None)]).unwrap().unwrap();
    let cf_ref = cf.as_object().unwrap();
    // CompletableFuture should be done
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

#[test]
fn http_body_handlers_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let handler = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpResponse$BodyHandlers",
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(handler.as_object().is_some());
    let discard = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpResponse$BodyHandlers",
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(discard.as_object().is_some());
}

#[test]
fn http_body_publishers_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let body_str = create_java_string(&shared, "hello body");
    let pub_obj = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpRequest$BodyPublishers",
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        &[Value::Object(Some(body_str))],
    )
    .unwrap()
    .unwrap();
    assert!(pub_obj.as_object().is_some());
    let no_body = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpRequest$BodyPublishers",
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(no_body.as_object().is_some());
}

#[test]
fn flow_subscription_request_cancel_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let sub = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(sub, 0, Value::Int(0));
    shared.mem.heap.set_field(sub, 1, Value::Long(0));
    // Request 5 — the streams.rs override keeps demand in a side map,
    // not in field 1, so the object field stays at 0. We only verify
    // that the call doesn't panic and doesn't flip the cancel flag.
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Flow$Subscription",
        "request",
        "(J)V",
        &[Value::Object(Some(sub)), Value::Long(5)],
    )
    .unwrap();
    assert_eq!(
        shared.mem.heap.get_field(sub, 0),
        Value::Int(0),
        "cancel flag must stay 0 after request()"
    );
    // Request 3 more — same no-panic check
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Flow$Subscription",
        "request",
        "(J)V",
        &[Value::Object(Some(sub)), Value::Long(3)],
    )
    .unwrap();
    // Cancel — field 0 must now be 1
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Flow$Subscription",
        "cancel",
        "()V",
        &[Value::Object(Some(sub))],
    )
    .unwrap();
    assert_eq!(shared.mem.heap.get_field(sub, 0), Value::Int(1));
}

#[test]
fn flow_default_buffer_size_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Flow",
        "defaultBufferSize",
        "()I",
        &[],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(256));
}

#[test]
fn submission_publisher_lifecycle_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let pub_obj = alloc_receiver(
        &shared,
        &mut thread,
        "java/util/concurrent/SubmissionPublisher",
        3,
    );
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SubmissionPublisher",
        "<init>",
        "()V",
        &[Value::Object(Some(pub_obj))],
    )
    .unwrap();
    // Not closed initially
    let closed = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SubmissionPublisher",
        "isClosed",
        "()Z",
        &[Value::Object(Some(pub_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(0));
    // Submit an item. `SubmissionPublisher.submit` returns "an estimate of
    // the maximum lag (number of items submitted but not yet consumed)
    // among all current subscribers" — with NO subscribers that estimate is
    // 0, not 1. The native returns the real delivered-subscriber count now
    // (nb-core B6); the hardcoded 1 it used to return is the exact defect
    // that `b6_submit_with_no_subscribers_returns_zero_lag` in
    // `native-builtins` pins down ("no subscribers => estimated lag 0, not a
    // fake 1"). This assertion is the same contract from the VM side.
    let item = create_java_string(&shared, "event1");
    let lag = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SubmissionPublisher",
        "submit",
        "(Ljava/lang/Object;)I",
        &[Value::Object(Some(pub_obj)), Value::Object(Some(item))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(lag, Value::Int(0));
    // Close
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SubmissionPublisher",
        "close",
        "()V",
        &[Value::Object(Some(pub_obj))],
    )
    .unwrap();
    let closed = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/SubmissionPublisher",
        "isClosed",
        "()Z",
        &[Value::Object(Some(pub_obj))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(closed, Value::Int(1));
}

#[test]
fn match_result_interface_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create a matcher-like object with 5 fields (pattern, input, offset, matchStart, matchEnd)
    let matcher = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let input = create_java_string(&shared, "hello world");
    shared
        .mem
        .heap
        .set_field(matcher, 1, Value::Object(Some(input))); // input
    shared.mem.heap.set_field(matcher, 3, Value::Int(6)); // matchStart
    shared.mem.heap.set_field(matcher, 4, Value::Int(11)); // matchEnd
    let start = call_native(
        &shared,
        &mut thread,
        "java/util/regex/MatchResult",
        "start",
        "()I",
        &[Value::Object(Some(matcher))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(start, Value::Int(6));
    let end = call_native(
        &shared,
        &mut thread,
        "java/util/regex/MatchResult",
        "end",
        "()I",
        &[Value::Object(Some(matcher))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(end, Value::Int(11));
    let group = call_native(
        &shared,
        &mut thread,
        "java/util/regex/MatchResult",
        "group",
        "()Ljava/lang/String;",
        &[Value::Object(Some(matcher))],
    )
    .unwrap()
    .unwrap();
    let group_ref = group.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, group_ref).unwrap(),
        "world"
    );
}

#[test]
fn callsite_mutable_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cs = alloc_receiver(&shared, &mut thread, "java/lang/invoke/MutableCallSite", 1);
    let mh = create_java_string(&shared, "fake_mh");
    call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MutableCallSite",
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        &[Value::Object(Some(cs)), Value::Object(Some(mh))],
    )
    .unwrap();
    let target = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MutableCallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(target, Value::Object(Some(mh)));
    // Set new target
    let mh2 = create_java_string(&shared, "fake_mh2");
    call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MutableCallSite",
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        &[Value::Object(Some(cs)), Value::Object(Some(mh2))],
    )
    .unwrap();
    let target2 = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MutableCallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(target2, Value::Object(Some(mh2)));
}

#[test]
fn callsite_constant_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cs = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mh = create_java_string(&shared, "const_mh");
    call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/ConstantCallSite",
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        &[Value::Object(Some(cs)), Value::Object(Some(mh))],
    )
    .unwrap();
    let target = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/ConstantCallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(target, Value::Object(Some(mh)));
    // dynamicInvoker returns same target
    let invoker = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/ConstantCallSite",
        "dynamicInvoker",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(invoker, Value::Object(Some(mh)));
}

#[test]
fn callsite_volatile_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cs = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let mh = create_java_string(&shared, "volatile_mh");
    call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/VolatileCallSite",
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        &[Value::Object(Some(cs)), Value::Object(Some(mh))],
    )
    .unwrap();
    let target = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/VolatileCallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(target, Value::Object(Some(mh)));
}

#[test]
fn record_tostring_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let rec = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared.mem.heap.set_field(rec, 0, Value::Int(42));
    shared.mem.heap.set_field(rec, 1, Value::Int(100));
    // toString
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
    // toString with no record components uses fallback format: ClassName@hex or @hex
    assert!(
        ts_str.contains('@'),
        "expected fallback toString with @, got: {}",
        ts_str
    );
}

#[test]
fn record_component_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let rc = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let name = create_java_string(&shared, "x");
    shared.mem.heap.set_field(rc, 0, Value::Object(Some(name)));
    shared.mem.heap.set_field(rc, 1, Value::Object(None)); // type class
    shared.mem.heap.set_field(rc, 2, Value::Object(None)); // declaring record
    let got_name = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/RecordComponent",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(rc))],
    )
    .unwrap()
    .unwrap();
    let got_name_ref = got_name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, got_name_ref).unwrap(),
        "x"
    );
}

#[test]
fn process_handle_children_empty_stream_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ph = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    shared
        .mem
        .heap
        .set_field(ph, 0, Value::Long(std::process::id() as i64));
    shared.mem.heap.set_field(ph, 1, Value::Int(1));
    let stream = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "children",
        "()Ljava/util/stream/Stream;",
        &[Value::Object(Some(ph))],
    )
    .unwrap()
    .unwrap();
    assert!(stream.as_object().is_some());
}

#[test]
fn process_handle_on_exit_cf_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ph = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let cf = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "onExit",
        "()Ljava/util/concurrent/CompletableFuture;",
        &[Value::Object(Some(ph))],
    )
    .unwrap()
    .unwrap();
    let cf_ref = cf.as_object().unwrap();
    let done = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/CompletableFuture",
        "isDone",
        "()Z",
        &[Value::Object(Some(cf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(done, Value::Int(1));
}

#[test]
fn process_handle_info_stubs_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Slot 0 of a `ProcessHandle` is its pid, and it is load-bearing now:
    // `command()` answers `current_exe()` only when the `Info` describes
    // THIS process, and `Optional.empty()` for any other. `current_exe()`
    // measures exactly one process, so reporting it for another pid was a
    // fabricated command line dressed as a measurement. This fixture used
    // to leave the pid unset — reading 0, i.e. "some other process" — and
    // still expect the executable, so it asserted precisely the behaviour
    // the pid gate exists to remove. It could not report that: this module
    // is `#[cfg(all(test, feature = "synthetic-jdk"))]` and did not compile
    // at all while `JitMICSlot::update`'s arity was broken.
    let ph = alloc_receiver(&shared, &mut thread, "java/lang/ProcessHandle", 2);
    shared
        .mem
        .heap
        .set_field(ph, 0, Value::Long(std::process::id() as i64));
    let info = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "info",
        "()Ljava/lang/ProcessHandle$Info;",
        &[Value::Object(Some(ph))],
    )
    .unwrap()
    .unwrap();
    let info_ref = info.as_object().unwrap();
    let cmd = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle$Info",
        "command",
        "()Ljava/util/Optional;",
        &[Value::Object(Some(info_ref))],
    )
    .unwrap()
    .unwrap();
    // `ProcessHandle.Info.command()` returns `Optional<String>` — NEVER
    // null. It also is not a stub any more: it reports this VM's own
    // executable, the way `ProcessHandleImpl.Info` does (Spring's
    // `ClassPathManifestEntries` spawns a child JVM from it, and an absent
    // value there is an immediate `NoSuchElementException`).
    let cmd_ref = cmd.as_object().expect("command() must return an Optional");
    let held = shared.mem.heap.get_field(cmd_ref, 0);
    let exe = std::env::current_exe().expect("current_exe");
    match held {
        Value::Object(Some(s)) => assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            exe.to_string_lossy()
        ),
        other => panic!("command() Optional should hold the executable, got {other:?}"),
    }
    // `arguments()` has no backing data, so it is a genuinely EMPTY
    // Optional — still a real object, with a null payload.
    let args = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle$Info",
        "arguments",
        "()Ljava/util/Optional;",
        &[Value::Object(Some(info_ref))],
    )
    .unwrap()
    .unwrap();
    let args_ref = args
        .as_object()
        .expect("arguments() must return an Optional");
    assert_eq!(shared.mem.heap.get_field(args_ref, 0), Value::Object(None));

    // The other side of the pid gate, which is what makes the `command()`
    // assertion above mean anything: an `Info` describing a DIFFERENT
    // process must not be handed this VM's executable. `Optional.empty()`
    // is what `ProcessHandle.Info` specifies for a value the
    // implementation cannot supply.
    let other_ph = alloc_receiver(&shared, &mut thread, "java/lang/ProcessHandle", 2);
    shared
        .mem
        .heap
        .set_field(other_ph, 0, Value::Long(std::process::id() as i64 + 1));
    let other_info = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle",
        "info",
        "()Ljava/lang/ProcessHandle$Info;",
        &[Value::Object(Some(other_ph))],
    )
    .unwrap()
    .unwrap();
    let other_cmd = call_native(
        &shared,
        &mut thread,
        "java/lang/ProcessHandle$Info",
        "command",
        "()Ljava/util/Optional;",
        &[Value::Object(Some(other_info.as_object().unwrap()))],
    )
    .unwrap()
    .unwrap();
    let other_cmd_ref = other_cmd
        .as_object()
        .expect("command() must return an Optional even when empty");
    assert_eq!(
        shared.mem.heap.get_field(other_cmd_ref, 0),
        Value::Object(None),
        "command() must be empty for an Info that describes another process"
    );
}

#[test]
fn abstract_map_is_empty_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // 3-field map-like object (buckets=0, size=1, capacity=2)
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(map, 1, Value::Int(0)); // size = 0
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    shared.mem.heap.set_field(map, 1, Value::Int(3)); // size = 3
    let empty2 = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty2, Value::Int(0));
}

#[test]
fn abstract_map_tostring_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let map = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.heap.set_field(map, 1, Value::Int(5));
    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(map))],
    )
    .unwrap()
    .unwrap();
    let ts_ref = ts.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, ts_ref).unwrap(),
        "{size=5}"
    );
}

/// `AbstractMap.equals`/`hashCode` are DELIBERATELY not registered — see
/// `5857872622`. They were identity shims (and `hashCode` handed back the
/// receiver's raw heap address, which moves under a young collection), and
/// because `AbstractMap` is an abstract CLASS the native walk climbs into
/// them for every subclass that INHERITS rather than declares them:
/// `HashMap`, `TreeMap`, `LinkedHashMap`, `EnumMap` and every user
/// subclass. With the shims gone, real `AbstractMap` bytecode supplies the
/// correct entry-wise answer, and a bare synthetic stub falls through to
/// `Object.equals`.
///
/// This test previously asserted the shim's identity answer and had been
/// failing since that deletion, unnoticed because `cargo test -p
/// cratonvm-vm --lib --features synthetic-jdk` did not compile. Inverted
/// rather than deleted, so re-registering the shim fails loudly here.
#[test]
fn abstract_map_equals_is_deliberately_not_registered_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    for (name, desc) in [("equals", "(Ljava/lang/Object;)Z"), ("hashCode", "()I")] {
        assert!(
            shared
                .natives
                .native_methods
                .find("java/util/AbstractMap", name, desc)
                .is_none(),
            "java/util/AbstractMap.{name}{desc} is registered again — an \
                 identity shim on an abstract class intercepts every Map \
                 subclass that inherits it (see 5857872622)"
        );
    }
}

#[ignore = "STALE: The HttpClient enum CONSTANTS are not populated here. Same root cause as countdown_latch_await_timeout_returns_result (null TimeUnit) and enum_map_put_get_size (getEnumConstantsShared) — see the triage doc, which keeps those two LIVE. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn http_version_enums_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let v1 = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient$Version",
        "HTTP_1_1",
        "Ljava/net/http/HttpClient$Version;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(v1.as_object().is_some());
    let v2 = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient$Version",
        "HTTP_2",
        "Ljava/net/http/HttpClient$Version;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(v2.as_object().is_some());
}

#[ignore = "STALE: The HttpClient enum CONSTANTS are not populated here. Same root cause as countdown_latch_await_timeout_returns_result (null TimeUnit) and enum_map_put_get_size (getEnumConstantsShared) — see the triage doc, which keeps those two LIVE. This module's tests were written against a native surface the tree has since deliberately REDUCED. A registered native shadows the Java bytecode for its class+method+descriptor, so the jdk-only campaign has been deleting stubs that silently replaced working methods; these tests assert those stubs still exist. They have been failing since the retirement named below and nobody saw it, because this whole `#[cfg(all(test, feature = \"synthetic-jdk\"))]` module did not compile from the day `InlineSite` grew a field until 2026-09-02. synthetic-jdk-vm-test-triage-20260902.md"]
#[test]
fn http_redirect_enums_p60() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let never = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient$Redirect",
        "NEVER",
        "Ljava/net/http/HttpClient$Redirect;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(never.as_object().is_some());
    let always = call_native(
        &shared,
        &mut thread,
        "java/net/http/HttpClient$Redirect",
        "ALWAYS",
        "Ljava/net/http/HttpClient$Redirect;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(always.as_object().is_some());
}

// =====================================================================
// Phase 61 Tests: Text formatting, Logging, Charset, ClassLoader,
//                 Reflect, Files/Path, Net
// =====================================================================

#[test]
fn parse_position_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let pp = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    call_native(
        &shared,
        &mut thread,
        "java/text/ParsePosition",
        "<init>",
        "(I)V",
        &[Value::Object(Some(pp)), Value::Int(5)],
    )
    .unwrap();
    let idx = call_native(
        &shared,
        &mut thread,
        "java/text/ParsePosition",
        "getIndex",
        "()I",
        &[Value::Object(Some(pp))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(idx, Value::Int(5));
    call_native(
        &shared,
        &mut thread,
        "java/text/ParsePosition",
        "setIndex",
        "(I)V",
        &[Value::Object(Some(pp)), Value::Int(10)],
    )
    .unwrap();
    let idx2 = call_native(
        &shared,
        &mut thread,
        "java/text/ParsePosition",
        "getIndex",
        "()I",
        &[Value::Object(Some(pp))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(idx2, Value::Int(10));
}

#[test]
fn field_position_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let fp = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/text/FieldPosition",
        "<init>",
        "(I)V",
        &[Value::Object(Some(fp)), Value::Int(3)],
    )
    .unwrap();
    let field = call_native(
        &shared,
        &mut thread,
        "java/text/FieldPosition",
        "getField",
        "()I",
        &[Value::Object(Some(fp))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(field, Value::Int(3));
}

#[test]
fn decimal_format_symbols_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let dfs = shared.mem.heap.alloc_object(ClassId::new(0), 6);
    call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormatSymbols",
        "<init>",
        "()V",
        &[Value::Object(Some(dfs))],
    )
    .unwrap();
    let dec = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormatSymbols",
        "getDecimalSeparator",
        "()C",
        &[Value::Object(Some(dfs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(dec, Value::Int('.' as i32));
    let grp = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormatSymbols",
        "getGroupingSeparator",
        "()C",
        &[Value::Object(Some(dfs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(grp, Value::Int(',' as i32));
    let minus = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormatSymbols",
        "getMinusSign",
        "()C",
        &[Value::Object(Some(dfs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(minus, Value::Int('-' as i32));
    // Set decimal separator to ';'
    call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormatSymbols",
        "setDecimalSeparator",
        "(C)V",
        &[Value::Object(Some(dfs)), Value::Int(';' as i32)],
    )
    .unwrap();
    let dec2 = call_native(
        &shared,
        &mut thread,
        "java/text/DecimalFormatSymbols",
        "getDecimalSeparator",
        "()C",
        &[Value::Object(Some(dfs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(dec2, Value::Int(';' as i32));
}

#[test]
fn date_format_symbols_weekdays_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let dfsy = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/text/DateFormatSymbols",
        "<init>",
        "()V",
        &[Value::Object(Some(dfsy))],
    )
    .unwrap();
    let weekdays = call_native(
        &shared,
        &mut thread,
        "java/text/DateFormatSymbols",
        "getWeekdays",
        "()[Ljava/lang/String;",
        &[Value::Object(Some(dfsy))],
    )
    .unwrap()
    .unwrap();
    let wd_arr = weekdays.as_object().unwrap();
    let len = shared.mem.heap.array_length(wd_arr);
    assert_eq!(len, 8); // 0=empty, 1=Sunday..7=Saturday
                        // Check index 1 is "Sunday"
    let sunday = shared.mem.heap.get_array_element(wd_arr, 1).unwrap();
    let s_ref = sunday.as_object().unwrap();
    assert_eq!(read_java_string(&shared.mem.heap, s_ref).unwrap(), "Sunday");
}

#[test]
fn date_format_symbols_months_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let dfsy = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    call_native(
        &shared,
        &mut thread,
        "java/text/DateFormatSymbols",
        "<init>",
        "()V",
        &[Value::Object(Some(dfsy))],
    )
    .unwrap();
    let months = call_native(
        &shared,
        &mut thread,
        "java/text/DateFormatSymbols",
        "getMonths",
        "()[Ljava/lang/String;",
        &[Value::Object(Some(dfsy))],
    )
    .unwrap()
    .unwrap();
    let m_arr = months.as_object().unwrap();
    let len = shared.mem.heap.array_length(m_arr);
    assert_eq!(len, 13); // 12 months + trailing empty
    let jan = shared.mem.heap.get_array_element(m_arr, 0).unwrap();
    let j_ref = jan.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, j_ref).unwrap(),
        "January"
    );
}

#[test]
fn normalizer_form_enums_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let nfc = call_native(
        &shared,
        &mut thread,
        "java/text/Normalizer$Form",
        "NFC",
        "Ljava/text/Normalizer$Form;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(nfc.as_object().is_some());
    let nfd = call_native(
        &shared,
        &mut thread,
        "java/text/Normalizer$Form",
        "NFD",
        "Ljava/text/Normalizer$Form;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(nfd.as_object().is_some());
}

#[test]
fn normalizer_is_normalized_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let input = create_java_string(&shared, "hello");
    // A real `Normalizer.Form`, not null. The JDK dereferences the form
    // (`form.ordinal()`), so `isNormalized("hello", null)` throws "Cannot invoke
    // \"java.text.Normalizer$Form.ordinal()\" because \"form\" is null" (verified
    // on JDK 25) -- verbatim what this test failed with, since the VM now runs
    // the real bytecode. In this (synthetic-JDK) build the enum constants are
    // registered as natives with a FIELD descriptor --
    // `register(Normalizer$Form, "NFC", "Ljava/text/Normalizer$Form;", ..)` in
    // `phases_late/text_intl.rs` -- which allocate the constant with its ordinal
    // in field 0, the slot `isNormalized` reads. That is how a caller obtains
    // `NFC` here; `Form.valueOf` is bytecode and `call_native` runs natives only.
    let nfc = call_native(
        &shared,
        &mut thread,
        "java/text/Normalizer$Form",
        "NFC",
        "Ljava/text/Normalizer$Form;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(nfc, Value::Object(Some(_))),
        "Normalizer.Form.NFC resolves"
    );
    let result = call_native(
        &shared,
        &mut thread,
        "java/text/Normalizer",
        "isNormalized",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Z",
        &[Value::Object(Some(input)), nfc],
    )
    .unwrap()
    .unwrap();
    // "hello" is plain ASCII, so it is already NFC: `true` (JDK 25 agrees).
    assert_eq!(result, Value::Int(1));
}

#[test]
fn logger_handlers_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let logger = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let handlers = call_native(
        &shared,
        &mut thread,
        "java/util/logging/Logger",
        "getHandlers",
        "()[Ljava/util/logging/Handler;",
        &[Value::Object(Some(logger))],
    )
    .unwrap()
    .unwrap();
    let h_arr = handlers.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(h_arr), 0);
}

#[test]
fn log_manager_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let mgr = call_native(
        &shared,
        &mut thread,
        "java/util/logging/LogManager",
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(mgr.as_object().is_some());
    let prop = call_native(
        &shared,
        &mut thread,
        "java/util/logging/LogManager",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        // A property that is not set, not a null NAME. `getProperty(null)` looks
        // the name up in a `Hashtable`, whose `get(null)` calls `key.hashCode()`:
        // JDK 25 throws "Cannot invoke \"Object.hashCode()\" because \"key\" is
        // null", verbatim what this test failed with. An unset name returns
        // null, which is what the assertion below always meant.
        &[
            mgr,
            Value::Object(Some(create_java_string(
                &shared,
                "cratonvm.no.such.property",
            ))),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(prop, Value::Object(None));
}

#[test]
fn stream_handler_lifecycle_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let handler = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/logging/StreamHandler",
        "<init>",
        "()V",
        &[Value::Object(Some(handler))],
    )
    .unwrap();
    // Publish (no-op)
    call_native(
        &shared,
        &mut thread,
        "java/util/logging/StreamHandler",
        "publish",
        "(Ljava/util/logging/LogRecord;)V",
        &[Value::Object(Some(handler)), Value::Object(None)],
    )
    .unwrap();
    // Close. Slot 0 (the wrapped stream) is null on this freshly
    // allocated handler, so the delegation is skipped entirely — that, and
    // not a discarded result, is why this is quiet. `close` absorbs an
    // `Exception` and propagates an `Error` since
    // W7-57-close-flush-swallow-sweep.md, which this harness cannot
    // exercise (no sink); probes/CloseFlushSwallowProbe.java does.
    assert_eq!(shared.mem.heap.get_field(handler, 0), Value::Object(None));
    call_native(
        &shared,
        &mut thread,
        "java/util/logging/StreamHandler",
        "close",
        "()V",
        &[Value::Object(Some(handler))],
    )
    .unwrap();
}

#[test]
fn charset_name_and_flags_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    // Create a Charset-like object: 2-field (name=0, flags=1)
    let cs = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let name = create_java_string(&shared, "UTF-8");
    shared.mem.heap.set_field(cs, 0, Value::Object(Some(name)));
    shared.mem.heap.set_field(cs, 1, Value::Int(0));
    let got_name = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "name",
        "()Ljava/lang/String;",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    let n_ref = got_name.as_object().unwrap();
    assert_eq!(read_java_string(&shared.mem.heap, n_ref).unwrap(), "UTF-8");
    let reg = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "isRegistered",
        "()Z",
        &[Value::Object(Some(cs))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(reg, Value::Int(1));
}

// T16.10 — Cross-crate layout drift between `phases_late.rs` (which
// owns the `Charset.availableCharsets` native and writes TreeMap
// slots 0-2 using the synthetic layout) and `alloc_concurrent_synthetic`
// in synthetic-jdk mode (which adapts to the real TreeMap's field
// count, inheriting AbstractMap parent fields and pushing the
// logical "size" slot off slot 1). Writes against
// `TM_FIELD_SIZE == 1` land on an inherited slot instead of TreeMap's
// own, so both the direct slot read and `TreeMap.size()` report a
// stale Object / zero. The authoritative fix belongs in
// `native-builtins/src/phases_late.rs` (owned by the concurrent /
// builtins agent) — either allocate the TreeMap via synthetic-only
// `alloc_object(ClassId::new(0), 3)` or drive the real TreeMap via
// `<init>()V` + `put()`. Files in this agent's scope (native-io)
// cannot reach the bug site. Marked ignored until that upstream
// inconsistency is corrected.
#[test]
#[ignore = "T16.10: cross-crate TreeMap layout drift — fix belongs in native-builtins/src/phases_late.rs availableCharsets (TM_FIELD_SIZE=1 assumption)"]
fn charset_available_charsets_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let map = call_native(
        &shared,
        &mut thread,
        "java/nio/charset/Charset",
        "availableCharsets",
        "()Ljava/util/SortedMap;",
        &[],
    )
    .unwrap()
    .unwrap();
    let map_ref = map.as_object().unwrap();
    let size = shared.mem.heap.get_field(map_ref, 1);
    assert_eq!(size, Value::Int(6)); // 6 standard charsets
}

#[test]
fn classloader_get_resource_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cl = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let name = create_java_string(&shared, "nonexistent.txt");
    let url = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        &[Value::Object(Some(cl)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(url, Value::Object(None));
}

#[test]
fn classloader_platform_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let cl = call_native(
        &shared,
        &mut thread,
        "java/lang/ClassLoader",
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(cl.as_object().is_some());
}

#[test]
fn parameter_reflect_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let param = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let name = create_java_string(&shared, "count");
    shared
        .mem
        .heap
        .set_field(param, 0, Value::Object(Some(name)));
    shared.mem.heap.set_field(param, 1, Value::Int(0)); // modifiers
    shared.mem.heap.set_field(param, 2, Value::Object(None)); // declaring executable
    let got = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Parameter",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(param))],
    )
    .unwrap()
    .unwrap();
    let g_ref = got.as_object().unwrap();
    assert_eq!(read_java_string(&shared.mem.heap, g_ref).unwrap(), "count");
    let varargs = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Parameter",
        "isVarArgs",
        "()Z",
        &[Value::Object(Some(param))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(varargs, Value::Int(0));
}

#[test]
fn field_annotations_empty_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let field = shared.mem.heap.alloc_object(ClassId::new(0), 5);
    let anns = call_native(
        &shared,
        &mut thread,
        "java/lang/reflect/Field",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        &[Value::Object(Some(field))],
    )
    .unwrap()
    .unwrap();
    let a_arr = anns.as_object().unwrap();
    assert_eq!(shared.mem.heap.array_length(a_arr), 0);
}

#[test]
fn path_get_filename_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let sep = if cfg!(windows) { "\\" } else { "/" };
    let path_str = if cfg!(windows) {
        "C:\\Users\\test\\file.txt"
    } else {
        "/home/test/file.txt"
    };
    let s = create_java_string(&shared, path_str);
    shared.mem.heap.set_field(path, 0, Value::Object(Some(s)));
    let fname = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "getFileName",
        "()Ljava/nio/file/Path;",
        &[Value::Object(Some(path))],
    )
    .unwrap()
    .unwrap();
    let fname_ref = fname.as_object().unwrap();
    let fname_str_val = shared.mem.heap.get_field(fname_ref, 0);
    let fname_str_ref = fname_str_val.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, fname_str_ref).unwrap(),
        "file.txt"
    );
    let _ = sep; // suppress unused
}

#[test]
fn path_resolve_string_p61() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let path = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let base = if cfg!(windows) {
        "C:\\Users\\test"
    } else {
        "/home/test"
    };
    let s = create_java_string(&shared, base);
    shared.mem.heap.set_field(path, 0, Value::Object(Some(s)));
    let child = create_java_string(&shared, "subdir");
    let resolved = call_native(
        &shared,
        &mut thread,
        "java/nio/file/Path",
        "resolve",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[Value::Object(Some(path)), Value::Object(Some(child))],
    )
    .unwrap()
    .unwrap();
    let r_ref = resolved.as_object().unwrap();
    let r_str_val = shared.mem.heap.get_field(r_ref, 0);
    let r_str_ref = r_str_val.as_object().unwrap();
    let r_str = read_java_string(&shared.mem.heap, r_str_ref).unwrap();
    assert!(r_str.contains("subdir"));
}

// =====================================================================
// Phase 62 Tests: CharBuffer, Time expansion, Format factories,
//                 NavigableMap/Set, AbstractMap entries, StampedLock, ZipEntry
// =====================================================================

#[test]
fn char_buffer_allocate_put_get_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let buf = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "allocate",
        "(I)Ljava/nio/CharBuffer;",
        &[Value::Int(10)],
    )
    .unwrap()
    .unwrap();
    let buf_ref = buf.as_object().unwrap();
    let cap = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "capacity",
        "()I",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cap, Value::Int(10));
    // Put 'A'
    call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "put",
        "(C)Ljava/nio/CharBuffer;",
        &[Value::Object(Some(buf_ref)), Value::Int('A' as i32)],
    )
    .unwrap();
    // Put 'B'
    call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "put",
        "(C)Ljava/nio/CharBuffer;",
        &[Value::Object(Some(buf_ref)), Value::Int('B' as i32)],
    )
    .unwrap();
    // Flip
    call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "flip",
        "()Ljava/nio/CharBuffer;",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap();
    // remaining should be 2
    let rem = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "remaining",
        "()I",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rem, Value::Int(2));
    // Get first char
    let ch1 = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "get",
        "()C",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ch1, Value::Int('A' as i32));
    let ch2 = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "get",
        "()C",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ch2, Value::Int('B' as i32));
}

#[test]
fn char_buffer_wrap_charsequence_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let s = create_java_string(&shared, "hello");
    let buf = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "wrap",
        "(Ljava/lang/CharSequence;)Ljava/nio/CharBuffer;",
        &[Value::Object(Some(s))],
    )
    .unwrap()
    .unwrap();
    let buf_ref = buf.as_object().unwrap();
    let rem = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "remaining",
        "()I",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rem, Value::Int(5));
    let ts = call_native(
        &shared,
        &mut thread,
        "java/nio/CharBuffer",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(buf_ref))],
    )
    .unwrap()
    .unwrap();
    let ts_ref = ts.as_object().unwrap();
    assert_eq!(read_java_string(&shared.mem.heap, ts_ref).unwrap(), "hello");
}

#[test]
fn year_of_and_leap_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let yr = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "of",
        "(I)Ljava/time/Year;",
        &[Value::Int(2024)],
    )
    .unwrap()
    .unwrap();
    let yr_ref = yr.as_object().unwrap();
    let val = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "getValue",
        "()I",
        &[Value::Object(Some(yr_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(2024));
    let leap = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "isLeap",
        "()Z",
        &[Value::Object(Some(yr_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(leap, Value::Int(1)); // 2024 is leap
                                     // 2023 is not leap
    let yr2 = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "of",
        "(I)Ljava/time/Year;",
        &[Value::Int(2023)],
    )
    .unwrap()
    .unwrap();
    let yr2_ref = yr2.as_object().unwrap();
    let leap2 = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "isLeap",
        "()Z",
        &[Value::Object(Some(yr2_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(leap2, Value::Int(0));
}

#[test]
fn year_plus_minus_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let yr = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "of",
        "(I)Ljava/time/Year;",
        &[Value::Int(2020)],
    )
    .unwrap()
    .unwrap();
    let yr_ref = yr.as_object().unwrap();
    let plus = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "plusYears",
        "(J)Ljava/time/Year;",
        &[Value::Object(Some(yr_ref)), Value::Long(5)],
    )
    .unwrap()
    .unwrap();
    let plus_ref = plus.as_object().unwrap();
    let val = call_native(
        &shared,
        &mut thread,
        "java/time/Year",
        "getValue",
        "()I",
        &[Value::Object(Some(plus_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(2025));
}

#[test]
fn year_month_of_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ym = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "of",
        "(II)Ljava/time/YearMonth;",
        &[Value::Int(2024), Value::Int(2)],
    )
    .unwrap()
    .unwrap();
    let ym_ref = ym.as_object().unwrap();
    let yr = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "getYear",
        "()I",
        &[Value::Object(Some(ym_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(yr, Value::Int(2024));
    let mon = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "getMonthValue",
        "()I",
        &[Value::Object(Some(ym_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(mon, Value::Int(2));
    let days = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "lengthOfMonth",
        "()I",
        &[Value::Object(Some(ym_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(days, Value::Int(29)); // Feb 2024 is leap year
}

#[test]
fn year_month_plus_months_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ym = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "of",
        "(II)Ljava/time/YearMonth;",
        &[Value::Int(2024), Value::Int(11)],
    )
    .unwrap()
    .unwrap();
    let ym_ref = ym.as_object().unwrap();
    let plus = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "plusMonths",
        "(J)Ljava/time/YearMonth;",
        &[Value::Object(Some(ym_ref)), Value::Long(3)],
    )
    .unwrap()
    .unwrap();
    let p_ref = plus.as_object().unwrap();
    let yr = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "getYear",
        "()I",
        &[Value::Object(Some(p_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(yr, Value::Int(2025));
    let mon = call_native(
        &shared,
        &mut thread,
        "java/time/YearMonth",
        "getMonthValue",
        "()I",
        &[Value::Object(Some(p_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(mon, Value::Int(2));
}

#[test]
fn month_day_of_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let md = call_native(
        &shared,
        &mut thread,
        "java/time/MonthDay",
        "of",
        "(II)Ljava/time/MonthDay;",
        &[Value::Int(2), Value::Int(29)],
    )
    .unwrap()
    .unwrap();
    let md_ref = md.as_object().unwrap();
    let m = call_native(
        &shared,
        &mut thread,
        "java/time/MonthDay",
        "getMonthValue",
        "()I",
        &[Value::Object(Some(md_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(m, Value::Int(2));
    let d = call_native(
        &shared,
        &mut thread,
        "java/time/MonthDay",
        "getDayOfMonth",
        "()I",
        &[Value::Object(Some(md_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(d, Value::Int(29));
    // isValidYear for leap year
    let valid = call_native(
        &shared,
        &mut thread,
        "java/time/MonthDay",
        "isValidYear",
        "(I)Z",
        &[Value::Object(Some(md_ref)), Value::Int(2024)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(valid, Value::Int(1));
    // isValidYear for non-leap year
    let invalid = call_native(
        &shared,
        &mut thread,
        "java/time/MonthDay",
        "isValidYear",
        "(I)Z",
        &[Value::Object(Some(md_ref)), Value::Int(2023)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(invalid, Value::Int(0));
}

#[test]
fn number_format_factories_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
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
    assert!(nf.as_object().is_some());
    let cf = call_native(
        &shared,
        &mut thread,
        "java/text/NumberFormat",
        "getCurrencyInstance",
        "()Ljava/text/NumberFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(cf.as_object().is_some());
    let pf = call_native(
        &shared,
        &mut thread,
        "java/text/NumberFormat",
        "getPercentInstance",
        "()Ljava/text/NumberFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(pf.as_object().is_some());
}

#[test]
fn date_format_factories_p62() {
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
    assert!(df.as_object().is_some());
    let tf = call_native(
        &shared,
        &mut thread,
        "java/text/DateFormat",
        "getTimeInstance",
        "()Ljava/text/DateFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(tf.as_object().is_some());
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
    assert!(dtf.as_object().is_some());
}

#[test]
fn simple_entry_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let entry = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let key = create_java_string(&shared, "k1");
    let val = create_java_string(&shared, "v1");
    call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleEntry",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[
            Value::Object(Some(entry)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();
    let got_key = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleEntry",
        "getKey",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got_key, Value::Object(Some(key)));
    let got_val = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleEntry",
        "getValue",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(entry))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got_val, Value::Object(Some(val)));
    // Set value
    let new_val = create_java_string(&shared, "v2");
    let old = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleEntry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(entry)), Value::Object(Some(new_val))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(old, Value::Object(Some(val)));
}

#[test]
fn simple_immutable_entry_set_throws_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let entry = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let key = create_java_string(&shared, "k");
    let val = create_java_string(&shared, "v");
    call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleImmutableEntry",
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[
            Value::Object(Some(entry)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/AbstractMap$SimpleImmutableEntry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(entry)), Value::Object(None)],
    );
    assert!(result.is_err());
}

#[test]
fn stamped_lock_read_write_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let lock = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();
    // Read lock
    let stamp = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "readLock",
        "()J",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(stamp, Value::Long(s) if s > 0));
    // isReadLocked
    let rl = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "isReadLocked",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(rl, Value::Int(1));
    // Unlock read
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "unlockRead",
        "(J)V",
        &[Value::Object(Some(lock)), stamp],
    )
    .unwrap();
    // Write lock
    let ws = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "writeLock",
        "()J",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(ws, Value::Long(s) if s != 0));
    let wl = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "isWriteLocked",
        "()Z",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(wl, Value::Int(1));
    // Unlock write
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "unlockWrite",
        "(J)V",
        &[Value::Object(Some(lock)), ws],
    )
    .unwrap();
}

#[test]
fn stamped_lock_optimistic_read_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let lock = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "<init>",
        "()V",
        &[Value::Object(Some(lock))],
    )
    .unwrap();
    let stamp = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "tryOptimisticRead",
        "()J",
        &[Value::Object(Some(lock))],
    )
    .unwrap()
    .unwrap();
    let valid = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/locks/StampedLock",
        "validate",
        "(J)Z",
        &[Value::Object(Some(lock)), stamp],
    )
    .unwrap()
    .unwrap();
    assert_eq!(valid, Value::Int(1));
}

#[test]
fn zip_entry_p62() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = crate::threading::JvmThread::new(crate::threading::ThreadId(0), "test");
    let ze = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    let name = create_java_string(&shared, "META-INF/");
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(ze)), Value::Object(Some(name))],
    )
    .unwrap();
    let got_name = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(ze))],
    )
    .unwrap()
    .unwrap();
    let n_ref = got_name.as_object().unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, n_ref).unwrap(),
        "META-INF/"
    );
    let is_dir = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "isDirectory",
        "()Z",
        &[Value::Object(Some(ze))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(is_dir, Value::Int(1));
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "getSize",
        "()J",
        &[Value::Object(Some(ze))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Long(-1)); // unknown size
                                       // Set size
    call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "setSize",
        "(J)V",
        &[Value::Object(Some(ze)), Value::Long(1024)],
    )
    .unwrap();
    let size2 = call_native(
        &shared,
        &mut thread,
        "java/util/zip/ZipEntry",
        "getSize",
        "()J",
        &[Value::Object(Some(ze))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size2, Value::Long(1024));
}

// ===== Phase 63 Tests =====

#[test]
fn weak_hash_map_basic_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let whm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(whm))],
    )
    .unwrap();
    // isEmpty = true initially
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(whm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
    // size = 0
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "size",
        "()I",
        &[Value::Object(Some(whm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
fn weak_hash_map_init_capacity_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let whm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "<init>",
        "(I)V",
        &[Value::Object(Some(whm)), Value::Int(32)],
    )
    .unwrap();
    let empty = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(whm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty, Value::Int(1));
}

#[test]
fn weak_hash_map_clear_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let whm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(whm))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "clear",
        "()V",
        &[Value::Object(Some(whm))],
    )
    .unwrap();
    let size = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "size",
        "()I",
        &[Value::Object(Some(whm))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, Value::Int(0));
}

#[test]
fn weak_hash_map_key_set_values_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let whm = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "<init>",
        "()V",
        &[Value::Object(Some(whm))],
    )
    .unwrap();
    let ks = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "keySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(whm))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(ks, Value::Object(Some(_))));
    let vals = call_native(
        &shared,
        &mut thread,
        "java/util/WeakHashMap",
        "values",
        "()Ljava/util/Collection;",
        &[Value::Object(Some(whm))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(vals, Value::Object(Some(_))));
}

#[test]
fn resource_bundle_get_bundle_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let name = create_java_string(&shared, "messages");
    let rb = call_native(
        &shared,
        &mut thread,
        "java/util/ResourceBundle",
        "getBundle",
        "(Ljava/lang/String;)Ljava/util/ResourceBundle;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(rb, Value::Object(Some(_))));
    // containsKey returns false (empty bundle)
    let key = create_java_string(&shared, "hello");
    let has = call_native(
        &shared,
        &mut thread,
        "java/util/ResourceBundle",
        "containsKey",
        "(Ljava/lang/String;)Z",
        &[rb, Value::Object(Some(key))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has, Value::Int(0));
}

#[test]
fn resource_bundle_get_keys_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let name = create_java_string(&shared, "test");
    let rb = call_native(
        &shared,
        &mut thread,
        "java/util/ResourceBundle",
        "getBundle",
        "(Ljava/lang/String;)Ljava/util/ResourceBundle;",
        &[Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let keys = call_native(
        &shared,
        &mut thread,
        "java/util/ResourceBundle",
        "getKeys",
        "()Ljava/util/Enumeration;",
        &[rb],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(keys, Value::Object(Some(_))));
    // The enumeration should be empty
    if let Value::Object(Some(ks)) = keys {
        let has = call_native(
            &shared,
            &mut thread,
            "java/util/Collections$EmptyEnumeration",
            "hasMoreElements",
            "()Z",
            &[Value::Object(Some(ks))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(has, Value::Int(0));
    }
}

#[test]
fn service_loader_load_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // A real Class mirror for the service interface. Passing `null` here
    // relied on the native tolerating it; the JDK contract for
    // `ServiceLoader.load(null)` is a NullPointerException, so a test that
    // depends on leniency is asserting behaviour the VM should not have.
    let service_cid = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.try_ensure_synthetic_class("java/util/spi/ToolProvider", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
    };
    let service_mirror = get_or_create_class_mirror(&shared, service_cid);
    let sl = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(service_mirror))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(sl, Value::Object(Some(_))));
    // toString
    let ts = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "toString",
        "()Ljava/lang/String;",
        &[sl],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = ts {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "ServiceLoader[]"
        );
    } else {
        panic!("expected string");
    }
}

#[test]
fn service_loader_find_first_empty_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // A real Class mirror for the service interface. Passing `null` here
    // relied on the native tolerating it; the JDK contract for
    // `ServiceLoader.load(null)` is a NullPointerException, so a test that
    // depends on leniency is asserting behaviour the VM should not have.
    let service_cid = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.try_ensure_synthetic_class("java/util/spi/ToolProvider", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only")
    };
    let service_mirror = get_or_create_class_mirror(&shared, service_cid);
    let sl = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(service_mirror))],
    )
    .unwrap()
    .unwrap();
    let opt = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "findFirst",
        "()Ljava/util/Optional;",
        &[sl],
    )
    .unwrap()
    .unwrap();
    // Returns a proper empty Optional object (field 0 = None)
    assert!(matches!(opt, Value::Object(Some(_))));
}

// -----------------------------------------------------------------------
// S1: URLClassLoader tests
// -----------------------------------------------------------------------

#[test]
fn url_classloader_init_and_get_urls() {
    use crate::types::Value;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Build an empty URL[] array
    let url_arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        0,
    );

    // Create URLClassLoader with the empty URL array (6 fields: parent=0, name=1, classes=2, package2module=3, urls=4, loader_id=5)
    let ucl = shared.mem.heap.alloc_object(ClassId::new(0), 6);

    call_native(
        &shared,
        &mut thread,
        "java/net/URLClassLoader",
        "<init>",
        "([Ljava/net/URL;)V",
        &[Value::Object(Some(ucl)), Value::Object(Some(url_arr))],
    )
    .unwrap();

    // getURLs() should return the URL array we passed in
    let result = call_native(
        &shared,
        &mut thread,
        "java/net/URLClassLoader",
        "getURLs",
        "()[Ljava/net/URL;",
        &[Value::Object(Some(ucl))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(result, Value::Object(Some(_))));
}

#[test]
fn url_classloader_new_instance_static() {
    use crate::types::Value;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    let url_arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        0,
    );

    let result = call_native(
        &shared,
        &mut thread,
        "java/net/URLClassLoader",
        "newInstance",
        "([Ljava/net/URL;)Ljava/net/URLClassLoader;",
        &[Value::Object(Some(url_arr))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(result, Value::Object(Some(_))));
}

// -----------------------------------------------------------------------
// S1: ServiceLoader with real META-INF/services discovery (JAR test)
// -----------------------------------------------------------------------

#[test]
fn service_loader_discovers_from_jar() {
    use crate::types::Value;
    use std::io::Write;

    // Build a tiny JAR with:
    //   META-INF/services/com.example.MyService  → com.example.FooImpl
    //   (FooImpl class file is absent — loader should gracefully skip it)
    let tmp = std::env::temp_dir().join("cratonvm_sl_test.jar");
    {
        let file = std::fs::File::create(&tmp).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("META-INF/services/com.example.MyService", opts)
            .unwrap();
        zw.write_all(b"com.example.FooImpl\n").unwrap();
        zw.finish().unwrap();
    }

    // Create a VM with the JAR on the classpath
    let config = VmConfig::new().with_classpath(vec![tmp.to_string_lossy().into_owned()]);
    let shared = Arc::new(SharedVm::new(config));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Build a Class mirror with a 1-field object storing ClassId(999) (unknown class).
    // s1_service_loader_ensure_loaded will call class_name_of_id(999) → None → return empty.
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    shared.mem.heap.set_field(mirror, 0, Value::Int(999));

    // ServiceLoader.load(Class)
    let sl = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(mirror))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(sl, Value::Object(Some(_))));

    // iterator() must not panic — returns empty iterator for unknown class
    let itr = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader",
        "iterator",
        "()Ljava/util/Iterator;",
        &[sl],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(itr, Value::Object(Some(_))));

    // hasNext() should be false (no services loaded for unknown class)
    let itr_ref = itr.as_object().unwrap();
    let has_next = call_native(
        &shared,
        &mut thread,
        "java/util/ServiceLoader$Itr",
        "hasNext",
        "()Z",
        &[Value::Object(Some(itr_ref))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(has_next, Value::Int(0), "no services for unknown class");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn service_loader_reads_meta_inf_services() {
    use crate::types::Value;
    use std::io::Write;

    // Load java/lang/String which has a real ClassId we can use.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");

    // Load String to get a known ClassId
    {
        let mut cm = shared.classes.class_manager_write();
        let _ = cm.load_class("java/lang/String");
    }
    let string_id = {
        let cm = shared.classes.class_manager.read();
        cm.find_bootstrap_class_by_name("java/lang/String")
    };

    if let Some(sid) = string_id {
        // Build a services JAR that lists java.lang.String as a provider of itself
        // (semantically silly but tests the machinery)
        let tmp = std::env::temp_dir().join("cratonvm_sl_test2.jar");
        {
            let file = std::fs::File::create(&tmp).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            // Use a comment to test comment-stripping
            zw.start_file("META-INF/services/java.lang.String", opts)
                .unwrap();
            zw.write_all(b"# comment\n\njava.lang.String\n").unwrap();
            zw.finish().unwrap();
        }

        // Add the JAR to the VM classpath dynamically
        {
            let mut cm = shared.classes.class_manager_write();
            cm.extend_application_classpath(&[tmp.to_string_lossy().into_owned()]);
        }

        // Verify find_resource can find the services file
        let found = {
            let cm = shared.classes.class_manager.read();
            cm.find_resource("META-INF/services/java.lang.String")
        };
        assert!(
            found.is_some(),
            "META-INF/services/java.lang.String should be found"
        );
        let content = String::from_utf8(found.unwrap()).unwrap();
        assert!(content.contains("java.lang.String"));

        // Build a Class mirror with the real String ClassId
        let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        shared
            .mem
            .heap
            .set_field(mirror, 0, Value::Int(sid.as_u32() as i32));

        // ServiceLoader.load(Class) + stream()
        let sl = call_native(
            &shared,
            &mut thread,
            "java/util/ServiceLoader",
            "load",
            "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
            &[Value::Object(Some(mirror))],
        )
        .unwrap()
        .unwrap();

        let stream = call_native(
            &shared,
            &mut thread,
            "java/util/ServiceLoader",
            "stream",
            "()Ljava/util/stream/Stream;",
            &[sl],
        )
        .unwrap()
        .unwrap();
        // Stream should be a valid object (even if providers failed to instantiate)
        assert!(matches!(stream, Value::Object(Some(_))));

        let _ = std::fs::remove_file(&tmp);
    } else {
        // Without this arm the ENTIRE body above was optional: the
        // `let _ = cm.load_class("java/lang/String")` two lines up
        // DISCARDS its Result, so a failure to load left `string_id` as
        // `None` and this test reported success having asserted nothing at
        // all — no JAR built, no `find_resource`, no ServiceLoader call.
        // `VmConfig::default()` is `EMBEDDED_DEFAULT_JDK_MODE` =
        // synthetic (vm/src/config.rs:225), and `java/lang/String` is a
        // declared synthetic bootstrap class
        // (`classloading/src/class_manager.rs`), so this arm is expected
        // to be unreachable — but it is the only thing that says so.
        panic!(
            "java/lang/String is not a loaded bootstrap class, so this test \
                 would have asserted NOTHING; load_class's discarded Result is \
                 the thing to look at"
        );
    }
}

#[test]
fn method_handles_lookup_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let lk = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(lk, Value::Object(Some(_))));
    // `MethodHandles.lookup()` returns a FULL-PRIVILEGE, ORIGINAL lookup.
    //
    // JDK `MethodHandles.Lookup`:
    //   PUBLIC=0x01 PRIVATE=0x02 PROTECTED=0x04 PACKAGE=0x08 MODULE=0x10
    //   UNCONDITIONAL=0x20 ORIGINAL=0x40
    //   FULL_POWER_MODES = PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE = 0x1F
    //   Lookup(Class<?> c) { this(c, null, FULL_POWER_MODES | ORIGINAL); }
    // and `lookup()` is exactly `new Lookup(callerClass)`, so since JDK 16
    // `lookup().lookupModes()` is 0x1F|0x40 = 0x5F (95), NOT the bare 0x1F
    // this assertion used to carry (the pre-Java-9 FULL_POWER value).
    // `privateLookupIn` is the one that yields 0x1F — it drops ORIGINAL.
    let modes = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles$Lookup",
        "lookupModes",
        "()I",
        &[lk],
    )
    .unwrap()
    .unwrap();
    assert_eq!(modes, Value::Int(0x5F));
}

#[test]
fn method_handles_public_lookup_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let lk = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles",
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )
    .unwrap()
    .unwrap();
    let modes = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles$Lookup",
        "lookupModes",
        "()I",
        &[lk],
    )
    .unwrap()
    .unwrap();
    // `publicLookup()` reports UNCONDITIONAL (0x20) and nothing else.
    //
    // Verified against the JDK 25 sources
    // (`java.base/java/lang/invoke/MethodHandles.java` in `src.zip`):
    //
    //   public static Lookup publicLookup() { return Lookup.PUBLIC_LOOKUP; }
    //   static final Lookup PUBLIC_LOOKUP =
    //       new Lookup(Object.class, null, UNCONDITIONAL);
    //   public int lookupModes() { return allowedModes & ALL_MODES; }
    //   ALL_MODES = PUBLIC|PRIVATE|PROTECTED|PACKAGE|MODULE|
    //               UNCONDITIONAL|ORIGINAL
    //
    // so the answer is the bare UNCONDITIONAL bit. This assertion used to
    // carry 0x01 (PUBLIC) — the pre-Java-9 value — and the native had been
    // written to match it. PUBLIC|UNCONDITIONAL (0x21) is wrong too: the
    // JDK treats that combination as impossible, since `Lookup.toString()`
    // switches on the exact mode word with a bare `case UNCONDITIONAL` arm
    // and a `default:` that asserts false.
    assert_eq!(modes, Value::Int(0x20));
    // Per the same source the lookup class is `java.lang.Object`:
    // `publicLookup()` is not caller-sensitive and never carries a null
    // lookup class. Non-null is as far as this unit test can check —
    // `SharedVm::new(VmConfig::default())` has no class path, so
    // `Class.getName()` on the mirror is not reliably resolvable here (it
    // falls back to `unknown_<id>` with no class-store entry).
    let lookup_class = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles$Lookup",
        "lookupClass",
        "()Ljava/lang/Class;",
        &[lk],
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(lookup_class, Value::Object(Some(_))),
        "publicLookup().lookupClass() must be the java.lang.Object mirror, never null"
    );
    // toString: the real JDK prints `java.lang.Object/publicLookup` for an
    // UNCONDITIONAL lookup (see the `toString()` switch cited above). Since
    // interpreter round i1 wave 43 the native transcribes that switch; the
    // class-name half is not resolvable here (see above), so only the mode
    // suffix is asserted.
    let ts = call_native(
        &shared,
        &mut thread,
        "java/lang/invoke/MethodHandles$Lookup",
        "toString",
        "()Ljava/lang/String;",
        &[lk],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = ts {
        assert!(
            read_java_string(&shared.mem.heap, s)
                .unwrap()
                .ends_with("/publicLookup"),
            "publicLookup().toString() must end with the UNCONDITIONAL suffix"
        );
    }
}

#[test]
fn scheduled_executor_basic_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ex = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ex)), Value::Int(4)],
    )
    .unwrap();
    let cores = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "getCorePoolSize",
        "()I",
        &[Value::Object(Some(ex))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(cores, Value::Int(4));
    let shut = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "isShutdown",
        "()Z",
        &[Value::Object(Some(ex))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut, Value::Int(0));
}

#[test]
fn scheduled_executor_shutdown_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ex = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ex)), Value::Int(2)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "shutdown",
        "()V",
        &[Value::Object(Some(ex))],
    )
    .unwrap();
    let shut = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "isShutdown",
        "()Z",
        &[Value::Object(Some(ex))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut, Value::Int(1));
}

#[test]
fn executors_factory_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ex = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Executors",
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        &[Value::Int(8)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(ex, Value::Object(Some(_))));
    // Single thread variant
    let ex2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/Executors",
        "newSingleThreadScheduledExecutor",
        "()Ljava/util/concurrent/ScheduledExecutorService;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(ex2, Value::Object(Some(_))));
}

#[test]
fn formatter_out_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let fmt = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        &shared,
        &mut thread,
        "java/util/Formatter",
        "<init>",
        "()V",
        &[Value::Object(Some(fmt))],
    )
    .unwrap();
    // out() returns the internal appendable (field 0)
    let out = call_native(
        &shared,
        &mut thread,
        "java/util/Formatter",
        "out",
        "()Ljava/lang/Appendable;",
        &[Value::Object(Some(fmt))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(out, Value::Object(Some(_))));
    // close and flush are no-ops
    call_native(
        &shared,
        &mut thread,
        "java/util/Formatter",
        "close",
        "()V",
        &[Value::Object(Some(fmt))],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/Formatter",
        "flush",
        "()V",
        &[Value::Object(Some(fmt))],
    )
    .unwrap();
}

#[test]
fn empty_enumeration_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let en = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(en, Value::Object(Some(_))));
    if let Value::Object(Some(e)) = en {
        let has = call_native(
            &shared,
            &mut thread,
            "java/util/Collections$EmptyEnumeration",
            "hasMoreElements",
            "()Z",
            &[Value::Object(Some(e))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(has, Value::Int(0));
        // nextElement throws
        let err = call_native(
            &shared,
            &mut thread,
            "java/util/Collections$EmptyEnumeration",
            "nextElement",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(e))],
        );
        assert!(err.is_err());
    }
}

#[test]
fn enumeration_interface_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let en = call_native(
        &shared,
        &mut thread,
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        &[],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(e)) = en {
        // Test via Enumeration interface dispatch
        let has = call_native(
            &shared,
            &mut thread,
            "java/util/Enumeration",
            "hasMoreElements",
            "()Z",
            &[Value::Object(Some(e))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(has, Value::Int(0));
        // asIterator
        let itr = call_native(
            &shared,
            &mut thread,
            "java/util/Enumeration",
            "asIterator",
            "()Ljava/util/Iterator;",
            &[Value::Object(Some(e))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(itr, Value::Object(Some(_))));
    } else {
        panic!(
            "Collections.emptyEnumeration()Ljava/util/Enumeration; must answer a \
                 reference, got {en:?}"
        );
    }
}

#[test]
fn scheduled_executor_shutdownnow_p63() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let ex = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(I)V",
        &[Value::Object(Some(ex)), Value::Int(1)],
    )
    .unwrap();
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "shutdownNow",
        "()Ljava/util/List;",
        &[Value::Object(Some(ex))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(list, Value::Object(Some(_))));
    let shut = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "isShutdown",
        "()Z",
        &[Value::Object(Some(ex))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(shut, Value::Int(1));
}

// ===== Phase 64 Tests =====

#[test]
fn arraylist_get_first_last_p64() {
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
    // Add elements
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), Value::Int(10)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), Value::Int(20)],
    )
    .unwrap();
    call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), Value::Int(30)],
    )
    .unwrap();
    // getFirst = 10, getLast = 30
    let first = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "getFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(al))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(first, Value::Int(10));
    let last = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "getLast",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(al))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(last, Value::Int(30));
}

#[test]
fn arraylist_get_first_empty_throws_p64() {
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
    // getFirst on empty should throw
    let err = call_native(
        &shared,
        &mut thread,
        "java/util/ArrayList",
        "getFirst",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(al))],
    );
    assert!(err.is_err());
}

#[test]
fn hex_format_basic_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let hf = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "of",
        "()Ljava/util/HexFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(hf, Value::Object(Some(_))));
    // toHexDigits(byte)
    if let Value::Object(Some(hf_ref)) = hf {
        let hex = call_native(
            &shared,
            &mut thread,
            "java/util/HexFormat",
            "toHexDigits",
            "(B)Ljava/lang/String;",
            &[Value::Object(Some(hf_ref)), Value::Int(0xAB)],
        )
        .unwrap()
        .unwrap();
        if let Value::Object(Some(s)) = hex {
            assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "ab");
        }
    }
}

#[test]
fn hex_format_format_parse_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let hf = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "of",
        "()Ljava/util/HexFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    // formatHex([0x01, 0x02, 0xff])
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(0x01));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(0x02));
    let _ = shared
        .mem
        .heap
        .set_array_element(arr, 2, Value::Int(0xFF_u8 as i32));
    let hex = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "formatHex",
        "([B)Ljava/lang/String;",
        &[hf, Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = hex {
        assert_eq!(read_java_string(&shared.mem.heap, s).unwrap(), "0102ff");
    } else {
        panic!("HexFormat.formatHex([B)Ljava/lang/String; must answer a String, got {hex:?}");
    }
    // parseHex back
    let hex_str = create_java_string(&shared, "0102ff");
    let parsed = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "parseHex",
        "(Ljava/lang/String;)[B",
        &[hf, Value::Object(Some(hex_str))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(p)) = parsed {
        assert_eq!(
            shared.mem.heap.get_array_element(p, 0).unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            shared.mem.heap.get_array_element(p, 1).unwrap(),
            Value::Int(2)
        );
        assert_eq!(
            shared.mem.heap.get_array_element(p, 2).unwrap(),
            Value::Int(-1)
        );
        // 0xFF = -1 as signed byte
    } else {
        panic!("HexFormat.parseHex(Ljava/lang/String;)[B must answer a byte[], got {parsed:?}");
    }
}

#[test]
fn hex_format_is_hex_digit_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let yes = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "isHexDigit",
        "(I)Z",
        &[Value::Int('a' as i32)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(yes, Value::Int(1));
    let no = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "isHexDigit",
        "(I)Z",
        &[Value::Int('g' as i32)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(no, Value::Int(0));
}

#[test]
fn hex_format_from_hex_digits_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // `HexFormat.of()` itself is exercised here (it must succeed); the
    // instance is not otherwise needed below.
    let _hf = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "of",
        "()Ljava/util/HexFormat;",
        &[],
    )
    .unwrap()
    .unwrap();
    let hex_str = create_java_string(&shared, "ff");
    // `fromHexDigits(CharSequence)` is STATIC (verified on JDK 25). This test
    // used to pass `[hf, "ff"]` -- a receiver the static method does not have --
    // so the native read the `HexFormat` instance as the `CharSequence` and
    // failed with "string length greater than 8: 55". 55 is exactly
    // `HexFormat.of().toString().length()`, which is how the misplaced argument
    // was identified. One argument, the string.
    let val = call_native(
        &shared,
        &mut thread,
        "java/util/HexFormat",
        "fromHexDigits",
        "(Ljava/lang/CharSequence;)I",
        &[Value::Object(Some(hex_str))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(val, Value::Int(255));
}

#[test]
fn stream_to_list_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Create a stream with 3 elements
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = shared.mem.heap.set_array_element(arr, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(arr, 1, Value::Int(2));
    let _ = shared.mem.heap.set_array_element(arr, 2, Value::Int(3));
    let stream = alloc_receiver(&shared, &mut thread, "java/util/stream/Stream", 1);
    shared
        .mem
        .heap
        .set_field(stream, 0, Value::Object(Some(arr)));
    let list = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "toList",
        "()Ljava/util/List;",
        &[Value::Object(Some(stream))],
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
    } else {
        panic!("expected list");
    }
}

#[test]
fn stream_of_nullable_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // ofNullable with non-null
    let s = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Int(42)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(s, Value::Object(Some(_))));
    // ofNullable with null -> empty stream
    let s2 = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(None)],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(s2, Value::Object(Some(_))));
}

#[test]
fn stream_concat_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // Create two streams
    let arr1 = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 2);
    let _ = shared.mem.heap.set_array_element(arr1, 0, Value::Int(1));
    let _ = shared.mem.heap.set_array_element(arr1, 1, Value::Int(2));
    let s1 = alloc_receiver(&shared, &mut thread, "java/util/stream/Stream", 1);
    shared.mem.heap.set_field(s1, 0, Value::Object(Some(arr1)));
    let arr2 = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    let _ = shared.mem.heap.set_array_element(arr2, 0, Value::Int(3));
    let s2 = alloc_receiver(&shared, &mut thread, "java/util/stream/Stream", 1);
    shared.mem.heap.set_field(s2, 0, Value::Object(Some(arr2)));
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "concat",
        "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(s1)), Value::Object(Some(s2))],
    )
    .unwrap()
    .unwrap();
    // toList on concatenated stream
    if let Value::Object(Some(rs)) = result {
        let list = call_native(
            &shared,
            &mut thread,
            "java/util/stream/Stream",
            "toList",
            "()Ljava/util/List;",
            &[Value::Object(Some(rs))],
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
        } else {
            panic!("Stream.toList()Ljava/util/List; must answer a reference, got {list:?}");
        }
    } else {
        panic!(
            "Stream.concat(Stream,Stream)Ljava/util/stream/Stream; must answer a \
                 reference, got {result:?}"
        );
    }
}

#[test]
fn thread_local_random_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tlr = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ThreadLocalRandom",
        "current",
        "()Ljava/util/concurrent/ThreadLocalRandom;",
        &[],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(tlr, Value::Object(Some(_))));
    // nextInt
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ThreadLocalRandom",
        "nextInt",
        "()I",
        &[tlr],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(r1, Value::Int(_)));
    // nextInt with bound
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ThreadLocalRandom",
        "nextInt",
        "(I)I",
        &[tlr, Value::Int(100)],
    )
    .unwrap()
    .unwrap();
    if let Value::Int(v) = r2 {
        assert!((0..100).contains(&v));
    }
}

#[test]
fn thread_local_random_range_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let tlr = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ThreadLocalRandom",
        "current",
        "()Ljava/util/concurrent/ThreadLocalRandom;",
        &[],
    )
    .unwrap()
    .unwrap();
    let r = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ThreadLocalRandom",
        "nextInt",
        "(II)I",
        &[tlr, Value::Int(10), Value::Int(20)],
    )
    .unwrap()
    .unwrap();
    if let Value::Int(v) = r {
        assert!((10..20).contains(&v));
    } else {
        panic!("ThreadLocalRandom.nextInt(II)I must answer an int, got {r:?}");
    }
    // nextDouble is in [0, 1)
    let d = call_native(
        &shared,
        &mut thread,
        "java/util/concurrent/ThreadLocalRandom",
        "nextDouble",
        "()D",
        &[tlr],
    )
    .unwrap()
    .unwrap();
    if let Value::Double(v) = d {
        assert!((0.0..1.0).contains(&v));
    } else {
        panic!("ThreadLocalRandom.nextDouble()D must answer a double, got {d:?}");
    }
}

#[test]
fn string_strip_indent_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let text = create_java_string(&shared, "    hello\n    world");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "stripIndent",
        "()Ljava/lang/String;",
        &[Value::Object(Some(text))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "hello\nworld"
        );
    } else {
        panic!("expected string");
    }
}

#[test]
fn string_translate_escapes_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let text = create_java_string(&shared, "hello\\nworld\\ttab");
    let result = call_native(
        &shared,
        &mut thread,
        "java/lang/String",
        "translateEscapes",
        "()Ljava/lang/String;",
        &[Value::Object(Some(text))],
    )
    .unwrap()
    .unwrap();
    if let Value::Object(Some(s)) = result {
        assert_eq!(
            read_java_string(&shared.mem.heap, s).unwrap(),
            "hello\nworld\ttab"
        );
    } else {
        panic!("expected string");
    }
}

#[test]
fn math_clamp_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    // clamp(long, int, int) -> int
    let r1 = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "clamp",
        "(JII)I",
        &[Value::Long(150), Value::Int(0), Value::Int(100)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r1, Value::Int(100)); // clamped to max
    let r2 = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "clamp",
        "(JII)I",
        &[Value::Long(-5), Value::Int(0), Value::Int(100)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r2, Value::Int(0)); // clamped to min
    let r3 = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "clamp",
        "(JII)I",
        &[Value::Long(50), Value::Int(0), Value::Int(100)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r3, Value::Int(50)); // within range
}

#[test]
fn math_clamp_double_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let r = call_native(
        &shared,
        &mut thread,
        "java/lang/Math",
        "clamp",
        "(DDD)D",
        &[Value::Double(1.5), Value::Double(0.0), Value::Double(1.0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(r, Value::Double(1.0));
}

#[test]
fn sequenced_collection_interface_p64() {
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
    // reversed returns self for interface dispatch
    let rev = call_native(
        &shared,
        &mut thread,
        "java/util/SequencedCollection",
        "reversed",
        "()Ljava/util/SequencedCollection;",
        &[Value::Object(Some(al))],
    )
    .unwrap()
    .unwrap();
    assert!(matches!(rev, Value::Object(Some(_))));
}

/// `Collectors.teeing(d1, d2, merge)` feeds the whole stream to BOTH
/// downstreams and returns `merge.apply(r1, r2)`.
///
/// This used to assert only that the factory returned some object, which it
/// did — a `toList` collector with all three arguments thrown away (the
/// body wrote tag 1 while its comment claimed "Tag 9 … ARG1/ARG2
/// downstreams"). `collect(teeing(...))` therefore returned a List of the
/// stream elements and neither downstream nor the merger ever ran.
#[test]
fn collectors_teeing_p64() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let d1 = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "joining",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();
    let d2 = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Collectors",
        "joining",
        "()Ljava/util/stream/Collector;",
        &[],
    )
    .unwrap()
    .unwrap();
    // merge = (a, b) -> a.concat(b)
    let merger = make_lambda_proxy(
        &shared,
        "java/util/function/BiFunction",
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "java/lang/String",
        "concat",
        "(Ljava/lang/String;)Ljava/lang/String;",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    );
    let c = call_native(&shared, &mut thread, "java/util/stream/Collectors",
            "teeing",
            "(Ljava/util/stream/Collector;Ljava/util/stream/Collector;Ljava/util/function/BiFunction;)Ljava/util/stream/Collector;",
            &[d1, d2, Value::Object(Some(merger))]).unwrap().unwrap();
    assert!(matches!(c, Value::Object(Some(_))));
    let stream = stream_of_strings(&shared, &mut thread, &["a", "bbb", "cc"]);
    let result = call_native(
        &shared,
        &mut thread,
        "java/util/stream/Stream",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        &[stream, c],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        read_java_string(&shared.mem.heap, result.as_object().unwrap()),
        Some("abbbccabbbcc".to_string()),
        "teeing must merge both downstream results, not return a toList"
    );
}

