use super::*;
#[test]
fn effective_heap_size_skips_operands_and_program_arguments() {
    let scan = |args: &[&str]| {
        early_max_heap(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .map(|(size, _)| size)
    };
    assert_eq!(
        scan(&["cvm", "-Xmx1m", "-Xmx64m", "-version"]),
        Some(64 << 20)
    );
    assert_eq!(
        scan(&["cvm", "-XX:MaxHeapSize=1m", "-version"]),
        Some(1 << 20)
    );
    assert_eq!(scan(&["cvm", "-cp", "-Xmx1m", "-version"]), None);
    assert_eq!(scan(&["cvm", "Main", "--", "-Xmx1m"]), None);
    assert_eq!(scan(&["cvm", "--Xmx", "1m", "Main"]), Some(1 << 20));
    assert_eq!(scan(&["cvm", "-Xmx0", "-version"]), Some(0));
    assert_eq!(scan(&["cvm", "-XX:MaxHeapSize=0k", "-version"]), Some(0));
    assert_eq!(parse_max_heap_size("0kk"), None);
}
