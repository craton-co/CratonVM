use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -----------------------------------------------------------------------
// Archive constant tests
// -----------------------------------------------------------------------

#[test]
fn test_cds_magic_constant() {
    assert_eq!(CDS_MAGIC, 0xF00D_1234);
}

#[test]
fn test_cds_version_constant() {
    assert_eq!(CDS_VERSION, 1);
}

#[test]
fn test_cds_max_classes_constant() {
    assert_eq!(CDS_MAX_CLASSES, 65536);
}

// -----------------------------------------------------------------------
// CdsArchive header tests
// -----------------------------------------------------------------------

#[test]
fn test_cds_archive_new_is_valid() {
    let archive = CdsArchive::new();
    assert!(archive.is_valid());
    assert_eq!(archive.magic, CDS_MAGIC);
    assert_eq!(archive.version, CDS_VERSION);
    assert_eq!(archive.class_count, 0);
}

#[test]
fn test_cds_archive_default_is_valid() {
    let archive = CdsArchive::default();
    assert!(archive.is_valid());
}

#[test]
fn test_cds_archive_bad_magic_is_invalid() {
    let archive = CdsArchive {
        magic: 0xDEAD_BEEF,
        version: CDS_VERSION,
        class_count: 0,
        checksum: 0,
    };
    assert!(!archive.is_valid());
}

#[test]
fn test_cds_archive_bad_version_is_invalid() {
    let archive = CdsArchive {
        magic: CDS_MAGIC,
        version: 99,
        class_count: 0,
        checksum: 0,
    };
    assert!(!archive.is_valid());
}

// -----------------------------------------------------------------------
// CdsArchiveEntry tests
// -----------------------------------------------------------------------

#[test]
fn test_archive_entry_new() {
    let bytes = vec![0xCA; 512];
    let e = CdsArchiveEntry::new("java/lang/Object", 0, bytes);
    assert_eq!(e.class_name, "java/lang/Object");
    assert_eq!(e.bytes_offset, 0);
    assert_eq!(e.bytes_length, 512);
    assert_eq!(e.class_bytes.len(), 512);
    assert!(e.superclass.is_none());
    assert!(e.interfaces.is_empty());
}

#[test]
fn test_archive_entry_with_superclass() {
    let bytes = vec![0xFE; 2048];
    let mut e = CdsArchiveEntry::new("java/lang/String", 512, bytes);
    e.superclass = Some("java/lang/Object".to_string()); // gate-ok(not-a-Class): CdsArchiveEntry
    assert_eq!(e.superclass.as_deref(), Some("java/lang/Object"));
}

// -----------------------------------------------------------------------
// CdsArchiveGenerator tests
// -----------------------------------------------------------------------

#[test]
fn test_generator_starts_empty() {
    let gen = CdsArchiveGenerator::new("/tmp/classes.jsa");
    assert_eq!(gen.entry_count(), 0);
    assert_eq!(gen.output_path(), "/tmp/classes.jsa");
}

#[test]
fn test_generator_add_entries() {
    let mut gen = CdsArchiveGenerator::new("/tmp/out.jsa");
    gen.add_entry(CdsArchiveEntry::new("java/lang/Object", 0, vec![0xCA; 100]));
    gen.add_entry(CdsArchiveEntry::new(
        "java/lang/String",
        100,
        vec![0xFE; 200],
    ));
    assert_eq!(gen.entry_count(), 2);
}

#[test]
fn test_generator_header_class_count() {
    let mut gen = CdsArchiveGenerator::new("/tmp/out.jsa");
    gen.add_entry(CdsArchiveEntry::new("java/util/List", 0, vec![0xBA; 50]));
    let hdr = gen.build_header();
    assert!(hdr.is_valid());
    assert_eq!(hdr.class_count, 1);
}

#[test]
fn test_generator_write_archive_stub_succeeds() {
    let gen = CdsArchiveGenerator::new("/tmp/out.jsa");
    assert!(gen.write_archive().is_ok());
}

#[test]
fn test_generator_respects_max_classes() {
    let mut gen = CdsArchiveGenerator::new("/tmp/big.jsa");
    for i in 0..CDS_MAX_CLASSES + 10 {
        gen.add_entry(CdsArchiveEntry::new(
            format!("cls/{i}"),
            i as u64,
            vec![0x01],
        ));
    }
    assert_eq!(gen.entry_count(), CDS_MAX_CLASSES);
}

// -----------------------------------------------------------------------
// CdsArchiveLoader tests
// -----------------------------------------------------------------------

#[test]
fn test_loader_new() {
    let loader = CdsArchiveLoader::new("/var/cache/jsa/app.jsa");
    assert_eq!(loader.archive_path(), "/var/cache/jsa/app.jsa");
    assert!(!loader.is_loaded());
    assert_eq!(loader.classes_loaded(), 0);
}

#[test]
fn test_loader_try_load_returns_false() {
    let mut loader = CdsArchiveLoader::new("/non/existent.jsa");
    assert!(!loader.try_load());
    assert!(!loader.is_loaded());
}

// -----------------------------------------------------------------------
// CdsMetrics tests
// -----------------------------------------------------------------------

#[test]
fn test_cds_metrics_disabled() {
    let m = CdsMetrics::disabled();
    assert_eq!(m.total_classes_in_archive, 0);
    assert_eq!(m.classes_loaded_from_archive, 0);
    assert_eq!(m.archive_size_bytes, 0);
    assert_eq!(m.archive_load_time_ms, 0);
    assert!(m.archive_path.is_empty());
}

// -----------------------------------------------------------------------
// AppCdsConfig tests
// -----------------------------------------------------------------------

#[test]
fn test_appcds_config_new_accepts_all() {
    let cfg = AppCdsConfig::new("/tmp/dynamic.jsa");
    assert!(cfg.accepts("com/example/Foo"));
    assert!(cfg.accepts("java/lang/Object"));
}

