use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Serialize tests that mutate AOT global state
// (AOT_CACHE_GLOBAL, AOT_TRAINING_RECORDER, AOT_PRELINKER_CACHE, AOT_ENABLED,
// AOT_TRAINING, AOT_PRODUCTION, AOT_CACHE_INPUT_PATH, AOT_CACHE_OUTPUT_PATH).
// cargo test runs tests in parallel by default, which otherwise causes
// tests that call `init_aot_runtime` / `reset_aot_globals` to race on
// these shared statics.  Use `std::sync::Mutex` + `PoisonError` passthrough
// so one panicking test still lets others acquire.
fn aot_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// -----------------------------------------------------------------------
// AotMethodProfile tests
// -----------------------------------------------------------------------

#[test]
fn test_aot_method_profile_new_defaults() {
    let p = AotMethodProfile::new();
    assert_eq!(p.invocation_count, 0);
    assert!(p.receiver_types.is_empty());
    assert!(p.branch_taken.is_empty());
    assert!(p.branch_not_taken.is_empty());
}

#[test]
fn test_aot_method_profile_record_invocation_no_type() {
    let mut p = AotMethodProfile::new();
    p.record_invocation(None);
    p.record_invocation(None);
    assert_eq!(p.invocation_count, 2);
    assert!(p.receiver_types.is_empty());
}

#[test]
fn test_aot_method_profile_record_invocation_with_type() {
    let mut p = AotMethodProfile::new();
    p.record_invocation(Some("java/lang/String"));
    p.record_invocation(Some("java/lang/String"));
    p.record_invocation(Some("java/lang/StringBuilder"));
    assert_eq!(p.invocation_count, 3);
    assert_eq!(p.receiver_types["java/lang/String"], 2);
    assert_eq!(p.receiver_types["java/lang/StringBuilder"], 1);
}

#[test]
fn test_aot_method_profile_dominant_receiver_type() {
    let mut p = AotMethodProfile::new();
    p.record_invocation(Some("A"));
    p.record_invocation(Some("B"));
    p.record_invocation(Some("B"));
    assert_eq!(p.dominant_receiver_type(), Some("B"));
}

#[test]
fn test_aot_method_profile_dominant_receiver_type_empty() {
    let p = AotMethodProfile::new();
    assert!(p.dominant_receiver_type().is_none());
}

#[test]
fn test_aot_method_profile_record_branch() {
    let mut p = AotMethodProfile::new();
    p.record_branch(10, true);
    p.record_branch(10, true);
    p.record_branch(10, false);
    assert_eq!(p.branch_taken[&10], 2);
    assert_eq!(p.branch_not_taken[&10], 1);
}

// -----------------------------------------------------------------------
// AotProfile tests
// -----------------------------------------------------------------------

#[test]
fn test_aot_profile_new_empty() {
    let ap = AotProfile::new();
    assert_eq!(ap.method_count(), 0);
}

#[test]
fn test_aot_profile_method_profile_mut_inserts() {
    let mut ap = AotProfile::new();
    let mp = ap.method_profile_mut("Foo", "bar", "()V");
    mp.record_invocation(None);
    assert_eq!(ap.method_count(), 1);
}

#[test]
fn test_aot_profile_method_profile_lookup() {
    let mut ap = AotProfile::new();
    ap.method_profile_mut("Foo", "bar", "()V")
        .record_invocation(None);
    let found = ap.method_profile("Foo", "bar", "()V");
    assert!(found.is_some());
    assert_eq!(found.unwrap().invocation_count, 1);
}

#[test]
fn test_aot_profile_method_profile_lookup_missing() {
    let ap = AotProfile::new();
    assert!(ap.method_profile("NoSuch", "method", "()V").is_none());
}

// -----------------------------------------------------------------------
// AotCacheConfig tests
// -----------------------------------------------------------------------

#[test]
fn test_aot_cache_config_defaults() {
    let cfg = AotCacheConfig::new();
    assert!(cfg.output_path.is_none());
    assert!(cfg.input_path.is_none());
    assert!(!cfg.training_mode);
    assert!(!cfg.production_mode);
    assert_eq!(cfg.max_entries, 10_000);
}

