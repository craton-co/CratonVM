use super::resolved_flags;
#[test]
fn configured_string_and_double_facts_are_shared_rows() {
    let mut config = crate::config::VmConfig::default();
    config.heap_dump_path = Some("gcj output".into());
    config.jvm_args = vec!["-XX:G1ConcMarkStepDurationMillis=2.5".into(),
        "-XX:CompileCommandFile=".into(), "-XX:HeapDumpPath=gcj output".into()];
    let rows = resolved_flags(&config);
    let row = |name| rows.iter().find(|row| row.name == name).unwrap();
    assert_eq!((row("G1ConcMarkStepDurationMillis").ty,
        row("G1ConcMarkStepDurationMillis").value.as_str(),
        row("G1ConcMarkStepDurationMillis").origin),
        ("double", "2.500000", "{command line}"));
    assert_eq!((row("CompileCommandFile").value.as_str(), row("CompileCommandFile").origin),
        ("", "{command line}"));
    assert_eq!(row("HeapDumpPath").value, "gcj output");
}
