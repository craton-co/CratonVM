use super::*;

#[test]
fn g1_concurrent_flags_follow_parallel_and_retain_zero_ergonomics() {
    let mut config = crate::config::VmConfig::default();
    config.gc_algorithm = crate::config::GcAlgorithm::G1;
    config.g1_parallel_gc_threads = Some(2);
    config.jvm_args = vec!["-XX:+UseG1GC".into(), "-XX:ParallelGCThreads=2".into()];
    let row = resolved_flags(&config).into_iter().find(|row| row.name == "ConcGCThreads").unwrap();
    assert_eq!((row.value.as_str(), row.origin), ("1", "{ergonomic}"));
    config.zgc_conc_gc_threads = Some(0);
    config.jvm_args.push("-XX:ConcGCThreads=0".into());
    let row = resolved_flags(&config).into_iter().find(|row| row.name == "ConcGCThreads").unwrap();
    assert_eq!((row.value.as_str(), row.origin), ("1", "{command line, ergonomic}"));
    assert_eq!((row.jmm_origin(), row.jmx_origin(), row.jfr_origin()), (6, "ERGONOMIC", "Ergonomic"));
    config.zgc_conc_gc_threads = Some(3);
    *config.jvm_args.last_mut().unwrap() = "-XX:ConcGCThreads=3".into();
    let row = resolved_flags(&config).into_iter().find(|row| row.name == "ConcGCThreads").unwrap();
    assert_eq!((row.value.as_str(), row.origin), ("3", "{command line}"));
}
#[test]
fn zgc_default_age_follows_captured_heap_and_worker_budgets() {
    const MIB: usize = 1024 * 1024;
    for (heap, workers, expected) in [
        (64, 1, 8), (64, 2, 4), (128, 1, 6), (128, 2, 4),
        (256, 1, 7), (256, 2, 6), (512, 1, 8), (512, 2, 7),
        (512, 5, 5), (1024, 1, 8), (1024, 2, 8), (1024, 8, 6),
        (2048, 1, 15), (2048, 2, 15), (2048, 8, 11),
    ] {
        assert_eq!(zgc_default_max_tenuring_threshold(heap * MIB, workers), expected,
            "heap={heap} MiB workers={workers}");
    }
}

#[cfg(feature = "zgc")]
#[test]
fn zgc_derived_maximum_preserves_initial_age_and_explicit_clamps() {
    let mut config = crate::config::VmConfig::default();
    config.gc_algorithm = crate::config::GcAlgorithm::Zgc;
    config.max_heap_size = 256 * 1024 * 1024;
    config.zgc_conc_gc_threads = Some(1);
    let get = |config: &crate::config::VmConfig, name| resolved_flags(config)
        .into_iter().find(|row| row.name == name).unwrap();
    assert_eq!(get(&config, "MaxTenuringThreshold").value, "7");
    config.zgc_conc_gc_threads = Some(2);
    assert_eq!(get(&config, "MaxTenuringThreshold").value, "6");
    assert_eq!(get(&config, "InitialTenuringThreshold").value, "7");
    config.tenuring.max_tenuring_threshold = Some(4);
    let row = get(&config, "InitialTenuringThreshold");
    assert_eq!((row.value.as_str(), row.origin), ("4", "{ergonomic}"));
}