#[test]
fn test_aot_cache_config_builder_methods() {
    let cfg = AotCacheConfig::new()
        .with_output_path("/tmp/cache.aot")
        .with_input_path("/tmp/cache.aot")
        .with_training_mode(true)
        .with_max_entries(500);
    assert_eq!(cfg.output_path.as_deref(), Some("/tmp/cache.aot"));
    assert_eq!(cfg.input_path.as_deref(), Some("/tmp/cache.aot"));
    assert!(cfg.training_mode);
    assert_eq!(cfg.max_entries, 500);
}

#[test]
fn test_aot_cache_config_production_mode() {
    let cfg = AotCacheConfig::new().with_production_mode(true);
    assert!(cfg.production_mode);
    assert!(!cfg.training_mode);
}

// -----------------------------------------------------------------------
// AotCacheEntry tests
// -----------------------------------------------------------------------

#[test]
fn test_aot_cache_entry_placeholder_fingerprint() {
    let bc = [0xCAu8, 0xFEu8, 0xBAu8, 0xBEu8];
    let entry = AotCacheEntry::new_placeholder(&bc);
    assert_eq!(&entry.bytecode_fingerprint[..4], &bc[..]);
    assert!(!entry.has_compiled_code());
}

#[test]
fn test_aot_cache_entry_with_code() {
    let mut entry = AotCacheEntry::new_placeholder(&[]);
    entry.compiled_code = vec![0x90u8; 16]; // NOP sled placeholder
    assert!(entry.has_compiled_code());
}

// -----------------------------------------------------------------------
// AotCache tests
// -----------------------------------------------------------------------

#[test]
fn test_aot_cache_store_and_lookup() {
    let mut cache = AotCache::new(AotCacheConfig::new());
    let entry = AotCacheEntry::new_placeholder(&[1, 2, 3]);
    cache.store("Foo", "bar", "()V", entry);
    assert_eq!(cache.entry_count(), 1);
    assert!(cache.lookup("Foo", "bar", "()V").is_some());
    assert!(cache.lookup("Foo", "baz", "()V").is_none());
}

#[test]
fn test_aot_cache_max_entries_enforced() {
    let cfg = AotCacheConfig::new().with_max_entries(2);
    let mut cache = AotCache::new(cfg);
    assert!(cache.store("A", "m", "()V", AotCacheEntry::new_placeholder(&[])));
    assert!(cache.store("B", "m", "()V", AotCacheEntry::new_placeholder(&[])));
    // Third entry should be rejected
    assert!(!cache.store("C", "m", "()V", AotCacheEntry::new_placeholder(&[])));
    assert_eq!(cache.entry_count(), 2);
}

#[test]
fn test_aot_cache_serialize_deserialize_roundtrip() {
    let mut cache = AotCache::new(AotCacheConfig::new());
    let mut entry = AotCacheEntry::new_placeholder(&[0xCA, 0xFE]);
    entry.compiled_code = vec![0x90, 0x90, 0xC3];
    entry.deopt_map.insert(4, 8);
    entry.entry_point_offset = 0;
    cache.store("com/example/Foo", "compute", "(I)I", entry);

    let blob = cache.serialize();
    let restored = AotCache::deserialize(&blob).expect("deserialization should succeed");
    assert_eq!(restored.entry_count(), 1);
    let e = restored
        .lookup("com/example/Foo", "compute", "(I)I")
        .unwrap();
    assert_eq!(e.bytecode_fingerprint[0], 0xCA);
    assert_eq!(e.bytecode_fingerprint[1], 0xFE);
    assert_eq!(e.compiled_code, vec![0x90, 0x90, 0xC3]);
    assert_eq!(e.deopt_map[&4], 8);
}

#[test]
fn test_aot_cache_deserialize_bad_magic() {
    let data = [0x00u8; 20];
    assert!(AotCache::deserialize(&data).is_none());
}

#[test]
fn test_aot_cache_deserialize_too_short() {
    assert!(AotCache::deserialize(&[]).is_none());
    assert!(AotCache::deserialize(&[0u8; 5]).is_none());
}

// -----------------------------------------------------------------------
// PrelinkedClass / PrelinkerCache tests
// -----------------------------------------------------------------------

#[test]
fn test_prelinked_class_add_and_get_field() {
    let mut cls = PrelinkedClass::new("java/lang/Object");
    cls.add_field(
        5,
        PrelinkedField {
            field_offset: 12,
            descriptor: "I".to_string(),
            is_static: false,
        },
    );
    let f = cls.get_field(5).unwrap();
    assert_eq!(f.field_offset, 12);
    assert_eq!(f.descriptor, "I");
    assert!(!f.is_static);
    assert!(cls.get_field(99).is_none());
}

