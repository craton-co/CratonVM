use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;

#[test]
fn object_type_name_accepts_only_class_descriptors() {
    assert_eq!(
        object_type_name("Ljava/lang/String;"),
        Some("java/lang/String")
    );
    assert_eq!(object_type_name("L;"), None);
    assert_eq!(object_type_name("I"), None);
    assert_eq!(object_type_name("[Ljava/lang/String;"), None);
    // Unterminated: refused rather than sliced (a byte slice at `len - 1`
    // could split a multi-byte character).
    assert_eq!(object_type_name("Ljava/lang/Stri\u{e9}"), None);
}

/// A bootstrap declared with a primitive return type hands back a
/// primitive `Value`; the field type's widening applies, as
/// `MethodHandle.invoke` would apply it.
#[test]
fn condy_primitive_results_widen_to_the_field_type() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let t = &mut thread;
    assert_eq!(
        condy_convert_result(&shared, &mut *t, b'J', None, "J", Value::Int(5)).ok(),
        Some(Value::Long(5))
    );
    assert_eq!(
        condy_convert_result(&shared, &mut *t, b'D', None, "D", Value::Int(2)).ok(),
        Some(Value::Double(2.0))
    );
    assert_eq!(
        condy_convert_result(&shared, &mut *t, b'I', None, "I", Value::Int(42)).ok(),
        Some(Value::Int(42))
    );
    // A null reference result for a reference type is a legal answer.
    let null_ok = condy_convert_result(
        &shared,
        &mut *t,
        b'L',
        Some(ClassId::new(0)),
        "java/lang/Object",
        Value::Object(None),
    );
    assert_eq!(null_ok.ok(), Some(Value::Object(None)));
}

/// JVMS §5.4.3: a recorded `LinkageError` is rethrown as a NEW instance of
/// the same class with the same message, keyed per (class, cp index); a
/// throwable that is not a `LinkageError` is never recorded.
#[test]
fn a_recorded_linkage_error_is_rethrown_with_its_class_and_message() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let referencing = ClassId::new(0);
    let Ok(ncdfe) = crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NoClassDefFoundError",
        Some("p/Opt"),
    ) else {
        // Stripped VM (no class library on the test classpath).
        return;
    };
    let Ok(rte) = crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/RuntimeException",
        Some("not a linkage error"),
    ) else {
        return;
    };
    let _ = record_resolution_failure(
        &shared,
        referencing,
        7,
        MethodCallFailed::ExceptionThrown(rte),
    );
    assert!(recorded_resolution_failure(&shared, &mut thread, referencing, 7).is_none());

    let _ = record_resolution_failure(
        &shared,
        referencing,
        9,
        MethodCallFailed::ExceptionThrown(ncdfe),
    );
    let Some(MethodCallFailed::ExceptionThrown(again)) =
        recorded_resolution_failure(&shared, &mut thread, referencing, 9)
    else {
        panic!("the NoClassDefFoundError must have been recorded");
    };
    assert_ne!(again, ncdfe, "a new throwable, not the recorded object");
    // Same class and message as the original (compared through the same
    // reader, so the test does not depend on the Throwable field layout).
    let original = linkage_error_record(&shared, ncdfe).expect("a LinkageError");
    let rethrown = linkage_error_record(&shared, again).expect("still a LinkageError");
    assert_eq!(&*rethrown.error_class, "java/lang/NoClassDefFoundError");
    assert_eq!(rethrown, original);
    // Another entry of the same class is unaffected.
    assert!(recorded_resolution_failure(&shared, &mut thread, referencing, 10).is_none());
}

