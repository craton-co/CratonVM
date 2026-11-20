use super::*;

/// JVMS §5.4.3 failure record: first record wins, keyed per
/// (class, cp index), dropped by `invalidate_class` and `clear`.
#[test]
fn resolution_failure_first_record_wins_and_is_invalidated() {
    let mut cache = ResolutionCache::new();
    let a = ClassId::new(7);
    let b = ClassId::new(8);
    let ncdfe = ResolutionFailure {
        error_class: Arc::from("java/lang/NoClassDefFoundError"),
        message: Some(Arc::from("p/Opt")),
        cause: None,
    };
    assert!(cache.get_failure(a, 3).is_none());
    cache.put_failure(a, 3, ncdfe.clone());
    cache.put_failure(
        a,
        3,
        ResolutionFailure {
            error_class: Arc::from("java/lang/IllegalAccessError"),
            message: None,
            cause: None,
        },
    );
    cache.put_failure(b, 3, ncdfe.clone());
    assert_eq!(cache.get_failure(a, 3), Some(&ncdfe));
    assert!(cache.get_failure(a, 4).is_none());
    assert_eq!(cache.failure_count(), 2);
    cache.invalidate_class(a);
    assert!(cache.get_failure(a, 3).is_none());
    assert_eq!(cache.get_failure(b, 3), Some(&ncdfe));
    cache.clear();
    assert_eq!(cache.failure_count(), 0);
}

/// Interpreter round i1 wave 24, lane L5: a field-layout shift of a class
/// whose constant pool did not change drops its field resolutions (keyed
/// on it, or resolved into it from another class) and its record
/// `ObjectMethods` site, and keeps its recorded failure, its permanent
/// constant and its other call sites.
#[test]
fn a_field_layout_shift_keeps_failures_and_permanent_constants() {
    let mut cache = ResolutionCache::new();
    let moved = ClassId::new(7);
    let other = ClassId::new(8);
    let field_into = |declaring: ClassId| ResolvedField {
        declaring_class_id: declaring,
        field_index: 3,
        is_static: false,
        is_volatile: false,
        is_final: false,
        is_reference: false,
        desc_byte: 0,
    };
    let ncdfe = ResolutionFailure {
        error_class: Arc::from("java/lang/NoClassDefFoundError"),
        message: Some(Arc::from("p/Opt")),
        cause: None,
    };
    cache.put_field(moved, 1, field_into(other));
    cache.put_field(other, 1, field_into(moved));
    cache.put_field(other, 2, field_into(other));
    cache.put_failure(moved, 3, ncdfe.clone());
    cache.put_permanent_constant(moved, 4, Value::Int(44));
    cache.put_call_site(
        moved,
        5,
        ResolvedCallSite::RecordObjectMethod {
            method: RecordMethodKind::HashCode,
            component_names: vec![Arc::from("x")],
            field_indices: vec![0],
            field_descriptors: vec![Arc::from("I")],
            getter_driven: None,
            accessor_getters: Vec::new(),
        },
    );
    cache.put_call_site(
        moved,
        6,
        ResolvedCallSite::EnumSwitch { labels: Vec::new() },
    );

    cache.invalidate_class_field_layout(moved);

    assert!(
        cache.get_field(moved, 1).is_none(),
        "keyed on the moved class"
    );
    assert!(
        cache.get_field(other, 1).is_none(),
        "resolved into the moved class"
    );
    assert!(cache.get_field(other, 2).is_some(), "unrelated field kept");
    assert_eq!(cache.get_failure(moved, 3), Some(&ncdfe), "JVMS 5.4.3");
    assert!(
        matches!(cache.get_condy(moved, 4), Some(Value::Int(44))),
        "JVMS 5.4.3.6"
    );
    assert!(
        cache.get_call_site(moved, 5).is_none(),
        "record site carries field indices"
    );
    assert!(
        cache.get_call_site(moved, 6).is_some(),
        "other call sites kept"
    );
    assert_eq!(cache.field_count(), 1);
    assert_eq!(cache.call_site_count(), 1);
}

/// Interpreter round i1 wave 44, lane L4: a getter-driven record
/// `ObjectMethods` site keyed on ANOTHER class (the class holding the
/// `invokedynamic`) holds the record's field slots, so a layout shift of
/// the record drops it; a site driven by another record stays.
#[test]
fn a_record_layout_shift_drops_getter_driven_sites_keyed_elsewhere() {
    let mut cache = ResolutionCache::new();
    let record = ClassId::new(7);
    let holder = ClassId::new(8);
    let other_record = ClassId::new(9);
    let site = |driven: ClassId| ResolvedCallSite::RecordObjectMethod {
        method: RecordMethodKind::HashCode,
        component_names: vec![Arc::from("x")],
        field_indices: vec![0],
        field_descriptors: vec![Arc::from("I")],
        getter_driven: Some(driven),
        accessor_getters: Vec::new(),
    };
    cache.put_call_site(holder, 1, site(record));
    cache.put_call_site(holder, 2, site(other_record));

    cache.invalidate_class_field_layout(record);

    assert!(
        cache.get_call_site(holder, 1).is_none(),
        "holds the moved record's slots"
    );
    assert!(
        cache.get_call_site(holder, 2).is_some(),
        "another record's site kept"
    );
    assert_eq!(cache.call_site_count(), 1);
}