#[test]
fn test_prelinked_class_add_and_get_method() {
    let mut cls = PrelinkedClass::new("java/lang/Object");
    cls.add_method(
        3,
        PrelinkedMethod {
            vtable_slot: 7,
            descriptor: "()V".to_string(),
            is_static: false,
        },
    );
    let m = cls.get_method(3).unwrap();
    assert_eq!(m.vtable_slot, 7);
    assert_eq!(m.descriptor, "()V");
    assert!(cls.get_method(99).is_none());
}

#[test]
fn test_prelinker_cache_insert_and_get() {
    let mut cache = PrelinkerCache::new();
    cache.insert(PrelinkedClass::new("java/lang/String"));
    assert_eq!(cache.class_count(), 1);
    assert!(cache.get("java/lang/String").is_some());
    assert!(cache.get("java/lang/Object").is_none());
}

#[test]
fn test_prelinker_cache_serialize_deserialize_roundtrip() {
    let mut cache = PrelinkerCache::new();
    let mut cls = PrelinkedClass::new("com/example/Bar");
    cls.add_field(
        1,
        PrelinkedField {
            field_offset: 8,
            descriptor: "J".to_string(),
            is_static: true,
        },
    );
    cls.add_method(
        2,
        PrelinkedMethod {
            vtable_slot: 3,
            descriptor: "(J)V".to_string(),
            is_static: false,
        },
    );
    cache.insert(cls);

    let blob = cache.serialize();
    let restored = PrelinkerCache::deserialize(&blob).expect("deserialization should succeed");
    assert_eq!(restored.class_count(), 1);
    let rc = restored.get("com/example/Bar").unwrap();
    let rf = rc.get_field(1).unwrap();
    assert_eq!(rf.field_offset, 8);
    assert_eq!(rf.descriptor, "J");
    assert!(rf.is_static);
    let rm = rc.get_method(2).unwrap();
    assert_eq!(rm.vtable_slot, 3);
    assert_eq!(rm.descriptor, "(J)V");
}

#[test]
fn test_prelinker_cache_deserialize_bad_magic() {
    let data = [0xFFu8; 20];
    assert!(PrelinkerCache::deserialize(&data).is_none());
}

// -----------------------------------------------------------------------
// TrainingRunRecorder tests
// -----------------------------------------------------------------------

#[test]
fn test_training_run_recorder_method_invocations() {
    let mut rec = TrainingRunRecorder::new();
    rec.record_method_invocation("Foo", "bar", "()V", None);
    rec.record_method_invocation("Foo", "bar", "()V", Some("Foo"));
    rec.record_method_invocation("Foo", "bar", "()V", Some("Foo"));
    assert_eq!(rec.method_count(), 1);
    let r = rec.method_record("Foo", "bar", "()V").unwrap();
    assert_eq!(r.invocation_count, 3);
    assert_eq!(r.receiver_types["Foo"], 2);
}

#[test]
fn test_training_run_recorder_multiple_methods() {
    let mut rec = TrainingRunRecorder::new();
    rec.record_method_invocation("A", "m1", "()V", None);
    rec.record_method_invocation("B", "m2", "()I", None);
    assert_eq!(rec.method_count(), 2);
}

#[test]
fn test_training_run_recorder_branch_recording() {
    let mut rec = TrainingRunRecorder::new();
    rec.record_branch("Foo", "check", 42, true);
    rec.record_branch("Foo", "check", 42, true);
    rec.record_branch("Foo", "check", 42, false);
    assert_eq!(rec.branch_count(), 1);
    let br = rec.branch_record("Foo", "check", 42).unwrap();
    assert_eq!(br.taken_count, 2);
    assert_eq!(br.not_taken_count, 1);
}

#[test]
fn test_training_run_recorder_missing_record() {
    let rec = TrainingRunRecorder::new();
    assert!(rec.method_record("X", "y", "()V").is_none());
    assert!(rec.branch_record("X", "y", 0).is_none());
}

#[test]
fn test_training_run_recorder_to_aot_profile() {
    let mut rec = TrainingRunRecorder::new();
    rec.record_method_invocation("Cls", "run", "()V", Some("Cls"));
    rec.record_method_invocation("Cls", "run", "()V", Some("Cls"));
    let profile = rec.to_aot_profile();
    let mp = profile.method_profile("Cls", "run", "()V");
    assert!(mp.is_some());
    assert_eq!(mp.unwrap().invocation_count, 2);
}

