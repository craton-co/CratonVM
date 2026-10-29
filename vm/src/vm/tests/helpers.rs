// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

/// `ClassId` that `DeoptimizationController::deoptimize` falls back to when a
/// class name is not registered with the class manager. The `JitCache` fixtures below
/// own their cache end-to-end and never register their fake classes, so
/// keying on this id is what makes a later name-driven eviction match.
pub(crate) const CID0: cratonvm_types::ClassId = cratonvm_types::ClassId::new(0);

pub(crate) fn test_vm() -> Vm {
    Vm::new(VmConfig::default())
}

/// The native registry every [`call_native`] below resolves against.
///
/// Built once per process rather than once per call. It holds ~3,000
/// registrations and `call_native` runs on the order of 10,000 times
/// across this module, so rebuilding it per call was the dominant cost of
/// the whole binary: the `linked_` filter drops from 100s to 33s, and the
/// full suite from 56 minutes to roughly 19. That matters because 56
/// minutes is over the CI step's `timeout-minutes: 20` — the step could
/// not pass on time even at zero failures.
///
/// Sharing is sound because the registry is immutable once built and
/// `find` takes `&self`: the per-VM state a native touches arrives through
/// the `NativeContext` argument at call time, not through the registry.
/// Measured behaviour-neutral, not just argued: full runs with and without
/// this hoist reported the same 67 failures and stopped at the same test.
pub(crate) static TEST_NATIVE_REGISTRY: std::sync::LazyLock<crate::native::registry::NativeMethodRegistry> =
    std::sync::LazyLock::new(|| {
        let mut r = crate::native::registry::NativeMethodRegistry::new();
        crate::native::builtins::register_builtins(&mut r);
        crate::native::io::register_io_natives(&mut r);
        crate::native::collections::register_collections_natives(&mut r);
        // Real AQS is the VM's default, so `register_concurrent_natives` skips
        // the synthetic `ReentrantLock` / `Lock` / `Condition` / `Semaphore`
        // natives. That is right for the VM and wrong for this module: the
        // fixtures here are hand-built receivers with no real
        // `java.util.concurrent` bytecode behind them, so the real path cannot
        // run, and without this the synthetic path is not registered either —
        // leaving the lock and semaphore tests asserting natives that nothing
        // would ever register. Opt in explicitly, the same choice
        // `CRATONVM_SYNTHETIC_AQS=1` makes at runtime.
        cratonvm_native_builtins::util_concurrent_ext::register_synthetic_aqs_natives(&mut r);
        cratonvm_native_builtins::util_concurrent_ext::register_synthetic_rwlock_natives(&mut r);
        // Same story for `java.net.Socket` / `ServerSocket`, but guarded twice:
        // `register_phase53_socket_stubs` skips them under real NIO sockets,
        // AND `NativeMethodRegistry::register` drops any registration on those
        // two classes outright, because in a running VM one registered native
        // shadows the class's real bytecode everywhere. Neither concern
        // applies to a registry that exists only to unit-test the synthetic
        // natives — there is no real socket bytecode here to shadow — so opt
        // out of both and register them.
        r.allow_synthetic_net_sockets(true);
        cratonvm_native_builtins::phases_early::register_synthetic_socket_stubs(&mut r);
        r.allow_synthetic_net_sockets(false);
        r
    });

/// Allocate a receiver whose runtime class really is `class_name`.
///
/// Most receivers in this module are hand-rolled as
/// `heap.alloc_object(ClassId::new(0), n)` — an object with no class
/// identity at all. That was fine when the natives keyed purely off slot
/// indices, and it is not any more: a growing number of them first ask
/// whether the receiver IS the class they implement, and quietly decline
/// when it is not. `is_hashset_native_backed` is the clearest case —
/// `HashSet.add` resolves its backing map through it, so against a
/// `ClassId::new(0)` receiver the whole native short-circuits and `add`
/// reports `false` for a brand-new element.
///
/// `ensure_synthetic_class` registers (or reuses) a synthetic class of
/// that name with `num_fields` declared instance fields, which is what
/// `alloc_concurrent_synthetic` in `native-builtins` already does for the
/// same reason. Giving the receiver a real class id also stops every test
/// object in the process sharing id 0 — the ambiguity that let two VMs'
/// collections alias each other in `widened_obj_key`.
pub(crate) fn alloc_receiver(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    class_name: &str,
    num_fields: usize,
) -> ObjectRef {
    let mut ctx = NativeContextImpl { shared, thread };
    let cid = ctx
        .try_ensure_synthetic_class(class_name, num_fields)
        .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
    ctx.alloc_object(cid, num_fields)
}

