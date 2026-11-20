use super::*;
use cratonvm_native_api::{
    FieldMetadata, GcCollectorInfo, GcMemoryPoolInfo, NativeClassAccess, NativeHeapAccess,
    NativeMethodRegistry, NativeSystemAccess,
};

const USAGE: &str = "()Ljava/lang/management/MemoryUsage;";

/// Give `class` named instance fields at slots `0..`, so the natives'
/// by-name reads and writes resolve under the mock.
fn declare(ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext, class: &str, names: &[&str]) {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    let fields = names
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
        .collect();
    ctx.set_declared_fields(cid, fields);
}

fn ctx_with_layouts() -> cratonvm_native_builtins::test_utils::MockNativeContext {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    declare(
        &mut ctx,
        "sun/management/MemoryPoolImpl",
        &["name", "isHeap", "isValid", "peakUsage"],
    );
    declare(
        &mut ctx,
        "sun/management/GarbageCollectorImpl",
        &["name", "isValid"],
    );
    declare(
        &mut ctx,
        "java/lang/management/MemoryUsage",
        &["init", "used", "committed", "max"],
    );
    ctx
}

const YOUNG_POOLS: &[&str] = &["Eden Space", "Survivor Space"];
const ALL_POOLS: &[&str] = &["Eden Space", "Survivor Space", "Tenured Gen"];

fn serial_beans(ctx: &cratonvm_native_builtins::test_utils::MockNativeContext) {
    ctx.set_gc_beans(
        vec![
            GcCollectorInfo {
                name: "Copy",
                count: 7,
                time_ms: 15,
                pool_names: YOUNG_POOLS,
            },
            GcCollectorInfo {
                name: "MarkSweepCompact",
                count: 3,
                time_ms: 25,
                pool_names: ALL_POOLS,
            },
        ],
        vec![
            GcMemoryPoolInfo {
                name: "Eden Space",
                init: -1,
                used: 1_000,
                committed: 8_000,
                max: 16_000,
                collection_used: 200,
                collection_committed: 8_000,
                manager_names: &["Copy", "MarkSweepCompact"],
            },
            GcMemoryPoolInfo {
                name: "Tenured Gen",
                init: 32_000,
                used: 5_000,
                committed: 32_000,
                max: 32_000,
                collection_used: -1,
                collection_committed: -1,
                manager_names: &["MarkSweepCompact"],
            },
        ],
    );
}

fn call(
    r: &NativeMethodRegistry,
    ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    this: ObjectRef,
) -> Value {
    let f = r
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("{class}.{method}{desc} is not registered"));
    f(ctx, &[Value::Object(Some(this))])
        .expect("native call failed")
        .expect("native returned void")
}

fn usage_of(ctx: &cratonvm_native_builtins::test_utils::MockNativeContext, v: Value) -> [i64; 4] {
    let Value::Object(Some(mu)) = v else {
        panic!("expected a MemoryUsage, got {v:?}")
    };
    let mut out = [0i64; 4];
    for (i, o) in out.iter_mut().enumerate() {
        *o = match ctx.get_field(mu, i) {
            Value::Long(x) => x,
            other => panic!("MemoryUsage slot {i} is {other:?}"),
        };
    }
    out
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_memory_pool_impl(&mut r);
    register_memory_manager_impl(&mut r);
    register_garbage_collector_impl(&mut r);
    r
}

#[test]
fn a_described_pool_reports_its_real_usage_and_collection_usage() {
    let r = registry();
    let mut ctx = ctx_with_layouts();
    serial_beans(&ctx);
    let cls = "sun/management/MemoryPoolImpl";
    let eden = alloc_memory_pool_impl(&mut ctx, "Eden Space", true).unwrap();
    let u = call(&r, &mut ctx, cls, "getUsage0", USAGE, eden);
    assert_eq!(usage_of(&ctx, u), [-1, 1_000, 8_000, 16_000]);
    let c = call(&r, &mut ctx, cls, "getCollectionUsage0", USAGE, eden);
    assert_eq!(usage_of(&ctx, c), [-1, 200, 8_000, 16_000]);
    // Before the first collection (`collection_used == -1`) the used
    // figure is 0, as on HotSpot, never the undefined sentinel; gci
    // i14/mp: and so is `committed` (HotSpot's `_after_gc_usage(init, 0,
    // 0, max)`), not the pool's current 32 000.
    let tenured = alloc_memory_pool_impl(&mut ctx, "Tenured Gen", true).unwrap();
    let t = call(&r, &mut ctx, cls, "getCollectionUsage0", USAGE, tenured);
    assert_eq!(usage_of(&ctx, t), [32_000, 0, 0, 32_000]);
    // The peak is at least the current usage.
    let p = call(&r, &mut ctx, cls, "getPeakUsage0", USAGE, eden);
    assert_eq!(usage_of(&ctx, p)[1], 1_000);
}