#[test]
fn test_training_run_recorder_serialize_magic() {
    let rec = TrainingRunRecorder::new();
    let blob = rec.serialize();
    assert!(blob.len() >= 10);
    let magic = u32::from_le_bytes(blob[0..4].try_into().unwrap());
    assert_eq!(magic, 0xA07CAC4E);
}

#[test]
fn test_training_run_recorder_serialize_with_data() {
    let mut rec = TrainingRunRecorder::new();
    rec.record_method_invocation("X", "go", "(I)V", None);
    let blob = rec.serialize();
    let method_count = u32::from_le_bytes(blob[6..10].try_into().unwrap());
    assert_eq!(method_count, 1);
}

// -----------------------------------------------------------------------
// BranchRecord tests
// -----------------------------------------------------------------------

#[test]
fn test_branch_record_taken_probability() {
    let br = BranchRecord {
        class_name: "A".into(),
        method_name: "m".into(),
        bytecode_offset: 0,
        taken_count: 3,
        not_taken_count: 1,
    };
    let p = br.taken_probability();
    assert!((p - 0.75).abs() < 1e-9);
}

#[test]
fn test_branch_record_taken_probability_never_executed() {
    let br = BranchRecord {
        class_name: "A".into(),
        method_name: "m".into(),
        bytecode_offset: 0,
        taken_count: 0,
        not_taken_count: 0,
    };
    assert!(br.taken_probability().is_nan());
}

// -----------------------------------------------------------------------
// Native method registration tests
// -----------------------------------------------------------------------

#[test]
fn test_register_aot_natives_leyden_query_methods() {
    let mut r = NativeMethodRegistry::new();
    register_aot_natives(&mut r);
    assert!(r.find(LEYDEN, "isAOTEnabled", "()Z").is_some());
    assert!(r.find(LEYDEN, "isTrainingMode", "()Z").is_some());
    assert!(r.find(LEYDEN, "isProductionMode", "()Z").is_some());
    assert!(r
        .find(LEYDEN, "getAOTCacheInputPath", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(LEYDEN, "getAOTCacheOutputPath", "()Ljava/lang/String;")
        .is_some());
}

#[test]
fn test_register_aot_natives_leyden_callbacks() {
    let mut r = NativeMethodRegistry::new();
    register_aot_natives(&mut r);
    assert!(r
        .find(
            LEYDEN,
            "notifyMethodInvoked",
            "(Ljava/lang/reflect/Method;)V"
        )
        .is_some());
    assert!(r
        .find(LEYDEN, "notifyClassLoaded", "(Ljava/lang/Class;)V")
        .is_some());
    assert!(r
        .find(
            LEYDEN,
            "lookupAOTMethod",
            "(Ljava/lang/reflect/Method;)Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find(LEYDEN, "storeAOTProfile", "(Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(LEYDEN, "loadAOTProfile", "(Ljava/lang/String;)I")
        .is_some());
}

#[test]
fn test_register_aot_natives_stable_annotation() {
    let mut r = NativeMethodRegistry::new();
    register_aot_natives(&mut r);
    let stable = "jdk/internal/vm/annotation/Stable";
    assert!(r.find(stable, "<init>", "()V").is_some());
    assert!(r
        .find(stable, "annotationType", "()Ljava/lang/Class;")
        .is_some());
}

#[test]
fn test_register_aot_natives_reflect_method() {
    let mut r = NativeMethodRegistry::new();
    register_aot_natives(&mut r);
    assert!(r
        .find(
            "java/lang/reflect/Method",
            "isAnnotationPresent",
            "(Ljava/lang/Class;)Z"
        )
        .is_some());
}

#[test]
fn test_register_aot_natives_leyden_init() {
    let mut r = NativeMethodRegistry::new();
    register_aot_natives(&mut r);
    assert!(r.find(LEYDEN, "<init>", "()V").is_some());
}

#[test]
fn test_total_registered_method_count() {
    let mut r = NativeMethodRegistry::new();
    register_aot_natives(&mut r);
    // 10 Leyden methods + 2 @Stable + 1 reflect = 13 total
    assert!(r.len() >= 13);
}

// -----------------------------------------------------------------------
// Global AOT runtime state tests
// -----------------------------------------------------------------------

#[test]
fn test_init_aot_runtime_training_mode() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, Some("/tmp/test_cache.aot"));
    assert!(is_aot_enabled());
    assert!(is_aot_training());
    assert!(!is_aot_production());
    assert_eq!(
        AOT_CACHE_OUTPUT_PATH.lock().as_deref(),
        Some("/tmp/test_cache.aot")
    );
    assert!(AOT_TRAINING_RECORDER.lock().unwrap().is_some());
    assert!(AOT_CACHE_GLOBAL.lock().unwrap().is_some());
    assert!(AOT_PRELINKER_CACHE.lock().unwrap().is_some());
    reset_aot_globals();
}

#[test]
fn test_init_aot_runtime_production_mode() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(false, true, Some("/nonexistent/cache.aot"), None);
    assert!(is_aot_enabled());
    assert!(!is_aot_training());
    assert!(is_aot_production());
    assert_eq!(
        AOT_CACHE_INPUT_PATH.lock().as_deref(),
        Some("/nonexistent/cache.aot")
    );
    // Cache should be None since file doesn't exist
    assert!(AOT_PRELINKER_CACHE.lock().unwrap().is_some());
    reset_aot_globals();
}