#[test]
fn test_appcds_config_exclude_wins() {
    let mut cfg = AppCdsConfig::new("/tmp/dyn.jsa");
    cfg.exclude_patterns
        .push("com/example/internal/**".to_string());
    cfg.include_patterns = vec!["com/example/**".to_string()];
    assert!(!cfg.accepts("com/example/internal/Secret"));
    assert!(cfg.accepts("com/example/Public"));
}

#[test]
fn test_appcds_config_include_only() {
    let cfg = AppCdsConfig {
        include_patterns: vec!["com/myapp/**".to_string()],
        exclude_patterns: vec![],
        output_path: "/tmp/out.jsa".to_string(),
    };
    assert!(cfg.accepts("com/myapp/Main"));
    assert!(!cfg.accepts("java/lang/Object"));
}

// -----------------------------------------------------------------------
// DynamicArchive tests
// -----------------------------------------------------------------------

#[test]
fn test_dynamic_archive_record_and_finalize() {
    let cfg = AppCdsConfig::new("/tmp/dyn.jsa");
    let mut dyn_arch = DynamicArchive::new(cfg);
    dyn_arch.record_class(CdsArchiveEntry::new("com/foo/Bar", 0, vec![0xBE; 128]));
    assert_eq!(dyn_arch.recorded_count(), 1);
    assert!(!dyn_arch.is_finalized());
    assert!(dyn_arch.finalize().is_ok());
    assert!(dyn_arch.is_finalized());
}

#[test]
fn test_dynamic_archive_exclude_filters() {
    let mut cfg = AppCdsConfig::new("/tmp/dyn.jsa");
    cfg.include_patterns = vec!["com/keep/**".to_string()];
    let mut dyn_arch = DynamicArchive::new(cfg);
    dyn_arch.record_class(CdsArchiveEntry::new("com/keep/Good", 0, vec![0xAA; 10]));
    dyn_arch.record_class(CdsArchiveEntry::new("com/drop/Bad", 0, vec![0xBB; 10]));
    assert_eq!(dyn_arch.recorded_count(), 1);
}

// -----------------------------------------------------------------------
// Class-list parse/format tests
// -----------------------------------------------------------------------

#[test]
fn test_parse_class_list_basic() {
    let text = "java/lang/Object\njava/lang/String\n";
    let list = parse_class_list(text);
    assert_eq!(list, vec!["java/lang/Object", "java/lang/String"]);
}

#[test]
fn test_parse_class_list_skips_comments() {
    let text = "# header\njava/util/List\n# more\njava/util/Map\n";
    let list = parse_class_list(text);
    assert_eq!(list, vec!["java/util/List", "java/util/Map"]);
}

#[test]
fn test_parse_class_list_normalises_dots() {
    let text = "java.lang.Object\n";
    let list = parse_class_list(text);
    assert_eq!(list, vec!["java/lang/Object"]);
}

#[test]
fn test_parse_class_list_blank_lines() {
    let text = "\n  \njava/util/Set\n\n";
    let list = parse_class_list(text);
    assert_eq!(list, vec!["java/util/Set"]);
}

#[test]
fn test_format_class_list() {
    let classes = vec!["java/lang/Object".to_string(), "java/util/List".to_string()];
    let text = format_class_list(&classes);
    assert_eq!(text, "java/lang/Object\njava/util/List");
}

// -----------------------------------------------------------------------
// Registration tests — verify all methods are findable
// -----------------------------------------------------------------------

#[test]
fn test_management_factory_helper_registration() {
    let mut r = NativeMethodRegistry::new();
    register_cds_natives(&mut r);
    let cls = "sun/management/ManagementFactoryHelper";
    assert!(r.find(cls, "<init>", "()V").is_some());
}

/// F17-1 (2026-08-13) — REPLACES four tests that asserted the presence of
/// registrations for classes and members JDK 25 does not have
/// (`test_cds_metrics_accessors_registered`,
/// `test_classloader_cds_archive_path_registered`,
/// `test_sun_misc_vm_registered`, and the `getCDSMetrics` half of
/// `test_management_factory_helper_registration`).
///
/// Those tests were not neutral about the fabrication — they PINNED it.
/// Each one asserted `is_some()` on a triple whose defect was that it
/// existed at all, so the only way to fail them was to fix the bug. Flipping
/// them to `is_none()` is what turns roughly a hundred lines of deletion
/// into something a reader can check without re-running `javap`.
#[test]
fn f17_1_registrar_mints_nothing_absent_from_the_jdk25_image() {
    let mut r = NativeMethodRegistry::new();
    register_cds_natives(&mut r);
    // `javap` on Microsoft 25.0.3+9-LTS answers "class not found" for each of
    // these three classes, so every triple on them was unreachable.
    let absent_classes = ["sun/management/CDSMetrics", "sun/misc/VM"];
    let registered: Vec<(&str, &str, &str)> = r
        .dump_registrations()
        .into_iter()
        .map(|(c, m, d, _)| (c, m, d))
        .collect();
    for cls in absent_classes {
        let minted: Vec<_> = registered
            .iter()
            .filter(|(c, _, _)| *c == cls)
            .map(|(_, m, d)| format!("{m}{d}"))
            .collect();
        assert!(
            minted.is_empty(),
            "{cls} is not in the JDK 25 runtime image, but the CDS registrar \
             still mints natives for it: {minted:?}"
        );
    }
    // These two classes DO exist; the members did not.
    let absent_members: &[(&str, &str, &str)] = &[
        (
            "sun/management/ManagementFactoryHelper",
            "getCDSMetrics",
            "()Lsun/management/CDSMetrics;",
        ),
        (
            "java/lang/ClassLoader",
            "getCdsArchivePath",
            "()Ljava/lang/String;",
        ),
        ("jdk/internal/misc/CDS", "isDumpingClassList", "()Z"),
        ("jdk/internal/misc/CDS", "isSharingEnabled", "()Z"),
    ];
    for (cls, name, desc) in absent_members {
        assert!(
            r.find(cls, name, desc).is_none(),
            "{cls}.{name}{desc} is not declared by JDK 25 — re-registering it \
             re-introduces the fabrication F17-1 removed"
        );
    }
}

