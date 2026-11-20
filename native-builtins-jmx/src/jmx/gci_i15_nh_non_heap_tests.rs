use super::*;
use cratonvm_native_api::registry::{JMX_NON_HEAP_MANAGER_NAMES, JMX_NON_HEAP_POOL_NAMES};
use cratonvm_native_api::{
    FieldMetadata, GcCollectorInfo, GcMemoryPoolInfo, GcPoolUsageSnapshot, NativeClassAccess,
    NativeHeapAccess, NativeMethodRegistry, NativeSystemAccess,
};

const MIB: u64 = 1024 * 1024;
const USAGE: &str = "()Ljava/lang/management/MemoryUsage;";
const POOLS0: &str = "()[Ljava/lang/management/MemoryPoolMXBean;";
const MGRS0: &str = "()[Ljava/lang/management/MemoryManagerMXBean;";

#[test]
fn gci_i15_nh_the_lists_are_hotspots_in_name_and_order() {
    let pools: Vec<&str> = CODE_HEAP_POOLS
        .iter()
        .chain(METASPACE_POOLS.iter())
        .copied()
        .collect();
    assert_eq!(pools, JMX_NON_HEAP_POOL_NAMES);
    assert_eq!(
        JMX_NON_HEAP_POOL_NAMES,
        [
            "CodeHeap 'non-nmethods'",
            "CodeHeap 'profiled nmethods'",
            "CodeHeap 'non-profiled nmethods'",
            "Metaspace",
            "Compressed Class Space",
        ]
    );
    assert_eq!(JMX_NON_HEAP_MANAGER_NAMES, ["CodeCacheManager", "Metaspace Manager"]);
    assert_eq!(non_heap_manager_pools("CodeCacheManager"), CODE_HEAP_POOLS);
    assert_eq!(non_heap_manager_pools("Metaspace Manager"), METASPACE_POOLS);
    assert!(non_heap_manager_pools("Copy").is_empty());
    // Every pool names exactly the manager that lists it, and is never
    // collected.
    let s = NonHeapSources::default();
    for name in JMX_NON_HEAP_POOL_NAMES.iter().copied() {
        let p = non_heap_pool_from(name, &s).expect("every non-heap name is described");
        assert_eq!(p.name, name);
        assert_eq!(p.manager_names.len(), 1, "{name}");
        assert!(non_heap_manager_pools(p.manager_names[0]).contains(&name), "{name}");
        assert!(is_non_heap_pool(&p));
        assert_eq!((p.collection_used, p.collection_committed), (-1, -1), "{name}");
    }
    assert!(non_heap_pool_from("Eden Space", &s).is_none());
    assert!(non_heap_pool_name("G1 Old Gen").is_none());
}

#[test]
fn gci_i15_nh_pool_figures_come_from_what_the_vm_holds() {
    let quad = |p: GcMemoryPoolInfo| [p.init, p.used, p.committed, p.max];
    let at = |name: &'static str, s: &NonHeapSources| quad(non_heap_pool_from(name, s).unwrap());
    let s = NonHeapSources {
        metadata_used: 5_000,
        max_metaspace: None,
        metaspace_split: None,
        code: Some((300_000, Some(256 * MIB))),
    };
    assert_eq!(at("Metaspace", &s), [0, 5_000, 5_000, -1]);
    assert_eq!(
        at("CodeHeap 'non-profiled nmethods'", &s),
        [NON_HEAP_INIT_BYTES, 300_000, 300_000, (256 * MIB) as i64]
    );
    // No counterpart: empty, max undefined.
    for name in [
        "CodeHeap 'non-nmethods'",
        "CodeHeap 'profiled nmethods'",
        "Compressed Class Space",
    ] {
        assert_eq!(at(name, &s), [0, 0, 0, -1], "{name}");
    }
    // -XX:MaxMetaspaceSize is the max, widened so `committed <= max` holds
    // (the measured figure is not the one the limit is enforced on).
    let limited = NonHeapSources {
        max_metaspace: Some(64 * MIB),
        ..s
    };
    assert_eq!(at("Metaspace", &limited), [0, 5_000, 5_000, (64 * MIB) as i64]);
    let tight = NonHeapSources {
        max_metaspace: Some(4_000),
        ..s
    };
    assert_eq!(at("Metaspace", &tight), [0, 5_000, 5_000, 5_000]);
    // An unbounded cache has no max; no cache at all is empty.
    let unbounded = NonHeapSources {
        code: Some((300_000, None)),
        ..s
    };
    assert_eq!(
        at("CodeHeap 'non-profiled nmethods'", &unbounded),
        [NON_HEAP_INIT_BYTES, 300_000, 300_000, -1]
    );
    let none = NonHeapSources { code: None, ..s };
    assert_eq!(
        at("CodeHeap 'non-profiled nmethods'", &none),
        [NON_HEAP_INIT_BYTES, 0, 0, -1]
    );
    // A cache past its cap (the cap is checked before a compile) keeps
    // `committed <= max`.
    let over = NonHeapSources {
        code: Some((300_000, Some(200_000))),
        ..s
    };
    assert_eq!(
        at("CodeHeap 'non-profiled nmethods'", &over),
        [NON_HEAP_INIT_BYTES, 300_000, 300_000, 300_000]
    );
}