/// `Integer.valueOf(v)` — what the interpreter has already done before any
/// `Map.put`, whose descriptor is `(Object,Object)Object`.
///
/// A test that hands a raw `Value::Int` to a map native is exercising a
/// call no real caller can make. Since `7bf427af1` the map natives box it
/// themselves (`java/util/HashMap$Node.value` is `Ljava/lang/Object;` on
/// the real class the nodes now bind to, and the descriptor-aware write
/// path coerces a primitive there to **null** — which is what deleted
/// elements from `LinkedHashSet` while reporting `false`, and broke
/// Jersey). So the value that comes back out of a map is a box, and a test
/// asserting `Value::Int` against it is asserting the old, impossible
/// calling convention.
///
/// Box on the way in and unbox on the way out, exactly as bytecode would.
pub(crate) fn box_int(shared: &Arc<SharedVm>, thread: &mut JvmThread, v: i32) -> Value {
    match call_native(
        shared,
        thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(v)],
    ) {
        Ok(Some(boxed @ Value::Object(Some(_)))) => boxed,
        other => panic!("Integer.valueOf({v}) did not box: {other:?}"),
    }
}

/// The inverse of [`box_int`]: `((Integer) v).intValue()`.
///
/// Takes the `Option<Value>` a native returns so the assertions read the
/// same way they did before boxing, i.e. against a plain `Value::Int`.
pub(crate) fn unbox_int(shared: &Arc<SharedVm>, thread: &mut JvmThread, v: Option<Value>) -> Option<Value> {
    match v {
        Some(Value::Object(Some(obj))) => call_native(
            shared,
            thread,
            "java/lang/Integer",
            "intValue",
            "()I",
            &[Value::Object(Some(obj))],
        )
        .unwrap_or_else(|e| panic!("Integer.intValue() failed: {e:?}")),
        // Not a box: hand it back unchanged so the assertion reports the
        // real value (a null, or a stray primitive) rather than a panic
        // that hides it.
        other => other,
    }
}

