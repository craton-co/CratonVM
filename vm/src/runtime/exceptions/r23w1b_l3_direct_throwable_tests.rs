use super::*;
use crate::config::VmConfig;
use crate::vm::realms::thread_realm::ThrowableLayoutCache;
use crate::vm::Vm;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;

fn method(flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: flags,
        name: Arc::from("<init>"),
        descriptor: Arc::from("()V"),
        attributes: Vec::new(),
    }
}

/// A method whose `Code` is not decoded is "not known yet"; a native one
/// (no `Code` by definition) and the empty set are decided.
#[test]
fn r23w1b_shape_refusal_is_transient_only_while_code_is_undecoded() {
    let undecoded = method(MethodAccessFlags::PUBLIC);
    let native = method(MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE);
    assert_eq!(shape_refusal(&[&undecoded]), PlanRefusal::Transient);
    assert_eq!(shape_refusal(&[&native]), PlanRefusal::Structural);
    assert_eq!(
        shape_refusal(&[&native, &undecoded]),
        PlanRefusal::Transient
    );
    assert_eq!(shape_refusal(&[]), PlanRefusal::Structural);
}

/// `java/lang/Object` is no throwable: the walk refuses it for a reason
/// that cannot change, both constructors, and the per-thread memo keeps
/// that refusal; a proof (where the class library has one) is untouched.
#[test]
fn r23w1b_a_non_throwable_class_is_refused_structurally_and_remembered() {
    let mut vm = Vm::new(VmConfig::default());
    // Loads `Throwable` and a hierarchy below it, so a refusal is not just
    // "not loaded yet".
    let arith = "java/lang/ArithmeticException";
    if create_exception_object_constructed(&vm.shared, &mut vm.main_thread, arith, Some("x"))
        .is_err()
    {
        return; // no usable class library in this environment
    }
    let (object, arith_id) = {
        let cm = vm.shared.classes.class_manager.read();
        let bootstrap = cratonvm_types::ClassLoaderId::Bootstrap;
        (
            cm.loaded_class_under_exact_key("java/lang/Object", bootstrap),
            cm.loaded_class_under_exact_key(arith, bootstrap),
        )
    };
    let Some(object) = object else {
        return;
    };
    for with_message in [true, false] {
        assert_eq!(
            direct_build_plan(&vm.shared, object, with_message),
            Err(PlanRefusal::Structural),
            "with_message={with_message}"
        );
        let key = (vm.shared.vm_identity, object.as_u32(), with_message);
        let remembered = || FAST_THROW_SITES.with(|tls| tls.borrow().direct_refused.contains(&key));
        // A redefinition elsewhere in the process moves the stamp and drops
        // the memo: only a run that saw it stay put asserts the memo.
        let stamp = cratonvm_classloading::class_redefinition_count();
        assert_eq!(
            cached_direct_build_plan(&vm.shared, object, with_message),
            None
        );
        if cratonvm_classloading::class_redefinition_count() == stamp {
            assert!(remembered(), "with_message={with_message}: remembered");
        }
        // The second question has the same answer, from the memo or not.
        assert_eq!(
            cached_direct_build_plan(&vm.shared, object, with_message),
            None
        );
    }
    // A proof is cached as a proof, not as a refusal.
    if let Some(arith_id) = arith_id {
        if let Ok(plan) = direct_build_plan(&vm.shared, arith_id, true) {
            assert_eq!(
                cached_direct_build_plan(&vm.shared, arith_id, true),
                Some(plan)
            );
            let key = (vm.shared.vm_identity, arith_id.as_u32(), true);
            assert!(!FAST_THROW_SITES.with(|tls| tls.borrow().direct_refused.contains(&key)));
        }
    }
}