#[test]
fn gci_i15_nh_the_non_heap_usage_is_the_pools_sum() {
    let s = NonHeapSources {
        metadata_used: 5_000,
        max_metaspace: Some(64 * MIB),
        metaspace_split: None,
        code: Some((300_000, Some(256 * MIB))),
    };
    let pools: Vec<GcMemoryPoolInfo> = JMX_NON_HEAP_POOL_NAMES
        .iter()
        .copied()
        .filter_map(|n| non_heap_pool_from(n, &s))
        .collect();
    assert_eq!(pools.len(), 5);
    // `init` is unchanged (2 MiB); `max` stays undefined because the empty
    // code heaps' is.
    assert_eq!(non_heap_usage_sum(&pools), [NON_HEAP_INIT_BYTES, 305_000, 305_000, -1]);
    // HotSpot's rule when every max is defined: their sum.
    let defined: Vec<GcMemoryPoolInfo> = pools
        .iter()
        .cloned()
        .map(|mut p| {
            if p.max < 0 {
                p.max = 1_000;
            }
            p
        })
        .collect();
    assert_eq!(
        non_heap_usage_sum(&defined)[3],
        (64 * MIB + 256 * MIB) as i64 + 3 * 1_000
    );
    // An undefined init makes the sum's undefined.
    let mut undefined_init = defined.clone();
    undefined_init[3].init = -1;
    assert_eq!(non_heap_usage_sum(&undefined_init)[0], -1);
    // The mock (no class store, no code cache): the figures of before.
    let ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let [init, used, committed, max] = non_heap_memory_usage(&ctx);
    assert_eq!(init, NON_HEAP_INIT_BYTES);
    assert_eq!(used, non_heap_used_bytes(None, ctx.loaded_class_count()));
    assert_eq!(committed, used);
    assert_eq!(max, -1);
}

#[test]
fn gci_i15_nh_a_gc_info_entry_for_a_non_heap_pool_is_its_usage_now() {
    let snap = [GcPoolUsageSnapshot {
        name: "Eden Space",
        init: -1,
        used: 10,
        committed: 20,
        max: 40,
    }];
    let s = NonHeapSources {
        metadata_used: 7_000,
        ..NonHeapSources::default()
    };
    let non_heap = vec![non_heap_pool_from("Metaspace", &s).unwrap()];
    assert_eq!(gc_info_pool_usage(&snap, &non_heap, "Eden Space"), [-1, 10, 20, 40]);
    assert_eq!(gc_info_pool_usage(&snap, &non_heap, "Metaspace"), [0, 7_000, 7_000, -1]);
    // Neither: the undefined entry, as before.
    assert_eq!(gc_info_pool_usage(&snap, &[], "Metaspace"), [-1, 0, 0, -1]);
}

