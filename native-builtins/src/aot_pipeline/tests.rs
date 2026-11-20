use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn tmp_dir_with_suffix(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    let pid = std::process::id();
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    d.push(format!("cratonvm_cp12_{tag}_{pid}_{ns}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// -----------------------------------------------------------------------
// AotProfileEntry / AotProfileBundle unit tests
// -----------------------------------------------------------------------

#[test]
fn profile_entry_new_defaults_are_empty() {
    let e = AotProfileEntry::new([0u8; 32], "Foo", "bar", "()V", 0);
    assert!(e.jit_code.is_empty());
    assert_eq!(e.jit_flags, 0);
    assert!(!e.has_jit_code());
}

#[test]
fn profile_entry_with_jit_code() {
    let e = AotProfileEntry::new([1u8; 32], "Foo", "bar", "()V", 5)
        .with_jit_code(vec![0x90, 0xC3], 0xDEAD_BEEF);
    assert!(e.has_jit_code());
    assert_eq!(e.jit_code, vec![0x90, 0xC3]);
    assert_eq!(e.jit_flags, 0xDEAD_BEEF);
}

#[test]
fn bundle_serialize_roundtrip_preserves_entries() {
    let mut bundle = AotProfileBundle::new();
    bundle.add(
        AotProfileEntry::new([0xAA; 32], "com/foo/Bar", "run", "()V", 42)
            .with_jit_code(vec![0x48, 0x89, 0xE5, 0xC3], 0x01),
    );
    bundle.add(AotProfileEntry::new([0xBB; 32], "X", "y", "(I)I", 7));

    let blob = bundle.serialize();
    let restored = AotProfileBundle::deserialize(&blob).expect("roundtrip");
    assert_eq!(restored.entries.len(), 2);
    assert_eq!(restored.entries[0].call_count, 42);
    assert_eq!(restored.entries[0].jit_code, vec![0x48, 0x89, 0xE5, 0xC3]);
    assert_eq!(restored.entries[0].jit_flags, 0x01);
    assert_eq!(restored.entries[1].class_name, "X");
}

#[test]
fn bundle_deserialize_rejects_bad_magic() {
    let mut data = AotProfileBundle::new().serialize();
    data[0] ^= 0xFF;
    assert!(AotProfileBundle::deserialize(&data).is_none());
}

#[test]
fn bundle_deserialize_rejects_tampered_payload() {
    let mut bundle = AotProfileBundle::new();
    bundle.add(AotProfileEntry::new([0xCC; 32], "A", "m", "()V", 1));
    let mut blob = bundle.serialize();
    // Flip a byte in the middle so the SHA-256 integrity check trips.
    let mid = blob.len() / 2;
    blob[mid] ^= 0xFF;
    assert!(AotProfileBundle::deserialize(&blob).is_none());
}

#[test]
fn bundle_deserialize_rejects_wrong_version() {
    let mut b = Vec::new();
    b.extend_from_slice(&AOT_PROFILE_MAGIC.to_le_bytes());
    b.extend_from_slice(&99u16.to_le_bytes()); // bad version
    b.extend_from_slice(&0u32.to_le_bytes()); // no entries
    b.extend_from_slice(&sha256_bytes(&b.clone()));
    assert!(AotProfileBundle::deserialize(&b).is_none());
}

#[test]
fn filter_by_live_class_hashes_drops_stale_entries() {
    let mut bundle = AotProfileBundle::new();
    bundle.add(AotProfileEntry::new([0xAA; 32], "Good", "m", "()V", 1));
    bundle.add(AotProfileEntry::new([0xBB; 32], "Stale", "m", "()V", 1));
    bundle.add(AotProfileEntry::new([0xCC; 32], "Missing", "m", "()V", 1));

    let mut live = HashMap::new();
    live.insert("Good".to_string(), [0xAA; 32]);
    live.insert("Stale".to_string(), [0xDD; 32]); // wrong hash

    let filtered = bundle.filter_by_live_class_hashes(&live);
    assert_eq!(filtered.entries.len(), 1);
    assert_eq!(filtered.entries[0].class_name, "Good");
}

// -----------------------------------------------------------------------
// Path helpers
// -----------------------------------------------------------------------

#[test]
fn profile_file_name_is_deterministic_and_scoped() {
    let a = aot_profile_file_name("java/lang/String", "length", "()I");
    let b = aot_profile_file_name("java/lang/String", "length", "()I");
    assert_eq!(a, b);
    assert!(a.ends_with(".profile"));

    let c = aot_profile_file_name("java/lang/String", "length", "()J");
    assert_ne!(a, c);
}

#[test]
fn config_paths_honour_root_override() {
    let root = tmp_dir_with_suffix("root");
    let cfg = AotPipelineConfig {
        root_override: Some(root.clone()),
        ..Default::default()
    };
    assert_eq!(cfg.root(), root);
    assert_eq!(cfg.aot_cache_dir(), root.join(AOT_CACHE_DIR));
    assert_eq!(
        cfg.cds_archive_path(),
        root.join(CDS_DIR).join(CDS_ARCHIVE_NAME)
    );
}

// -----------------------------------------------------------------------
// JDK build-ID parsing
// -----------------------------------------------------------------------

#[test]
fn read_jdk_build_id_parses_java_version() {
    let d = tmp_dir_with_suffix("jh1");
    std::fs::write(d.join("release"), "JAVA_VERSION=\"25.0.1\"\nOTHER=x\n").unwrap();
    assert_eq!(read_jdk_build_id(&d), "25.0.1");
}

#[test]
fn read_jdk_build_id_missing_file_is_empty() {
    let d = tmp_dir_with_suffix("jh2");
    // No release file.
    assert!(read_jdk_build_id(&d).is_empty());
}

#[test]
fn read_jdk_build_id_falls_back_to_sha_for_unknown_format() {
    let d = tmp_dir_with_suffix("jh3");
    std::fs::write(d.join("release"), "HELLO=world\n").unwrap();
    let id = read_jdk_build_id(&d);
    assert!(id.starts_with("sha:"));
}

// -----------------------------------------------------------------------
// CdsArchiveWithBuildIdV2 — write & load
// -----------------------------------------------------------------------

#[test]
fn cds_with_build_id_roundtrip() {
    let d = tmp_dir_with_suffix("cds1");
    let path = d.join("core.jsa");
    let p = path.to_string_lossy().to_string();

    let mut archive = CdsArchiveWithBuildIdV2::new(p.clone(), "JDK-25.0.1");
    archive.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34],
    ));
    archive.add_entry(CdsArchiveEntry::new(
        "java/lang/String",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34, 0x01, 0x02],
    ));
    assert_eq!(archive.entry_count(), 2);
    archive.write().expect("write");
    assert!(path.exists());

    let loaded = CdsArchiveWithBuildIdV2::try_load(&p, "JDK-25.0.1").expect("load");
    assert_eq!(loaded.0, "JDK-25.0.1");
    assert_eq!(loaded.1.len(), 2);
    assert!(loaded.1.contains_key("java/lang/Object"));
    assert!(loaded.1.contains_key("java/lang/String"));
}