/// Interpreter round i1 wave 25, lane L5: the "a failure was recorded"
/// pre-filter is the VM's own (`ClassRealm::resolution_failure_recorded`),
/// not a process static: one VM's record leaves another VM's miss paths
/// off the table, and a record raises the recording VM's bit.
#[test]
fn the_failure_prefilter_is_per_vm() {
    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let Ok(ncdfe) = crate::runtime::exceptions::create_exception_object(
        &first,
        &mut thread,
        "java/lang/NoClassDefFoundError",
        Some("p/Opt"),
    ) else {
        // Stripped VM (no class library on the test classpath).
        return;
    };
    assert!(!any_resolution_failure_recorded(&first));
    assert!(!any_resolution_failure_recorded(&second));
    let _ = record_resolution_failure(
        &first,
        ClassId::new(0),
        9,
        MethodCallFailed::ExceptionThrown(ncdfe),
    );
    assert!(
        any_resolution_failure_recorded(&first),
        "the recording VM's pre-filter is raised"
    );
    assert!(
        !any_resolution_failure_recorded(&second),
        "another VM's record must not raise this VM's pre-filter"
    );
    assert!(!resolution_failure_is_recorded(&second, ClassId::new(0), 9));
    assert!(resolution_failure_is_recorded(&first, ClassId::new(0), 9));
}

/// The compatible-mode native `Lookup.find*` reports a miss as an
/// unmaterialised `VmError::Runtime(NoSuchMethodException)`; it must map
/// to the `NoSuchMethodError` HotSpot raises, exactly as a thrown
/// `NoSuchMethodException` does (probe L6/GenericIndyStaticArgsProbe).
#[test]
fn an_unmaterialised_lookup_miss_maps_to_the_linkage_error() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NoSuchMethodError",
        None,
    )
    .is_err()
    {
        // Stripped VM (no class library on the test classpath).
        return;
    }
    // The mapping recognises the reflective family by its real superclass
    // chain; a VM whose `NoSuchMethodException` is a synthetic stand-in
    // (no `ReflectiveOperationException` above it) cannot exercise it.
    let Ok(probe) = crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NoSuchMethodException",
        None,
    ) else {
        return;
    };
    {
        let cm = shared.classes.class_manager.read();
        let reflective = cm
            .get_loaded_class_id("java/lang/ReflectiveOperationException")
            .is_some_and(|roe| cm.is_subclass_of(shared.mem.heap.class_id_of(probe), roe));
        if !reflective {
            return;
        }
    }
    let failure = MethodCallFailed::InternalError(VmError::Runtime(
        crate::error::RuntimeError::NoSuchMethodException {
            message: "p.C.missing()I".to_string(),
        },
    ));
    let MethodCallFailed::ExceptionThrown(err) =
        map_lookup_exception_to_error(&shared, &mut thread, failure)
    else {
        panic!("a reflective miss must come back as a thrown Java error");
    };
    let cm = shared.classes.class_manager.read();
    let name = cm
        .get_class(shared.mem.heap.class_id_of(err))
        .map(|c| c.name.to_string());
    assert_eq!(name.as_deref(), Some("java/lang/NoSuchMethodError"));
}

fn thrown_class_name(shared: &SharedVm, failure: &MethodCallFailed) -> Option<String> {
    let MethodCallFailed::ExceptionThrown(obj) = failure else {
        return None;
    };
    let cm = shared.classes.class_manager.read();
    cm.get_class(shared.mem.heap.class_id_of(*obj))
        .map(|c| c.name.to_string())
}

#[test]
fn descriptor_class_names_reads_each_name_to_its_semicolon() {
    assert_eq!(
        descriptor_class_names("(Lp/A;[[Lp/B;IJ)Lp/C;"),
        vec!["p/A", "p/B", "p/C"]
    );
    // An `L` inside a name is part of the name, not a new type.
    assert_eq!(descriptor_class_names("(ILcom/Lib;[I)V"), vec!["com/Lib"]);
    assert!(descriptor_class_names("(IJ)[D").is_empty());
    // A malformed tail stops rather than slicing past the end.
    assert_eq!(descriptor_class_names("(Lp/A;Lp/Unterminated"), vec!["p/A"]);
}