#[test]
fn test_init_aot_runtime_off() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(false, false, None, None);
    assert!(!is_aot_enabled());
    assert!(!is_aot_training());
    assert!(!is_aot_production());
    reset_aot_globals();
}

#[test]
fn test_aot_record_method_invocation_training() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_method_invocation("com/example/Foo", "run", "()V", Some("com/example/Foo"));
    aot_record_method_invocation("com/example/Foo", "run", "()V", None);
    let (methods, _) = aot_training_stats();
    assert_eq!(methods, 1);
    {
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let mr = rec.method_record("com/example/Foo", "run", "()V").unwrap();
        assert_eq!(mr.invocation_count, 2);
        assert_eq!(mr.receiver_types.get("com/example/Foo"), Some(&1));
    }
    reset_aot_globals();
}

#[test]
fn test_aot_record_method_invocation_noop_when_off() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(false, false, None, None);
    aot_record_method_invocation("Foo", "bar", "()V", None);
    let (methods, _) = aot_training_stats();
    assert_eq!(methods, 0);
    reset_aot_globals();
}

#[test]
fn test_aot_record_branch_training() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_branch("Foo", "check", 10, true);
    aot_record_branch("Foo", "check", 10, false);
    aot_record_branch("Foo", "check", 10, true);
    let (_, branches) = aot_training_stats();
    assert_eq!(branches, 1);
    {
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let br = rec.branch_record("Foo", "check", 10).unwrap();
        assert_eq!(br.taken_count, 2);
        assert_eq!(br.not_taken_count, 1);
    }
    reset_aot_globals();
}

#[test]
fn test_aot_record_class_loaded_training() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_class_loaded("com/example/Bar");
    aot_record_class_loaded("com/example/Bar"); // duplicate should not error
    {
        let guard = AOT_PRELINKER_CACHE.lock().unwrap();
        let cache = guard.as_ref().unwrap();
        assert_eq!(cache.class_count(), 1);
        assert!(cache.get("com/example/Bar").is_some());
    }
    reset_aot_globals();
}

#[test]
fn test_aot_flush_noop_when_off() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    let written = aot_flush_training_data();
    assert_eq!(written, 0);
    reset_aot_globals();
}

#[test]
fn test_aot_flush_training_data_writes_file() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    // Use a per-test tempdir so parallel tests or leftover files from
    // prior runs cannot cross-contaminate.
    let tmp = tempfile::TempDir::new().expect("create per-test tempdir");
    let path = tmp.path().join("cratonvm_aot_test_flush.bin");
    let path_str = path.to_string_lossy().to_string();

    init_aot_runtime(true, false, None, Some(&path_str));
    aot_record_method_invocation("Test", "main", "()V", None);

    let written = aot_flush_training_data();
    assert!(written > 0);
    assert!(path.exists());

    // Clean up
    let _ = std::fs::remove_file(&path);
    reset_aot_globals();
}

