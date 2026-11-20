use super::*;

#[test]
fn a_null_stream_reads_as_null_only_after_its_own_setter_ran() {
    let a = SharedVm::new(crate::config::VmConfig::default());
    let b = SharedVm::new(crate::config::VmConfig::default());
    let all = [
        SystemStreamField::Out,
        SystemStreamField::Err,
        SystemStreamField::In,
    ];
    for f in all {
        assert!(!f.null_reads_as_null(&a), "{f:?} before any setter");
    }
    assert_eq!(
        SystemStreamField::from_field_name("out"),
        Some(SystemStreamField::Out)
    );
    SystemStreamField::Out.mark_written(&a);
    assert!(SystemStreamField::Out.null_reads_as_null(&a));
    assert!(!SystemStreamField::Err.null_reads_as_null(&a));
    assert!(!SystemStreamField::In.null_reads_as_null(&a));
    for f in all {
        assert!(!f.null_reads_as_null(&b), "{f:?} leaked into another VM");
    }
    assert_eq!(SystemStreamField::from_field_name("lineSeparator"), None);
}
