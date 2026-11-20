use super::*;

#[test]
fn gci_jit_referent_screen_accepts_the_cached_nonzero_field_index() {
    use crate::config::{GcAlgorithm, VmConfig};

    #[allow(unused_mut)]
    let mut algorithms = vec![GcAlgorithm::Generational, GcAlgorithm::G1];
    #[cfg(feature = "zgc")]
    algorithms.push(GcAlgorithm::Zgc);
    for algorithm in algorithms {
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: algorithm,
            ..VmConfig::with_host_jdk_default()
        });
        let (reference, weak, object) = {
            let mut cm = shared.classes.class_manager_write();
            // `load_class`, not a loaded-class probe: the host-JDK default is
            // `--compatible`, whose fast boot no longer preloads `Reference`.
            let reference = cm
                .load_class("java/lang/ref/Reference")
                .expect("bootstrap Reference");
            let weak = cm
                .load_class("java/lang/ref/WeakReference")
                .expect("bootstrap WeakReference");
            let object = cm.load_class("java/lang/Object").expect("bootstrap Object");
            (reference, weak, object)
        };
        let receiver = shared.mem.heap.alloc_object(weak, 4);
        let payload = if matches!(algorithm, GcAlgorithm::Generational) {
            shared
                .mem
                .heap
                .try_alloc_object_old(object, 0)
                .expect("old payload")
        } else {
            shared.mem.heap.alloc_object(object, 0)
        };
        shared
            .mem
            .heap
            .set_field(receiver, 1, Value::Object(Some(payload)));
        // Simulate the resolved layout produced by a real JDK whose
        // hierarchy-wide Reference field index is nonzero.
        shared.mem.ref_discovery.referent.store(
            (1u64 << 32) | (u64::from(reference.as_u32()) + 1),
            Ordering::Relaxed,
        );
        if shared.mem.heap.is_generational() {
            shared.mem.concurrent_satb.activate();
        } else if let Some(q) = shared.mem.heap.g1_satb_queue() {
            q.activate();
        } else {
            shared.mem.heap.zgc_set_mark_active(true);
        }
        keep_alive_referent_read_raw(&shared, receiver, 1, payload.as_ptr() as usize);
        let retained = if shared.mem.heap.is_generational() {
            cratonvm_gc::satb::flush_thread_satb_buffer(&shared.mem.concurrent_satb);
            shared.mem.concurrent_satb.drain()
        } else if let Some(q) = shared.mem.heap.g1_satb_queue() {
            cratonvm_gc::satb::flush_thread_satb_buffer(q);
            q.drain()
        } else {
            let mut entries = Vec::new();
            shared.mem.heap.zgc_drain_mark_ingress(&mut entries);
            entries.into_iter().map(|entry| entry as usize).collect()
        };
        assert!(
            retained.contains(&(payload.as_ptr() as usize)),
            "{algorithm:?}: nonzero cached hierarchy field index"
        );
    }
}