#[test]
fn cds_with_build_id_rejects_mismatch() {
    let d = tmp_dir_with_suffix("cds2");
    let path = d.join("core.jsa");
    let p = path.to_string_lossy().to_string();

    let mut archive = CdsArchiveWithBuildIdV2::new(p.clone(), "JDK-25.0.1");
    archive.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34],
    ));
    archive.write().expect("write");

    // Load with a different build ID — should be refused.
    let res = CdsArchiveWithBuildIdV2::try_load(&p, "JDK-24.0.0");
    assert!(res.is_none());
}

#[test]
fn cds_with_build_id_rejects_bare_archive() {
    // A CDS archive WITHOUT the build-id header must be refused.
    let d = tmp_dir_with_suffix("cds3");
    let path = d.join("core.jsa");
    let p = path.to_string_lossy().to_string();

    // Produce a bare archive via the existing generator.
    let mut gen = CdsArchiveGenerator::new(&p);
    gen.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE],
    ));
    gen.write_archive().expect("write");

    assert!(CdsArchiveWithBuildIdV2::try_load(&p, "anything").is_none());
}

// -----------------------------------------------------------------------
// Integration tests — full startup/shutdown pipeline
// -----------------------------------------------------------------------

/// FIX(test-isolation): the integration tests below drive the PROCESS-GLOBAL
/// `PIPELINE_STATE` through multi-step sequences (reset → record → flush →
/// load → assert). Each `with_state` op is individually locked, but the
/// *sequences* interleave under parallel `cargo test` — and the `--workspace`
/// build enables `experimental-aot`, running these alongside the whole
/// suite — so a sibling's `reset_pipeline_state()` / records clobber another
/// test's accumulated state, yielding wrong counts (e.g.
/// `aot_profiles_written != 2`, `cds_classes_loaded != 2`). Serialize the
/// whole-test sequences on this lock so each runs atomically against the
/// global pipeline state. Poison-tolerant so a panicking test can't wedge
/// the rest.
fn pipeline_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Use a fresh state for integration tests so they don't fight the global
/// mutex-backed globals in other tests. We simulate the pipeline by
/// inserting config, populating state, flushing, then reading back.
fn reset_pipeline_state() {
    with_state(|s| {
        s.config = None;
        s.bundle = AotProfileBundle::new();
        s.cds_classes.clear();
        s.cds_loaded.clear();
        s.aot_loaded.clear();
    });
}