#[test]
fn test_jdk_internal_cds_all_methods_registered() {
    let mut r = NativeMethodRegistry::new();
    register_cds_natives(&mut r);
    let cls = "jdk/internal/misc/CDS";
    // F17-1 (2026-08-13): every entry below is checked against `javap -p
    // jdk.internal.misc.CDS`, i.e. ALL access levels. F23-1 made
    // `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv` agree: it now
    // lists all 28 members including the five `private,static,native` ones,
    // and `jdk_baseline::f23_1_every_cds_registration_is_on_the_jdk25_\
    // declared_surface` audits this exact list against it. `isDumpingClassList`
    // and `isSharingEnabled` were removed from this list; `isUsingArchive`,
    // `getCDSConfigStatus` and `needsClassInitBarrier0` were added.
    let methods: &[(&str, &str)] = &[
        ("<init>", "()V"),
        ("isDumpingArchive", "()Z"),
        ("isUsingArchive", "()Z"),
        ("getCDSConfigStatus", "()I"),
        ("needsClassInitBarrier0", "(Ljava/lang/Class;)Z"),
        ("initializeFromArchive", "(Ljava/lang/Class;)V"),
        ("getRandomSeedForDumping", "()J"),
        ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
        (
            "defineArchivedModules",
            "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
        ),
        ("dumpClassList", "(Ljava/lang/String;)V"),
        ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
    ];
    for (name, desc) in methods {
        assert!(
            r.find(cls, name, desc).is_some(),
            "Missing jdk/internal/misc/CDS.{name}{desc}"
        );
    }
}

/// F23-1 NOM-4: the census `audit`'s `UNCOVERED` denominator deliberately
/// does NOT run (§2.1 of that record) — a two-way check of this registrar
/// against `Baseline::native_surface()`, the surgical opt-in the record
/// built for exactly this. `getCDSConfigStatus` and `needsClassInitBarrier0`
/// were found and fixed by hand with `javap -p`; this test is what would
/// have found them without that, and it is what stops a THIRD one from
/// needing the same manual search: every `native` method JDK 25 declares on
/// `jdk/internal/misc/CDS` must have a registration here.
#[test]
fn f23_1_nom4_every_declared_cds_native_is_registered() {
    let b = crate::jdk_baseline::parse(crate::jdk_baseline::CDS);
    let mut r = NativeMethodRegistry::new();
    register_cds_natives(&mut r);
    let cls = "jdk/internal/misc/CDS";
    let missing: Vec<(&str, &str)> = b
        .native_surface()
        .into_iter()
        .filter(|(name, desc)| r.find(cls, name, desc).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "jdk.internal.misc.CDS declares native methods with no registration in \
         register_cds_natives: {missing:?}. Each one is unreachable from real-JDK bytecode \
         until it is registered — see F17-1 §2.3 and F23-1 NOM-4."
    );
}

/// F17-1 (2026-08-13): was `test_registration_count_at_least_20`. The
/// registrar now mints 12, so a floor of 20 would be red — but the floor was
/// never the point, and raising or lowering a `>=` is not either. What
/// matters is that the count is EXACT and moves only when someone means it
/// to: a one-sided `>=` cannot notice a fabricated class being added back,
/// which is the regression this file has already had once.
#[test]
fn f17_1_registration_count_is_exact() {
    let mut r = NativeMethodRegistry::new();
    register_cds_natives(&mut r);
    assert_eq!(
        r.len(),
        12,
        "CDS registrar count changed. Expected 12: \
         ManagementFactoryHelper.<init> (1) + jdk/internal/misc/CDS (11). \
         If you added a registration, check it against `javap -p` on the \
         real JDK 25 image FIRST — `javap` with no flag, and the frozen \
         baselines, are public-only and cannot see a `private static \
         native`."
    );
}

// -----------------------------------------------------------------------
// Return-value correctness tests (via registry dispatch)
//
// For functions that return pure constants and require no heap access we
// can invoke the retrieved function pointer with a no-op stub context.
// To keep the tests self-contained we use a minimal inline stub rather
// than pulling in the full VM, consistent with the pattern used in
// jmx_tests and crypto_tests which test only `r.find(…).is_some()`.
// -----------------------------------------------------------------------