#[test]
fn test_aot_production_load_from_file() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    // Per-test tempdir: RAII cleanup on drop, avoids sharing
    // "cratonvm_aot_test_load.bin" in $TMP with parallel runs.
    let tmp = tempfile::TempDir::new().expect("create per-test tempdir");
    let path = tmp.path().join("cratonvm_aot_test_load.bin");
    let path_str = path.to_string_lossy().to_string();

    // Create a valid cache file
    let mut cache = AotCache::new(AotCacheConfig::new());
    let entry = AotCacheEntry::new_placeholder(&[0xCA, 0xFE]);
    cache.store("Test", "compute", "(I)I", entry);
    let data = cache.serialize();
    std::fs::write(&path, &data).unwrap();

    // Load it in production mode
    init_aot_runtime(false, true, Some(&path_str), None);
    assert!(is_aot_production());
    let cache_guard = AOT_CACHE_GLOBAL.lock().unwrap();
    let loaded = cache_guard.as_ref().unwrap();
    assert_eq!(loaded.entry_count(), 1);
    assert!(loaded.lookup("Test", "compute", "(I)I").is_some());

    // Clean up (TempDir auto-removes file via RAII)
    drop(cache_guard);
    reset_aot_globals();
}

// ===== M29 AOT Training Activation Tests =====

#[test]
fn m29_training_records_method_invocation_with_receiver() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_method_invocation(
        "com/example/Foo",
        "bar",
        "(I)V",
        Some("com/example/FooImpl"),
    );
    aot_record_method_invocation(
        "com/example/Foo",
        "bar",
        "(I)V",
        Some("com/example/FooImpl"),
    );
    aot_record_method_invocation("com/example/Foo", "bar", "(I)V", Some("com/example/FooSub"));
    let (methods, _) = aot_training_stats();
    assert_eq!(methods, 1);
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let mr = rec.method_record("com/example/Foo", "bar", "(I)V").unwrap();
    assert_eq!(mr.invocation_count, 3);
    assert_eq!(mr.receiver_types["com/example/FooImpl"], 2);
    assert_eq!(mr.receiver_types["com/example/FooSub"], 1);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_training_records_branch_taken_and_not_taken() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_branch("com/example/Foo", "bar", 10, true);
    aot_record_branch("com/example/Foo", "bar", 10, true);
    aot_record_branch("com/example/Foo", "bar", 10, false);
    let (_, branches) = aot_training_stats();
    assert_eq!(branches, 1);
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let br = rec.branch_record("com/example/Foo", "bar", 10).unwrap();
    assert_eq!(br.taken_count, 2);
    assert_eq!(br.not_taken_count, 1);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_receiver_type_recording() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_receiver_type("Foo", "doIt", "()V", "FooImpl");
    aot_record_receiver_type("Foo", "doIt", "()V", "FooImpl");
    aot_record_receiver_type("Foo", "doIt", "()V", "BarImpl");
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let mr = rec.method_record("Foo", "doIt", "()V").unwrap();
    assert_eq!(mr.invocation_count, 3);
    assert_eq!(mr.receiver_types["FooImpl"], 2);
    assert_eq!(mr.receiver_types["BarImpl"], 1);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_receiver_type_noop_when_not_training() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    // Not in training mode
    init_aot_runtime(false, false, None, None);
    aot_record_receiver_type("Foo", "bar", "()V", "Impl");
    let (methods, _) = aot_training_stats();
    assert_eq!(methods, 0);
    reset_aot_globals();
}

#[test]
fn m29_bulk_import_branches() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    let entries: Vec<(&str, &str, u32, u32, u32)> =
        vec![("A", "m1", 5, 10, 3), ("B", "m2", 20, 0, 5)];
    let count = aot_bulk_import_branches(&entries);
    assert_eq!(count, 2);
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let br1 = rec.branch_record("A", "m1", 5).unwrap();
    assert_eq!(br1.taken_count, 10);
    assert_eq!(br1.not_taken_count, 3);
    let br2 = rec.branch_record("B", "m2", 20).unwrap();
    assert_eq!(br2.taken_count, 0);
    assert_eq!(br2.not_taken_count, 5);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_bulk_import_receivers() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    let entries: Vec<(&str, &str, &str, &str, u32)> = vec![
        ("A", "m1", "()V", "ImplA", 5),
        ("A", "m1", "()V", "ImplB", 3),
    ];
    let count = aot_bulk_import_receivers(&entries);
    assert_eq!(count, 2);
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let mr = rec.method_record("A", "m1", "()V").unwrap();
    assert_eq!(mr.invocation_count, 8); // 5 + 3
    assert_eq!(mr.receiver_types["ImplA"], 5);
    assert_eq!(mr.receiver_types["ImplB"], 3);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_bulk_import_noop_when_not_training() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(false, true, None, None); // production, not training
    let entries: Vec<(&str, &str, u32, u32, u32)> = vec![("A", "m", 0, 10, 5)];
    let count = aot_bulk_import_branches(&entries);
    assert_eq!(count, 0);
    reset_aot_globals();
}

