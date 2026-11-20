    use super::*;
    use cratonvm_native_api::{
        FieldMetadata, GcCollectorInfo, GcPoolUsageSnapshot, NativeClassAccess, NativeHeapAccess,
        NativeMethodRegistry,
    };
    use cratonvm_types::ArrayElementType;

    fn registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_garbage_collector_impl(&mut r);
        r
    }

    fn call(
        r: &NativeMethodRegistry,
        ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext,
        class: &str,
        method: &str,
        desc: &str,
        args: &[Value],
    ) -> Value {
        let f = r
            .find(class, method, desc)
            .unwrap_or_else(|| panic!("{class}.{method}{desc} is not registered"));
        f(ctx, args)
            .expect("native call failed")
            .unwrap_or(Value::Object(None))
    }

    const SET_ENABLED: &str = "(Lcom/sun/management/GarbageCollectorMXBean;Z)V";
    const NUM_ATTRS: &str = "(Ljava/lang/management/GarbageCollectorMXBean;)I";
    const FILL_ATTRS: &str =
        "(Ljava/lang/management/GarbageCollectorMXBean;I[Ljava/lang/String;[C[Ljava/lang/String;)V";
    const LAST_GC_INFO: &str =
        "(Ljava/lang/management/GarbageCollectorMXBean;I[Ljava/lang/Object;[C\
[Ljava/lang/management/MemoryUsage;[Ljava/lang/management/MemoryUsage;)Lcom/sun/management/GcInfo;";

    /// A `Copy` collector bean whose `name` resolves by name under the mock,
    /// in a context that describes a `Copy` collector.
    fn copy_bean(ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext) -> ObjectRef {
        let cid = ctx
            .ensure_class_initialized("sun/management/GarbageCollectorImpl")
            .unwrap();
        ctx.set_declared_fields(
            cid,
            ["name", "isValid"]
                .iter()
                .enumerate()
                .map(|(i, n)| FieldMetadata {
                    name: (*n).to_string(),
                    descriptor: "Ljava/lang/Object;".to_string(),
                    access_flags: 0,
                    slot_index: i,
                    declaring_class_id: cid,
                    is_static: false,
                })
                .collect(),
        );
        ctx.set_gc_beans(
            vec![GcCollectorInfo {
                name: "Copy",
                count: 1,
                time_ms: 1,
                pool_names: &["Eden Space", "Survivor Space"],
            }],
            Vec::new(),
        );
        alloc_garbage_collector_impl(ctx, "Copy").unwrap()
    }

    #[test]
    fn every_native_is_an_explicit_bridge() {
        let r = registry();
        for (class, method, desc) in [
            (GC_EXT_IMPL, "setNotificationEnabled", SET_ENABLED),
            (GC_INFO_BUILDER, "getNumGcExtAttributes", NUM_ATTRS),
            (GC_INFO_BUILDER, "fillGcAttributeInfo", FILL_ATTRS),
            (GC_INFO_BUILDER, "getLastGcInfo0", LAST_GC_INFO),
        ] {
            let (_, kind) = r
                .find_with_kind(class, method, desc)
                .unwrap_or_else(|| panic!("{class}.{method}{desc} is not registered"));
            assert_eq!(kind, cratonvm_native_api::NativeKind::Bridge, "{class}.{method}{desc}");
        }
    }

    #[test]
    fn set_notification_enabled_reaches_the_vm_by_bean_name() {
        let r = registry();
        let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
        let b = Value::Object(Some(copy_bean(&mut ctx)));
        let m = "setNotificationEnabled";
        for on in [1, 0] {
            let args = [b, b, Value::Int(on)];
            call(&r, &mut ctx, GC_EXT_IMPL, m, SET_ENABLED, &args);
        }
        assert_eq!(
            *ctx.gc_notification_enabled_calls.borrow(),
            vec![("Copy".to_string(), true), ("Copy".to_string(), false)]
        );
    }

    thread_local! {
        /// gci i48b/oe47d: the `(class, method)` pairs the warm-up invoked.
        static WARM_UP_CALLS: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Records every call; `getGcInfoBuilder` THROWS (a build that fails at
    /// its first step), everything else falls through to the mock.
    fn warm_up_hook(
        _ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext,
        class_name: &str,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        WARM_UP_CALLS.with(|c| {
            c.borrow_mut()
                .push((class_name.to_string(), method_name.to_string()))
        });
        if method_name == "getGcInfoBuilder" {
            return Some(Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: "Java heap space".to_string(),
                }),
            )));
        }
        None
    }

    /// gci i48b/oe47d: enabling a bean's notifications builds (and drops) one
    /// notification on the registering thread, starting at the bean's
    /// `getGcInfoBuilder()` as the drain's build does; disabling builds
    /// nothing; a build that throws does not fail the native, and nothing is
    /// sent (`createGCNotification` / `getNextSeqNumber` are never called).
    #[test]
    fn gci_i47w_oe47_enabling_notifications_warms_the_build() {
        let r = registry();
        let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
        let b = Value::Object(Some(copy_bean(&mut ctx)));
        let m = "setNotificationEnabled";
        ctx.set_invoke_hook(warm_up_hook);
        WARM_UP_CALLS.with(|c| c.borrow_mut().clear());
        call(&r, &mut ctx, GC_EXT_IMPL, m, SET_ENABLED, &[b, b, Value::Int(0)]);
        assert!(WARM_UP_CALLS.with(|c| c.borrow().is_empty()), "disable builds nothing");
        // `call` panics on an `Err`: the throwing build did not fail the native.
        let out = call(&r, &mut ctx, GC_EXT_IMPL, m, SET_ENABLED, &[b, b, Value::Int(1)]);
        assert!(matches!(out, Value::Object(None)), "{out:?}");
        let calls = WARM_UP_CALLS.with(|c| c.borrow().clone());
        assert_eq!(
            calls.first(),
            Some(&(GC_EXT_IMPL.to_string(), "getGcInfoBuilder".to_string())),
            "{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|(_, m)| m == "createGCNotification" || m == "getNextSeqNumber"),
            "{calls:?}"
        );
        assert_eq!(
            *ctx.gc_notification_enabled_calls.borrow(),
            vec![("Copy".to_string(), false), ("Copy".to_string(), true)]
        );
    }

    #[test]
    fn the_builder_is_told_about_hotspots_one_extension_attribute() {
        let r = registry();
        let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
        let bean = Value::Object(Some(copy_bean(&mut ctx)));
        // The receiver (`this` builder) is not read by either native.
        let builder = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 0)));
        let cls = GC_INFO_BUILDER;
        let m = "getNumGcExtAttributes";
        let n = call(&r, &mut ctx, cls, m, NUM_ATTRS, &[builder, bean]);
        assert_eq!(n, Value::Int(1));
        let names = ctx.new_ref_array(ClassId::new(0), 1);
        let types = ctx.new_array(ArrayElementType::Char, 1);
        let descs = ctx.new_ref_array(ClassId::new(0), 1);
        let args = [
            builder,
            bean,
            Value::Int(1),
            Value::Object(Some(names)),
            Value::Object(Some(types)),
            Value::Object(Some(descs)),
        ];
        call(&r, &mut ctx, cls, "fillGcAttributeInfo", FILL_ATTRS, &args);
        let text = |ctx: &cratonvm_native_builtins::test_utils::MockNativeContext, arr: ObjectRef| {
            let s = match ctx.get_array_element(arr, 0) {
                Value::Object(Some(s)) => s,
                other => panic!("expected a String, got {other:?}"),
            };
            ctx.read_string(s).unwrap_or_default()
        };
        assert_eq!(text(&ctx, names), "GcThreadCount");
        assert_eq!(text(&ctx, descs), "Number of GC threads");
        assert_eq!(ctx.get_array_element(types, 0), Value::Int(i32::from(b'I')));
    }

    /// gci i19/ob (`gci-i14mp-...-review-findings-20261013` item 6): HotSpot
    /// 25's `GcThreadCount` -- 5 on each G1 manager (HS.err of
    /// `GciMp14PoolNumbersProbe info` under `-XX:+UseG1GC`), 1 on Serial's
    /// (`MarkSweepCompact`, `Copy`) and on every other bean.
    #[test]
    fn gci_i19_ob_gc_thread_count_is_hotspots_per_collector() {
        for g1 in ["G1 Young Generation", "G1 Old Generation", "G1 Concurrent GC"] {
            assert_eq!(gc_ext_thread_count(g1), 5, "{g1}");
        }
        for other in ["Copy", "MarkSweepCompact", "ZGC Cycles", "ZGC Pauses", ""] {
            assert_eq!(gc_ext_thread_count(other), 1, "{other}");
        }
    }

    #[test]
    fn get_last_gc_info_is_null_before_a_recorded_collection() {
        // The mock's `gc_last_collection` is the trait default (`None`), which
        // is also what G1 and ZGC answer.
        let r = registry();
        let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
        let bean = Value::Object(Some(copy_bean(&mut ctx)));
        let builder = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 0)));
        let none = Value::Object(None);
        let args = [builder, bean, Value::Int(1), none, none, none, none];
        let m = "getLastGcInfo0";
        let got = call(&r, &mut ctx, GC_INFO_BUILDER, m, LAST_GC_INFO, &args);
        assert_eq!(got, Value::Object(None));
    }

    #[test]
    fn a_pool_the_record_does_not_describe_is_undefined_not_null() {
        let snap = [GcPoolUsageSnapshot {
            name: "Eden Space",
            init: -1,
            used: 10,
            committed: 20,
            max: 40,
        }];
        assert_eq!(pool_usage_in(&snap, "Eden Space"), [-1, 10, 20, 40]);
        assert_eq!(pool_usage_in(&snap, "Metaspace"), [-1, 0, 0, -1]);
    }

    #[test]
    fn no_bean_to_deliver_to_is_not_a_failure() {
        // `MemoryImpl.mgrs` was never populated (the mock defines no such
        // class): nobody can have registered a listener, so delivery is a
        // no-op rather than an error.
        let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
        let now = std::time::Instant::now();
        let info = cratonvm_native_api::GcNotificationInfo {
            gc_name: "Copy",
            gc_action: "end of minor GC",
            gc_cause: "Allocation Failure",
            id: 1,
            start: now,
            end: now,
            timestamp_ms: 0,
            before: Vec::new(),
            after: Vec::new(),
        };
        let delivered = deliver_gc_notification(&mut ctx, &info);
        assert!(matches!(delivered, Ok(false)));
    }
