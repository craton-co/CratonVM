use super::g1_young_sizer_warning_lines as lines;

#[test]
fn gci_i43gy_sizer_warnings_print_in_hotspots_default_output_shape() {
    let w = vec!["-XX:NewSize and -XX:MaxNewSize override -XX:NewRatio".to_string()];
    assert_eq!(
        lines(None, 0.004, &w),
        vec!["[0.004s][warning][gc,ergo] -XX:NewSize and -XX:MaxNewSize override -XX:NewRatio"
            .to_string()]
    );
    assert_eq!(lines(Some("gc*"), 0.0125, &w).len(), 1);
    assert!(lines(Some("gc;disable"), 0.004, &w).is_empty());
    assert!(lines(Some(" DISABLE "), 0.004, &w).is_empty());
    assert!(lines(None, 0.004, &[]).is_empty());
}