/// Helper to call a native method by (class, name, descriptor).
pub(crate) fn call_native(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    class: &str,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> crate::error::MethodCallResult {
    let callback = TEST_NATIVE_REGISTRY
        .find(class, method, descriptor)
        .unwrap_or_else(|| panic!("{class}.{method}{descriptor} not registered"));
    // Grant FFM native access, the equivalent of launching with
    // `--enable-native-access=ALL-UNNAMED`. The gate is deliberately
    // fail-closed, so without this every `MemorySegment` get/set/fill in
    // this module throws `IllegalCallerException` — the `panama_*` tests
    // exercise the FFM API itself and cannot do so from behind a denial. No
    // test in this module asserts that access is refused. zt t18/df7: on the
    // test's own VM (`panama::NativeAccessState`); a process global before,
    // set once by the registry's first use.
    shared.loader_identity.native_access().set_enabled(true);
    let mut ctx = NativeContextImpl { shared, thread };
    callback(&mut ctx, args)
}

/// A `java.lang.foreign.ValueLayout` constant, minted exactly as the deleted
/// field-shaped `ValueLayout.JAVA_*` / `ADDRESS` registrations minted it
/// (`foreign_ffm::p67_layout_object`). Those rows could never be dispatched
/// (`getstatic` has no registry door) and were held only by these tests (round
/// 14 wave 4, lane ffm4; `r13w13-shadow5-field-shaped-rows-held-by-tests-patch`
/// item 4).
pub(crate) fn ffm_value_layout(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    field: &str,
) -> crate::error::MethodCallResult {
    let (class, size) = match field {
        "JAVA_BYTE" => ("java/lang/foreign/ValueLayout$OfByte", 1),
        "JAVA_BOOLEAN" => ("java/lang/foreign/ValueLayout$OfBoolean", 1),
        "JAVA_CHAR" => ("java/lang/foreign/ValueLayout$OfChar", 2),
        "JAVA_SHORT" => ("java/lang/foreign/ValueLayout$OfShort", 2),
        "JAVA_INT" => ("java/lang/foreign/ValueLayout$OfInt", 4),
        "JAVA_LONG" => ("java/lang/foreign/ValueLayout$OfLong", 8),
        "JAVA_FLOAT" => ("java/lang/foreign/ValueLayout$OfFloat", 4),
        "JAVA_DOUBLE" => ("java/lang/foreign/ValueLayout$OfDouble", 8),
        "ADDRESS" => ("java/lang/foreign/AddressLayout", 8),
        other => panic!("not a ValueLayout constant: {other}"),
    };
    let mut ctx = NativeContextImpl { shared, thread };
    let layout = cratonvm_native_builtins::phases_late::foreign_ffm::p67_layout_object(
        &mut ctx, class, size, size,
    )?;
    Ok(Some(Value::Object(Some(layout))))
}

/// Helper to create a synthetic class directly in the ClassManager's ClassStore.
pub(crate) fn make_test_class(
    shared: &Arc<SharedVm>,
    name: &str,
    fields: Vec<cratonvm_reader::field::ClassFileField>,
    methods: Vec<cratonvm_reader::method::ClassFileMethod>,
) -> ClassId {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let mut cm = shared.classes.class_manager_write();
    let id = cm.class_store.next_id();
    let num_fields = fields.iter().filter(|f| !f.is_static()).count();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields,
        methods,
        first_field_index: 0,
        num_total_fields: num_fields,
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
    id
}

/// Like make_test_class but with custom access_flags, fields, and methods.
pub(crate) fn register_test_class(
    shared: &Arc<SharedVm>,
    name: &str,
    access_flags_raw: u16,
    fields: &[cratonvm_reader::field::ClassFileField],
    methods: &[cratonvm_reader::method::ClassFileMethod],
) -> ClassId {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let mut cm = shared.classes.class_manager_write();
    let id = cm.class_store.next_id();
    let num_fields = fields.iter().filter(|f| !f.is_static()).count();
    let class_name = name.to_string();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(&class_name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::from_bits_truncate(access_flags_raw),
        superclass: None,
        interfaces: vec![],
        fields: fields.to_vec(),
        methods: methods.to_vec(),
        first_field_index: 0,
        num_total_fields: num_fields,
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
    // Also register in the loaded_classes map so find_class_by_name works
    cm.register_class_name(ClassLoaderId::Application, &class_name, id);
    id
}

pub(crate) fn make_lambda_proxy(
    shared: &Arc<SharedVm>,
    functional_interface: &str,
    sam_name: &str,
    sam_desc: &str,
    impl_class: &str,
    impl_method: &str,
    impl_desc: &str,
    kind: MethodHandleKind,
    capture_types: Vec<char>,
    captures: &[Value],
) -> ObjectRef {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle};
    let proxy_class_id = shared.alloc_lambda_proxy_id();
    let call_site = LambdaCallSite {
        functional_interface_id: None,
        functional_interface: functional_interface.into(),
        sam_method_name: sam_name.into(),
        sam_descriptor: sam_desc.into(),
        impl_handle: MethodHandle {
            kind,
            class_name: impl_class.into(),
            member_name: impl_method.into(),
            descriptor: impl_desc.into(),
        },
        instantiated_descriptor: sam_desc.into(),
        capture_types: capture_types.clone(),
        proxy_class_id,
        serializable_flag: false,
    };
    {
        let mut proxies = shared.classes.lambda_proxies.write();
        if crate::runtime::invokedynamic::lambda_proxy_table_has_room(proxies.len()) {
            proxies.insert(proxy_class_id, std::sync::Arc::new(call_site));
        }
    }
    let proxy = shared.mem.heap.alloc_object(proxy_class_id, captures.len());
    for (i, val) in captures.iter().enumerate() {
        shared.mem.heap.set_field(proxy, i, *val);
    }
    proxy
}

pub(crate) fn sorted_stream_arg(stream: Value) -> Value {
    stream
}

/// Helper: create a StringBuilder, init it, append a string, return the sb ref
pub(crate) fn make_sb(shared: &Arc<SharedVm>, thread: &mut JvmThread, initial: &str) -> ObjectRef {
    let sb = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        shared,
        thread,
        "java/lang/StringBuilder",
        "<init>",
        "()V",
        &[Value::Object(Some(sb))],
    )
    .unwrap();
    if !initial.is_empty() {
        let s = create_java_string(shared, initial);
        call_native(
            shared,
            thread,
            "java/lang/StringBuilder",
            "append",
            "(Ljava/lang/String;)Ljava/lang/StringBuilder;",
            &[Value::Object(Some(sb)), Value::Object(Some(s))],
        )
        .unwrap();
    }
    sb
}

pub(crate) fn sb_to_string(shared: &Arc<SharedVm>, thread: &mut JvmThread, sb: ObjectRef) -> String {
    let result = call_native(
        shared,
        thread,
        "java/lang/StringBuilder",
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(sb))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Object(Some(sr)) => read_java_string(&shared.mem.heap, sr).unwrap(),
        _ => panic!("expected string"),
    }
}