/// zs s3/ob2: a non-heap pool the VM describes (ZGC's `Metaspace`)
/// reports its usage, no collection usage (`null`, as HotSpot 25), and
/// its plain manager lists it; without one (`=0`, every other backend)
/// the manager lists gci i15/nh's metaspace pools.
#[test]
fn a_non_heap_pool_reports_usage_null_collection_usage_and_its_manager() {
    let r = registry();
    let mut ctx = ctx_with_layouts();
    serial_beans(&ctx);
    ctx.set_non_heap_pools(vec![GcMemoryPoolInfo {
        name: "Metaspace",
        init: 0,
        used: 700,
        committed: 700,
        max: -1,
        collection_used: -1,
        collection_committed: -1,
        manager_names: &["Metaspace Manager"],
    }]);
    let cls = "sun/management/MemoryPoolImpl";
    let ms = alloc_memory_pool_impl(&mut ctx, "Metaspace", false).unwrap();
    let u = call(&r, &mut ctx, cls, "getUsage0", USAGE, ms);
    assert_eq!(usage_of(&ctx, u), [0, 700, 700, -1]);
    let c = call(&r, &mut ctx, cls, "getCollectionUsage0", USAGE, ms);
    assert_eq!(c, Value::Object(None));
    // A heap pool keeps its collection usage.
    let eden = alloc_memory_pool_impl(&mut ctx, "Eden Space", true).unwrap();
    let c = call(&r, &mut ctx, cls, "getCollectionUsage0", USAGE, eden);
    assert_eq!(usage_of(&ctx, c)[1], 200);
    // zs s4/rv3: a heap pool's queries never measure the non-heap pools
    // (the VM's measure takes the class-manager lock and may walk the
    // class store); the Metaspace query above did.
    assert!(ctx.non_heap_pools_calls.get() > 0, "positive control");
    let before = ctx.non_heap_pools_calls.get();
    let _ = call(&r, &mut ctx, cls, "getUsage0", USAGE, eden);
    let _ = call(&r, &mut ctx, cls, "getCollectionUsage0", USAGE, eden);
    assert_eq!(ctx.non_heap_pools_calls.get(), before);
    assert!(is_zgc_non_heap_pool(&ctx, "Metaspace"));
    assert!(!is_zgc_non_heap_pool(&ctx, "Eden Space"));
    assert_eq!(non_heap_manager_names(&ctx), vec!["Metaspace Manager"]);

    let pools_of = |ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext, mgr: ObjectRef| {
        let v = call(
            &r,
            ctx,
            "sun/management/MemoryManagerImpl",
            "getMemoryPools0",
            "()[Ljava/lang/management/MemoryPoolMXBean;",
            mgr,
        );
        let Value::Object(Some(arr)) = v else {
            panic!("expected an array")
        };
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Object(Some(p)) => bean_name(&*ctx, p).unwrap_or_default(),
                other => panic!("pool array element is {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let mgr = alloc_garbage_collector_impl(&mut ctx, "Metaspace Manager").unwrap();
    assert_eq!(pools_of(&mut ctx, mgr), vec!["Metaspace".to_string()]);
    // Merge with gci i15/nh: without VM-modelled pools the manager
    // answers gci i15/nh's metaspace pools.
    ctx.set_non_heap_pools(Vec::new());
    assert_eq!(pools_of(&mut ctx, mgr), METASPACE_POOLS);
    assert!(non_heap_manager_names(&ctx).is_empty());
}

#[test]
fn collector_beans_count_per_bean_and_list_their_pools() {
    let r = registry();
    let mut ctx = ctx_with_layouts();
    serial_beans(&ctx);
    let cls = "sun/management/GarbageCollectorImpl";
    let copy = alloc_garbage_collector_impl(&mut ctx, "Copy").unwrap();
    let full = alloc_garbage_collector_impl(&mut ctx, "MarkSweepCompact").unwrap();
    assert_eq!(
        call(&r, &mut ctx, cls, "getCollectionCount", "()J", copy),
        Value::Long(7)
    );
    assert_eq!(
        call(&r, &mut ctx, cls, "getCollectionCount", "()J", full),
        Value::Long(3)
    );
    assert_eq!(
        call(&r, &mut ctx, cls, "getCollectionTime", "()J", copy),
        Value::Long(15)
    );
    assert_eq!(
        call(&r, &mut ctx, cls, "getCollectionTime", "()J", full),
        Value::Long(25)
    );

    // `getMemoryPoolNames()` walks `MemoryManagerImpl.getMemoryPools0`.
    let pools = call(
        &r,
        &mut ctx,
        "sun/management/MemoryManagerImpl",
        "getMemoryPools0",
        "()[Ljava/lang/management/MemoryPoolMXBean;",
        full,
    );
    let Value::Object(Some(arr)) = pools else {
        panic!("expected an array")
    };
    let names: Vec<String> = (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Object(Some(p)) => match ctx.get_field_by_name(p, "name") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                other => panic!("pool bean name is {other:?}"),
            },
            other => panic!("pool array element is {other:?}"),
        })
        .collect();
    assert_eq!(names, ["Eden Space", "Survivor Space", "Tenured Gen"]);
}