/// zt t1/ob: a `GcInfo` samples only the non-heap sources of the pools
/// its record does not carry at both edges -- none for a ZGC pause record
/// (all five carried as zeros), the code heaps only for a ZGC cycle
/// record carrying both metaspace pools, both off ZGC (positive control:
/// a record carrying nothing), metaspace again when one edge lacks it.
#[test]
fn zt1ob_a_gc_info_samples_only_the_non_heap_pools_its_record_lacks() {
    let entry = |name: &'static str| GcPoolUsageSnapshot {
        name,
        init: 0,
        used: 0,
        committed: 0,
        max: 0,
    };
    let heap = ["ZGC Young Generation", "ZGC Old Generation"];
    let names: Vec<String> = JMX_NON_HEAP_POOL_NAMES
        .iter()
        .chain(heap.iter())
        .map(|n| n.to_string())
        .collect();
    let all: Vec<GcPoolUsageSnapshot> = JMX_NON_HEAP_POOL_NAMES
        .iter()
        .chain(heap.iter())
        .map(|n| entry(*n))
        .collect();
    assert_eq!(gc_info_non_heap_wanted(&names, &all, &all), (false, false));
    let cycle: Vec<GcPoolUsageSnapshot> = ["Metaspace", "Compressed Class Space"]
        .into_iter()
        .chain(heap)
        .map(entry)
        .collect();
    assert_eq!(gc_info_non_heap_wanted(&names, &cycle, &cycle), (false, true));
    let bare: Vec<GcPoolUsageSnapshot> = heap.into_iter().map(entry).collect();
    assert_eq!(gc_info_non_heap_wanted(&names, &bare, &bare), (true, true));
    assert_eq!(gc_info_non_heap_wanted(&names, &cycle, &bare), (true, true));
    // A builder that names no non-heap pool samples nothing.
    let heap_only: Vec<String> = heap.iter().map(|n| n.to_string()).collect();
    assert_eq!(gc_info_non_heap_wanted(&heap_only, &bare, &bare), (false, false));
    // Nothing asked: nothing sampled (no class-store walk).
    let ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    assert!(non_heap_pool_infos_sampled(&ctx, false, false).is_empty());
    let code_only: Vec<&str> = non_heap_pool_infos_sampled(&ctx, false, true)
        .iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(code_only, CODE_HEAP_POOLS);
}

/// zt t3/ob (`zgc-zt1ob-non-heap-pool-residuals-CLOSED-20261019.md` item 1):
/// with HotSpot's per-code-heap `init` stated, the three code heaps carry
/// it and the non-heap `init` is HotSpot 25.0.3's 7667712; the metaspace
/// pools keep 0. Kill / every other backend (`None`): gci i15/nh's 2 MiB.
#[test]
fn zt3ob_the_code_heaps_state_hotspots_init() {
    let s = NonHeapSources {
        metadata_used: 5_000,
        code: Some((300_000, Some(240 * MIB))),
        ..NonHeapSources::default()
    };
    let all = || -> Vec<GcMemoryPoolInfo> {
        JMX_NON_HEAP_POOL_NAMES
            .iter()
            .copied()
            .filter_map(|n| non_heap_pool_from(n, &s))
            .collect()
    };
    let mut pools = all();
    with_code_cache_layout(
        &mut pools,
        Some(&cratonvm_types::code_cache_layout::CodeCacheLayout::DEFAULT_SEGMENTED),
    );
    let inits: Vec<i64> = pools.iter().map(|p| p.init).collect();
    assert_eq!(inits, [2_555_904, 2_555_904, 2_555_904, 0, 0]);
    assert_eq!(non_heap_usage_sum(&pools)[0], 7_667_712);
    // Positive control: without it, gci i15/nh's 2 MiB.
    let mut kept = all();
    with_code_cache_layout(&mut kept, None);
    assert_eq!(non_heap_usage_sum(&kept)[0], NON_HEAP_INIT_BYTES);
    // The mock states none: the aggregate keeps 2 MiB.
    let ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    assert_eq!(non_heap_memory_usage(&ctx)[0], NON_HEAP_INIT_BYTES);
}