/// Poll `getNumberWaiting` until it reaches `want`, then return; fail the
/// test rather than hang if it never gets there. Used by the
/// `CyclicBarrier` tests below to hand off between the party threads and
/// the main thread without a sleep-and-hope.
///
/// Sleeps rather than spinning: each poll goes through `call_native`, which
/// rebuilds the whole native registry, so a hot loop here would burn the
/// cores the party threads need to reach their `await` in the first place.
pub(crate) fn cb_await_waiting_count(shared: &Arc<SharedVm>, barrier: ObjectRef, want: i32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut thread = JvmThread::new(ThreadId(900), "cb-poll");
    loop {
        let waiting = call_native(
            shared,
            &mut thread,
            "java/util/concurrent/CyclicBarrier",
            "getNumberWaiting",
            "()I",
            &[Value::Object(Some(barrier))],
        )
        .unwrap()
        .unwrap();
        if waiting == Value::Int(want) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "getNumberWaiting stuck at {waiting:?}, expected {want}",
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Read one of `java.nio.charset.StandardCharsets`' static finals, the way
/// production does: run the registered `<clinit>`, then read the field.
///
/// `StandardCharsets.UTF_8` is a static FIELD, not a method. The two tests
/// below used to ask the registry for a *method* named `UTF_8` with
/// descriptor `()Ljava/nio/charset/Charset;`, which was how these constants
/// were served until the charset-NPE fix of 2026-05-21 replaced the
/// field-shaped natives with a `<clinit>` that populates the real static
/// slots (`phases_early.rs`). Nothing has registered the method form since,
/// so both tests panicked with "not registered" — invisibly, because this
/// whole module is `#[cfg(all(test, feature = "synthetic-jdk"))]` and did
/// not compile at all between the `JitMICSlot::update` arity change and its
/// repair.
///
/// The `<clinit>` writes through `set_static_field_by_name`, which resolves
/// the class by name and is a SILENT no-op when it is not loaded — hence
/// the explicit `ensure_class_initialized` first. Calling `<clinit>` again
/// afterwards is idempotent (it just rewrites the same six slots) and makes
/// the population unconditional whether or not the load already ran it.
pub(crate) fn standard_charset_static(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    field: &str,
) -> ObjectRef {
    const SCS: &str = "java/nio/charset/StandardCharsets";
    {
        let mut ctx = NativeContextImpl { shared, thread };
        ctx.ensure_class_initialized(SCS)
            .unwrap_or_else(|e| panic!("{SCS} failed to initialize: {e:?}"));
    }
    call_native(shared, thread, SCS, "<clinit>", "()V", &[])
        .unwrap_or_else(|e| panic!("{SCS}.<clinit> failed: {e:?}"));
    let mut ctx = NativeContextImpl { shared, thread };
    let cid = ctx
        .class_id_by_name(SCS)
        .unwrap_or_else(|| panic!("{SCS} not loaded after <clinit>"));
    let idx = ctx
        .static_field_index_by_name(cid, field)
        .unwrap_or_else(|| panic!("{SCS}.{field} is not a modelled static field"));
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(o)) => o,
        other => panic!("{SCS}.{field} = {other:?}, expected a Charset"),
    }
}

/// A real AES `SecretKeySpec`, built through its registered `<init>([B, String)`.
///
/// Two crypto tests used to hand `Cipher.init` / `Mac.init` a FABRICATED key --
/// `alloc_object(ClassId::new(0), 1)`, an object of class 0 that is not a `Key`
/// at all, or `null`. The old stubs accepted anything; the natives now apply the
/// JDK's key contract, and failed with "No installed provider supports this key:
/// (null)" -- verbatim the JDK's own `InvalidKeyException` text (verified on JDK
/// 25). A key built through `SecretKeySpec`'s `<init>` passes the same checks a
/// real caller's key would, including the "Missing argument" / "Empty key" ones
/// that `<init>` enforces. `len` bytes, all 0x2a: the value does not matter to
/// these tests, only that the key is well-formed.
pub(crate) fn make_secret_key(
    shared: &Arc<SharedVm>,
    thread: &mut crate::threading::JvmThread,
    algorithm: &str,
    len: usize,
) -> ObjectRef {
    const SKS: &str = "javax/crypto/spec/SecretKeySpec";
    let cid = {
        let mut ctx = NativeContextImpl { shared, thread };
        let _ = ctx.ensure_class_initialized(SKS);
        ctx.class_id_by_name(SKS).unwrap_or(ClassId::new(0))
    };
    let key = shared.mem.heap.alloc_object(cid, 2);
    let bytes = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, len);
    for i in 0..len {
        let _ = shared
            .mem
            .heap
            .set_array_element(bytes, i, Value::Int(0x2a));
    }
    let algo = create_java_string(shared, algorithm);
    call_native(
        shared,
        thread,
        SKS,
        "<init>",
        "([BLjava/lang/String;)V",
        &[
            Value::Object(Some(key)),
            Value::Object(Some(bytes)),
            Value::Object(Some(algo)),
        ],
    )
    .unwrap_or_else(|e| panic!("SecretKeySpec.<init>({algorithm}) failed: {e:?}"));
    key
}

