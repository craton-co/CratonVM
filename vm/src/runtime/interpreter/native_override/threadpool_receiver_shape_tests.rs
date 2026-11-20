/// The `vm` crate's source root, for the scan below.
fn vm_src(rel: &str) -> String {
    // `rel` is repo-relative (`vm/src/...`) so the paths read the way an
    // `rg` invocation would; strip the crate prefix to get a path under
    // this crate's manifest dir.
    let under_crate = rel
        .strip_prefix("vm/")
        .expect("scan paths are vm-crate paths");
    let path = format!("{}/{under_crate}", env!("CARGO_MANIFEST_DIR"));
    let path = std::path::Path::new(&path);
    if path.is_dir() {
        return std::fs::read_dir(path)
            .expect("dispatch source directory")
            .map(|entry| entry.expect("dispatch source entry").path())
            .filter(|entry| entry.extension().is_some_and(|ext| ext == "rs"))
            .map(|entry| std::fs::read_to_string(entry).expect("dispatch source"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The four files that carried the eight `ThreadPoolExecutor.execute`
/// receiver-shape probes, retired 2026-08-06.
const FORMER_SITE_FILES: &[&str] = &[
    "vm/src/vm/vm_exec",
    "vm/src/runtime/interpreter/invoke.rs",
    "vm/src/runtime/interpreter/native_override.rs",
    "vm/src/runtime/interpreter/dispatch_virtual.rs",
];

/// **Zero** dispatch sites probe the receiver's shape, and none of them
/// re-inlines the `workers` lookup by hand either.
///
/// This replaces `exactly_eight_dispatch_sites_probe_the_threadpool_receiver_shape`
/// and `no_hand_inlined_workers_probe_outside_the_helper` (2026-08-04 →
/// 2026-08-06), which pinned the count at eight so that a *partial* sweep
/// would fail. The sweep happened; what can regress now is a copy growing
/// back, one site at a time, the way the original eight did — each for a
/// real observed failure, each locally reasonable.
///
/// If you are here because this test failed: a receiver-shape probe is the
/// wrong fix. The class-scoped arbitration
/// (`real_protected_stub_class_common` + `NativeKind::SyntheticStub` on
/// `native_es_execute`) is only correct while every
/// `ThreadPoolExecutor`-tagged object on a real-JDK image is genuinely
/// real. If something reintroduced a synthetic-layout producer, fix THAT —
/// make it drive `initialize_real_thread_pool_executor` — rather than
/// teaching one dispatch path to tell the two apart again.
#[test]
fn every_threadpool_receiver_shape_site_is_gone() {
    // Assembled at runtime so this scanner's own source does not contain
    // the strings it looks for.
    let probe_call = format!("{}(", "threadpool_executor_has_real_workers");
    let hand_inlined = format!(
        r#"{}(recv_class_id, "workers""#,
        "resolve_field_index_in_hierarchy"
    );

    for file in FORMER_SITE_FILES {
        let src = vm_src(file);
        assert!(
            !src.contains(probe_call.as_str()),
            "{file} calls the deleted ThreadPoolExecutor receiver-shape probe again. \
             All eight sites and the probe itself were removed on 2026-08-06 together \
             with the receiver-blind ninth arm in `force_native_over_real_jdk_bytecode`; \
             re-adding one alone recreates the cold-path/warm-path split the census \
             constant existed to prevent."
        );
        assert!(
            !src.contains(hand_inlined.as_str()),
            "{file} hand-inlines the ThreadPoolExecutor `workers` probe. That is how \
             the predicate came to have three implementations, two of which took a \
             plain `read()` where a nested `read_recursive()` is required."
        );
    }
}

/// The class-scoped replacement is actually in place.
///
/// Two independent facts, and the pair is the whole fix: without the
/// allow-list entry the retagged native would win over real bytecode for
/// every receiver (the recursion the eight probes prevented); without the
/// retag the allow-list entry is inert, because the arbitration only fires
/// for `NativeKind::SyntheticStub`.
///
/// The `NativeKind` half is pinned in
/// `the native-registry census`, which builds the real boot
/// registry; this half pins the allow-list, which is `vm`'s.
#[test]
fn threadpool_executor_is_real_protected() {
    assert!(
        super::real_protected_stub_class("java/util/concurrent/ThreadPoolExecutor"),
        "`java/util/concurrent/ThreadPoolExecutor` left the real-protected-stub \
         allow-list. `native_es_execute` is tagged `SyntheticStub`, so without this \
         entry it wins over the real `execute()` body for every receiver — including \
         the async worker pool a native calls `.execute()` on, which recurses into \
         the same native and aborts the process rather than throwing."
    );
}

/// The scan corpus above is non-empty and every path in it is real.
///
/// `every_threadpool_receiver_shape_site_is_gone` is a loop over
/// `FORMER_SITE_FILES` asserting an absence. **A loop over nothing asserts
/// nothing** — rename or move one of those four files and the gate keeps
/// passing while scanning three, or zero, and the regression it exists to
/// catch walks straight through the file it stopped reading.
///
/// That is the same failure as the count this module's predecessor froze at
/// eight silently becoming zero, which is why the count gate was replaced
/// rather than emptied. An absence-gate needs its corpus pinned for exactly
/// the reason a presence-gate needs its number pinned.
///
/// `vm_src` already panics on an unreadable path, so this adds the two
/// things it cannot check: that the list is not empty, and that a path
/// resolving to something implausibly small (a stub left behind by a move)
/// is not silently accepted as "no matches found".
#[test]
fn the_receiver_shape_scan_set_is_real() {
    assert!(
        !FORMER_SITE_FILES.is_empty(),
        "FORMER_SITE_FILES is empty, so `every_threadpool_receiver_shape_site_is_gone` \
         scans nothing and cannot fail"
    );
    for file in FORMER_SITE_FILES {
        let src = vm_src(file);
        assert!(
            src.len() > 10_000,
            "{file} is {} bytes — far smaller than any of the four dispatch files that \
             carried the receiver-shape probes. The scan is reading the wrong path, and \
             the gate above is passing vacuously over it.",
            src.len()
        );
    }
}
