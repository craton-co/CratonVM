use super::*;


fn tripwire_body() {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    assert!(!c.failed, "the test compiler starts clean");
    c.emit_load_local(RAX, 8);
    c.emit_store_local(16, RAX);
    assert!(!c.failed, "a real frame slot is not refused");
    c.emit_load_local(RAX, 0);
    assert!(c.failed, "a load from [rbp - 0] must refuse the compile");
    assert_eq!(
        c.failed_site.map(|(site, _, _)| site),
        Some("singlepass-codegen/frame-slot-offset-not-below-rbp")
    );

    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    c.emit_store_local(-8, RAX);
    assert!(c.failed, "a store above rbp must refuse the compile");
}