/// Integration test #1: AOT profile roundtrip.
///
/// Simulates a training run: `startup_load` is a no-op, we record a few
/// profile entries, `shutdown_flush` persists them, and a second
/// `startup_load` recovers them.
#[test]
fn integration_aot_profile_roundtrip() {
    let _guard = pipeline_test_lock(); // FIX(test-isolation): serialize global PIPELINE_STATE seq
    reset_pipeline_state();
    let root = tmp_dir_with_suffix("aot_rt");
    let java_home = tmp_dir_with_suffix("aot_rt_jh");
    std::fs::write(java_home.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();

    let cfg = AotPipelineConfig {
        aot_cache: true,
        root_override: Some(root.clone()),
        java_home_override: Some(java_home.clone()),
        ..Default::default()
    };

    // First boot: empty cache.
    let stats = startup_load(cfg.clone());
    assert_eq!(stats.aot_profiles_loaded, 0);

    // Record a couple of hot methods.
    record_profile_entry(
        AotProfileEntry::new([0xAA; 32], "com/foo/Bar", "hot", "()V", 9_999)
            .with_jit_code(vec![0x48, 0x89, 0xE5, 0xC3], 0x1),
    );
    record_profile_entry(AotProfileEntry::new(
        [0xBB; 32],
        "com/foo/Baz",
        "warm",
        "(I)I",
        100,
    ));

    // Shutdown writes them to <hash>.profile files.
    let shut = shutdown_flush();
    assert_eq!(shut.aot_profiles_written, 2);

    // Second boot: cache is repopulated.
    reset_pipeline_state();
    let stats = startup_load(cfg);
    assert_eq!(stats.aot_profiles_loaded, 2);
    let loaded = loaded_aot_entries();
    let names: std::collections::HashSet<_> =
        loaded.iter().map(|e| e.class_name.clone()).collect();
    assert!(names.contains("com/foo/Bar"));
    assert!(names.contains("com/foo/Baz"));
    // Also verify JIT code bytes survived.
    let bar = loaded
        .iter()
        .find(|e| e.class_name == "com/foo/Bar")
        .unwrap();
    assert_eq!(bar.jit_code, vec![0x48, 0x89, 0xE5, 0xC3]);

    reset_pipeline_state();
}

/// Integration test #2: CDS roundtrip with matching build ID.
#[test]
fn integration_cds_roundtrip() {
    let _guard = pipeline_test_lock(); // FIX(test-isolation): serialize global PIPELINE_STATE seq
    reset_pipeline_state();
    let root = tmp_dir_with_suffix("cds_rt");
    let java_home = tmp_dir_with_suffix("cds_rt_jh");
    std::fs::write(java_home.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();

    // Dump.
    let dump_cfg = AotPipelineConfig {
        cds_dump: true,
        root_override: Some(root.clone()),
        java_home_override: Some(java_home.clone()),
        ..Default::default()
    };
    startup_load(dump_cfg);
    record_cds_class(
        "java/lang/Object",
        vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34],
    );
    record_cds_class(
        "java/lang/String",
        vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x34, 0x10, 0x20],
    );
    let shut = shutdown_flush();
    assert_eq!(shut.cds_classes_written, 2);
    assert!(root.join(CDS_DIR).join(CDS_ARCHIVE_NAME).exists());

    // Load with matching build ID.
    reset_pipeline_state();
    let load_cfg = AotPipelineConfig {
        cds_enabled: true,
        root_override: Some(root.clone()),
        java_home_override: Some(java_home.clone()),
        ..Default::default()
    };
    let stats = startup_load(load_cfg);
    assert_eq!(stats.cds_classes_loaded, 2);
    assert!(!stats.cds_rejected);
    let classes = loaded_cds_classes();
    assert!(classes.contains_key("java/lang/Object"));
    assert!(classes.contains_key("java/lang/String"));

    reset_pipeline_state();
}