/// Build a `Stream` over the given strings via `Stream.of(Object[])`.
pub(crate) fn stream_of_strings(
    shared: &Arc<SharedVm>,
    thread: &mut crate::threading::JvmThread,
    texts: &[&str],
) -> Value {
    let arr =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, texts.len());
    for (i, t) in texts.iter().enumerate() {
        let s = create_java_string(shared, t);
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Object(Some(s)));
    }
    call_native(
        shared,
        thread,
        "java/util/stream/Stream",
        "of",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(arr))],
    )
    .unwrap()
    .unwrap()
}

/// A `Comparator` lambda whose `compare(Object,Object)I` is `String.compareTo`.
pub(crate) fn string_order_comparator(shared: &Arc<SharedVm>) -> ObjectRef {
    make_lambda_proxy(
        shared,
        "java/util/Comparator",
        "compare",
        "(Ljava/lang/Object;Ljava/lang/Object;)I",
        "java/lang/String",
        "compareTo",
        "(Ljava/lang/String;)I",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    )
}

/// A `ToIntFunction` lambda whose `applyAsInt(Object)I` is `String.length`.
pub(crate) fn string_length_to_int_function(shared: &Arc<SharedVm>) -> ObjectRef {
    make_lambda_proxy(
        shared,
        "java/util/function/ToIntFunction",
        "applyAsInt",
        "(Ljava/lang/Object;)I",
        "java/lang/String",
        "length",
        "()I",
        MethodHandleKind::InvokeVirtual,
        vec![],
        &[],
    )
}

/// Allocate an object with a proper ClassId registered under `class_name`.
pub(crate) fn alloc_named(vm: &mut Vm, class_name: &str, fields: usize) -> crate::types::ObjectRef {
    let mut ctx = NativeContextImpl {
        shared: &vm.shared,
        thread: &mut vm.main_thread,
    };
    match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.alloc_object(cid, fields),
        Err(_) => ctx.alloc_object(ClassId::new(0), fields),
    }
}

/// Helper to create a VM with real JDK on boot classpath.
/// Returns None if no JDK is available.
pub(crate) fn vm_with_real_jdk() -> Option<Vm> {
    let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
        .ok()
        .or_else(|| {
            let p = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
            if p.exists() {
                Some(p.to_string_lossy().into_owned())
            } else {
                None
            }
        })?;
    let jmod = std::path::PathBuf::from(&java_home)
        .join("jmods")
        .join("java.base.jmod");
    if !jmod.exists() {
        return None;
    }
    let config = VmConfig {
        boot_classpath: vec![jmod.to_string_lossy().into_owned()],
        ..VmConfig::default()
    };
    Some(Vm::new(config))
}

/// Helper: build a ConstantPool with UTF-8 entries at 1-based indices.
pub(crate) fn annotation_cp(entries: &[&str]) -> cratonvm_reader::constant_pool::ConstantPool {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let mut cp_entries = vec![ConstantPoolEntry::Tombstone]; // index 0
    for s in entries {
        cp_entries.push(ConstantPoolEntry::Utf8((*s).into()));
    }
    ConstantPool::new(cp_entries)
}

/// Helper: build a reader Annotation (no elements).
pub(crate) fn mk_annotation(type_index: u16) -> cratonvm_reader::attribute::Annotation {
    cratonvm_reader::attribute::Annotation {
        type_index,
        element_value_pairs: vec![],
    }
}

/// Helper: build a reader Annotation with element-value pairs.
pub(crate) fn mk_annotation_with_elems(
    type_index: u16,
    pairs: Vec<cratonvm_reader::attribute::ElementValuePair>,
) -> cratonvm_reader::attribute::Annotation {
    cratonvm_reader::attribute::Annotation {
        type_index,
        element_value_pairs: pairs,
    }
}

/// Helper: create a class with given field definitions for M16 tests.
pub(crate) fn m16_register_class(
    shared: &Arc<SharedVm>,
    name: &str,
    field_defs: &[(&str, &str)], // (name, descriptor)
) -> ClassId {
    use cratonvm_reader::class_access_flags::FieldAccessFlags;
    use cratonvm_reader::field::ClassFileField;

    let fields: Vec<ClassFileField> = field_defs
        .iter()
        .map(|(n, d)| ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC,
            name: std::sync::Arc::<str>::from(*n),
            descriptor: std::sync::Arc::<str>::from(*d),
            attributes: vec![],
        })
        .collect();

    register_test_class(shared, name, 0x0021, &fields, &[]) // PUBLIC | SUPER
}