/// Verify every registered native can be located in the registry.
#[test]
fn test_all_registered_methods_findable() {
    let mut r = NativeMethodRegistry::new();
    register_cds_natives(&mut r);

    // F17-1 (2026-08-13): eleven rows removed here — the whole
    // `sun/management/CDSMetrics` and `sun/misc/VM` blocks,
    // `ManagementFactoryHelper.getCDSMetrics`,
    // `ClassLoader.getCdsArchivePath`, and `CDS.{isDumpingClassList,
    // isSharingEnabled}` — and three added. The removals are classes and
    // members `javap` cannot find on Microsoft 25.0.3+9-LTS; the transcripts
    // are in the tombstone above the handler functions. This list is now the
    // registrar's full contents, which is what
    // `f17_1_registration_count_is_exact` cross-checks it against.
    let expected: &[(&str, &str, &str)] = &[
        ("sun/management/ManagementFactoryHelper", "<init>", "()V"),
        ("jdk/internal/misc/CDS", "<init>", "()V"),
        ("jdk/internal/misc/CDS", "isDumpingArchive", "()Z"),
        ("jdk/internal/misc/CDS", "isUsingArchive", "()Z"),
        ("jdk/internal/misc/CDS", "getCDSConfigStatus", "()I"),
        (
            "jdk/internal/misc/CDS",
            "needsClassInitBarrier0",
            "(Ljava/lang/Class;)Z",
        ),
        (
            "jdk/internal/misc/CDS",
            "initializeFromArchive",
            "(Ljava/lang/Class;)V",
        ),
        ("jdk/internal/misc/CDS", "getRandomSeedForDumping", "()J"),
        (
            "jdk/internal/misc/CDS",
            "logLambdaFormInvoker",
            "(Ljava/lang/String;)V",
        ),
        (
            "jdk/internal/misc/CDS",
            "defineArchivedModules",
            "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
        ),
        (
            "jdk/internal/misc/CDS",
            "dumpClassList",
            "(Ljava/lang/String;)V",
        ),
        (
            "jdk/internal/misc/CDS",
            "dumpDynamicArchive",
            "(Ljava/lang/String;)V",
        ),
    ];

    for (cls, name, desc) in expected {
        assert!(
            r.find(cls, name, desc).is_some(),
            "Missing registration for {cls}.{name}{desc}"
        );
    }
    // F17-1: make the check two-sided. `is_some()` over a hand-written list
    // only ever proves the list is a SUBSET of what is registered — it is
    // structurally incapable of noticing an extra registration, which is
    // precisely how the eleven fabricated triples removed above sat here
    // being asserted-present rather than being questioned.
    assert_eq!(
        expected.len(),
        r.len(),
        "the registrar holds registrations this list does not name: {:?}",
        r.dump_registrations()
            .into_iter()
            .map(|(c, m, d, _)| (c, m, d))
            .filter(|t| !expected.contains(t))
            .collect::<Vec<_>>()
    );
}

// -----------------------------------------------------------------------
// CDS real class bytes round-trip test
// -----------------------------------------------------------------------

#[test]
fn test_cds_write_and_read_real_class_bytes() {
    // Write an archive with real (simulated) class bytes and read it back
    let class_bytes_a = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34, 0xFF, 0xAB];
    let class_bytes_b = vec![
        0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x3D, 0x01, 0x02, 0x03,
    ];

    let tmp_dir = std::env::temp_dir();
    let archive_path = tmp_dir.join("test_cds_real_bytes.jsa");
    let path_str = archive_path.to_str().unwrap().to_string();

    // Write archive
    let mut gen = CdsArchiveGenerator::new(&path_str);
    gen.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        class_bytes_a.clone(),
    ));
    gen.add_entry(CdsArchiveEntry::new(
        "java/lang/String",
        class_bytes_a.len() as u64,
        class_bytes_b.clone(),
    ));
    let header = gen.build_header();
    assert!(header.is_valid());
    assert_eq!(header.class_count, 2);
    assert!(gen.write_archive().is_ok());

    // Read it back via CdsArchiveLoader
    let mut loader = CdsArchiveLoader::new(&path_str);
    assert!(loader.try_load());
    assert!(loader.is_loaded());
    assert_eq!(loader.classes_loaded(), 2);

    // Manually read the file to verify bytes are preserved
    use std::io::Read;
    let mut file = std::fs::File::open(&path_str).unwrap();
    // Skip header (4 + 2 + 4 + 8 = 18 bytes)
    let mut header_buf = [0u8; 18];
    file.read_exact(&mut header_buf).unwrap();

    // Read first entry
    let mut len_buf = [0u8; 2];
    file.read_exact(&mut len_buf).unwrap();
    let name_len = u16::from_be_bytes(len_buf) as usize;
    let mut name_buf = vec![0u8; name_len];
    file.read_exact(&mut name_buf).unwrap();
    assert_eq!(String::from_utf8(name_buf).unwrap(), "java/lang/Object");

    let mut blen_buf = [0u8; 4];
    file.read_exact(&mut blen_buf).unwrap();
    let bytes_len = u32::from_be_bytes(blen_buf) as usize;
    let mut read_bytes = vec![0u8; bytes_len];
    file.read_exact(&mut read_bytes).unwrap();
    assert_eq!(
        read_bytes, class_bytes_a,
        "First entry class bytes must be preserved"
    );

    // Read second entry
    file.read_exact(&mut len_buf).unwrap();
    let name_len2 = u16::from_be_bytes(len_buf) as usize;
    let mut name_buf2 = vec![0u8; name_len2];
    file.read_exact(&mut name_buf2).unwrap();
    assert_eq!(String::from_utf8(name_buf2).unwrap(), "java/lang/String");

    file.read_exact(&mut blen_buf).unwrap();
    let bytes_len2 = u32::from_be_bytes(blen_buf) as usize;
    let mut read_bytes2 = vec![0u8; bytes_len2];
    file.read_exact(&mut read_bytes2).unwrap();
    assert_eq!(
        read_bytes2, class_bytes_b,
        "Second entry class bytes must be preserved"
    );

    // Cleanup
    let _ = std::fs::remove_file(&path_str);
}

// -----------------------------------------------------------------------
// Direct function invocation tests for zero-context handlers
//
// The handlers below only return a constant `Value`; they never call
// methods on the `NativeContext` trait, so we pass a dummy reference
// through a thin stub that satisfies the borrow checker.
// -----------------------------------------------------------------------

// =========================================================================
// TEST-ONLY MOCK: PanicContext
//
// This is a no-op NativeContext stub used solely for unit tests that call
// native handlers which never interact with the context. If a test
// unexpectedly invokes a context method, the panic message identifies
// exactly which method was called to aid debugging.
//
// This struct lives inside #[cfg(test)] and must NEVER be used in
// production code.
// =========================================================================
struct PanicContext;