/// A permanent constant record (condy / MethodHandle) survives the FIFO
/// eviction of the recomputable records, and the first record wins.
#[test]
fn permanent_constant_survives_fifo_eviction_and_first_record_wins() {
    let mut cache = ResolutionCache::new();
    let condy_owner = ClassId::new(3);
    assert_eq!(
        cache.put_permanent_constant(condy_owner, 9, Value::Int(1)),
        Value::Int(1)
    );
    // A losing racer gets the winner back and does not replace it.
    assert_eq!(
        cache.put_permanent_constant(condy_owner, 9, Value::Int(2)),
        Value::Int(1)
    );
    // Push the recomputable store past its cap.
    for i in 0..=RESOLUTION_CACHE_CAP {
        // Cast: test-only; `i` is at most 2^16, so both halves fit.
        let (class, cp) = (ClassId::new(100 + (i >> 16) as u32), (i & 0xFFFF) as u16);
        cache.put_condy(class, cp, Value::Int(i as i32));
    }
    assert!(cache.condy.len() <= RESOLUTION_CACHE_CAP);
    assert_eq!(cache.get_condy(condy_owner, 9), Some(&Value::Int(1)));
    assert_eq!(cache.permanent_constant_count(), 1);
    // A recomputable record cannot shadow a permanent one.
    cache.put_condy(condy_owner, 9, Value::Int(7));
    assert_eq!(cache.get_condy(condy_owner, 9), Some(&Value::Int(1)));
}

/// Unloading drops every record keyed on the dead classes — the constant
/// records are loader-conditional GC roots and must not outlive the pin.
#[test]
fn forget_classes_drops_constant_and_failure_records() {
    let mut cache = ResolutionCache::new();
    let dead = ClassId::new(11);
    let live = ClassId::new(12);
    cache.put_condy(dead, 1, Value::Int(1));
    cache.put_permanent_constant(dead, 2, Value::Int(2));
    cache.put_permanent_constant(live, 2, Value::Int(3));
    cache.put_failure(
        dead,
        5,
        ResolutionFailure {
            error_class: Arc::from("java/lang/NoClassDefFoundError"),
            message: None,
            cause: None,
        },
    );
    cache.forget_classes(|id| id == dead);
    assert!(cache.get_condy(dead, 1).is_none());
    assert!(cache.get_condy(dead, 2).is_none());
    assert!(cache.get_failure(dead, 5).is_none());
    assert_eq!(cache.get_condy(live, 2), Some(&Value::Int(3)));
    let mut visited = Vec::new();
    cache.for_each_condy_root(|id, _| visited.push(id));
    assert!(visited.is_empty(), "no object values were recorded");
    // `invalidate_class` shares the rules.
    cache.invalidate_class(live);
    assert_eq!(cache.permanent_constant_count(), 0);
}

#[test]
fn cache_field_put_and_get() {
    let mut cache = ResolutionCache::new();
    let key_class = ClassId::new(1);
    let cp_index = 5;

    assert!(cache.get_field(key_class, cp_index).is_none());

    cache.put_field(
        key_class,
        cp_index,
        ResolvedField {
            declaring_class_id: ClassId::new(2),
            field_index: 3,
            is_static: true,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: 0,
        },
    );

    let resolved = cache.get_field(key_class, cp_index).unwrap();
    assert_eq!(resolved.declaring_class_id, ClassId::new(2));
    assert_eq!(resolved.field_index, 3);
    assert!(resolved.is_static);
    assert_eq!(cache.field_count(), 1);
}

#[test]
fn cache_method_put_and_get() {
    fn cached_native(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[cratonvm_types::Value],
    ) -> cratonvm_types::error::MethodCallResult {
        Ok(None)
    }

    let mut cache = ResolutionCache::new();
    let key_class = ClassId::new(0);
    let cp_index = 10;

    assert!(cache.get_method(key_class, cp_index).is_none());

    cache.put_method(
        key_class,
        cp_index,
        ResolvedMethod {
            declaring_class_id: ClassId::new(3),
            class_name: Arc::from("java/lang/Object"),
            method_name: Arc::from("toString"),
            method_descriptor: Arc::from("()Ljava/lang/String;"),
            num_params: 0,
            native_target: Some(cached_native),
            native_kind: Some(cratonvm_native_api::NativeKind::Bridge),
        },
    );

    let resolved = cache.get_method(key_class, cp_index).unwrap();
    assert_eq!(resolved.declaring_class_id, ClassId::new(3));
    assert_eq!(&*resolved.method_name, "toString");
    assert_eq!(
        resolved.native_target.map(|target| target as usize),
        Some(cached_native as usize)
    );
    assert_eq!(
        resolved.native_kind,
        Some(cratonvm_native_api::NativeKind::Bridge)
    );
    assert_eq!(cache.method_count(), 1);
}

#[test]
fn cache_different_classes_same_index() {
    let mut cache = ResolutionCache::new();
    let class_a = ClassId::new(1);
    let class_b = ClassId::new(2);

    cache.put_field(
        class_a,
        5,
        ResolvedField {
            declaring_class_id: ClassId::new(10),
            field_index: 0,
            is_static: false,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: 0,
        },
    );
    cache.put_field(
        class_b,
        5,
        ResolvedField {
            declaring_class_id: ClassId::new(20),
            field_index: 1,
            is_static: true,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: 0,
        },
    );

    assert_eq!(
        cache.get_field(class_a, 5).unwrap().declaring_class_id,
        ClassId::new(10),
    );
    assert_eq!(
        cache.get_field(class_b, 5).unwrap().declaring_class_id,
        ClassId::new(20),
    );
    assert_eq!(cache.field_count(), 2);
}

#[test]
fn cache_overwrites_same_key() {
    let mut cache = ResolutionCache::new();
    let class_id = ClassId::new(1);

    cache.put_field(
        class_id,
        3,
        ResolvedField {
            declaring_class_id: ClassId::new(5),
            field_index: 0,
            is_static: false,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: 0,
        },
    );
    cache.put_field(
        class_id,
        3,
        ResolvedField {
            declaring_class_id: ClassId::new(6),
            field_index: 1,
            is_static: true,
            is_volatile: false,
            is_final: false,
            is_reference: false,
            desc_byte: 0,
        },
    );

    let resolved = cache.get_field(class_id, 3).unwrap();
    assert_eq!(resolved.declaring_class_id, ClassId::new(6));
    assert_eq!(cache.field_count(), 1);
}