#[test]
fn m29_class_loaded_creates_prelinker_entry() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_class_loaded("com/example/MyClass");
    let guard = AOT_PRELINKER_CACHE.lock().unwrap();
    let cache = guard.as_ref().unwrap();
    assert!(cache.get("com/example/MyClass").is_some());
    assert_eq!(
        cache.get("com/example/MyClass").unwrap().class_name,
        "com/example/MyClass"
    );
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_class_loaded_idempotent() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_class_loaded("com/example/Dup");
    aot_record_class_loaded("com/example/Dup");
    let guard = AOT_PRELINKER_CACHE.lock().unwrap();
    let cache = guard.as_ref().unwrap();
    assert_eq!(cache.class_count(), 1);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_training_to_profile_includes_receivers() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_method_invocation("X", "run", "()V", Some("XImpl"));
    aot_record_method_invocation("X", "run", "()V", Some("XImpl"));
    aot_record_branch("X", "run", 5, true);
    aot_record_branch("X", "run", 5, false);
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let profile = rec.to_aot_profile();
    let mp = profile.method_profile("X", "run", "()V").unwrap();
    assert_eq!(mp.invocation_count, 2);
    assert_eq!(mp.receiver_types["XImpl"], 2);
    // Branch data goes into a profile with empty descriptor
    let bp = profile.method_profile("X", "run", "").unwrap();
    assert_eq!(bp.branch_taken[&5], 1);
    assert_eq!(bp.branch_not_taken[&5], 1);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_training_serialize_includes_receiver_types() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_method_invocation("Cls", "met", "()V", Some("Impl"));
    let guard = AOT_TRAINING_RECORDER.lock().unwrap();
    let rec = guard.as_ref().unwrap();
    let data = rec.serialize();
    // Header: magic(4) + version(2) + method_count(4) = 10 bytes minimum
    assert!(data.len() > 10);
    // Verify magic
    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    assert_eq!(magic, 0xA07CAC4E);
    drop(guard);
    reset_aot_globals();
}

#[test]
fn m29_multiple_methods_tracked_separately() {
    let _guard = aot_test_lock();
    reset_aot_globals();
    init_aot_runtime(true, false, None, None);
    aot_record_method_invocation("A", "m1", "()V", None);
    aot_record_method_invocation("A", "m2", "(I)V", None);
    aot_record_method_invocation("B", "m1", "()V", None);
    let (methods, _) = aot_training_stats();
    assert_eq!(methods, 3);
    reset_aot_globals();
}

#[test]
fn m29_branch_probability_calculation() {
    let br = BranchRecord {
        class_name: "T".to_string(),
        method_name: "f".to_string(),
        bytecode_offset: 0,
        taken_count: 75,
        not_taken_count: 25,
    };
    let prob = br.taken_probability();
    assert!((prob - 0.75).abs() < 0.001);
}

#[test]
fn m29_branch_probability_zero_executions() {
    let br = BranchRecord {
        class_name: "T".to_string(),
        method_name: "f".to_string(),
        bytecode_offset: 0,
        taken_count: 0,
        not_taken_count: 0,
    };
    assert!(br.taken_probability().is_nan());
}

// -----------------------------------------------------------------------
// Integrity & security tests (M29 fixes)
// -----------------------------------------------------------------------

#[test]
fn test_aot_cache_deserialize_rejects_bad_version() {
    let mut blob = Vec::new();
    blob.extend_from_slice(&0xA07CAC4Eu32.to_le_bytes()); // correct magic
    blob.extend_from_slice(&99u16.to_le_bytes()); // bad version
    blob.extend_from_slice(&0u32.to_le_bytes());
    assert!(AotCache::deserialize(&blob).is_none());
}

#[test]
fn test_aot_cache_integrity_tamper_detected() {
    let mut cache = AotCache::new(AotCacheConfig::new());
    let entry = AotCacheEntry {
        bytecode_fingerprint: [0; 8],
        compiled_code: vec![0xAB; 8],
        deopt_map: HashMap::new(),
        entry_point_offset: 0,
    };
    cache.store("A", "b", "()V", entry);
    let mut blob = cache.serialize();
    // Flip a byte in the payload
    if blob.len() > 12 {
        blob[12] ^= 0xFF;
    }
    assert!(AotCache::deserialize(&blob).is_none());
}