/// Wall-clock milliseconds, the same base
/// `NativeContext::touch_soft_reference` stamps soft entries with.
pub(crate) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before the Unix epoch")
        .as_millis() as u64
}

/// Build a `SoftReference` whose referent is unmarked (so it is a clear
/// candidate) and return `(reference address, is_marked-visible address)`.
pub(crate) fn soft_ref_under_test(shared: &Arc<SharedVm>, thread: &mut JvmThread) -> usize {
    let referent = shared.mem.heap.alloc_object(ClassId::new(0), 1);
    let soft_ref = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    call_native(
        shared,
        thread,
        "java/lang/ref/SoftReference",
        "<init>",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(soft_ref)), Value::Object(Some(referent))],
    )
    .unwrap();
    soft_ref.as_ptr() as usize
}

/// Set a `java.lang.reflect.Field` mirror's JDK-layout field BY NAME.
///
/// The two tests below used to hand-number these slots from the fabricated
/// model (`0=override, 1=clazz, 2=slot, 3=name, ...`), and one of them
/// derived an extra-slot index arithmetically from a 7-field stub. The
/// model moved on 2026-08-05 — `java.lang.reflect.AccessibleObject` declares
/// TWO fields, not one — and frozen indices are exactly what such a change
/// silently rots. Production never numbered these: `create_field_object`
/// and `read_field_meta` both go through `set_field_by_name` /
/// `get_field_by_name`, so the tests do too.
pub(crate) fn m11_set_field_mirror_by_name(
    shared: &Arc<SharedVm>,
    field: cratonvm_types::ObjectRef,
    class_id: ClassId,
    name: &str,
    value: Value,
) {
    let idx = {
        let cm = shared.classes.class_manager.read();
        crate::vm::vm_exec::resolve_field_index_in_hierarchy(class_id, name, &cm.class_store)
            .unwrap_or_else(|| panic!("java/lang/reflect/Field declares no `{name}`"))
    };
    shared.mem.heap.set_field(field, idx, value);
}

/// Helper: generate a key pair for the given algorithm via native calls.
/// Returns (KeyPair object, PublicKey object, PrivateKey object).
pub(crate) fn m22_generate_keypair(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    algorithm: &str,
    key_size: i32,
) -> (ObjectRef, ObjectRef, ObjectRef) {
    // KeyPairGenerator.getInstance(algorithm)
    let alg_str = crate::vm::vm_object::create_java_string(shared, algorithm);
    let kpg = call_native(
        shared,
        thread,
        "java/security/KeyPairGenerator",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;",
        &[Value::Object(Some(alg_str))],
    )
    .unwrap()
    .unwrap();
    let kpg_ref = match kpg {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected KPG object"),
    };

    // initialize(keySize)
    call_native(
        shared,
        thread,
        "java/security/KeyPairGenerator",
        "initialize",
        "(I)V",
        &[Value::Object(Some(kpg_ref)), Value::Int(key_size)],
    )
    .unwrap();

    // generateKeyPair()
    let kp = call_native(
        shared,
        thread,
        "java/security/KeyPairGenerator",
        "generateKeyPair",
        "()Ljava/security/KeyPair;",
        &[Value::Object(Some(kpg_ref))],
    )
    .unwrap()
    .unwrap();
    let kp_ref = match kp {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected KeyPair"),
    };

    // getPublic() / getPrivate()
    let pub_val = call_native(
        shared,
        thread,
        "java/security/KeyPair",
        "getPublic",
        "()Ljava/security/PublicKey;",
        &[Value::Object(Some(kp_ref))],
    )
    .unwrap()
    .unwrap();
    let pub_ref = match pub_val {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected PublicKey"),
    };

    let priv_val = call_native(
        shared,
        thread,
        "java/security/KeyPair",
        "getPrivate",
        "()Ljava/security/PrivateKey;",
        &[Value::Object(Some(kp_ref))],
    )
    .unwrap()
    .unwrap();
    let priv_ref = match priv_val {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected PrivateKey"),
    };

    (kp_ref, pub_ref, priv_ref)
}

