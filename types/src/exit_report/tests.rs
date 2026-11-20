use super::*;

#[test]
fn a_sink_line_is_appended_whole() {
    let dir = std::env::temp_dir().join(format!(
        "cratonvm-exit-report-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("report.txt");
    let _ = std::fs::remove_file(&path);
    emit_line_to(Some(&path), "[cratonvm] one");
    emit_line_to(Some(&path), "[cratonvm] two");
    let text = std::fs::read_to_string(&path).expect("sink written");
    assert_eq!(text, "[cratonvm] one\n[cratonvm] two\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_macro_spellings_expand() {
    // Both arms compile and evaluate their condition. The `or` arm's own
    // switch is evaluated even when the report is off.
    let mut asked = 0;
    crate::exit_report!(or { asked += 1; false }; "[cratonvm] x {}", 1);
    assert_eq!(asked, 1);
    crate::exit_report!("[cratonvm] y {}", 2);
}
