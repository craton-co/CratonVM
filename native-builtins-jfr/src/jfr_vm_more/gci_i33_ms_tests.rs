use super::metaspace_summary_spaces;

#[test]
fn gci_i33ms_metaspace_summary_splits_the_class_space() {
    const K: i64 = 1024;
    let g = 1024 * K * K;
    let [metaspace, data, class] =
        metaspace_summary_spaces(5_000, Some([320 * K, 1088 * K * K, 1_200, 128 * K, g]));
    assert_eq!(metaspace, (320 * K, 5_000, 1088 * K * K));
    assert_eq!(data, (192 * K, 3_800, 64 * K * K));
    assert_eq!(class, (128 * K, 1_200, g));
    // The no-class-space shape: the pre-i33 rows.
    assert_eq!(
        metaspace_summary_spaces(5_000, None),
        [(5_000, 5_000, 5_000), (5_000, 5_000, 5_000), (0, 0, 0)]
    );
}