/// zt t30/nh (G1/Generational carried-items page, item 2): a stated
/// layout gives each heap its own `init` (`InitialCodeCacheSize=8m` on
/// the host: 5836800 / 8388608 / 8388608, sum 22614016), and an
/// unsegmented one lists ONE `CodeCache` pool ahead of the metaspace
/// pools, `init` 2555904, `max` the reserved size, managed by
/// `CodeCacheManager`.
#[test]
fn zt30nh_the_layout_states_each_code_pool() {
    use cratonvm_types::code_cache_layout::CodeCacheLayout;
    let s = NonHeapSources {
        metadata_used: 5_000,
        code: Some((300_000, Some(1 << 30))),
        ..NonHeapSources::default()
    };
    let build = |names: &[&'static str], layout: &CodeCacheLayout| {
        let mut pools: Vec<GcMemoryPoolInfo> = names
            .iter()
            .copied()
            .filter_map(|n| non_heap_pool_from(n, &s))
            .collect();
        with_code_cache_layout(&mut pools, Some(layout));
        pools
    };
    let eight = CodeCacheLayout {
        inits: [5_836_800, 8_388_608, 8_388_608],
        ..CodeCacheLayout::DEFAULT_SEGMENTED
    };
    let pools = build(JMX_NON_HEAP_POOL_NAMES, &eight);
    assert_eq!(non_heap_usage_sum(&pools)[0], 22_614_016);
    let flat = CodeCacheLayout {
        segmented: false,
        inits: [2_555_904, 0, 0],
        reserved: 64 << 20,
    };
    let names = cratonvm_native_api::registry::jmx_non_heap_pool_names(Some(&flat));
    assert_eq!(names, ["CodeCache", "Metaspace", "Compressed Class Space"]);
    let pools = build(names, &flat);
    assert_eq!(pools[0].name, "CodeCache");
    assert_eq!(
        [pools[0].init, pools[0].used, pools[0].max],
        [2_555_904, 300_000, 64 << 20]
    );
    assert_eq!(pools[0].manager_names, ["CodeCacheManager"]);
    assert_eq!(non_heap_usage_sum(&pools)[0], 2_555_904);
    assert_eq!(code_pool_names(Some(&flat)), ["CodeCache"]);
    assert_eq!(code_pool_names(None), CODE_HEAP_POOLS);
    assert_eq!(non_heap_pool_name("CodeCache"), Some("CodeCache"));
    assert!(!non_heap_pool_is_metaspace("CodeCache"));
}

/// zt t1/ob (the merge decision): a collector that describes its heap
/// pools and no VM-modelled non-heap pool (ZGC by default since zt t1,
/// as G1 and Generational) lists HotSpot 25's five non-heap pools first
/// and the two plain managers; the kill arm's modelled set
/// (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0`: `Metaspace` alone)
/// replaces them.
#[test]
fn zt1ob_the_default_non_heap_pool_list_is_hotspots_five() {
    let ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let heap = ["ZGC Young Generation", "ZGC Old Generation"];
    let mut want: Vec<&str> = JMX_NON_HEAP_POOL_NAMES.to_vec();
    want.extend(heap);
    assert_eq!(pool_names_with_non_heap(&ctx, &heap), want);
    assert!(non_heap_manager_names(&ctx).is_empty());
    assert_eq!(non_heap_manager_pools_of(&ctx, "Metaspace Manager"), METASPACE_POOLS);
    assert_eq!(non_heap_manager_pools_of(&ctx, "CodeCacheManager"), CODE_HEAP_POOLS);
    // Positive control: the kill arm's VM-modelled `Metaspace`.
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
    assert_eq!(
        pool_names_with_non_heap(&ctx, &heap),
        vec!["Metaspace", "ZGC Young Generation", "ZGC Old Generation"]
    );
    assert_eq!(non_heap_manager_pools_of(&ctx, "Metaspace Manager"), vec!["Metaspace"]);
}

