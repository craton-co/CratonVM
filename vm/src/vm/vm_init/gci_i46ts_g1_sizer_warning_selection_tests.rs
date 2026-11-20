use super::g1_young_sizer_warning_lines as lines;

#[test]
fn ts46_sizer_warnings_follow_the_stdout_gc_ergo_warning_selection() {
    let w = vec!["-XX:NewSize and -XX:MaxNewSize override -XX:NewRatio".to_string()];
    // gc+ergo at warning turned off on stdout (no output part = stdout).
    for off in [
        "all=off",
        "gc+ergo=off",
        "all=error",
        "gc+ergo=error",
        "ALL=OFF",
        " all=off ",
        "all=off:stdout",
        "gc+ergo=off:stdout",
        "gc;all=off",
    ] {
        assert!(lines(Some(off), 0.004, &w).is_empty(), "{off}");
    }
    // Another output (`:file=`, `:stderr`) is configured; stdout keeps
    // `all=warning`. And selections that leave gc+ergo warnings on.
    for on in [
        "all=off:file=gc.log",
        "gc+ergo=off:file=gc.log",
        "all=error:file=gc.log::filecount=0",
        "all=off:stderr",
        "gc+ergo=off:stderr",
        "all=warning",
        "gc+ergo=warning",
        "all=info",
        "gc=off",
    ] {
        let want = format!("[0.004s][warning][gc,ergo] {}", w[0]);
        assert_eq!(lines(Some(on), 0.004, &w), vec![want], "{on}");
    }
}