/// Integration test #3: stale cache rejection on JDK-build-ID mismatch.
#[test]
fn integration_stale_cache_rejected_on_build_id_mismatch() {
    let _guard = pipeline_test_lock(); // FIX(test-isolation): serialize global PIPELINE_STATE seq
    reset_pipeline_state();
    let root = tmp_dir_with_suffix("stale");
    let java_home_a = tmp_dir_with_suffix("stale_jha");
    let java_home_b = tmp_dir_with_suffix("stale_jhb");
    std::fs::write(java_home_a.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();
    std::fs::write(java_home_b.join("release"), "JAVA_VERSION=\"26.0.0\"\n").unwrap();

    // Dump with JDK A.
    let dump_cfg = AotPipelineConfig {
        cds_dump: true,
        root_override: Some(root.clone()),
        java_home_override: Some(java_home_a.clone()),
        ..Default::default()
    };
    startup_load(dump_cfg);
    record_cds_class("java/lang/Object", vec![0xCA, 0xFE, 0xBA, 0xBE]);
    let shut = shutdown_flush();
    assert_eq!(shut.cds_classes_written, 1);

    // Load with JDK B — must be rejected.
    reset_pipeline_state();
    let load_cfg = AotPipelineConfig {
        cds_enabled: true,
        root_override: Some(root.clone()),
        java_home_override: Some(java_home_b.clone()),
        ..Default::default()
    };
    let stats = startup_load(load_cfg);
    assert_eq!(stats.cds_classes_loaded, 0);
    assert!(stats.cds_rejected);
    assert!(loaded_cds_classes().is_empty());

    reset_pipeline_state();
}

/// Integration test #4 (bonus): stale AOT entry rejection via class SHA.
#[test]
fn integration_stale_aot_entry_rejected_on_class_sha_mismatch() {
    let mut bundle = AotProfileBundle::new();
    bundle.add(AotProfileEntry::new(
        [0xAA; 32],
        "com/foo/Bar",
        "m",
        "()V",
        100,
    ));
    bundle.add(AotProfileEntry::new(
        [0xBB; 32],
        "com/foo/Baz",
        "m",
        "()V",
        50,
    ));

    // Only Bar is live, and its hash has changed since the profile was
    // written. Baz is live with the matching hash.
    let mut live = HashMap::new();
    live.insert("com/foo/Bar".to_string(), [0xDD; 32]); // changed
    live.insert("com/foo/Baz".to_string(), [0xBB; 32]); // same

    let filtered = bundle.filter_by_live_class_hashes(&live);
    assert_eq!(filtered.entries.len(), 1);
    assert_eq!(filtered.entries[0].class_name, "com/foo/Baz");
}