impl cratonvm_native_api::NativeClassAccess for PanicContext {
    fn is_package_exported_unqualified(&self, _: &str, _: &str) -> bool {
        panic!("MockNativeContext: is_package_exported_unqualified not implemented for testing")
    }
    fn is_package_exported_to(&self, _: &str, _: &str, _: &str) -> bool {
        panic!("MockNativeContext: is_package_exported_to not implemented for testing")
    }
    fn is_package_open_unqualified(&self, _: &str, _: &str) -> bool {
        panic!("MockNativeContext: is_package_open_unqualified not implemented for testing")
    }
    fn is_package_open_to(&self, _: &str, _: &str, _: &str) -> bool {
        panic!("MockNativeContext: is_package_open_to not implemented for testing")
    }
    fn check_deep_reflection_access(
        &self,
        _: cratonvm_types::ClassId,
        _: cratonvm_types::ClassId,
    ) -> Result<(), String> {
        panic!("MockNativeContext: check_deep_reflection_access not implemented for testing")
    }
    fn load_class(&mut self, _: &str) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: load_class not implemented for testing")
    }
    fn class_name_of_id(&self, _: cratonvm_types::ClassId) -> Option<String> {
        panic!("MockNativeContext: class_name_of_id not implemented for testing")
    }
    fn class_id_of_object(&self, _: cratonvm_types::ObjectRef) -> cratonvm_types::ClassId {
        panic!("MockNativeContext: class_id_of_object not implemented for testing")
    }
    fn method_exists(&self, _: &str, _: &str, _: &str) -> bool {
        false
    }
    fn ensure_class_initialized(
        &mut self,
        _: &str,
    ) -> Result<cratonvm_types::ClassId, cratonvm_types::error::MethodCallFailed> {
        panic!("MockNativeContext: ensure_class_initialized not implemented for testing")
    }
    fn is_subclass(&self, _: cratonvm_types::ClassId, _: cratonvm_types::ClassId) -> bool {
        panic!("MockNativeContext: is_subclass not implemented for testing")
    }
    fn superclass_of(&self, _: cratonvm_types::ClassId) -> Option<cratonvm_types::ClassId> {
        panic!("MockNativeContext: superclass_of not implemented for testing")
    }
    fn class_id_by_name(&self, _: &str) -> Option<cratonvm_types::ClassId> {
        panic!("MockNativeContext: class_id_by_name not implemented for testing")
    }
    fn loader_id_of_class(&self, _: cratonvm_types::ClassId) -> i32 {
        2
    }
    fn is_record_class(&self, _: cratonvm_types::ClassId) -> bool {
        panic!("MockNativeContext: is_record_class not implemented for testing")
    }
    fn record_components(&self, _: cratonvm_types::ClassId) -> Vec<(String, String)> {
        panic!("MockNativeContext: record_components not implemented for testing")
    }
    fn is_sealed_class(&self, _: cratonvm_types::ClassId) -> bool {
        panic!("MockNativeContext: is_sealed_class not implemented for testing")
    }
    fn permitted_subclasses(&self, _: cratonvm_types::ClassId) -> Vec<String> {
        panic!("MockNativeContext: permitted_subclasses not implemented for testing")
    }
    fn declared_fields(
        &self,
        _: cratonvm_types::ClassId,
    ) -> Vec<cratonvm_native_api::FieldMetadata> {
        panic!("MockNativeContext: declared_fields not implemented for testing")
    }
    fn declared_methods(
        &self,
        _: cratonvm_types::ClassId,
    ) -> Vec<cratonvm_native_api::MethodMetadata> {
        panic!("MockNativeContext: declared_methods not implemented for testing")
    }
    fn class_interfaces(&self, _: cratonvm_types::ClassId) -> Vec<cratonvm_types::ClassId> {
        panic!("MockNativeContext: class_interfaces not implemented for testing")
    }
    fn class_access_flags(&self, _: cratonvm_types::ClassId) -> u16 {
        panic!("MockNativeContext: class_access_flags not implemented for testing")
    }
    fn primitive_class_mirror(&mut self, _: &str) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: primitive_class_mirror not implemented for testing")
    }
    fn class_annotations(
        &self,
        _: cratonvm_types::ClassId,
    ) -> Vec<cratonvm_native_api::AnnotationData> {
        panic!("MockNativeContext: class_annotations not implemented for testing")
    }
    fn method_annotations(
        &self,
        _: cratonvm_types::ClassId,
        _: &str,
        _: &str,
    ) -> Vec<cratonvm_native_api::AnnotationData> {
        panic!("MockNativeContext: method_annotations not implemented for testing")
    }
    fn field_annotations(
        &self,
        _: cratonvm_types::ClassId,
        _: &str,
    ) -> Vec<cratonvm_native_api::AnnotationData> {
        panic!("MockNativeContext: field_annotations not implemented for testing")
    }
    fn module_name_of_class(&self, _: cratonvm_types::ClassId) -> Option<String> {
        panic!("MockNativeContext: module_name_of_class not implemented for testing")
    }
    fn find_resource(&self, _: &str) -> Option<Vec<u8>> {
        panic!("MockNativeContext: find_resource not implemented for testing")
    }
    fn list_application_class_names(&self) -> Vec<String> {
        panic!("MockNativeContext: list_application_class_names not implemented for testing")
    }
    fn register_dynamic_classpath(&mut self, _: &[String]) {
        panic!("MockNativeContext: register_dynamic_classpath not implemented for testing")
    }
    fn define_class_from_bytes(
        &mut self,
        _: &str,
        _: &[u8],
    ) -> Option<cratonvm_types::ClassId> {
        panic!("MockNativeContext: define_class_from_bytes not implemented for testing")
    }
    fn define_class_with_loader(
        &mut self,
        _: &str,
        _: &[u8],
        _: u32,
    ) -> Option<cratonvm_types::ClassId> {
        panic!("MockNativeContext: define_class_with_loader not implemented for testing")
    }
    fn class_id_by_name_and_loader(&self, _: &str, _: u32) -> Option<cratonvm_types::ClassId> {
        panic!("MockNativeContext: class_id_by_name_and_loader not implemented for testing")
    }
    fn allocate_loader_id(&mut self) -> u32 {
        panic!("MockNativeContext: allocate_loader_id not implemented for testing")
    }
    fn method_parameter_annotations(
        &self,
        _: cratonvm_types::ClassId,
        _: &str,
        _: &str,
    ) -> Vec<Vec<cratonvm_native_api::AnnotationData>> {
        panic!("PanicContext::method_parameter_annotations not implemented for testing")
    }
    fn method_annotation_default(
        &self,
        _: cratonvm_types::ClassId,
        _: &str,
        _: &str,
    ) -> Option<cratonvm_native_api::AnnotationElementValue> {
        panic!("PanicContext::method_annotation_default not implemented for testing")
    }
    fn class_signature(&self, _: cratonvm_types::ClassId) -> Option<String> {
        None
    }
    fn method_signature(&self, _: cratonvm_types::ClassId, _: &str, _: &str) -> Option<String> {
        None
    }
    fn field_signature(&self, _: cratonvm_types::ClassId, _: &str) -> Option<String> {
        None
    }
}