/// A fake class chain: id -> (name, superclass).
fn chain(rows: &[(u32, &'static str, Option<u32>)]) -> Vec<(u32, &'static str, Option<u32>)> {
    rows.to_vec()
}

fn kind_in(rows: &[(u32, &'static str, Option<u32>)], start: u32) -> Option<AllocationKind> {
    walk_reference_kind(ClassId::new(start), |cid| {
        rows.iter()
            .find(|r| r.0 == cid.as_u32())
            .map(|r| (r.1, r.2.map(ClassId::new)))
    })
}

/// The walk finds the first JDK reference class up the chain; `Cleaner`
/// wins over its `PhantomReference` superclass; `FinalReference`, a bare
/// `Reference` and every non-reference class have no kind (the negative
/// controls), and a hole in the chain answers none.
#[test]
fn gcg_g4rs_reference_kind_walks_to_the_first_jdk_reference_class() {
    let rows = chain(&[
        (1, "java/lang/Object", None),
        (2, "java/lang/ref/Reference", Some(1)),
        (3, "java/lang/ref/WeakReference", Some(2)),
        (4, "java/lang/ref/SoftReference", Some(2)),
        (5, "java/lang/ref/PhantomReference", Some(2)),
        (6, "jdk/internal/ref/Cleaner", Some(5)),
        (7, "java/lang/ref/FinalReference", Some(2)),
        (8, "com/example/MyWeak", Some(3)),
        (9, "com/example/MyWeakChild", Some(8)),
        (10, "java/util/WeakHashMap$Entry", Some(3)),
        (11, "java/lang/String", Some(1)),
        (12, "com/example/Orphan", Some(99)),
        (
            13,
            "jdk/internal/ref/CleanerImpl$PhantomCleanableRef",
            Some(5),
        ),
    ]);
    let weak = Some(AllocationKind {
        ref_type: ReferenceType::Weak,
        runs_cleaner: false,
    });
    assert_eq!(kind_in(&rows, 3), weak);
    assert_eq!(kind_in(&rows, 8), weak, "a user subclass inherits the kind");
    assert_eq!(kind_in(&rows, 9), weak);
    assert_eq!(kind_in(&rows, 10), weak);
    assert_eq!(
        kind_in(&rows, 4).map(|k| k.ref_type),
        Some(ReferenceType::Soft)
    );
    assert_eq!(
        kind_in(&rows, 5),
        Some(AllocationKind {
            ref_type: ReferenceType::Phantom,
            runs_cleaner: false,
        })
    );
    assert_eq!(
        kind_in(&rows, 13),
        Some(AllocationKind {
            ref_type: ReferenceType::Phantom,
            runs_cleaner: false,
        }),
        "a JDK 25 cleanable is an ordinary phantom"
    );
    assert_eq!(
        kind_in(&rows, 6),
        Some(AllocationKind {
            ref_type: ReferenceType::Phantom,
            runs_cleaner: true,
        }),
        "Cleaner is checked before its PhantomReference superclass"
    );
    assert_eq!(
        kind_in(&rows, 7),
        None,
        "FinalReference keeps the finalizer registration"
    );
    assert_eq!(kind_in(&rows, 2), None);
    assert_eq!(kind_in(&rows, 11), None);
    assert_eq!(kind_in(&rows, 1), None);
    assert_eq!(kind_in(&rows, 12), None, "a hole in the chain");
    assert_eq!(kind_in(&rows, 42), None, "an unknown class");

    // A malformed cycle ends the walk.
    let cyclic = chain(&[(20, "a/A", Some(21)), (21, "a/B", Some(20))]);
    assert_eq!(kind_in(&cyclic, 20), None);
}

/// gci i46/ha: a reference subclass deeper than the old 64-step bound still
/// has its kind (HotSpot inherits the reference type whatever the depth),
/// up to the class store's own hierarchy bound; past that bound the walk
/// still ends, with no kind.
#[test]
fn ha46_reference_kind_walk_reaches_a_deep_subclass() {
    let names: Vec<String> = (0..1100).map(|i| format!("com/example/Deep{i}")).collect();
    // Ids 1..=3: Object, Reference, WeakReference; 100 + i: Deep{i}, whose
    // superclass is Deep{i - 1}, and Deep0's is WeakReference.
    let mut rows: Vec<(u32, &str, Option<u32>)> = vec![
        (1, "java/lang/Object", None),
        (2, "java/lang/ref/Reference", Some(1)),
        (3, "java/lang/ref/WeakReference", Some(2)),
    ];
    for (i, name) in names.iter().enumerate() {
        let id = 100 + i as u32;
        rows.push((id, name.as_str(), Some(if i == 0 { 3 } else { id - 1 })));
    }
    let kind_at = |start: u32| {
        walk_reference_kind(ClassId::new(start), |cid| {
            rows.iter()
                .find(|r| r.0 == cid.as_u32())
                .map(|r| (r.1, r.2.map(ClassId::new)))
        })
    };
    let weak = Some(AllocationKind {
        ref_type: ReferenceType::Weak,
        runs_cleaner: false,
    });
    // Deep{i} is i + 1 levels below WeakReference.
    assert_eq!(kind_at(100 + 62), weak, "inside the old bound");
    assert_eq!(
        kind_at(100 + 63),
        weak,
        "64 levels below: the old bound answered no kind here"
    );
    assert_eq!(kind_at(100 + 199), weak, "200 levels below");
    assert_eq!(
        kind_at(100 + (REFERENCE_KIND_WALK_DEPTH as u32 - 2)),
        weak,
        "the deepest chain the bound admits"
    );
    assert_eq!(
        kind_at(100 + (REFERENCE_KIND_WALK_DEPTH as u32 - 1)),
        None,
        "past the class store's hierarchy bound the walk ends with no kind"
    );
}

/// SBR3 `ref`: the kind code a `ClassAllocInfo` bakes round-trips through
/// `kind_from_code`, `0` is no kind, and an unknown code is no kind.
#[test]
fn sbr3_ref_kind_code_round_trips() {
    assert_eq!(code_of_kind(None), 0);
    assert!(kind_from_code(0).is_none());
    assert!(kind_from_code(200).is_none());
    for code in 1u8..=4 {
        let kind = kind_from_code(code);
        assert!(kind.is_some(), "code {code}");
        assert_eq!(code_of_kind(kind), code);
    }
    assert_eq!(
        kind_from_code(4),
        Some(AllocationKind {
            ref_type: ReferenceType::Phantom,
            runs_cleaner: true,
        })
    );
}

/// gch h2/jr: HotSpot's figure is whole megabytes of `capacity - used`,
/// rounded down, and never negative.
#[test]
fn gch_jr_hotspot_free_mb_is_capacity_minus_used_rounded_down() {
    const MB: usize = 1024 * 1024;
    assert_eq!(hotspot_free_mb(64 * MB, 0), 64);
    assert_eq!(hotspot_free_mb(64 * MB, 3 * MB + 1), 60, "rounded down");
    assert_eq!(
        hotspot_free_mb(64 * MB, 64 * MB - 1),
        0,
        "sub-megabyte is 0"
    );
    assert_eq!(hotspot_free_mb(MB, 2 * MB), 0, "saturates");
}

/// gch h3/jx: HotSpot's policy capacity is `MaxHeapSize` (the `-Xmx`
/// budget, both survivor spaces included), floored at the heap's own
/// capacity for a heap built larger than its budget.
#[test]
fn gch_jx_soft_lru_capacity_is_max_heap_size_not_one_semispace_plus_old() {
    const MB: usize = 1024 * 1024;
    // -Xmx64m, a 21/2 MiB semispace pair and 43 MiB old: 53.5 MiB "capacity".
    let semi_plus_old = 10 * MB + MB / 2 + 43 * MB;
    assert_eq!(hotspot_policy_capacity(64 * MB, semi_plus_old), 64 * MB);
    assert_eq!(
        hotspot_free_mb(hotspot_policy_capacity(64 * MB, semi_plus_old), 4 * MB),
        60
    );
    assert_eq!(
        hotspot_policy_capacity(16 * MB, 32 * MB),
        32 * MB,
        "explicit arenas"
    );
}

/// gch h2/jr: `soft_lru_free_mb` is the default figure with the switch off
/// (the positive control: byte-for-byte the old input) and HotSpot's figure
/// with it on (counted). gci i48/zl2: on every collector -- Generational,
/// G1 and ZGC (G1 used to answer its own figure whatever the switch said).
#[test]
fn gci_i48_zl2_soft_lru_free_mb_follows_the_switch_on_every_collector() {
    use crate::config::{GcAlgorithm, VmConfig};
    for algorithm in [GcAlgorithm::Generational, GcAlgorithm::G1, GcAlgorithm::Zgc] {
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: algorithm,
            ..VmConfig::default()
        });
        let state = &shared.mem.ref_discovery;
        let heap = &shared.mem.heap;
        state.set_soft_hotspot_free(false);
        let off = soft_lru_free_mb(&shared);
        assert_eq!(
            off,
            heap.soft_ref_policy_free_mb(),
            "{algorithm:?} off: the old input"
        );
        assert_eq!(state.soft_hotspot_free_reads(), 0, "{algorithm:?}");
        state.set_soft_hotspot_free(true);
        let on = soft_lru_free_mb(&shared);
        assert_eq!(
            on,
            hotspot_free_mb(
                hotspot_policy_capacity(shared.config.max_heap_size, heap.heap_capacity()),
                heap.allocated_bytes()
            ),
            "{algorithm:?}"
        );
        assert_eq!(state.soft_hotspot_free_reads(), 1, "{algorithm:?}: engaged");
        // gch h3/jx: the capacity is `-Xmx` (MaxHeapSize), never below the
        // heap's own capacity, never below `-Xmx`.
        assert!(on >= hotspot_free_mb(heap.heap_capacity(), heap.allocated_bytes()));
        assert!(
            on >= hotspot_free_mb(shared.config.max_heap_size, heap.allocated_bytes()),
            "{algorithm:?}: MaxHeapSize - used, as LRUMaxHeapPolicy::setup"
        );
        if algorithm == GcAlgorithm::G1 {
            // G1's own figure is its soft capacity less allocated, never
            // above `-Xmx` less allocated: HotSpot's is the wider window.
            assert!(on >= off, "G1: {on} MB < the soft-capacity figure {off} MB");
        }
    }
}

/// gch h4/wr: `CRATONVM_SOFTREF_HOTSPOT_FREE` is default ON. Unset (and
/// any word but an off word) builds a VM state with HotSpot's figure;
/// `=0` (the kill switch) builds the young-bounded one. Both
/// constructor arms (`constructors_are_bytecode`) read it the same way, and
/// the explicit `with` stays off.
#[test]
fn gch_wr_soft_hotspot_free_is_default_on_and_zero_kills_it() {
    for bytecode in [false, true] {
        for (value, expect) in [(None, true), (Some("1"), true), (Some("0"), false)] {
            let state = cratonvm_types::flags::with_thread_overrides(
                &[(SOFT_HOTSPOT_FREE_FLAG, value)],
                || RefDiscoveryState::from_flags(bytecode),
            );
            assert_eq!(
                state.soft_hotspot_free.load(Ordering::Relaxed),
                expect,
                "value={value:?} bytecode={bytecode}",
            );
        }
    }
    assert!(!RefDiscoveryState::with(true)
        .soft_hotspot_free
        .load(Ordering::Relaxed));
}

/// i77-L5 retirement (2026-10-28): `CRATONVM_REF_NOTES_PER_THREAD` is
/// default ON. Unset (and any word but an off word) builds the striped
/// notes; `=0` (the kill switch) the locked push. Both constructor arms
/// read it the same way, and the explicit `with` stays on the locked push.
#[test]
fn i77_ref_notes_per_thread_is_default_on_and_zero_kills_it() {
    for bytecode in [false, true] {
        for (value, expect) in [(None, true), (Some("1"), true), (Some("0"), false)] {
            let state = cratonvm_types::flags::with_thread_overrides(
                &[(REF_NOTES_PER_THREAD_FLAG, value)],
                || RefDiscoveryState::from_flags(bytecode),
            );
            assert_eq!(
                state.note_stripes().is_some(),
                expect,
                "value={value:?} bytecode={bytecode}",
            );
        }
    }
    assert!(RefDiscoveryState::with(true).note_stripes().is_none());
}

/// `note_allocation` registers nothing with the switch off (the positive
/// control of the test below) and nothing for a class it cannot resolve.
#[test]
fn gcg_g4rs_note_allocation_is_inert_when_off_or_unresolved() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let obj = shared.mem.heap.alloc_object(ClassId::new(1), 2);
    shared.mem.ref_discovery.set_engaged(false);
    note_allocation(&shared, obj, ClassId::new(1));
    assert_eq!(shared.mem.ref_processor.lock().allocated_unbound_len(), 0);
    assert_eq!(shared.mem.ref_discovery.noted(), 0);

    shared.mem.ref_discovery.set_engaged(true);
    // A class id no class occupies: no kind, nothing registered.
    note_allocation(&shared, obj, ClassId::new(u32::MAX - 7));
    assert_eq!(shared.mem.ref_processor.lock().allocated_unbound_len(), 0);
    assert_eq!(shared.mem.ref_discovery.noted(), 0);
}

/// With the switch on, an allocation of a weak-reference class leaves one
/// unbound entry in the processor, carrying the object's class id; a
/// non-reference class leaves none.
#[test]
fn gcg_g4rs_note_allocation_registers_a_reference_class_when_engaged() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (weak_cid, string_cid) = {
        let cm = shared.classes.class_manager.read();
        (
            cm.find_bootstrap_class_by_name("java/lang/ref/WeakReference"),
            cm.find_bootstrap_class_by_name("java/lang/String"),
        )
    };
    let Some(weak_cid) = weak_cid else {
        // A class image without `java.lang.ref` loaded at construction:
        // the walk itself is covered by the pure test above.
        return;
    };
    shared.mem.ref_discovery.set_engaged(true);
    let obj = shared.mem.heap.alloc_object(weak_cid, 4);
    note_allocation(&shared, obj, weak_cid);
    if let Some(string_cid) = string_cid {
        let s = shared.mem.heap.alloc_object(string_cid, 4);
        note_allocation(&shared, s, string_cid);
    }
    let mut rp = shared.mem.ref_processor.lock();
    assert_eq!(rp.allocated_unbound_len(), 1);
    let pending = rp.take_allocated_unbound();
    assert_eq!(pending[0].class_id, weak_cid.as_u32());
    assert_eq!(pending[0].row.reference_obj, obj.as_ptr() as usize);
    assert_eq!(pending[0].row.ref_type, ReferenceType::Weak);
    assert_eq!(
        pending[0].row.referent, 0,
        "unbound until the pre-collection pass"
    );
    drop(rp);
    assert_eq!(shared.mem.ref_discovery.noted(), 1);
}