#[test]
fn direct_throw_cache_handles_hits_misses_vm_isolation_and_eviction() {
    use crate::classloading::ClassId;
    use std::sync::Arc;

    let plan = DirectBuildPlan {
        throwable_class: ClassId::new(10),
        unassigned_stack_static: 0,
        suppressed_sentinel_static: 1,
        jfr_tracing_static: 2,
        npe_fill_override: false,
    };
    let slots = DirectBuildSlots {
        plan,
        cause: 0,
        stack_trace: 1,
        suppressed: 2,
        detail_message: 3,
        backtrace: 4,
        depth: 5,
        npe_state: None,
    };
    let resolved = ResolvedDirectThrow {
        slots,
        compact: None,
    };

    let mut cache = direct_throw_cache::DirectThrowCache::default();
    let stamp = 7u64;
    let domain = 1u32;
    let lgen = 3u64;
    let lepoch = 2u32;

    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        None
    );

    // Fill 8 entries (classes 11..19).
    for cid in 11u32..19u32 {
        cache.insert(direct_throw_cache::DirectThrowCacheEntry {
            vm_identity: 100,
            class_id: cid,
            with_message: true,
            redef_stamp: stamp,
            layout_domain: domain,
            layout_gen: lgen,
            layout_epoch: lepoch,
            class_name: Arc::from("java/lang/RuntimeException"),
            resolved,
        });
    }

    // All 8 hit for exact match.
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        Some(resolved)
    );
    // Misses on different with_message, class_name, vm_identity, redef_stamp, layout_domain, layout_gen, layout_epoch.
    assert_eq!(
        cache.lookup(
            100,
            11,
            false,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/IllegalStateException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            200,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp + 1,
            domain,
            lgen,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain + 1,
            lgen,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen + 1,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen,
            lepoch + 1
        ),
        None
    );

    // Inserting a 9th entry evicts slot 0 (class_id 11) and keeps the remaining 7 + new 1.
    cache.insert(direct_throw_cache::DirectThrowCacheEntry {
        vm_identity: 100,
        class_id: 99,
        with_message: true,
        redef_stamp: stamp,
        layout_domain: domain,
        layout_gen: lgen,
        layout_epoch: lepoch,
        class_name: Arc::from("java/lang/IllegalArgumentException"),
        resolved,
    });
    assert_eq!(
        cache.lookup(
            100,
            11,
            true,
            "java/lang/RuntimeException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        None
    );
    assert_eq!(
        cache.lookup(
            100,
            99,
            true,
            "java/lang/IllegalArgumentException",
            stamp,
            domain,
            lgen,
            lepoch
        ),
        Some(resolved)
    );
}

#[test]
fn jit_new_throwable_direct_caches_and_initializes_fields_across_vms() {
    use crate::types::Value;
    use crate::vm::try_create_java_string;

    let mut vm1 = Vm::new(VmConfig::default());
    let mut vm2 = Vm::new(VmConfig::default());
    let rt = "java/lang/RuntimeException";
    if create_exception_object_constructed(&vm1.shared, &mut vm1.main_thread, rt, Some("boot"))
        .is_err()
        || create_exception_object_constructed(&vm2.shared, &mut vm2.main_thread, rt, Some("boot"))
            .is_err()
    {
        return; // no usable class library in this environment
    }
    let (rt_id1, rt_fields1) = {
        let cm = vm1.shared.classes.class_manager.read();
        let id = cm
            .loaded_class_under_exact_key(rt, cratonvm_types::ClassLoaderId::Bootstrap)
            .expect("RuntimeException id");
        let n = cm.get_class(id).expect("class").num_total_fields;
        (id, n)
    };
    if direct_build_plan(&vm1.shared, rt_id1, true).is_err() {
        return; // class library does not match modelled JDK 25 direct-build shape
    }

    let msg1 = try_create_java_string(&vm1.shared, "boom").expect("alloc msg");
    let recv1 = vm1
        .shared
        .mem
        .heap
        .try_alloc_object(rt_id1, rt_fields1)
        .expect("alloc RuntimeException");
    assert!(jit_new_throwable_direct(
        &vm1.shared,
        &mut vm1.main_thread,
        rt,
        "(Ljava/lang/String;)V",
        recv1,
        Some(msg1),
    ));

    let cached1 = lookup_direct_throw_cache(&vm1.shared, rt_id1, true, rt)
        .expect("jit_new_throwable_direct must populate TLS cache");
    assert_eq!(
        lookup_direct_throw_cache(&vm1.shared, rt_id1, false, rt),
        None,
        "()V entry must be distinct from (String)V"
    );
    assert_eq!(
        lookup_direct_throw_cache(&vm2.shared, rt_id1, true, rt),
        None,
        "VM-1 cache entry must never match VM-2"
    );

    let slots = cached1.slots;
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv1, slots.cause),
        Value::Object(Some(recv1))
    );
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv1, slots.detail_message),
        Value::Object(Some(msg1))
    );
    let expected_unassigned = crate::vm::get_static_shared(
        &vm1.shared,
        slots.plan.throwable_class,
        slots.plan.unassigned_stack_static,
    );
    let expected_sentinel = crate::vm::get_static_shared(
        &vm1.shared,
        slots.plan.throwable_class,
        slots.plan.suppressed_sentinel_static,
    );
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv1, slots.stack_trace),
        expected_unassigned
    );
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv1, slots.suppressed),
        expected_sentinel
    );
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv1, slots.backtrace),
        Value::Object(Some(recv1))
    );

    // Second throw on VM-1 hits cache and initializes empty-message constructor properly.
    let recv2 = vm1
        .shared
        .mem
        .heap
        .try_alloc_object(rt_id1, rt_fields1)
        .expect("alloc RuntimeException #2");
    assert!(jit_new_throwable_direct(
        &vm1.shared,
        &mut vm1.main_thread,
        rt,
        "()V",
        recv2,
        None,
    ));
    assert!(lookup_direct_throw_cache(&vm1.shared, rt_id1, false, rt).is_some());
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv2, slots.detail_message),
        Value::Object(None)
    );
    assert_eq!(
        vm1.shared.mem.heap.get_field(recv2, slots.cause),
        Value::Object(Some(recv2))
    );
}