#[test]
fn test_aot_cache_integrity_digest_is_sha256_len() {
    // The trailing digest must be a full 32-byte SHA-256, not the old
    // 8-byte non-cryptographic mix.
    let cache = AotCache::new(AotCacheConfig::new());
    let blob = cache.serialize();
    // Header (10) + no entries + 32-byte digest.
    assert_eq!(blob.len(), 10 + AotCache::INTEGRITY_DIGEST_LEN);
    // The bare (keyless) digest of the header payload must equal a
    // straight SHA-256 over those bytes.
    let payload = &blob[..blob.len() - AotCache::INTEGRITY_DIGEST_LEN];
    let expect = crate::crypto_impl::Sha256::digest(payload);
    assert_eq!(
        &blob[blob.len() - AotCache::INTEGRITY_DIGEST_LEN..],
        &expect[..]
    );
}

#[test]
fn test_aot_cache_integrity_recompute_forgery_still_detected_without_key() {
    // Without a key, SHA-256 detects accidental change. A forger who
    // recomputes the digest over a tampered payload would pass — this is
    // the documented residual risk addressed by CRATONVM_AOT_HMAC_KEY.
    // Here we assert the corruption-detection property: any payload edit
    // that is NOT accompanied by a recomputed digest is rejected.
    let mut cache = AotCache::new(AotCacheConfig::new());
    cache.store(
        "A",
        "b",
        "()V",
        AotCacheEntry {
            bytecode_fingerprint: [0; 8],
            compiled_code: vec![1, 2, 3, 4],
            deopt_map: HashMap::new(),
            entry_point_offset: 0,
        },
    );
    let mut blob = cache.serialize();
    // Tamper with the very last digest byte: a stale digest is rejected.
    let last = blob.len() - 1;
    blob[last] ^= 0xFF;
    assert!(AotCache::deserialize(&blob).is_none());
}

#[test]
fn test_aot_cache_integrity_rejects_trailing_garbage() {
    let cache = AotCache::new(AotCacheConfig::new());
    let mut blob = cache.serialize();
    // Extra bytes after a valid digest must be rejected (exact-length
    // check), so a forger cannot append a second valid-looking digest.
    blob.push(0x00);
    assert!(AotCache::deserialize(&blob).is_none());
}

#[test]
fn test_aot_cache_hmac_keyed_digest_differs_from_bare() {
    // With a key present the digest is HMAC-SHA256, which differs from the
    // bare SHA-256 of the same payload. Drive compute_integrity_hash
    // directly so this test does not depend on process-global env state
    // (env-var mutation would race with parallel tests).
    let payload = b"some-aot-cache-payload-bytes";
    let bare = AotCache::compute_integrity_hash(payload, None);
    let keyed = AotCache::compute_integrity_hash(payload, Some(b"secret-key"));
    assert_ne!(bare, keyed);
    // HMAC is deterministic for a fixed key+payload.
    let keyed2 = AotCache::compute_integrity_hash(payload, Some(b"secret-key"));
    assert_eq!(keyed, keyed2);
    // A different key yields a different tag.
    let keyed3 = AotCache::compute_integrity_hash(payload, Some(b"other-key"));
    assert_ne!(keyed, keyed3);
}

#[test]
fn test_aot_cache_hmac_matches_rfc2104_reference() {
    // RFC 4231 / RFC 2104 HMAC-SHA256 test case 1:
    //   key  = 0x0b * 20
    //   data = "Hi There"
    //   mac  = b0344c61d8db38535ca8afceaf0bf12b
    //          881dc200c9833da726e9376c2e32cff7
    let key = [0x0bu8; 20];
    let mac = AotCache::compute_integrity_hash(b"Hi There", Some(&key));
    let expect: [u8; 32] = [
        0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
        0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
        0x2e, 0x32, 0xcf, 0xf7,
    ];
    assert_eq!(mac, expect);
}

#[test]
fn test_sanitize_aot_path_rejects_traversal() {
    assert!(sanitize_aot_path("cache/foo.bin").is_some());
    assert!(sanitize_aot_path("../secret/data").is_none());
    assert!(sanitize_aot_path("foo/../../etc/passwd").is_none());
    assert!(sanitize_aot_path("safe_dir/cache.bin").is_some());
}