impl cratonvm_native_api::NativeInvokeAccess for PanicContext {
    fn invoke(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: invoke not implemented for testing")
    }
    fn invoke_virtual(
        &mut self,
        _: cratonvm_types::ObjectRef,
        _: &str,
        _: &str,
        _: &[Value],
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: invoke_virtual not implemented for testing")
    }
}

impl cratonvm_native_api::NativeHeapAccess for PanicContext {
    fn resolve_field_index_by_class_id(
        &self,
        _: cratonvm_types::ClassId,
        _: &str,
    ) -> Option<usize> {
        panic!("MockNativeContext: resolve_field_index_by_class_id not implemented for testing")
    }
    fn new_object(&mut self, _: &str) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: new_object not implemented for testing")
    }
    fn identity_hash_code(&self, _: cratonvm_types::ObjectRef) -> i32 {
        panic!("MockNativeContext: identity_hash_code not implemented for testing")
    }
    fn get_field(&self, _: cratonvm_types::ObjectRef, _: usize) -> Value {
        panic!("MockNativeContext: get_field not implemented for testing")
    }
    fn set_field(&self, _: cratonvm_types::ObjectRef, _: usize, _: Value) {
        panic!("MockNativeContext: set_field not implemented for testing")
    }
    fn get_field_by_name(&self, _: cratonvm_types::ObjectRef, _: &str) -> Value {
        panic!("MockNativeContext: get_field_by_name not implemented for testing")
    }
    fn set_field_by_name(&self, _: cratonvm_types::ObjectRef, _: &str, _: Value) {
        panic!("MockNativeContext: set_field_by_name not implemented for testing")
    }
    fn resolve_field_index(&self, _: &str, _: &str) -> Option<usize> {
        panic!("MockNativeContext: resolve_field_index not implemented for testing")
    }
    fn new_array(
        &mut self,
        _: cratonvm_types::ArrayElementType,
        _: usize,
    ) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: new_array not implemented for testing")
    }
    fn new_ref_array(
        &mut self,
        _: cratonvm_types::ClassId,
        _: usize,
    ) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: new_ref_array not implemented for testing")
    }
    fn array_length(&self, _: cratonvm_types::ObjectRef) -> usize {
        panic!("MockNativeContext: array_length not implemented for testing")
    }
    fn get_array_element(&self, _: cratonvm_types::ObjectRef, _: usize) -> Value {
        panic!("MockNativeContext: get_array_element not implemented for testing")
    }
    fn set_array_element(&self, _: cratonvm_types::ObjectRef, _: usize, _: Value) {
        panic!("MockNativeContext: set_array_element not implemented for testing")
    }
    fn heap_kind_of(&self, _: cratonvm_types::ObjectRef) -> cratonvm_types::ObjectKind {
        panic!("MockNativeContext: heap_kind_of not implemented for testing")
    }
    fn heap_element_type_of(
        &self,
        _: cratonvm_types::ObjectRef,
    ) -> cratonvm_types::ArrayElementType {
        panic!("MockNativeContext: heap_element_type_of not implemented for testing")
    }
    fn create_string(&mut self, _: &str) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: create_string not implemented for testing")
    }
    fn read_string(&self, _: cratonvm_types::ObjectRef) -> Option<String> {
        panic!("MockNativeContext: read_string not implemented for testing")
    }
    fn get_class_mirror(&mut self, _: cratonvm_types::ClassId) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: get_class_mirror not implemented for testing")
    }
    fn alloc_object(
        &mut self,
        _: cratonvm_types::ClassId,
        _: usize,
    ) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: alloc_object not implemented for testing")
    }
    fn object_num_fields(&self, _: cratonvm_types::ObjectRef) -> usize {
        panic!("MockNativeContext: object_num_fields not implemented for testing")
    }
    fn get_field_volatile(&self, _: cratonvm_types::ObjectRef, _: usize) -> Value {
        panic!("MockNativeContext: get_field_volatile not implemented for testing")
    }
    fn set_field_volatile(&self, _: cratonvm_types::ObjectRef, _: usize, _: Value) {
        panic!("MockNativeContext: set_field_volatile not implemented for testing")
    }
    fn compare_and_swap_field(
        &mut self,
        _: cratonvm_types::ObjectRef,
        _: usize,
        _: Value,
        _: Value,
    ) -> bool {
        panic!("MockNativeContext: compare_and_swap_field not implemented for testing")
    }
    fn allocate_instance(&mut self, _: &str) -> Option<cratonvm_types::ObjectRef> {
        panic!("MockNativeContext: allocate_instance not implemented for testing")
    }
    fn discover_reference(
        &mut self,
        _: u8,
        _: cratonvm_types::ObjectRef,
        _: cratonvm_types::ObjectRef,
        _: Option<cratonvm_types::ObjectRef>,
    ) {
        panic!("MockNativeContext: discover_reference not implemented for testing")
    }
    fn heap_allocated_bytes(&self) -> usize {
        panic!("PanicContext::heap_allocated_bytes not implemented for testing")
    }
}

