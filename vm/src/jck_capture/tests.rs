use super::*;

#[test]
fn empty_report_serializes() {
    let report = JckReport {
        timestamp: 1713200000,
        total: 0,
        passed: 0,
        failed: 0,
        failures: vec![],
    };
    let json = report.to_json();
    assert!(json.contains("\"total\": 0"));
    assert!(json.contains("\"failures\": []"));
}

#[test]
fn report_with_failure_serializes() {
    let mut report = JckReport::new();
    report.record_pass();
    report.capture_failure(
        "api/java_lang/String/EqualsTest".into(),
        "api/java_lang".into(),
        "RuntimeError".into(),
        "assertion failed".into(),
        "at EqualsTest.main(EqualsTest.java:10)".into(),
    );
    let json = report.to_json();
    assert!(json.contains("\"total\": 2"));
    assert!(json.contains("\"passed\": 1"));
    assert!(json.contains("\"failed\": 1"));
    assert!(json.contains("EqualsTest"));
}

#[test]
fn capture_failure_creates_record() {
    let f = capture_failure(
        "TestFoo".into(),
        "api/java_lang".into(),
        "ClassNotFound".into(),
        "missing".into(),
        String::new(),
    );
    assert_eq!(f.test_name, "TestFoo");
    assert_eq!(f.error_type, "ClassNotFound");
}