/// C12 regression: the invoke cache MUST distinguish `invokespecial`
/// from `invokevirtual`/`invokeinterface` on the same (caller_class,
/// cp_index) pair. Scala trait bridges like `IterableOnceOps.mkString$`
/// share a single InterfaceMethodref cp_index with the virtual call from
/// the trait's own default method. Without the `is_special` key bit, the
/// virtual cache entry (receiver = concrete subclass override) hijacks
/// the invokespecial call and recurses through the subclass override,
/// blowing the stack.
#[test]
fn invoke_cache_distinguishes_special_vs_virtual() {
    use std::sync::Arc;
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(7);
    let cp_index = 666u16;

    let make_cached = |name: &str| {
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_reader::CachedMethodParts {
                declaring_class_id: ClassId::new(99),
                class_name: Arc::from(name),
                method_name: Arc::from("mkString"),
                method_descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/String;"),
                source_file: None,
                code: Arc::from(&[0xb1u8][..]),
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 1,
                num_params: 1,
                is_synchronized: false,
                is_static: false,
            },
        ))
    };

    // invokespecial entry: points at the interface default method itself.
    // One `pc` for both entries on purpose: this test is about `is_special`
    // disambiguating two entries at the SAME site, and `pc` became a key
    // dimension in `6e392453f`. Varying it here would make them differ for a
    // reason the test is not about, and the assertions below would then hold
    // whether or not `is_special` was doing anything.
    const SITE_PC: u32 = 0;
    cache.put(
        caller,
        cp_index,
        true,
        SITE_PC,
        CachedInvokeTarget::Bytecode {
            cached: make_cached("scala/collection/IterableOnceOps"),
            gate: RedefineGate::never_stale(),
        },
    );
    // invokevirtual/interface entry at the same cp_index: points at the
    // concrete subclass's override.
    cache.put(
        caller,
        cp_index,
        false,
        SITE_PC,
        CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: ClassId::new(42),
            cached: make_cached("scala/collection/AbstractIterable"),
            gate: RedefineGate::never_stale(),
        },
    );

    // The two entries must NOT alias each other.
    let special = cache
        .get(caller, cp_index, true, SITE_PC)
        .expect("special entry");
    match special {
        CachedInvokeTarget::Bytecode { cached: m, .. } => {
            assert_eq!(&*m.class_name, "scala/collection/IterableOnceOps");
        }
        other => panic!("expected Bytecode for invokespecial, got {other:?}"),
    }
    let virt = cache
        .get(caller, cp_index, false, SITE_PC)
        .expect("virtual entry");
    match virt {
        CachedInvokeTarget::VirtualBytecode { cached, .. } => {
            assert_eq!(&*cached.class_name, "scala/collection/AbstractIterable");
        }
        other => panic!("expected VirtualBytecode for invokevirtual, got {other:?}"),
    }
}