/// gci i14/mp findings 2 and 3: HotSpot's `getCollectionUsage()` before a
/// collection that affected the pool is `(init, 0, 0, max)`; after one, the
/// used AND committed recorded at that collection's end (not today's
/// committed), widened only to keep `MemoryUsage`'s invariants.
#[test]
fn gci_i14_mp_collection_usage_before_the_first_collection_is_zero_committed() {
    let pool = |collection_used: i64, collection_committed: i64, max: i64| GcMemoryPoolInfo {
        name: "G1 Old Gen",
        init: 4_096,
        used: 9_000,
        committed: 16_000,
        max,
        collection_used,
        collection_committed,
        manager_names: &["G1 Young Generation"],
    };
    assert_eq!(collection_usage_quad(&pool(-1, -1, 64_000)), [4_096, 0, 0, 64_000]);
    assert_eq!(collection_usage_quad(&pool(-1, -1, -1)), [4_096, 0, 0, -1]);
    // Finding 3: another collection stamped `used` (0) but none that
    // affected this pool: still HotSpot's initial usage.
    assert_eq!(collection_usage_quad(&pool(0, -1, 64_000)), [4_096, 0, 0, 64_000]);
    // An affecting collection: its used and ITS committed, not today's
    // 16 000.
    assert_eq!(
        collection_usage_quad(&pool(3_000, 12_000, 64_000)),
        [4_096, 3_000, 12_000, 64_000]
    );
    // A recorded used above the recorded committed widens committed and max.
    assert_eq!(
        collection_usage_quad(&pool(20_000, 16_000, 18_000)),
        [4_096, 20_000, 20_000, 20_000]
    );
    // A recorded 0 keeps the recorded committed.
    assert_eq!(collection_usage_quad(&pool(0, 8_000, -1)), [4_096, 0, 8_000, -1]);
}

/// The negative control: a context with no per-bean model (G1, ZGC, every
/// other mock) keeps the historical answers byte-for-byte.
#[test]
fn an_undescribed_backend_keeps_the_historical_answers() {
    let r = registry();
    let mut ctx = ctx_with_layouts();
    let eden = alloc_memory_pool_impl(&mut ctx, "Eden Space", true).unwrap();
    let u = call(
        &r,
        &mut ctx,
        "sun/management/MemoryPoolImpl",
        "getUsage0",
        USAGE,
        eden,
    );
    assert_eq!(usage_of(&ctx, u), [-1, -1, -1, -1]);
    let g1 = alloc_garbage_collector_impl(&mut ctx, "G1 Young Generation").unwrap();
    let count = call(
        &r,
        &mut ctx,
        "sun/management/GarbageCollectorImpl",
        "getCollectionCount",
        "()J",
        g1,
    );
    assert_eq!(count, Value::Long(ctx.gc_collection_count() as i64));
}

/// gc-common w6-d: a G1 / ZGC collector bean built here has its
/// `NotificationEmitterSupport` fields, so `addNotificationListener`'s
/// `synchronized (listenerLock)` does not throw `NullPointerException`.
#[test]
fn w6d_the_collector_bean_can_take_a_notification_listener() {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    declare(
        &mut ctx,
        "sun/management/GarbageCollectorImpl",
        &["name", "isValid", "listenerLock", "listenerList"],
    );
    let bean = alloc_garbage_collector_impl(&mut ctx, "G1 Young Generation").unwrap();
    for field in ["listenerLock", "listenerList"] {
        assert!(
            matches!(ctx.get_field_by_name(bean, field), Value::Object(Some(_))),
            "`{field}` must be initialised"
        );
    }
}