/// gci i33/ms: with HotSpot's figures the `Metaspace` pool's committed is
/// its own (granules, not `used`) and `Compressed Class Space` reports the
/// class space, `max` = `CompressedClassSpaceSize`; the threshold samples
/// are the same figures.
#[test]
fn gci_i33ms_metaspace_pools_carry_the_class_space() {
    let quad = |p: &GcMemoryPoolInfo| [p.init, p.used, p.committed, p.max];
    let split = metaspace_split_of([5_000, 65_536, 1_200, 65_536, 1 << 30]);
    assert_eq!(
        split,
        MetaspaceSplit {
            committed: 65_536,
            class_used: 1_200,
            class_committed: 65_536,
            class_max: 1 << 30,
        }
    );
    let s = NonHeapSources {
        metadata_used: 5_000,
        metaspace_split: Some(split),
        ..NonHeapSources::default()
    };
    let at = |name: &'static str, s: &NonHeapSources| quad(&non_heap_pool_from(name, s).unwrap());
    assert_eq!(at("Metaspace", &s), [0, 5_000, 65_536, -1]);
    assert_eq!(at("Compressed Class Space", &s), [0, 1_200, 65_536, 1 << 30]);
    // -XX:MaxMetaspaceSize below committed: max widened to committed.
    let tight = NonHeapSources {
        max_metaspace: Some(10_000),
        ..s
    };
    assert_eq!(at("Metaspace", &tight)[3], 65_536);
    let samples = non_heap_threshold_samples_split(Some((5_000, None)), Some(split), None);
    let ccs = samples
        .iter()
        .find(|p| p.name == "Compressed Class Space")
        .expect("sampled");
    assert_eq!(quad(ccs), [0, 1_200, 65_536, 1 << 30]);
    // Without a split: the pre-i33 shape, the old function's figures.
    let flat = non_heap_threshold_samples_split(Some((5_000, None)), None, None);
    let old = non_heap_threshold_samples(Some((5_000, None)), None);
    assert_eq!(
        flat.iter().map(quad).collect::<Vec<_>>(),
        old.iter().map(quad).collect::<Vec<_>>()
    );
    assert_eq!(quad(&flat[1]), [0, 0, 0, -1], "an empty class space");
}

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

