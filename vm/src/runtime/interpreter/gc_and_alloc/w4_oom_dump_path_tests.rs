use super::heap_dump_path_for;

/// gen r4w4/oom: HotSpot's `-XX:HeapDumpPath` rule.
#[test]
fn heap_dump_path_follows_hotspots_rule() {
    assert_eq!(heap_dump_path_for(None, 42, false), "java_pid42.hprof");
    assert_eq!(heap_dump_path_for(None, 42, true), "java_pid42.hprof.gz", "zt t28/mf");
    assert_eq!(
        heap_dump_path_for(Some("no-such-dir-w4-oom/x.hprof"), 42, true),
        "no-such-dir-w4-oom/x.hprof",
        "a path that is not an existing directory is the file itself",
    );
    let dir = std::env::temp_dir();
    let got = heap_dump_path_for(dir.to_str(), 42, false);
    assert_eq!(
        std::path::PathBuf::from(got),
        dir.join("java_pid42.hprof"),
        "an existing directory gets java_pid<pid>.hprof inside it",
    );
}