/// Helper: sign data with the given algorithm and private key.
pub(crate) fn m22_sign(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    sig_algorithm: &str,
    private_key: ObjectRef,
    data: &[u8],
) -> ObjectRef {
    let alg_str = crate::vm::vm_object::create_java_string(shared, sig_algorithm);
    let sig = call_native(
        shared,
        thread,
        "java/security/Signature",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/Signature;",
        &[Value::Object(Some(alg_str))],
    )
    .unwrap()
    .unwrap();
    let sig_ref = match sig {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected Signature"),
    };

    // initSign(privateKey)
    call_native(
        shared,
        thread,
        "java/security/Signature",
        "initSign",
        "(Ljava/security/PrivateKey;)V",
        &[
            Value::Object(Some(sig_ref)),
            Value::Object(Some(private_key)),
        ],
    )
    .unwrap();

    // update(byte[])
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        data.len(),
    );
    for (i, &b) in data.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    call_native(
        shared,
        thread,
        "java/security/Signature",
        "update",
        "([B)V",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(arr))],
    )
    .unwrap();

    // sign() -> byte[]
    let sig_bytes = call_native(
        shared,
        thread,
        "java/security/Signature",
        "sign",
        "()[B",
        &[Value::Object(Some(sig_ref))],
    )
    .unwrap()
    .unwrap();
    match sig_bytes {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected signature byte[]"),
    }
}

/// Helper: verify signature.
pub(crate) fn m22_verify(
    shared: &Arc<SharedVm>,
    thread: &mut JvmThread,
    sig_algorithm: &str,
    public_key: ObjectRef,
    data: &[u8],
    signature: ObjectRef,
) -> bool {
    let alg_str = crate::vm::vm_object::create_java_string(shared, sig_algorithm);
    let sig = call_native(
        shared,
        thread,
        "java/security/Signature",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/Signature;",
        &[Value::Object(Some(alg_str))],
    )
    .unwrap()
    .unwrap();
    let sig_ref = match sig {
        Value::Object(Some(r)) => r,
        _ => panic!("M22: expected Signature"),
    };

    // initVerify(publicKey)
    call_native(
        shared,
        thread,
        "java/security/Signature",
        "initVerify",
        "(Ljava/security/PublicKey;)V",
        &[
            Value::Object(Some(sig_ref)),
            Value::Object(Some(public_key)),
        ],
    )
    .unwrap();

    // update(byte[])
    let arr = shared.mem.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Byte,
        data.len(),
    );
    for (i, &b) in data.iter().enumerate() {
        let _ = shared
            .mem
            .heap
            .set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    call_native(
        shared,
        thread,
        "java/security/Signature",
        "update",
        "([B)V",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(arr))],
    )
    .unwrap();

    // verify(byte[]) -> boolean
    let result = call_native(
        shared,
        thread,
        "java/security/Signature",
        "verify",
        "([B)Z",
        &[Value::Object(Some(sig_ref)), Value::Object(Some(signature))],
    )
    .unwrap()
    .unwrap();
    match result {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

/// Helper: create a SharedVm with custom native methods registered.
pub(crate) fn m6_shared_with_natives(
    registrations: &[(
        &str,
        &str,
        &str,
        fn(&mut dyn cratonvm_native_api::NativeContext, &[Value]) -> MethodCallResult,
    )],
) -> Arc<SharedVm> {
    let mut shared = SharedVm::new(VmConfig::default());
    for &(class_name, method_name, descriptor, callback) in registrations {
        shared
            .natives
            .native_methods
            .register(class_name, method_name, descriptor, callback);
    }
    let shared = Arc::new(shared);
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    shared
}

/// Helper: create SharedVm with builtins registered (for ThreadGroup tests).
pub(crate) fn p86_shared_with_builtins() -> Arc<SharedVm> {
    let mut shared = SharedVm::new(VmConfig::default());
    crate::native::builtins::register_builtins(&mut shared.natives.native_methods);
    let shared = Arc::new(shared);
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    shared
}

/// Helper: create SharedVm with collection natives registered (for CHM tests).
pub(crate) fn p86_shared_with_collections() -> Arc<SharedVm> {
    let mut shared = SharedVm::new(VmConfig::default());
    crate::native::collections::register_collections_natives(&mut shared.natives.native_methods);
    let shared = Arc::new(shared);
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    shared
}

/// Helper: invoke a native method from the collections registry.
pub(crate) fn p86_chm_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class: &str,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> crate::error::MethodCallResult {
    let cb = shared
        .natives
        .native_methods
        .find(class, method, descriptor)
        .unwrap_or_else(|| panic!("{class}.{method}{descriptor} not found"));
    let mut ctx = NativeContextImpl { shared, thread };
    cb(&mut ctx, args)
}

pub(crate) fn p90_shared() -> Arc<SharedVm> {
    let shared = SharedVm::new(VmConfig::default());
    Arc::new(shared)
}

pub(crate) fn p90_shared_with_builtins() -> Arc<SharedVm> {
    let mut shared = SharedVm::new(VmConfig::default());
    crate::native::builtins::register_builtins(&mut shared.natives.native_methods);
    let shared = Arc::new(shared);
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    shared
}

/// Serializes the four `p90_jfr_*` tests below.
///
/// They drain the process-global JFR ring, so running two of them at once
/// would let each steal the other's events.
pub(crate) static P90_JFR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `emit` with a recording active and return the events it produced.
///
/// `emit_*` does NOT write into the recording's repository: it pushes onto
/// the per-thread ring that `RingRegistry::drain_all` collects, and nothing
/// moves ring events into a `Recording` unless a dump asks for them. These
/// tests used to start a recording, emit, and then read
/// `Recording::get_events()` — a repository nothing had filled — so they
/// asserted against an empty vector however well the emit worked. Draining
/// the ring is both what actually proves the emit fired and what
/// `cratonvm-jfr`'s own tests do.
///
/// The recording still has to be started: every `emit_*` returns early
/// unless `cratonvm_jfr::is_enabled()`, which is what starting one sets.
pub(crate) fn p90_jfr_emitted(
    emit: impl FnOnce(&mut cratonvm_jfr::FlightRecorder),
) -> Vec<cratonvm_jfr::EventInstance> {
    let _guard = P90_JFR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let shared = p90_shared();
    let rec_id = {
        let mut fr = shared.debug.flight_recorder.lock();
        let id = fr.new_recording(cratonvm_jfr::RecordingSettings::new("test"));
        fr.start_recording(id);
        id
    };
    // Discard whatever an earlier test left on this thread's ring so the
    // events returned below are exactly the ones `emit` produced.
    let _ = cratonvm_jfr::repository::global_ring_registry().drain_all();
    {
        let mut fr = shared.debug.flight_recorder.lock();
        emit(&mut fr);
    }
    let drained = cratonvm_jfr::repository::global_ring_registry().drain_all();
    shared.debug.flight_recorder.lock().stop_recording(rec_id);
    drained
}

/// Helper to call a native method by looking it up in the registry directly.
pub(crate) fn p90_call_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class: &str,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = shared
        .natives
        .native_methods
        .find(class, method, descriptor)
        .unwrap_or_else(|| panic!("native not found: {}.{}{}", class, method, descriptor));
    let mut ctx = NativeContextImpl { shared, thread };
    cb(&mut ctx, args)
}