/// WP2.4-F1: redefining a class must invalidate the cache entry on the
/// next hit. Bumping the live counter (which is what `redefine_class`
/// does) renders the snapshotted entry stale.
#[test]
fn invoke_cache_evicts_stale_entry_after_redefine_bump() {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(7);
    let cp_index = 11u16;

    let counter = Arc::new(AtomicU32::new(0));
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_reader::CachedMethodParts {
            declaring_class_id: ClassId::new(99),
            class_name: Arc::from("p/Target"),
            method_name: Arc::from("answer"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(&[0x10u8, 0x2A, 0xACu8][..]), // bipush 42; ireturn
            exception_table: Arc::from(&[][..]),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let entry = CachedInvokeTarget::Bytecode {
        cached,
        gate: RedefineGate::snapshot(Arc::clone(&counter)),
    };
    cache.put(caller, cp_index, false, 0, entry);

    // First hit — counter unchanged, must return Some.
    assert!(cache.get(caller, cp_index, false, 0).is_some());

    // Simulate `redefine_class` step 7 — bump the live counter.
    counter.fetch_add(1, Ordering::Release);

    // Next hit — entry is stale, getter must auto-evict and return None.
    assert!(
        cache.get(caller, cp_index, false, 0).is_none(),
        "stale entry must be evicted after redefine bump"
    );

    // Subsequent populate with a fresh snapshot succeeds.
    let new_cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_reader::CachedMethodParts {
            declaring_class_id: ClassId::new(99),
            class_name: Arc::from("p/Target"),
            method_name: Arc::from("answer"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(&[0x10u8, 0x63, 0xACu8][..]), // bipush 99; ireturn
            exception_table: Arc::from(&[][..]),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    cache.put(
        caller,
        cp_index,
        false,
        0,
        CachedInvokeTarget::Bytecode {
            cached: new_cached,
            gate: RedefineGate::snapshot(Arc::clone(&counter)),
        },
    );
    let hit = cache
        .get(caller, cp_index, false, 0)
        .expect("fresh entry hit");
    match hit {
        CachedInvokeTarget::Bytecode { cached, .. } => {
            // The bipush byte for 99 (0x63) confirms we got the new body.
            assert_eq!(cached.code[1], 0x63);
        }
        _ => panic!("expected Bytecode entry"),
    }
}

/// `retain_jit` drops exactly the `Jit` entries its predicate rejects, in
/// the monomorphic map AND the polymorphic overflow, and nothing else.
/// It exists so the code-cache sweeper can release bodies that a call
/// site which never runs again would otherwise pin for good; a version
/// that skipped the poly map would leave every megamorphic site pinning.
#[test]
fn retain_jit_drops_only_rejected_jit_entries_in_both_maps() {
    use std::sync::Arc;
    let counter = Arc::new(AtomicU32::new(0));
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_reader::CachedMethodParts {
            declaring_class_id: ClassId::new(99),
            class_name: Arc::from("p/Target"),
            method_name: Arc::from("answer"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(&[0x10u8, 0x2A, 0xACu8][..]),
            exception_table: Arc::from(&[][..]),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    // The artifact is a plain id here: `retain_jit` is generic over it.
    let jit = |id: u32| CachedInvokeTarget::Jit {
        compiled: id,
        num_params: 0,
        return_type: b'I',
        needs_heap: false,
        cached: Arc::clone(&cached),
        gate: RedefineGate::snapshot(Arc::clone(&counter)),
        supersede_epoch: SupersedeGate::never_stale(),
    };
    let bytecode = CachedInvokeTarget::Bytecode {
        cached: Arc::clone(&cached),
        gate: RedefineGate::snapshot(Arc::clone(&counter)),
    };
    let caller = ClassId::new(7);
    let mut cache = InvokeCache::<u32>::new();
    cache.put(caller, 1, false, 0, jit(1)); // withdrawn
    cache.put(caller, 2, false, 0, jit(2)); // live
    cache.put(caller, 3, false, 0, bytecode);
    cache.put_poly(caller, 4, false, 0, ClassId::new(40), jit(1)); // withdrawn
    cache.put_poly(caller, 4, false, 0, ClassId::new(41), jit(2)); // live
    cache.put_poly(caller, 5, false, 0, ClassId::new(50), jit(1)); // withdrawn, alone

    let removed = cache.retain_jit(|id| *id != 1);

    assert_eq!(
        removed, 3,
        "the three entries naming artifact 1, and no others"
    );
    assert!(cache.get(caller, 1, false, 0).is_none());
    assert!(
        cache.get(caller, 2, false, 0).is_some(),
        "a live body stays cached"
    );
    assert!(
        cache.get(caller, 3, false, 0).is_some(),
        "non-JIT targets are untouched"
    );
    assert!(cache
        .get_poly(caller, 4, false, 0, ClassId::new(40))
        .is_none());
    assert!(cache
        .get_poly(caller, 4, false, 0, ClassId::new(41))
        .is_some());
    assert!(cache
        .get_poly(caller, 5, false, 0, ClassId::new(50))
        .is_none());
}

/// Interpreter round i1 wave 19, lane L5: a `Jit` entry goes stale on its
/// own VM's supersede epoch only. Two counters stand for two VMs; the one
/// that advances evicts its entry and leaves the other's hit.
#[test]
fn a_jit_entry_goes_stale_on_its_own_vms_supersede_epoch_only() {
    use std::sync::Arc;
    let redefine = Arc::new(AtomicU32::new(0));
    // Each VM leaks its cell; so does this test.
    let epoch_a: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(5)));
    let epoch_b: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(5)));
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_reader::CachedMethodParts {
            declaring_class_id: ClassId::new(19),
            class_name: Arc::from("p/Hot"),
            method_name: Arc::from("run"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(&[0x10u8, 0x2A, 0xACu8][..]),
            exception_table: Arc::from(&[][..]),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let jit = |epoch: &'static AtomicU32| CachedInvokeTarget::Jit {
        compiled: 1u32,
        num_params: 0,
        return_type: b'I',
        needs_heap: false,
        cached: Arc::clone(&cached),
        gate: RedefineGate::snapshot(Arc::clone(&redefine)),
        supersede_epoch: SupersedeGate::snapshot(epoch),
    };
    let caller = ClassId::new(7);
    let mut in_a = InvokeCache::<u32>::new();
    let mut in_b = InvokeCache::<u32>::new();
    in_a.put(caller, 1, false, 0, jit(epoch_a));
    in_b.put(caller, 1, false, 0, jit(epoch_b));
    let evictions = epoch_stale_evictions();

    // VM A publishes a replacing C2 body.
    epoch_a.fetch_add(1, Ordering::AcqRel);

    assert!(
        in_b.get(caller, 1, false, 0).is_some(),
        "another VM's C2 publish leaves this VM's entry current"
    );
    assert!(
        in_a.get(caller, 1, false, 0).is_none(),
        "the publishing VM's entry may hold the replaced body"
    );
    assert!(epoch_stale_evictions() > evictions, "counted");
    assert!(!SupersedeGate::never_stale().is_stale());
}

#[test]
fn method_handle_kind_from_tag() {
    assert_eq!(
        MethodHandleKind::from_tag(1),
        Some(MethodHandleKind::GetField)
    );
    assert_eq!(
        MethodHandleKind::from_tag(5),
        Some(MethodHandleKind::InvokeVirtual)
    );
    assert_eq!(
        MethodHandleKind::from_tag(6),
        Some(MethodHandleKind::InvokeStatic)
    );
    assert_eq!(
        MethodHandleKind::from_tag(8),
        Some(MethodHandleKind::NewInvokeSpecial)
    );
    assert_eq!(
        MethodHandleKind::from_tag(9),
        Some(MethodHandleKind::InvokeInterface)
    );
    assert_eq!(MethodHandleKind::from_tag(0), None);
    assert_eq!(MethodHandleKind::from_tag(10), None);
}

#[test]
fn method_handle_kind_display() {
    assert_eq!(
        MethodHandleKind::InvokeStatic.to_string(),
        "REF_invokeStatic"
    );
    assert_eq!(
        MethodHandleKind::InvokeVirtual.to_string(),
        "REF_invokeVirtual"
    );
}

#[test]
fn method_handle_construction() {
    let mh = MethodHandle {
        kind: MethodHandleKind::InvokeStatic,
        class_name: Arc::from("com/example/Foo"),
        member_name: Arc::from("bar"),
        descriptor: Arc::from("(I)V"),
    };
    assert_eq!(mh.kind, MethodHandleKind::InvokeStatic);
    assert_eq!(&*mh.class_name, "com/example/Foo");
    assert_eq!(&*mh.member_name, "bar");
    assert_eq!(&*mh.descriptor, "(I)V");
}

#[test]
fn cache_call_site_put_and_get() {
    let mut cache = ResolutionCache::new();
    let class_id = ClassId::new(1);
    let cp_index = 42;

    assert!(cache.get_call_site(class_id, cp_index).is_none());

    cache.put_call_site(
        class_id,
        cp_index,
        ResolvedCallSite::StringConcat {
            recipe: Arc::from(
                "Hello, \u{0001}!"
                    .encode_utf16()
                    .collect::<Vec<u16>>()
                    .as_slice(),
            ),
            constant_args: vec![],
            target_descriptor: Arc::from("(Ljava/lang/String;)Ljava/lang/String;"),
            latin1_shape: true,
        },
    );

    assert!(cache.get_call_site(class_id, cp_index).is_some());
    assert_eq!(cache.call_site_count(), 1);
}

#[test]
fn cache_lambda_call_site() {
    let mut cache = ResolutionCache::new();
    let class_id = ClassId::new(1);
    let proxy_id = ClassId::new(0x8000_0000);

    cache.put_call_site(
        class_id,
        10,
        ResolvedCallSite::Lambda(LambdaCallSite {
            functional_interface_id: None,
            functional_interface: Arc::from("java/lang/Runnable"),
            sam_method_name: Arc::from("run"),
            sam_descriptor: Arc::from("()V"),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: Arc::from("com/example/App"),
                member_name: Arc::from("lambda$main$0"),
                descriptor: Arc::from("()V"),
            },
            instantiated_descriptor: Arc::from("()V"),
            capture_types: vec![],
            proxy_class_id: proxy_id,
            serializable_flag: false,
        }),
    );

    let site = cache.get_call_site(class_id, 10).unwrap();
    match site {
        ResolvedCallSite::Lambda(lcs) => {
            assert_eq!(&*lcs.functional_interface, "java/lang/Runnable");
            assert_eq!(&*lcs.sam_method_name, "run");
            assert_eq!(lcs.proxy_class_id, proxy_id);
        }
        _ => panic!("Expected Lambda call site"),
    }
}

/// Round 5 audit fix (MED #10) / Round 7 audit fix (MED #11):
/// `LinkResolver` round-trip — populate a triple, read it back,
/// confirm `NotFound` is also cached so the next probe
/// short-circuits, and verify `invalidate_class` drops every entry
/// whose key or resolved declaring class matches — plus every
/// negative, whose provenance the key does not record.
#[test]
fn link_resolver_caches_and_invalidates() {
    let resolver = LinkResolver::new();
    let class_a = ClassId::new(1);
    let class_b = ClassId::new(2);
    let name: Arc<str> = Arc::from("iterator");
    let desc: Arc<str> = Arc::from("()Ljava/util/Iterator;");

    // Cold miss.
    assert!(resolver.get(class_a, &name, &desc).is_none());

    // Populate with a Method resolution whose declaring class is
    // class_b — both the key class and the resolved declaring
    // class participate in invalidation, so we exercise both
    // axes.
    resolver.insert(
        class_a,
        Arc::clone(&name),
        Arc::clone(&desc),
        ResolvedMember::Method {
            declaring_class_id: class_b,
            index: 7,
        },
    );

    // Hit.
    match resolver.get(class_a, &name, &desc) {
        Some(ResolvedMember::Method {
            declaring_class_id,
            index,
        }) => {
            assert_eq!(declaring_class_id, class_b);
            assert_eq!(index, 7);
        }
        other => panic!("expected cached Method, got {other:?}"),
    }
    assert_eq!(resolver.len(), 1);

    // NotFound is also cached.
    let missing_name: Arc<str> = Arc::from("totallyNotAMethod");
    resolver.insert(
        class_a,
        Arc::clone(&missing_name),
        Arc::clone(&desc),
        ResolvedMember::NotFound,
    );
    assert!(matches!(
        resolver.get(class_a, &missing_name, &desc),
        Some(ResolvedMember::NotFound)
    ));
    assert_eq!(resolver.len(), 2);

    // Invalidating class_b drops the Method entry (resolved declaring
    // class match) AND the NotFound entry. The latter changed in the
    // 2026-07-26 memoized-negative fix: a negative asserts something about
    // the whole hierarchy walk, and the key does not record which classes
    // that walk passed through, so it cannot be proven unaffected.
    resolver.invalidate_class(class_b);
    assert!(resolver.get(class_a, &name, &desc).is_none());
    assert!(
        resolver.get(class_a, &missing_name, &desc).is_none(),
        "a negative must not survive an unrelated-looking invalidation — \
         the changed class may be an ancestor the walk crossed"
    );
    assert!(resolver.is_empty());
}

/// REGRESSION (memoized-negative, 2026-07-26): a `NotFound` cached under a
/// *subclass* key must be dropped when an **ancestor** is invalidated.
///
/// Concrete failure this reproduces: `GetMethodID(Sub, "m", "()V")` runs
/// while `Base` is still a synthetic stub that lacks `m`, so `NotFound` is
/// cached at key `(Sub, "m", "()V")`. `upgrade_synthetic_class` then swaps
/// in the real `Base`, which declares `m`, and fires the invalidate hook
/// for `Base`. Under the old `retain`, the entry's key class was `Sub`, not
/// `Base`, and its value carried no `declaring_class_id`, so it was kept —
/// and every later `GetMethodID` returned a null `jmethodID`, surfacing as
/// `NoSuchMethodError` for a method that now exists. The only escapes were
/// `clear()` (no production caller) and the CLOCK sweep at 128 K entries.
#[test]
fn negative_cached_under_subclass_is_dropped_when_ancestor_changes() {
    let resolver = LinkResolver::new();
    let base = ClassId::new(10);
    let sub = ClassId::new(11);
    let name: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");

    // The walk from `Sub` climbed through `Base` and found nothing.
    resolver.insert(
        sub,
        Arc::clone(&name),
        Arc::clone(&desc),
        ResolvedMember::NotFound,
    );
    assert!(matches!(
        resolver.get(sub, &name, &desc),
        Some(ResolvedMember::NotFound)
    ));

    // `Base` gains the method and fires the invalidate hook for itself.
    // Nothing in the key mentions `Base`, so only the negative-dropping
    // rule can save us here.
    resolver.invalidate_class(base);

    assert!(
        resolver.get(sub, &name, &desc).is_none(),
        "the stale negative must be gone so the next probe re-walks the \
         hierarchy and finds the newly-declared method"
    );
}

/// The fix must not throw away positives it has no reason to distrust: a
/// resolution whose declaring class is unrelated to the invalidated class
/// still survives.
#[test]
fn unrelated_positive_survives_invalidation() {
    let resolver = LinkResolver::new();
    let keeper = ClassId::new(20);
    let declaring = ClassId::new(21);
    let unrelated = ClassId::new(22);
    let name: Arc<str> = Arc::from("size");
    let desc: Arc<str> = Arc::from("()I");

    resolver.insert(
        keeper,
        Arc::clone(&name),
        Arc::clone(&desc),
        ResolvedMember::Method {
            declaring_class_id: declaring,
            index: 3,
        },
    );
    resolver.invalidate_class(unrelated);
    assert!(
        matches!(
            resolver.get(keeper, &name, &desc),
            Some(ResolvedMember::Method { index: 3, .. })
        ),
        "positives keyed and declared elsewhere must not be collateral damage"
    );
}

/// gc-common w9-e: the batch form evicts exactly what a loop of the
/// per-class form evicts, for every subset of a mixed cache (keyed on a
/// gone class, resolved into one, both, neither, and negatives), and an
/// empty set evicts nothing.
#[test]
fn invalidate_classes_matches_a_loop_of_invalidate_class() {
    fn fill(resolver: &LinkResolver) {
        let desc: Arc<str> = Arc::from("()V");
        for key in 1..=6u32 {
            for decl in 1..=6u32 {
                resolver.insert(
                    ClassId::new(key),
                    Arc::from(format!("m{decl}")),
                    Arc::clone(&desc),
                    ResolvedMember::Method {
                        declaring_class_id: ClassId::new(decl),
                        index: decl,
                    },
                );
                resolver.insert(
                    ClassId::new(key),
                    Arc::from(format!("f{decl}")),
                    Arc::from(""),
                    ResolvedMember::Field {
                        declaring_class_id: ClassId::new(decl),
                        absolute_index: decl,
                        is_static: decl % 2 == 0,
                    },
                );
            }
            resolver.insert(
                ClassId::new(key),
                Arc::from("missing"),
                Arc::clone(&desc),
                ResolvedMember::NotFound,
            );
        }
    }
    fn keys(resolver: &LinkResolver) -> Vec<(u32, String, String)> {
        let mut out: Vec<(u32, String, String)> = resolver
            .cache
            .read()
            .keys()
            .map(|(c, n, d)| (c.as_u32(), n.to_string(), d.to_string()))
            .collect();
        out.sort();
        out
    }

    let sets: [&[u32]; 5] = [&[], &[3], &[2, 5], &[1, 2, 3, 4, 5, 6], &[99]];
    for set in sets {
        let looped = LinkResolver::new();
        fill(&looped);
        for &id in set {
            looped.invalidate_class(ClassId::new(id));
        }
        let batched = LinkResolver::new();
        fill(&batched);
        let ids: std::collections::HashSet<ClassId> =
            set.iter().map(|&id| ClassId::new(id)).collect();
        batched.invalidate_classes(&ids);
        assert_eq!(
            keys(&batched),
            keys(&looped),
            "batch and loop disagree for the set {set:?}"
        );
        if set.is_empty() {
            assert_eq!(batched.len(), 6 * 13, "an empty set evicts nothing");
        }
    }
}

/// Round 9 audit fix (vm LOW #11): `resolve_or_compute` must detect
/// a race-winner that populated the same key while we were
/// computing, and return the winner's entry instead of clobbering
/// it. Simulate the race by populating the cache from outside
/// inside the closure (which models another thread winning).
#[test]
fn resolve_or_compute_handles_race_loser() {
    let resolver = LinkResolver::new();
    let class_a = ClassId::new(1);
    let class_winner = ClassId::new(99);
    let class_loser = ClassId::new(42);

    // Loser computes a result, but the closure simulates a winner
    // by inserting into the cache mid-compute. The race-loser
    // check must spot the winner and return it.
    let got = resolver.resolve_or_compute(class_a, "m", "()V", || {
        // Simulate a different thread winning: insert under the
        // same key with `class_winner` as the declaring class.
        resolver.insert(
            class_a,
            Arc::from("m"),
            Arc::from("()V"),
            ResolvedMember::Method {
                declaring_class_id: class_winner,
                index: 7,
            },
        );
        // The "loser" returns a different declaring class.
        (
            Arc::from("m"),
            Arc::from("()V"),
            ResolvedMember::Method {
                declaring_class_id: class_loser,
                index: 99,
            },
        )
    });
    match got {
        ResolvedMember::Method {
            declaring_class_id,
            index,
        } => {
            assert_eq!(
                declaring_class_id, class_winner,
                "race loser should have returned the winner's entry"
            );
            assert_eq!(index, 7);
        }
        other => panic!("expected Method, got {other:?}"),
    }
    // Cache must still hold the winner's entry, not the loser's.
    match resolver.get(class_a, "m", "()V").expect("entry present") {
        ResolvedMember::Method {
            declaring_class_id,
            index,
        } => {
            assert_eq!(declaring_class_id, class_winner);
            assert_eq!(index, 7);
        }
        other => panic!("expected Method, got {other:?}"),
    }
    assert_eq!(resolver.len(), 1);    }

/// PERF (bounded cache): the cache must stay at or under [`CACHE_CAP`]
/// no matter how many distinct triples are inserted, and a hot entry
/// (kept warm via `get`) must survive the CLOCK eviction sweep.
#[test]
fn link_resolver_cache_is_bounded() {
    let resolver = LinkResolver::new();
    let desc: Arc<str> = Arc::from("()V");

    // Insert a "hot" entry and keep touching it so its CLOCK bit
    // stays set across sweeps.
    let hot_class = ClassId::new(1);
    let hot_name: Arc<str> = Arc::from("hot");
    resolver.insert(
        hot_class,
        hot_name.clone(),
        desc.clone(),
        ResolvedMember::Method {
            declaring_class_id: hot_class,
            index: 0,
        },
    );

    // Flood the cache with far more than CACHE_CAP distinct triples,
    // re-touching the hot entry between batches.
    for i in 0..(CACHE_CAP * 3) {
        // Use ClassId values that never collide with `hot_class`.
        let cid = ClassId::new((i as u32) + 2);
        let name: Arc<str> = Arc::from(format!("m{i}"));
        resolver.insert(
            cid,
            name,
            desc.clone(),
            ResolvedMember::Method {
                declaring_class_id: cid,
                index: i as u32,
            },
        );
        if i % 64 == 0 {
            // Keep the hot entry's reference bit set.
            let _ = resolver.get(hot_class, &hot_name, &desc);
        }
    }

    // Hard bound: never exceeds the cap.
    assert!(
        resolver.len() <= CACHE_CAP,
        "cache size {} exceeded cap {}",
        resolver.len(),
        CACHE_CAP
    );

    // The continuously-touched hot entry survived all the sweeps.
    assert!(
        resolver.get(hot_class, &hot_name, &desc).is_some(),
        "hot entry was evicted despite being kept warm"
    );
}

fn i18_l4_static_target(declaring: u32) -> CachedInvokeTarget<()> {
    use std::sync::Arc;
    CachedInvokeTarget::Bytecode {
        cached: Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_reader::CachedMethodParts {
                declaring_class_id: ClassId::new(declaring),
                class_name: Arc::from("p/Helper"),
                method_name: Arc::from("pickA"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(&[0x04u8, 0xACu8][..]), // iconst_1; ireturn
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 0,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )),
        // The TARGET never changes in these tests: what retires the
        // entry is the caller side.
        gate: RedefineGate::never_stale(),
    }
}

/// Interpreter round i1 wave 18, lane L4: a redefinition replaces the
/// CALLER's constant pool under the same `ClassId`, so an entry keyed on
/// `(caller, cp index)` may name another member afterwards although its
/// target's gate is unchanged. The next lookup after a redefinition drops
/// the maps (primary and poly), and the cache serves what is filled after
/// it.
#[test]
fn a_redefinition_retires_the_callers_entries_and_the_cache_keeps_serving() {
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(18_401);
    let receiver = ClassId::new(18_402);
    cache.put(caller, 5, false, 0, i18_l4_static_target(18_403));
    cache.put_poly(caller, 6, false, 0, receiver, i18_l4_static_target(18_403));
    assert!(cache.get(caller, 5, false, 0).is_some());
    assert!(cache.get_poly(caller, 6, false, 0, receiver).is_some());

    tests_redefinition_count::bump();
    assert!(
        cache.get(caller, 5, false, 0).is_none(),
        "an entry filled before the redefinition may name the old pool's member"
    );
    assert!(
        cache.get_poly(caller, 6, false, 0, receiver).is_none(),
        "the poly entries are keyed on the caller too"
    );

    cache.put(caller, 5, false, 0, i18_l4_static_target(18_404));
    match cache.get(caller, 5, false, 0) {
        Some(CachedInvokeTarget::Bytecode { cached, .. }) => {
            assert_eq!(cached.declaring_class_id, ClassId::new(18_404));
        }
        other => panic!("a fill after the redefinition must be served, got {other:?}"),
    }
}

/// A fill whose resolution straddled a redefinition (the count moved after
/// the lookup that missed) is not stored: it may have read the old pool.
#[test]
fn a_fill_that_straddles_a_redefinition_is_not_stored() {
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(18_411);
    let receiver = ClassId::new(18_412);
    assert!(
        cache.get(caller, 5, false, 0).is_none(),
        "the missing lookup"
    );
    tests_redefinition_count::bump();
    cache.put(caller, 5, false, 0, i18_l4_static_target(18_413));
    cache.put_poly(caller, 5, false, 0, receiver, i18_l4_static_target(18_413));
    assert!(cache.get(caller, 5, false, 0).is_none());
    assert!(cache.get_poly(caller, 5, false, 0, receiver).is_none());
    // The lookup above caught up with the count: the next fill is kept.
    cache.put(caller, 5, false, 0, i18_l4_static_target(18_413));
    assert!(cache.get(caller, 5, false, 0).is_some());
    assert_eq!(cache.redefinitions_seen(), tests_redefinition_count::get());
}

/// Interpreter round i1 wave 19, lane L4: a fill whose resolution ran
/// Java that looked the cache up AFTER a redefinition (a user loader's
/// `loadClass`, a `<clinit>`) is not stored. The nested lookup moved
/// `redefinitions_seen` up to the new count, so `put` alone would admit
/// the fill; the snapshot taken before the resolution refuses it.
#[test]
fn a_fill_whose_resolution_ran_a_lookup_after_a_redefinition_is_not_stored() {
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(19_401);
    let receiver = ClassId::new(19_402);
    assert!(
        cache.get(caller, 5, false, 0).is_none(),
        "the missing lookup"
    );
    let as_of = cache.redefinitions_seen();
    // The resolution runs Java; another thread redefines the caller; the
    // Java performs an invoke on this thread.
    tests_redefinition_count::bump();
    assert!(
        cache.get(caller, 9, false, 0).is_none(),
        "the nested lookup"
    );
    assert_ne!(cache.redefinitions_seen(), as_of);

    cache.put_as_of(caller, 5, false, 0, as_of, i18_l4_static_target(19_403));
    cache.put_poly_as_of(
        caller,
        5,
        false,
        0,
        receiver,
        as_of,
        i18_l4_static_target(19_403),
    );
    assert!(
        cache.get(caller, 5, false, 0).is_none(),
        "resolved against the old pool"
    );
    assert!(cache.get_poly(caller, 5, false, 0, receiver).is_none());

    // A fill resolved after the nested lookup carries the new snapshot.
    let as_of = cache.redefinitions_seen();
    cache.put_as_of(caller, 5, false, 0, as_of, i18_l4_static_target(19_404));
    match cache.get(caller, 5, false, 0) {
        Some(CachedInvokeTarget::Bytecode { cached, .. }) => {
            assert_eq!(cached.declaring_class_id, ClassId::new(19_404));
        }
        other => panic!("a fill resolved under the new count is served, got {other:?}"),
    }
}

/// SBR5 lane interp: the direct-mapped front of the invoke cache answers
/// exactly what the map answers. A second lookup is served from the front
/// (the same target), and every mutation retires it: a `put` over the
/// key shows the replacement, an `evict` and a `clear` show a miss, a
/// fill of other keys (which can move the map's values) leaves every
/// earlier key answering correctly, and keys that collide in the front
/// still answer for themselves.
#[test]
fn the_invoke_cache_front_answers_what_the_map_answers() {
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(77_001);
    let declaring = |c: &mut InvokeCache<()>, cp: u16| match c.get(caller, cp, false, 0) {
        Some(CachedInvokeTarget::Bytecode { cached, .. }) => Some(cached.declaring_class_id),
        _ => None,
    };
    assert_eq!(declaring(&mut cache, 1), None);
    cache.put(caller, 1, false, 0, i18_l4_static_target(77_101));
    // First hit fills the front, the second is served from it.
    assert_eq!(declaring(&mut cache, 1), Some(ClassId::new(77_101)));
    assert_eq!(declaring(&mut cache, 1), Some(ClassId::new(77_101)));
    // A replacement of the same key is what the next lookup sees.
    cache.put(caller, 1, false, 0, i18_l4_static_target(77_102));
    assert_eq!(declaring(&mut cache, 1), Some(ClassId::new(77_102)));
    // Enough fills to grow the table several times: nothing earlier moves
    // out from under a slot.
    for cp in 2..400u16 {
        cache.put(
            caller,
            cp,
            false,
            0,
            i18_l4_static_target(77_200 + u32::from(cp)),
        );
        assert_eq!(declaring(&mut cache, 1), Some(ClassId::new(77_102)));
    }
    for cp in 2..400u16 {
        assert_eq!(
            declaring(&mut cache, cp),
            Some(ClassId::new(77_200 + u32::from(cp)))
        );
    }
    // The special flag is part of the key.
    assert!(cache.get(caller, 1, true, 0).is_none());
    // Removal retires the slot.
    cache.evict(caller, 1, false);
    assert_eq!(declaring(&mut cache, 1), None);
    assert_eq!(declaring(&mut cache, 7), Some(ClassId::new(77_207)));
    cache.clear();
    assert_eq!(declaring(&mut cache, 7), None);
    // A retained-jit sweep that drops nothing still leaves answers intact.
    cache.put(caller, 7, false, 0, i18_l4_static_target(77_300));
    assert_eq!(declaring(&mut cache, 7), Some(ClassId::new(77_300)));
    assert_eq!(cache.retain_jit(|_| false), 0);
    assert_eq!(declaring(&mut cache, 7), Some(ClassId::new(77_300)));
}

/// With no redefinition in between, `put_as_of` stores exactly as `put`
/// does, and a snapshot from before an unrelated earlier move that no
/// lookup has caught up with is refused like a plain `put`.
#[test]
fn put_as_of_with_a_current_snapshot_stores_like_put() {
    let mut cache = InvokeCache::<()>::new();
    let caller = ClassId::new(19_411);
    let receiver = ClassId::new(19_412);
    assert!(cache.get(caller, 5, false, 0).is_none());
    let as_of = cache.redefinitions_seen();
    cache.put_as_of(caller, 5, false, 0, as_of, i18_l4_static_target(19_413));
    cache.put_poly_as_of(
        caller,
        6,
        false,
        0,
        receiver,
        as_of,
        i18_l4_static_target(19_413),
    );
    assert!(cache.get(caller, 5, false, 0).is_some());
    assert!(cache.get_poly(caller, 6, false, 0, receiver).is_some());

    tests_redefinition_count::bump();
    cache.put_as_of(caller, 7, false, 0, as_of, i18_l4_static_target(19_414));
    assert!(cache.get(caller, 7, false, 0).is_none());
}