/// JIT round 21 wave 8, lane reflect (`CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES`,
/// page `r21-reflective-new-skips-finalizer-and-reference-registration-20261019`):
/// a constructor-less native allocation (`Unsafe.allocateInstance`, which
/// every constructor `MethodHandle` and JDK 25 `Constructor.newInstance`
/// allocates through) of a weak-reference class leaves one unbound entry,
/// as the interpreter's `new` does; with the switch off (the kill switch)
/// it leaves none, which was the defect.
#[test]
fn r21_no_constructor_allocation_notes_a_reference_class() {
    for notes in [true, false] {
        let config = crate::config::VmConfig {
            native_alloc_notes_references: notes,
            ..crate::config::VmConfig::default()
        };
        let shared = crate::vm::SharedVm::new(config);
        let weak_cid = shared
            .classes
            .class_manager
            .read()
            .find_bootstrap_class_by_name("java/lang/ref/WeakReference");
        let Some(weak_cid) = weak_cid else {
            // A class image without `java.lang.ref` loaded at construction.
            return;
        };
        shared.mem.ref_discovery.set_engaged(true);
        let obj = shared.mem.heap.alloc_object(weak_cid, 4);
        crate::runtime::interpreter::init_no_constructor_instance(&shared, obj, weak_cid);
        let mut rp = shared.mem.ref_processor.lock();
        if notes {
            assert_eq!(rp.allocated_unbound_len(), 1);
            let pending = rp.take_allocated_unbound();
            assert_eq!(pending[0].row.reference_obj, obj.as_ptr() as usize);
            assert_eq!(pending[0].row.ref_type, ReferenceType::Weak);
        } else {
            assert_eq!(rp.allocated_unbound_len(), 0, "kill switch: not noted");
        }
    }
}
