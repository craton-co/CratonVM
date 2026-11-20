use super::g1_sizer_warnings_wait_for_the_logger as wait;

#[test]
fn gci_i47w_xl47_sizer_warnings_wait_for_the_logger_under_xlog() {
    assert!(!wait(None));
    assert!(wait(Some("gc=info:stdout:uptimemillis,tags")));
    assert!(wait(Some("gc+ergo=warning:stdout:level,tags")));
    assert!(wait(Some("gc:file=gc.log")));
    // Needles split in two so this test's own text never matches them.
    let src = concat!(include_str!("../vm_init/mod.rs"), "\n", include_str!("../vm_init/missing_native_entry.rs"), "\n", include_str!("../vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../vm_init/wait_site.rs"), "\n", include_str!("../vm_init/class_store_cut_with.rs")).replace("\r\n", "\n");
    let at = |a: &str, b: &str| {
        let needle = [a, b].concat();
        src.find(needle.as_str())
            .unwrap_or_else(|| panic!("missing: {needle}"))
    };
    let held = at("g1_sizer_warnings_for_the_logger", " = warnings;");
    let init = at("unified_logging::init_unified_logging", "(spec)");
    let write = at("unified_logging::log_on_default_stdout", "(");
    let startup = at("unified_logging::log_gc_startup", "_with_errors(");
    assert!(held < init && init < write && write < startup);
}