impl cratonvm_native_api::NativeThreadAccess for PanicContext {
    fn thread_id(&self) -> u64 {
        panic!("MockNativeContext: thread_id not implemented for testing")
    }
    fn monitor_wait(
        &mut self,
        _: cratonvm_types::ObjectRef,
        _: Option<u64>,
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: monitor_wait not implemented for testing")
    }
    fn monitor_notify(
        &mut self,
        _: cratonvm_types::ObjectRef,
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: monitor_notify not implemented for testing")
    }
    fn monitor_notify_all(
        &mut self,
        _: cratonvm_types::ObjectRef,
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: monitor_notify_all not implemented for testing")
    }
    fn thread_start(
        &mut self,
        _: cratonvm_types::ObjectRef,
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: thread_start not implemented for testing")
    }
    fn thread_join(
        &mut self,
        _: cratonvm_types::ObjectRef,
    ) -> cratonvm_types::error::MethodCallResult {
        panic!("MockNativeContext: thread_join not implemented for testing")
    }
    fn thread_is_alive(&self, _: cratonvm_types::ObjectRef) -> bool {
        panic!("MockNativeContext: thread_is_alive not implemented for testing")
    }
    fn current_thread_object(&mut self) -> cratonvm_types::ObjectRef {
        panic!("MockNativeContext: current_thread_object not implemented for testing")
    }
    fn thread_interrupt(&mut self, _: cratonvm_types::ObjectRef) {
        panic!("MockNativeContext: thread_interrupt not implemented for testing")
    }
    fn is_interrupted(&self, _: bool) -> bool {
        panic!("MockNativeContext: is_interrupted not implemented for testing")
    }
    fn park(&mut self, _: Option<std::time::Duration>) {
        panic!("MockNativeContext: park not implemented for testing")
    }
    fn unpark(&self, _: cratonvm_types::ObjectRef) {
        panic!("MockNativeContext: unpark not implemented for testing")
    }
    fn get_scoped_value(&self, _: u64) -> Option<Value> {
        panic!("MockNativeContext: get_scoped_value not implemented for testing")
    }
    fn push_scoped_value(&mut self, _: u64, _: Value) {
        panic!("MockNativeContext: push_scoped_value not implemented for testing")
    }
    fn pop_scoped_value(&mut self) {
        panic!("MockNativeContext: pop_scoped_value not implemented for testing")
    }
    fn scoped_value_depth(&self) -> usize {
        panic!("MockNativeContext: scoped_value_depth not implemented for testing")
    }
    fn monitor_enter(&mut self, _: cratonvm_types::ObjectRef) {
        panic!("MockNativeContext: monitor_enter not implemented for testing")
    }
    fn monitor_exit(&mut self, _: cratonvm_types::ObjectRef) {
        panic!("MockNativeContext: monitor_exit not implemented for testing")
    }
    fn active_thread_count(&self) -> i32 {
        panic!("PanicContext::active_thread_count not implemented for testing")
    }
    fn enumerate_threads(&self, _: usize) -> Vec<cratonvm_types::ObjectRef> {
        panic!("PanicContext::enumerate_threads not implemented for testing")
    }
}

impl cratonvm_native_api::NativeExceptionAccess for PanicContext {
    fn capture_stack_trace(&mut self) -> Vec<cratonvm_native_api::StackTraceEntry> {
        panic!("MockNativeContext: capture_stack_trace not implemented for testing")
    }
}

impl cratonvm_native_api::NativeGpuAccess for PanicContext {}

impl cratonvm_native_api::NativeSystemAccess for PanicContext {
    fn record_printed_value(&mut self, _: Value) {
        panic!("MockNativeContext: record_printed_value not implemented for testing")
    }
    fn record_printed_line(&mut self, _: String) {
        panic!("MockNativeContext: record_printed_line not implemented for testing")
    }
    fn get_system_stream(&self, _: &str) -> Option<cratonvm_types::ObjectRef> {
        panic!("MockNativeContext: get_system_stream not implemented for testing")
    }
    fn get_system_property(&self, _: &str) -> Option<String> {
        None
    }
    fn set_system_property(&mut self, _: &str, _: &str) -> Option<String> {
        panic!("MockNativeContext: set_system_property not implemented for testing")
    }
    fn is_interface_class(&self, _: cratonvm_types::ClassId) -> bool {
        panic!("MockNativeContext: is_interface_class not implemented for testing")
    }
    fn get_static_field(&self, _: cratonvm_types::ClassId, _: usize) -> Value {
        panic!("MockNativeContext: get_static_field not implemented for testing")
    }
    fn set_static_field(&mut self, _: cratonvm_types::ClassId, _: usize, _: Value) {
        panic!("MockNativeContext: set_static_field not implemented for testing")
    }
    fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
        panic!("MockNativeContext: fd_table not implemented for testing")
    }
    fn allocate_native_memory(&mut self, _: usize, _: usize) -> Option<(i64, *mut u8)> {
        panic!("MockNativeContext: allocate_native_memory not implemented for testing")
    }
    fn free_native_memory(&mut self, _: i64) {
        panic!("MockNativeContext: free_native_memory not implemented for testing")
    }
    fn load_native_library(
        &mut self,
        _: &str,
    ) -> Result<i64, cratonvm_types::error::MethodCallFailed> {
        panic!("MockNativeContext: load_native_library not implemented for testing")
    }
    fn find_native_symbol(&self, _: i64, _: &str) -> Option<usize> {
        panic!("MockNativeContext: find_native_symbol not implemented for testing")
    }
    fn register_upcall(&mut self, _: cratonvm_native_api::ffi::UpcallEntry) -> usize {
        panic!("MockNativeContext: register_upcall not implemented for testing")
    }
    fn get_upcall_info(&self, _: usize) -> Option<(cratonvm_types::ObjectRef, Vec<i32>, i32)> {
        panic!("MockNativeContext: get_upcall_info not implemented for testing")
    }
    fn loaded_class_count(&self) -> usize {
        panic!("PanicContext::loaded_class_count not implemented for testing")
    }
    fn gc_collection_count(&self) -> u64 {
        panic!("PanicContext::gc_collection_count not implemented for testing")
    }
    fn force_gc(&mut self) {}
}