/// One `()I` method for a CHA fixture class, abstract or with a real body.
///
/// The hierarchies below are wired with `ClassStore::set_superclass` and
/// `ClassStore::set_interfaces` rather than a `superclass:` / `interfaces:`
/// field write, deliberately: those are the only two calls that keep the
/// store's two adjacency indices in step, and their union is exactly what
/// `subtypes_of` — and therefore `unique_implementor` — walks. Writing either
/// field through `get_mut` would produce a hierarchy that looks right and
/// analyses as if it had no subtypes at all.
///
/// These tests live in this module because the rest of the
/// `unique_implementor` tests already do, and splitting them would hide the
/// interface cases from anyone reading the others. The cost is that the whole
/// module is `#[cfg(all(test, feature = "synthetic-jdk"))]`, so they only run
/// in a feature build; the store-level index tests that need no VM at all are
/// in `classloading/src/class.rs` instead, where they run unconditionally.
pub(crate) fn cha_method(name: &str, abstract_method: bool) -> cratonvm_reader::method::ClassFileMethod {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let mut flags = MethodAccessFlags::PUBLIC;
    let attributes = if abstract_method {
        flags |= MethodAccessFlags::ABSTRACT;
        vec![]
    } else {
        vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                // iconst_1; ireturn
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0xAC]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))]
    };
    cratonvm_reader::method::ClassFileMethod {
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from("()I"),
        access_flags: flags,
        attributes,
    }
}

/// Emit through `emit` against a fresh recorder and return the events that
/// reached the JFR ring. Local-recorder sibling of [`p90_jfr_emitted`] —
/// see that helper for why `Recording::event_count()` reads empty here.
pub(crate) fn s41_jfr_emitted(
    name: &str,
    emit: impl FnOnce(&mut cratonvm_jfr::FlightRecorder),
) -> Vec<cratonvm_jfr::EventInstance> {
    let _guard = P90_JFR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut fr = cratonvm_jfr::create_flight_recorder();
    let rid = fr.new_recording(cratonvm_jfr::RecordingSettings::new(name));
    fr.start_recording(rid);
    let _ = cratonvm_jfr::repository::global_ring_registry().drain_all();
    emit(&mut fr);
    let drained = cratonvm_jfr::repository::global_ring_registry().drain_all();
    fr.stop_recording(rid);
    drained
}