/// A context describing HotSpot Serial's beans, with the bean layouts the
/// natives read by name.
fn serial_ctx() -> cratonvm_native_builtins::test_utils::MockNativeContext {
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    declare(
        &mut ctx,
        "sun/management/MemoryPoolImpl",
        &["name", "isHeap", "isValid", "peakUsage"],
    );
    declare(
        &mut ctx,
        "sun/management/MemoryManagerImpl",
        &["name", "isValid", "listenerLock", "listenerList"],
    );
    declare(&mut ctx, "sun/management/GarbageCollectorImpl", &["name", "isValid"]);
    declare(
        &mut ctx,
        "java/lang/management/MemoryUsage",
        &["init", "used", "committed", "max"],
    );
    ctx.set_gc_beans(
        vec![
            GcCollectorInfo {
                name: "Copy",
                count: 1,
                time_ms: 1,
                pool_names: &["Eden Space"],
            },
            GcCollectorInfo {
                name: "MarkSweepCompact",
                count: 1,
                time_ms: 1,
                pool_names: &["Eden Space", "Tenured Gen"],
            },
        ],
        vec![
            GcMemoryPoolInfo {
                name: "Eden Space",
                init: -1,
                used: 1_000,
                committed: 8_000,
                max: 16_000,
                collection_used: -1,
                collection_committed: -1,
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
    ctx
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_vm_management_impl(&mut r);
    register_memory_pool_impl(&mut r);
    register_memory_manager_impl(&mut r);
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
        .expect("native returned void")
}

fn names_of(ctx: &cratonvm_native_builtins::test_utils::MockNativeContext, v: Value) -> Vec<String> {
    let Value::Object(Some(arr)) = v else {
        panic!("expected an array, got {v:?}")
    };
    (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Object(Some(b)) => bean_name(ctx, b).unwrap_or_default(),
            other => panic!("array element {i} is {other:?}"),
        })
        .collect()
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

#[test]
fn gci_i15_nh_memory_impl_lists_the_non_heap_pools_and_managers_first() {
    let r = registry();
    let mut ctx = serial_ctx();
    let m = "sun/management/MemoryImpl";
    let pools = call(&r, &mut ctx, m, "getMemoryPools0", POOLS0, &[]);
    assert_eq!(
        names_of(&ctx, pools),
        [
            "CodeHeap 'non-nmethods'",
            "CodeHeap 'profiled nmethods'",
            "CodeHeap 'non-profiled nmethods'",
            "Metaspace",
            "Compressed Class Space",
            "Eden Space",
            "Tenured Gen",
        ]
    );
    let mgrs = call(&r, &mut ctx, m, "getMemoryManagers0", MGRS0, &[]);
    assert_eq!(
        names_of(&ctx, mgrs),
        ["CodeCacheManager", "Metaspace Manager", "Copy", "MarkSweepCompact"]
    );
    // A context that describes no heap pools keeps the historical pair.
    let mut bare = cratonvm_native_builtins::test_utils::mock_ctx();
    declare(
        &mut bare,
        "sun/management/MemoryPoolImpl",
        &["name", "isHeap", "isValid", "peakUsage"],
    );
    let pools = call(&r, &mut bare, m, "getMemoryPools0", POOLS0, &[]);
    assert_eq!(names_of(&bare, pools), ["Eden Space", "Old Gen"]);
}

/// zt t6/ob4 (zs s4/rv3 item 13, carried by
/// `zgc-zt1ob-non-heap-pool-residuals`): the plain managers. The manager
/// array leads with `CodeCacheManager` and `Metaspace Manager` built as
/// plain `MemoryManagerImpl`s (each answers its own pools), then the
/// collectors; the `GarbageCollectorMXBean` filter keeps the collectors
/// only, in order. Positive control: a filter naming no collector keeps
/// nothing, one naming a plain manager keeps it.
#[test]
fn zt6ob4_plain_managers_lead_and_the_collector_filter_drops_them() {
    let r = registry();
    let mut ctx = serial_ctx();
    let collectors = ["Copy", "MarkSweepCompact"];
    let arr = alloc_manager_bean_array(&mut ctx, &collectors).unwrap();
    assert_eq!(
        names_of(&ctx, Value::Object(Some(arr))),
        ["CodeCacheManager", "Metaspace Manager", "Copy", "MarkSweepCompact"]
    );
    let beans = bean_array_to_vec(&ctx, arr);
    assert_eq!(beans.len(), 4);
    let pools_of = |ctx: &mut cratonvm_native_builtins::test_utils::MockNativeContext, mgr: ObjectRef| {
        let v = call(
            &r,
            ctx,
            "sun/management/MemoryManagerImpl",
            "getMemoryPools0",
            POOLS0,
            &[Value::Object(Some(mgr))],
        );
        names_of(ctx, v)
    };
    assert_eq!(pools_of(&mut ctx, beans[0]), CODE_HEAP_POOLS);
    assert_eq!(pools_of(&mut ctx, beans[1]), METASPACE_POOLS);
    let mut gcs = beans.clone();
    retain_collector_beans(&ctx, &mut gcs, &collectors);
    let kept: Vec<String> = gcs.iter().filter_map(|b| bean_name(&ctx, *b)).collect();
    assert_eq!(kept, collectors);
    let mut none = beans.clone();
    retain_collector_beans(&ctx, &mut none, &[]);
    assert!(none.is_empty());
    let mut meta = beans;
    retain_collector_beans(&ctx, &mut meta, &["Metaspace Manager"]);
    assert_eq!(meta.len(), 1);
}

#[test]
fn gci_i15_nh_a_non_heap_pool_bean_answers_as_hotspot_shapes_it() {
    let r = registry();
    let mut ctx = serial_ctx();
    let p = "sun/management/MemoryPoolImpl";
    let meta = alloc_any_pool_impl(&mut ctx, "Metaspace").unwrap();
    // NON_HEAP.
    assert!(matches!(ctx.get_field_by_name(meta, "isHeap"), Value::Int(0)));
    let this = [Value::Object(Some(meta))];
    let used = non_heap_used_bytes(None, ctx.loaded_class_count());
    let v = call(&r, &mut ctx, p, "getUsage0", USAGE, &this);
    assert_eq!(usage_of(&ctx, v), [0, used, used, -1]);
    // No collector manages it: null, as `jmm_GetPoolCollectionUsage`.
    let v = call(&r, &mut ctx, p, "getCollectionUsage0", USAGE, &this);
    assert_eq!(v, Value::Object(None));
    // The peak is at least the usage.
    let v = call(&r, &mut ctx, p, "getPeakUsage0", USAGE, &this);
    assert!(usage_of(&ctx, v)[1] >= used);
    // Its manager is `Metaspace Manager` ...
    let v = call(&r, &mut ctx, p, "getMemoryManagers0", MGRS0, &this);
    assert_eq!(names_of(&ctx, v), ["Metaspace Manager"]);
    // ... and `CodeCacheManager`'s pools are the three code heaps.
    let code_mgr = alloc_memory_manager_impl(&mut ctx, "CodeCacheManager").unwrap();
    let v = call(
        &r,
        &mut ctx,
        "sun/management/MemoryManagerImpl",
        "getMemoryPools0",
        POOLS0,
        &[Value::Object(Some(code_mgr))],
    );
    assert_eq!(names_of(&ctx, v), CODE_HEAP_POOLS);
    // A code heap with no code cache in the context: the 2 MiB init only.
    let code = alloc_any_pool_impl(&mut ctx, "CodeHeap 'non-profiled nmethods'").unwrap();
    let v = call(&r, &mut ctx, p, "getUsage0", USAGE, &[Value::Object(Some(code))]);
    assert_eq!(usage_of(&ctx, v), [NON_HEAP_INIT_BYTES, 0, 0, -1]);
    // A heap pool is unchanged: a heap bean, its collector's figures.
    let eden = alloc_any_pool_impl(&mut ctx, "Eden Space").unwrap();
    assert!(matches!(ctx.get_field_by_name(eden, "isHeap"), Value::Int(1)));
    let v = call(&r, &mut ctx, p, "getUsage0", USAGE, &[Value::Object(Some(eden))]);
    assert_eq!(usage_of(&ctx, v), [-1, 1_000, 8_000, 16_000]);
}

/// gci i16/nt: the VM's non-heap threshold check compares exactly the
/// figures `getUsage()` answers, and only for the sources it sampled.
#[test]
fn gci_i16_nt_threshold_samples_are_the_get_usage_figures() {
    let quad = |p: &GcMemoryPoolInfo| [p.init, p.used, p.committed, p.max];
    let s = NonHeapSources {
        metadata_used: 5_000,
        max_metaspace: Some(64 * MIB),
        metaspace_split: None,
        code: Some((300_000, Some(256 * MIB))),
    };
    let both = non_heap_threshold_samples(
        Some((5_000, Some(64 * MIB))),
        Some((300_000, Some(256 * MIB))),
    );
    let names: Vec<&str> = both.iter().map(|p| p.name).collect();
    assert_eq!(names, JMX_NON_HEAP_POOL_NAMES, "HotSpot's order, all five");
    for p in &both {
        assert_eq!(
            quad(p),
            quad(&non_heap_pool_from(p.name, &s).unwrap()),
            "{}",
            p.name
        );
    }
    // Only the metaspace source sampled: its two pools, nothing else.
    let meta = non_heap_threshold_samples(Some((5_000, None)), None);
    let names: Vec<&str> = meta.iter().map(|p| p.name).collect();
    assert_eq!(names, METASPACE_POOLS);
    assert_eq!(quad(&meta[0]), [0, 5_000, 5_000, -1]);
    // Only the code source: the three code heaps.
    let code = non_heap_threshold_samples(None, Some((300_000, None)));
    let names: Vec<&str> = code.iter().map(|p| p.name).collect();
    assert_eq!(names, CODE_HEAP_POOLS);
    assert_eq!(quad(&code[2]), [NON_HEAP_INIT_BYTES, 300_000, 300_000, -1]);
    // A zero measured figure reads as getUsage reads it (never below 1).
    let zero = non_heap_threshold_samples(Some((0, None)), None);
    assert_eq!(zero[0].used, 1);
    assert!(non_heap_pool_is_metaspace("Metaspace"));
    assert!(non_heap_pool_is_metaspace("Compressed Class Space"));
    assert!(!non_heap_pool_is_metaspace("CodeHeap 'non-nmethods'"));
    assert!(non_heap_threshold_samples(None, None).is_empty());
}