// F17-1 (2026-08-13): `test_is_dumping_class_list_returns_zero` was deleted
// with the native it exercised. It is worth naming what that test was
// actually measuring: `isDumpingClassList` is not a member of JDK 25's
// `jdk.internal.misc.CDS` at any access level, so a green assertion here
// reported only that a Rust function returned the constant it was written to
// return — the invented Java method it was reachable through never appeared
// in the test at all. A unit test on a native's BODY cannot see that the
// native's TRIPLE is fabricated; only the registration list can.

#[test]
fn test_is_dumping_archive_returns_zero() {
    let result = native_cds_is_dumping_archive(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), Some(Value::Int(0)));
}

/// F17-1: was `test_is_sharing_enabled_returns_zero`. Same body, JDK-true
/// name — `isUsingArchive` is what JDK 25 calls this predicate.
#[test]
fn test_is_using_archive_returns_zero() {
    let result = native_cds_is_using_archive(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), Some(Value::Int(0)));
}

/// F17-1: `getCDSConfigStatus()I` must answer 0 — every bit in the word
/// (`IS_DUMPING_ARCHIVE`, `IS_DUMPING_METHOD_HANDLES`,
/// `IS_DUMPING_STATIC_ARCHIVE`, `IS_LOGGING_LAMBDA_FORM_INVOKERS`,
/// `IS_USING_ARCHIVE`) describes something CratonVM does not do.
///
/// This one is not cosmetic: JDK 25's `CDS.<clinit>` is
/// `configStatus = getCDSConfigStatus()`, so any non-zero answer here would
/// silently switch on a code path — e.g. `isLoggingLambdaFormInvokers()`
/// going true routes `logSpeciesType` and the 4-arg `logLambdaFormInvoker`
/// into a native that discards its input.
#[test]
fn f17_1_cds_config_status_is_zero() {
    let result = native_cds_get_config_status(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), Some(Value::Int(0)));
}

/// F17-1: `needsClassInitBarrier0(Class)Z` must answer false — the barrier
/// orders initialisation of classes reached through an archived heap
/// subgraph, and CratonVM maps none.
#[test]
fn f17_1_needs_class_init_barrier_is_false() {
    let result = native_cds_needs_class_init_barrier0(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), Some(Value::Int(0)));
}

#[test]
fn test_initialize_from_archive_returns_void() {
    // The descriptor is `(Ljava/lang/Class;)V`. Handing back an operand for
    // a void method leaves it on the stack — the caller emits no pop.
    let result = native_cds_initialize_from_archive(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), None);
}

#[test]
fn test_random_seed_for_dumping_is_zero_outside_dump() {
    // Must be 0 when not dumping a static archive, which CratonVM never
    // does. `ImmutableCollections` seeds SALT from this value and falls
    // back to `System.nanoTime()` only on 0; a fixed non-zero seed made
    // `Set.of` / `Map.of` iteration order identical on every run, hiding
    // exactly the order-dependence bugs the randomisation exists to expose.
    let result = native_cds_get_random_seed_for_dumping(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), Some(Value::Long(0)));
}

// F17-1 (2026-08-13): `test_vm_is_booted_returns_one` was deleted with
// `native_vm_is_booted`. `sun.misc.VM` is not in the JDK 25 image
// (`javap -p sun.misc.VM` → class not found). The equivalent that DOES exist,
// `jdk/internal/misc/VM.isBooted()Z`, is registered and tested by `lib.rs`
// (registration at lib.rs:14894) — a second copy here would have been a
// last-write-wins coin flip decided by registrar call order.

#[test]
fn test_log_lambda_form_invoker_is_noop() {
    let result = native_cds_log_lambda_form_invoker(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), None);
}

#[test]
fn test_define_archived_modules_is_noop() {
    let result = native_cds_define_archived_modules(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), None);
}

#[test]
fn test_dump_class_list_is_noop() {
    let result = native_cds_dump_class_list(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), None);
}

#[test]
fn test_dump_dynamic_archive_is_noop() {
    let result = native_cds_dump_dynamic_archive(&mut PanicContext, &[]);
    assert_eq!(result.unwrap(), None);
}

#[test]
fn test_checksum_is_sha256_based() {
    let mut gen1 = CdsArchiveGenerator::new("/tmp/test1.jsa");
    gen1.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE],
    ));
    let h1 = gen1.build_header();

    let mut gen2 = CdsArchiveGenerator::new("/tmp/test2.jsa");
    gen2.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE],
    ));
    let h2 = gen2.build_header();

    // Same entries should produce same checksum
    assert_eq!(h1.checksum, h2.checksum);
    assert_ne!(h1.checksum, 0);
}

#[test]
fn test_checksum_changes_with_different_data() {
    let mut gen1 = CdsArchiveGenerator::new("/tmp/test1.jsa");
    gen1.add_entry(CdsArchiveEntry::new(
        "java/lang/Object",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE],
    ));
    let h1 = gen1.build_header();

    let mut gen2 = CdsArchiveGenerator::new("/tmp/test2.jsa");
    gen2.add_entry(CdsArchiveEntry::new(
        "java/lang/String",
        0,
        vec![0xCA, 0xFE, 0xBA, 0xBE],
    ));
    let h2 = gen2.build_header();

    assert_ne!(h1.checksum, h2.checksum);
}
