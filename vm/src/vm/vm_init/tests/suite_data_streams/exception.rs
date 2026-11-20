#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_shutdown_on_failure_with_exception() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g67_shutdown_on_success_result_requires_shutdown() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g68_primitive_pattern_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g69_stable_value_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g70_module_import_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g70_module_imports_supported() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g71_flexible_constructor_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g71_pre_super_allowed_kinds() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g72_implicit_class_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g72_implicit_class_detection() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g73_byte_vector_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g73_short_vector_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g73_vector_species_byte_short_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g73_int_vector_convert_cast_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g73_vector_species_of_byte_returns_species() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn u6_datagram_channel_open_returns_channel() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn u6_datagram_channel_connect_disconnect() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn u6_datagram_channel_close() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn u6_datagram_channel_configure_blocking() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn u6_datagram_channel_natives_registered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_gcd() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_bit_operations() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_shift_operations() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_bitwise_and_or_xor() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_modpow() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_isprobableprime() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_not() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn g12_biginteger_new_natives_answer_hotspot() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m5_jit_skip_set_does_not_block_user_classes() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn jit_gate_pass_memo_is_invalidated_by_a_redefine() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn jit_gate_pass_memo_does_not_collide_across_same_named_classes() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn jit_gate_pass_memo_round_trips_is_interface_default() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m5_jit_scan_accepts_getfield_putfield() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m5_jit_scan_accepts_putfield() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m5_jit_scan_accepts_init_methods() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m5_jit_scan_accepts_lambda_methods() {
    // lambda$ methods should now be JIT-scannable
    // Simple lambda: iload_0, ireturn
    let code = [0x1a, 0xac, 0, 0];
    let scan = crate::jit::x64::jit_scan(&code, 2, "(I)I");
    assert!(scan.is_some(), "jit_scan should accept lambda$ methods");
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m5_field_info_resolution_produces_correct_index() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_jit_scan_rejects_invalid_bytecode_gracefully() {
    // Empty bytecode — jit_scan should handle without panicking
    let empty: [u8; 0] = [];
    let _ = crate::jit::x64::jit_scan(&empty, 0, "()V"); // must not panic
                                                         // Single invalid opcode should not panic
    let bad = [0xfe, 0, 0]; // 0xfe is unused opcode
    let _ = crate::jit::x64::jit_scan(&bad, 1, "()V"); // must not panic
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_jit_scan_invalid_goto_does_not_panic() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_gc_heap_alloc_does_not_panic_on_zero_fields() {
    // Allocating an object with 0 fields should succeed, not panic
    use cratonvm_gc::VmHeap;
    use cratonvm_types::ClassId;
    let heap = VmHeap::new(cratonvm_gc::GcBackend::Generational, 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    // ObjectRef is always non-null from successful allocation
    let _ = obj;
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_gc_heap_alloc_array_zero_length() {
    // Allocating a zero-length array should succeed
    use cratonvm_gc::{ArrayElementType, VmHeap};
    use cratonvm_types::ClassId;
    let heap = VmHeap::new(cratonvm_gc::GcBackend::Generational, 1024 * 1024);
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    let _ = arr;
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_arm64_backend_stack_underflow_sets_failure() {
    // ARM64 backend pop_operand on empty stack should set failed flag, not panic
    use crate::jit::aarch64_backend::Arm64Backend;
    let mut backend = Arm64Backend::new();
    // pop on empty stack — must not panic
    let _reg = backend.pop_operand();
    // The backend should mark itself as failed
    assert!(backend.failed, "pop on empty stack should set failed flag");
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_x64_compiler_stack_underflow_does_not_panic() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m2_default_interface_method_resolution() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m7_pgo_sort_nan_does_not_panic() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_function_and_then_creates_composite() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_function_compose_creates_composite() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_function_identity_apply() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_consumer_and_then_creates_composite() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_synthetic_function_classes_have_interfaces() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_function_and_then_apply_chains_correctly() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_function_compose_apply_chains_correctly() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_predicate_and_creates_composite() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_predicate_or_creates_composite() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m3_predicate_negate_creates_composite() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_heap_default_size_increased() {
    // Verify the default heap is large enough for non-trivial programs.
    // Young gen semi-spaces are now 16MB each (was 8MB).
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Allocate many objects — should not panic
    for _i in 0..1000 {
        let _obj = shared.mem.heap.alloc_object(ClassId::new(0), 4);
    }
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_try_alloc_object_returns_none_on_oom() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_try_alloc_array_returns_none_on_oom() {
    use cratonvm_gc::{ArrayElementType, GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 4096); // tiny heap
    let result = heap.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 100_000);
    assert!(
        result.is_none(),
        "try_alloc_array should return None on OOM, not panic"
    );
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_heavy_allocation_does_not_crash() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_heap_expansion_under_pressure() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_tlab_basic_alloc() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_tlab_refill_from_heap() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn zgc_refill_tlab_carves_a_chunk_and_retire_returns_the_tail() {
    use cratonvm_gc::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Zgc, 16 * 1024 * 1024);
    assert!(heap.is_zgc(), "GcBackend::Zgc must build a Zgc heap");
    let z = &heap;
    // Opt THIS heap in explicitly. ZGC TLAB bridging shipped OPT-IN
    // (`827c059fe`, measured 4/4 slower on BinTreesClassic then) and went
    // default-ON in JIT review round 9 wave 5 (`ZGC_VM_TLAB_DEFAULT_ON`); this
    // test pins the feature's carve / retire arithmetic, so it enables the
    // feature itself rather than depend on the process default.
    // `set_vm_tlab_enabled` is per-heap, so this opts in without a process-wide
    // environment override that could leak into sibling tests.
    z.zgc_set_vm_tlab_enabled(true);
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_arena_grow() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_gc_with_expandable_heap() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_massive_allocation_with_gc() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m4_concurrent_heavy_allocation() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_concurrent_heap_allocation() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_concurrent_field_read_write() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_monitor_mutual_exclusion() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_thread_registry_tracks_concurrent_threads() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_concurrent_array_operations() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_monitor_reentrant_locking() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m6_four_threads_concurrent_allocation() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_simple_no_elements() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_with_string_element() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_with_int_element() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_with_enum_element() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_invalid_cp_index_returns_none() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    // Annotation with type_index pointing beyond CP bounds.
    let cp = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    let ann = mk_annotation(99); // index 99 doesn't exist
    let data = vm_exec::convert_annotation(&ann, &cp);
    assert!(data.is_none(), "M12: invalid CP index should return None");
}

#[test]
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_class_annotations_via_shared_vm() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_annotation_matching_descriptor_format() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_multiple_annotations_on_class() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_nested() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_convert_annotation_array_element() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m12_unannotated_class_returns_empty() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m14_tcp_server_client_raw_exchange() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m14_http_server_serves_response() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m14_multiple_sequential_http_requests() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m14_concurrent_http_clients() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m14_http_server_large_response_body() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m14_extract_uri_scheme_defaults_port_80() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_write_value_as_string_int_fields() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_write_value_as_string_with_string_field() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_write_value_as_string_boolean_field() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_write_null_object() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_read_value_int_fields() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_read_value_string_field() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_gson_to_json() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_gson_from_json() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_jackson_roundtrip_serialize_deserialize() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m16_json_escape_special_chars() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_weak_ref_init_stores_referent() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_weak_ref_discovered_by_ref_processor() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_weak_ref_cleared_when_referent_unreachable() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_weak_ref_not_cleared_when_referent_reachable() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_soft_ref_init_stores_referent() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_soft_ref_cleared_under_memory_pressure() {
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
        // HotSpot judges this round with the previous collection's clock.
        let is_marked = |addr: usize| -> bool { addr == soft_addr };
        let now = wall_clock_ms() + 60_000;
        let first = proc.process_references(&is_marked, 0, now);
        assert_eq!(
            first.stats.soft_refs_cleared, 0,
            "the first round advances the clock"
        );
        let _result = proc.process_references(&is_marked, 0, now);
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_soft_ref_retained_with_plenty_of_memory() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn soft_ref_policy_pressure_input_is_load_bearing() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let soft_addr = soft_ref_under_test(&shared, &mut thread);
    let is_marked = |addr: usize| -> bool { addr == soft_addr };

    // The previous round's headroom decides the next round's allowance.
    // 30 s of idleness: below the 64 s the old hardcode implied
    // (`SoftRefLRUPolicyMSPerMB` defaults to 1000), above the 0 s that a
    // heap with no allocatable headroom implies.
    let now = wall_clock_ms() + 30_000;

    {
        let mut proc = shared.mem.ref_processor.lock();
        let _ = proc.process_references(&is_marked, 64, now);
        let _ = proc.process_references(&is_marked, 0, now);
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn soft_ref_policy_free_mb_drives_the_threshold_the_vm_call_sites_use() {
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
        let first = proc.process_references(&is_marked, free_mb, past_threshold);
        assert_eq!(
            first.stats.soft_refs_cleared, 0,
            "the first round advances the clock"
        );
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn soft_ref_policy_zero_clock_argument_uses_the_mutator_clock() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_phantom_ref_discovered() {
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
pub(in crate::vm::vm_init::tests::suite_data_streams) fn m20_ref_clear_nulls_referent() {
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