/// The JDK factory's `TypeNotPresentException` for a missing class becomes
/// the `NoClassDefFoundError` HotSpot's method-type resolution raises; any
/// other throwable passes through as the same object.
#[test]
fn a_method_type_miss_is_a_no_class_def_found_error() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let Ok(tnpe) = crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/TypeNotPresentException",
        None,
    ) else {
        // Stripped VM (no class library on the test classpath).
        return;
    };
    let Ok(other) = crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/IllegalStateException",
        None,
    ) else {
        return;
    };
    let desc = "(Lcratonvm/test/l2/NoSuchParamType;)V";
    let holder = ClassId::new(0);
    let mapped = method_type_failure_as_linkage_error(&shared, &mut thread, holder, desc, tnpe);
    assert_eq!(
        thrown_class_name(&shared, &mapped).as_deref(),
        Some("java/lang/NoClassDefFoundError")
    );
    let kept = method_type_failure_as_linkage_error(&shared, &mut thread, holder, desc, other);
    assert!(matches!(kept, MethodCallFailed::ExceptionThrown(o) if o == other));
}

/// Numeric `ldc` / `ldc2_w` push a correctly typed (and, for long/double,
/// tagged) value on the fill and on the hit, and a warm site is served
/// from the per-thread `PrimitiveConstantSiteCache`.
#[test]
fn numeric_ldc_sites_are_served_from_the_thread_table() {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

    // The cache on whatever the environment says (wave 58, lane L7: the
    // gate is per-VM).
    let mut config = VmConfig::default();
    config.ldc_const_cache = true;
    let shared = SharedVm::new(config);
    let class_id = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l2/Ldc2wSite", 0) else {
            return; // --real-jdk policy in the environment: nothing to fabricate
        };
        let Some(class) = cm.class_store.get_mut(id) else {
            return;
        };
        class.constant_pool = ConstantPool::new(vec![
            E::Tombstone,
            E::Long((i64::MIN + 7).into()),
            E::Tombstone,
            E::Double((-2.5).into()),
            E::Tombstone,
            E::Integer(-424_242),
            E::Float(1.5),
        ]);
        id
    };
    let mut thread = JvmThread::new(ThreadId(0), "ldc2w");
    thread.frames.push(Frame::new(
        class_id,
        "cratonvm/test/l2/Ldc2wSite".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    for round in 0..2 {
        let before = PrimitiveConstantSiteCache::epochs_now();
        assert!(
            execute_ldc2w(&shared, &mut thread, 0, 1).is_ok(),
            "round {round}"
        );
        assert!(
            execute_ldc2w(&shared, &mut thread, 0, 3).is_ok(),
            "round {round}"
        );
        assert!(
            execute_ldc(&shared, &mut thread, 0, 5).is_ok(),
            "round {round}"
        );
        assert!(
            execute_ldc(&shared, &mut thread, 0, 6).is_ok(),
            "round {round}"
        );
        assert_eq!(thread.frames[0].stack.pop_float().ok(), Some(1.5));
        assert_eq!(thread.frames[0].stack.pop_int().ok(), Some(-424_242));
        assert_eq!(thread.frames[0].stack.pop_double().ok(), Some(-2.5));
        assert_eq!(thread.frames[0].stack.pop_long().ok(), Some(i64::MIN + 7));
        // Epochs are process-wide and other tests define classes
        // concurrently, so the table is asserted only over a quiet window.
        let double_hit = thread
            .site_tables
            .prim_const_sites
            .get(class_id, 3)
            .copied();
        let int_hit = thread
            .site_tables
            .prim_const_sites
            .get(class_id, 5)
            .copied();
        if ldc_const_cache_enabled(&shared)
            && !cratonvm_classloading::any_class_redefined()
            && PrimitiveConstantSiteCache::epochs_now() == before
        {
            assert_eq!(
                double_hit,
                Some(PrimitiveConstant::Double(-2.5)),
                "round {round}"
            );
            assert_eq!(
                int_hit,
                Some(PrimitiveConstant::Int(-424_242)),
                "round {round}"
            );
        }
    }
}
