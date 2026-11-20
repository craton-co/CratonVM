use super::*;

// --- Registry tests ---

#[test]
fn test_registry_starts_empty() {
    let r = TckRegistry::new();
    assert_eq!(r.count(), 0);
}

#[test]
fn test_registry_register_single() {
    let mut r = TckRegistry::new();
    r.register(TckTest::passing(
        "test.one",
        TckCategory::Lang,
        "Test.java",
        vec!["tag1"],
    ));
    assert_eq!(r.count(), 1);
}

#[test]
fn test_registry_with_standard_tests_has_30_plus() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.count() >= 30, "Expected >=30 tests, got {}", r.count());
}

#[test]
fn test_registry_find_by_name_existing() {
    let r = TckRegistry::with_standard_tests();
    let found = r.find_by_name("lang.String.immutability");
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "lang.String.immutability");
}

#[test]
fn test_registry_find_by_name_missing() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("does.not.exist").is_none());
}

#[test]
fn test_registry_find_by_category_lang() {
    let r = TckRegistry::with_standard_tests();
    let lang_tests = r.find_by_category(TckCategory::Lang);
    assert!(!lang_tests.is_empty());
    for t in &lang_tests {
        assert_eq!(t.category, TckCategory::Lang);
    }
}

#[test]
fn test_registry_find_by_category_vm() {
    let r = TckRegistry::with_standard_tests();
    let vm_tests = r.find_by_category(TckCategory::Vm);
    assert!(!vm_tests.is_empty());
}

#[test]
fn test_registry_find_by_category_util() {
    let r = TckRegistry::with_standard_tests();
    let util_tests = r.find_by_category(TckCategory::Util);
    assert!(!util_tests.is_empty());
}

#[test]
fn test_registry_find_by_tag_returns_matches() {
    let r = TckRegistry::with_standard_tests();
    let tagged = r.find_by_tag("overflow");
    assert!(!tagged.is_empty());
    for t in &tagged {
        assert!(t.tags.contains(&"overflow".to_string()));
    }
}

#[test]
fn test_registry_find_by_tag_no_match() {
    let r = TckRegistry::with_standard_tests();
    let tagged = r.find_by_tag("nonexistent-tag-xyz");
    assert!(tagged.is_empty());
}

#[test]
fn test_registry_find_by_tag_jvm_semantics() {
    let r = TckRegistry::with_standard_tests();
    let tagged = r.find_by_tag("jvm-semantics");
    assert!(tagged.len() >= 5);
}

#[test]
fn test_registry_multiple_categories_populated() {
    let r = TckRegistry::with_standard_tests();
    assert!(!r.find_by_category(TckCategory::IO).is_empty());
    assert!(!r.find_by_category(TckCategory::Reflect).is_empty());
    assert!(!r.find_by_category(TckCategory::Vm).is_empty());
}

// --- TckTest construction ---

#[test]
fn test_tck_test_new_fields() {
    let t = TckTest::new(
        "my.test",
        TckCategory::Math,
        "path/To.java",
        TckExpectedResult::Pass,
        3000,
        vec!["a".to_string(), "b".to_string()],
    );
    assert_eq!(t.name, "my.test");
    assert_eq!(t.timeout_ms, 3000);
    assert_eq!(t.tags.len(), 2);
    assert_eq!(t.expected_result, TckExpectedResult::Pass);
}

#[test]
fn test_tck_test_passing_defaults() {
    let t = TckTest::passing("x", TckCategory::Net, "X.java", vec!["net"]);
    assert_eq!(t.timeout_ms, 5000);
    assert_eq!(t.expected_result, TckExpectedResult::Pass);
}

// --- TckTestResult helpers ---

#[test]
fn test_result_passed_is_pass() {
    let t = TckTest::passing("a", TckCategory::Lang, "A.java", vec![]);
    let r = TckTestResult::passed(t, 10);
    assert!(r.is_pass());
    assert!(!r.is_failure());
    assert!(!r.is_skipped());
}

#[test]
fn test_result_skipped_is_skipped() {
    let t = TckTest::passing("b", TckCategory::Lang, "B.java", vec![]);
    let r = TckTestResult::skipped(t, "no reason".to_string());
    assert!(r.is_skipped());
    assert!(!r.is_pass());
    assert!(!r.is_failure());
}

#[test]
fn test_result_failed_is_failure() {
    let t = TckTest::passing("c", TckCategory::Vm, "C.java", vec![]);
    let r = TckTestResult {
        test: t,
        actual_result: TckActualResult::Failed("assertion".to_string()),
        execution_time_ms: 5,
        error_message: None,
        stack_trace: None,
    };
    assert!(r.is_failure());
    assert!(!r.is_pass());
}

#[test]
fn test_result_error_is_failure() {
    let t = TckTest::passing("d", TckCategory::Vm, "D.java", vec![]);
    let r = TckTestResult {
        test: t,
        actual_result: TckActualResult::Error("oom".to_string()),
        execution_time_ms: 1,
        error_message: None,
        stack_trace: None,
    };
    assert!(r.is_failure());    }

#[test]
fn test_result_timed_out_is_failure() {
    let t = TckTest::passing("e", TckCategory::Concurrent, "E.java", vec![]);
    let r = TckTestResult {
        test: t,
        actual_result: TckActualResult::TimedOut,
        execution_time_ms: 5000,
        error_message: None,
        stack_trace: None,
    };
    assert!(r.is_failure());    }

// --- Executor simulation ---

#[test]
fn test_executor_simulate_pass() {
    let reg = TckRegistry::with_standard_tests();
    let exec = TckExecutor::new(&reg);
    let test = TckTest::passing("sim.test", TckCategory::Vm, "Sim.java", vec![]);
    let result = exec.simulate_test(&test);
    assert!(result.is_pass());
}

#[test]
fn test_executor_simulate_excluded_skipped() {
    let reg = TckRegistry::with_standard_tests();
    let config = TckExecutorConfig {
        exclusion_list: vec!["excluded.test".to_string()],
        ..Default::default()
    };
    let exec = TckExecutor::with_config(&reg, config);
    let test = TckTest::passing("excluded.test", TckCategory::Vm, "Exc.java", vec![]);
    let result = exec.simulate_test(&test);
    assert!(result.is_skipped());
}

#[test]
fn test_executor_simulate_skip_expected() {
    let reg = TckRegistry::with_standard_tests();
    let exec = TckExecutor::new(&reg);
    let test = TckTest::new(
        "skip.test",
        TckCategory::Vm,
        "Skip.java",
        TckExpectedResult::Skip("not ready".to_string()),
        5000,
        vec![],
    );
    let result = exec.simulate_test(&test);
    assert!(result.is_skipped());
}

#[test]
fn test_executor_run_all_returns_report() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let report = exec.run_all();
    assert_eq!(report.total, reg.count());
    assert_eq!(report.passed, report.total); // all pass in simulation
    assert_eq!(report.failed, 0);
    assert_eq!(report.skipped, 0);
}

#[test]
fn test_executor_run_category() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let report = exec.run_category(TckCategory::Lang);
    let expected_count = reg.find_by_category(TckCategory::Lang).len();
    assert_eq!(report.total, expected_count);
    assert_eq!(report.passed, expected_count);
}

#[test]
fn test_executor_run_single_found() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let result = exec.run_single("vm.floatNanComparison");
    assert!(result.is_some());
    assert!(result.unwrap().is_pass());
}

#[test]
fn test_executor_run_single_missing() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    assert!(exec.run_single("no.such.test").is_none());
}

#[test]
fn test_executor_results_accumulated() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    exec.run_single("vm.integerOverflowWraps");
    exec.run_single("vm.longArithmetic");
    assert_eq!(exec.results.len(), 2);
}

// --- TckRunReport ---

#[test]
fn test_report_pass_rate_all_pass() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let report = exec.run_all();
    assert!((report.pass_rate - 1.0).abs() < f64::EPSILON);
}

#[test]
fn test_report_pass_rate_empty() {
    let results: Vec<TckTestResult> = vec![];
    let report = TckRunReport::build(results, 0);
    assert_eq!(report.pass_rate, 0.0);
}

#[test]
fn test_report_format_summary_contains_key_fields() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let report = exec.run_all();
    let summary = report.format_summary();
    assert!(summary.contains("TCK Results:"));
    assert!(summary.contains("passed"));
    assert!(summary.contains("Failed:"));
    assert!(summary.contains("Skipped:"));
    assert!(summary.contains("Errors:"));
    assert!(summary.contains("Total time:"));
    assert!(summary.contains("ms"));
}

#[test]
fn test_report_format_summary_100_percent() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let report = exec.run_all();
    let summary = report.format_summary();
    assert!(summary.contains("100.0%"));
}

#[test]
fn test_report_failures_empty_when_all_pass() {
    let reg = TckRegistry::with_standard_tests();
    let mut exec = TckExecutor::new(&reg);
    let report = exec.run_all();
    assert!(report.failures.is_empty());
}

#[test]
fn test_report_failures_counted() {
    // Build a report manually with one failure.
    let t = TckTest::passing("fail.test", TckCategory::Vm, "Fail.java", vec![]);
    let fail_result = TckTestResult {
        test: t,
        actual_result: TckActualResult::Failed("bad".to_string()),
        execution_time_ms: 1,
        error_message: Some("bad".to_string()),
        stack_trace: None,
    };
    let report = TckRunReport::build(vec![fail_result], 1);
    assert_eq!(report.failed, 1);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.passed, 0);
}

// --- Exclusion list ---

#[test]
fn test_exclusion_list_starts_empty() {
    let el = TckExclusionList::new();
    assert_eq!(el.count(), 0);
}

#[test]
fn test_exclusion_list_add() {
    let mut el = TckExclusionList::new();
    el.add("foo.test", "reason", None);
    assert_eq!(el.count(), 1);
}

#[test]
fn test_exclusion_list_is_excluded_true() {
    let mut el = TckExclusionList::new();
    el.add("foo.test", "reason", Some("BUG-123"));
    assert!(el.is_excluded("foo.test"));
}

#[test]
fn test_exclusion_list_is_excluded_false() {
    let el = TckExclusionList::new();
    assert!(!el.is_excluded("bar.test"));
}

#[test]
fn test_exclusion_list_remove_existing() {
    let mut el = TckExclusionList::new();
    el.add("foo.test", "reason", None);
    assert_eq!(el.count(), 1);
    let removed = el.remove("foo.test");
    assert!(removed);
    assert_eq!(el.count(), 0);
    assert!(!el.is_excluded("foo.test"));
}

#[test]
fn test_exclusion_list_remove_nonexistent() {
    let mut el = TckExclusionList::new();
    let removed = el.remove("nonexistent");
    assert!(!removed);
}

#[test]
fn test_exclusion_list_multiple_entries() {
    let mut el = TckExclusionList::new();
    el.add("a", "r1", None);
    el.add("b", "r2", Some("BUG-1"));
    el.add("c", "r3", None);
    assert_eq!(el.count(), 3);
    el.remove("b");
    assert_eq!(el.count(), 2);
    assert!(!el.is_excluded("b"));
    assert!(el.is_excluded("a"));
    assert!(el.is_excluded("c"));
}

#[test]
fn test_exclusion_entry_has_bug_id() {
    let mut el = TckExclusionList::new();
    el.add("x.test", "known issue", Some("JDK-123456"));
    let entry = el.entries.iter().find(|e| e.test_name == "x.test").unwrap();
    assert_eq!(entry.bug_id, Some("JDK-123456".to_string()));
}

// --- CompatibilityChecker ---

#[test]
fn test_checker_null_pointer_semantics_pass() {
    let c = CompatibilityChecker::new();
    assert_eq!(c.check_null_pointer_semantics(), CheckResult::Pass);
}

#[test]
fn test_checker_integer_overflow_pass() {
    let c = CompatibilityChecker::new();
    assert_eq!(c.check_integer_overflow(), CheckResult::Pass);
}

#[test]
fn test_checker_float_nan_pass() {
    let c = CompatibilityChecker::new();
    assert_eq!(c.check_float_nan_comparison(), CheckResult::Pass);
}

#[test]
fn test_checker_string_interning_not_implemented() {
    let c = CompatibilityChecker::new();
    assert!(matches!(
        c.check_string_interning(),
        CheckResult::NotImplemented(_)
    ));
}

#[test]
fn test_checker_class_init_not_implemented() {
    let c = CompatibilityChecker::new();
    assert!(matches!(
        c.check_class_initialization_order(),
        CheckResult::NotImplemented(_)
    ));
}

#[test]
fn test_checker_exception_handling_pass() {
    let c = CompatibilityChecker::new();
    assert_eq!(c.check_exception_handling(), CheckResult::Pass);
}

#[test]
fn test_checker_interface_default_methods_pass() {
    let c = CompatibilityChecker::new();
    assert_eq!(c.check_interface_default_methods(), CheckResult::Pass);
}

#[test]
fn test_checker_run_all_checks_returns_full_set() {
    // The run_all_checks set is grown incrementally as new conformance
    // areas are added (originally 7, expanded in S48 with I/O checks).
    // The exact count is whatever the Vec literal in run_all_checks
    // currently contains; we just assert it matches the number of
    // distinct names returned, which catches accidental duplicates.
    let c = CompatibilityChecker::new();
    let results = c.run_all_checks();
    let names: std::collections::HashSet<&str> =
        results.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names.len(),
        results.len(),
        "run_all_checks must not contain duplicate check names"
    );
    assert!(
        results.len() >= 7,
        "run_all_checks must include at least the original 7 checks; got {}",
        results.len()
    );
}

#[test]
fn test_checker_run_all_checks_no_failures() {
    let c = CompatibilityChecker::new();
    for (name, result) in c.run_all_checks() {
        assert!(
            !matches!(result, CheckResult::Fail(_)),
            "Check '{}' returned Fail",
            name
        );
    }
}

#[test]
fn test_checker_run_all_checks_names_present() {
    let c = CompatibilityChecker::new();
    let names: Vec<String> = c.run_all_checks().into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&"null_pointer_semantics".to_string()));
    assert!(names.contains(&"float_nan_comparison".to_string()));
    assert!(names.contains(&"integer_overflow".to_string()));
}

// --- JEP Compliance Matrix ---

#[test]
fn test_jep_matrix_starts_empty() {
    let m = JepComplianceMatrix::new();
    assert_eq!(m.entries.len(), 0);
}

#[test]
fn test_jep_matrix_with_jdk25_has_13_entries() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    assert_eq!(m.entries.len(), 13);
}

#[test]
fn test_jep_matrix_compliant_count() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    // JEP 502, 506, 510, 511, 512, 513, 519, 484 = 8
    assert_eq!(m.compliant_count(), 8);
}

#[test]
fn test_jep_matrix_partial_count() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    // JEP 505, 507, 508, 496, 497 = 5
    assert_eq!(m.partial_count(), 5);
}

#[test]
fn test_jep_matrix_not_compliant_count_zero() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    assert_eq!(m.not_compliant_count(), 0);
}

#[test]
fn test_jep_matrix_add_entry() {
    let mut m = JepComplianceMatrix::new();
    m.add(999, "Test JEP", ComplianceStatus::Compliant, "note");
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.compliant_count(), 1);
}

#[test]
fn test_jep_matrix_generate_report_contains_header() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    let report = m.generate_report();
    assert!(report.contains("JEP Compliance Matrix"));
    assert!(report.contains("Summary:"));
}

#[test]
fn test_jep_matrix_generate_report_contains_jep_numbers() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    let report = m.generate_report();
    assert!(report.contains("502"));
    assert!(report.contains("505"));
    assert!(report.contains("519"));
    assert!(report.contains("484"));
    assert!(report.contains("497"));
}

#[test]
fn test_jep_matrix_generate_report_shows_compliant() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    let report = m.generate_report();
    assert!(report.contains("COMPLIANT"));
}

#[test]
fn test_jep_matrix_generate_report_shows_partial() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    let report = m.generate_report();
    assert!(report.contains("PARTIAL"));
}

#[test]
fn test_jep_matrix_generate_report_summary_counts() {
    let m = JepComplianceMatrix::with_jdk25_jeps();
    let report = m.generate_report();
    assert!(report.contains("8 Compliant"));
    assert!(report.contains("5 Partial"));
}

#[test]
fn test_compliance_status_label() {
    assert_eq!(ComplianceStatus::Compliant.label(), "COMPLIANT");
    assert_eq!(
        ComplianceStatus::Partial("x".to_string()).label(),
        "PARTIAL"
    );
    assert_eq!(
        ComplianceStatus::NotCompliant("x".to_string()).label(),
        "NOT_COMPLIANT"
    );
    assert_eq!(ComplianceStatus::NA.label(), "N/A");
}

// --- TckCategory ---

#[test]
fn test_category_as_str() {
    assert_eq!(TckCategory::Lang.as_str(), "java.lang");
    assert_eq!(TckCategory::Util.as_str(), "java.util");
    assert_eq!(TckCategory::IO.as_str(), "java.io");
    assert_eq!(TckCategory::Vm.as_str(), "jvm.semantics");
    assert_eq!(TckCategory::Reflect.as_str(), "java.lang.reflect");
}

// -----------------------------------------------------------------------
// S48 — TCK java.io / java.nio Tests
// -----------------------------------------------------------------------

#[test]
fn s48_registry_has_io_tests() {
    let r = TckRegistry::with_standard_tests();
    let io_tests = r.find_by_category(TckCategory::IO);
    // 3 original + 30 S48 additions = 33
    assert!(
        io_tests.len() >= 33,
        "Expected >=33 IO tests, got {}",
        io_tests.len()
    );
}

#[test]
fn s48_registry_has_nio_tests() {
    let r = TckRegistry::with_standard_tests();
    let nio_tests = r.find_by_category(TckCategory::Nio);
    assert!(
        nio_tests.len() >= 25,
        "Expected >=25 NIO tests, got {}",
        nio_tests.len()
    );
}

#[test]
fn s48_io_tests_have_correct_tags() {
    let r = TckRegistry::with_standard_tests();
    // File tests should have "file" tag
    let file_tests = r.find_by_tag("file");
    assert!(
        file_tests.len() >= 6,
        "Expected >=6 file-tagged tests, got {}",
        file_tests.len()
    );

    // Buffer tests should have "nio" tag
    let nio_tagged = r.find_by_tag("nio");
    assert!(
        nio_tagged.len() >= 20,
        "Expected >=20 nio-tagged tests, got {}",
        nio_tagged.len()
    );
}

#[test]
fn s48_io_file_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("io.File.createDeleteExists").is_some());
    assert!(r.find_by_name("io.File.isFileIsDirectory").is_some());
    assert!(r.find_by_name("io.File.lengthAndLastModified").is_some());
    assert!(r.find_by_name("io.File.mkdirListFiles").is_some());
    assert!(r.find_by_name("io.File.renameTo").is_some());
    assert!(r.find_by_name("io.File.absoluteCanonicalPaths").is_some());
}

#[test]
fn s48_io_stream_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r
        .find_by_name("io.FileInputStream.readSingleByte")
        .is_some());
    assert!(r.find_by_name("io.FileInputStream.readBulk").is_some());
    assert!(r
        .find_by_name("io.FileInputStream.availableAndSkip")
        .is_some());
    assert!(r
        .find_by_name("io.FileInputStream.closeIdempotent")
        .is_some());
    assert!(r
        .find_by_name("io.FileOutputStream.writeSingleByte")
        .is_some());
    assert!(r.find_by_name("io.FileOutputStream.writeBulk").is_some());
    assert!(r.find_by_name("io.FileOutputStream.appendMode").is_some());
}

#[test]
fn s48_io_buffered_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("io.BufferedReader.readLine").is_some());
    assert!(r.find_by_name("io.BufferedWriter.writeFlush").is_some());
    assert!(r.find_by_name("io.BufferedInputStream.markReset").is_some());
}

#[test]
fn s48_io_data_stream_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r
        .find_by_name("io.DataOutputStream.writePrimitives")
        .is_some());
    assert!(r.find_by_name("io.DataInputStream.readUTF").is_some());
}

#[test]
fn s48_io_reader_writer_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("io.StringReader.readCharArray").is_some());
    assert!(r.find_by_name("io.StringWriter.getBuffer").is_some());
    assert!(r.find_by_name("io.CharArrayReader.readMarkReset").is_some());
    assert!(r.find_by_name("io.CharArrayWriter.writeTo").is_some());
}

#[test]
fn s48_io_special_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r
        .find_by_name("io.RandomAccessFile.seekReadWrite")
        .is_some());
    assert!(r.find_by_name("io.PipedStreams.producerConsumer").is_some());
    assert!(r
        .find_by_name("io.InputStreamReader.charsetDecoding")
        .is_some());
    assert!(r.find_by_name("io.LineNumberReader.lineTracking").is_some());
}

#[test]
fn s48_io_hierarchy_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("io.InputStream.hierarchy").is_some());
    assert!(r.find_by_name("io.OutputStream.hierarchy").is_some());
    assert!(r.find_by_name("io.Closeable.autoClose").is_some());
}

#[test]
fn s48_io_scanner_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("io.Scanner.nextIntNextLine").is_some());
    assert!(r.find_by_name("io.Scanner.delimiterPattern").is_some());
}

#[test]
fn s48_nio_bytebuffer_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r
        .find_by_name("nio.ByteBuffer.allocateAndCapacity")
        .is_some());
    assert!(r.find_by_name("nio.ByteBuffer.putGetFlip").is_some());
    assert!(r.find_by_name("nio.ByteBuffer.wrapArray").is_some());
    assert!(r.find_by_name("nio.ByteBuffer.markReset").is_some());
    assert!(r.find_by_name("nio.ByteBuffer.sliceDuplicate").is_some());
    assert!(r.find_by_name("nio.ByteBuffer.typedAccess").is_some());
    assert!(r.find_by_name("nio.ByteBuffer.compactAndClear").is_some());
    assert!(r.find_by_name("nio.ByteBuffer.readOnlyView").is_some());
}

#[test]
fn s48_nio_typed_buffer_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("nio.CharBuffer.allocateAndAppend").is_some());
    assert!(r.find_by_name("nio.CharBuffer.wrapCharSequence").is_some());
    assert!(r.find_by_name("nio.IntBuffer.bulkPutGet").is_some());
    assert!(r.find_by_name("nio.LongBuffer.allocateAndAccess").is_some());
    assert!(r.find_by_name("nio.FloatBuffer.putGetCompare").is_some());
    assert!(r.find_by_name("nio.DoubleBuffer.wrapAndSlice").is_some());
    assert!(r
        .find_by_name("nio.ShortBuffer.positionLimitFlip")
        .is_some());
    assert!(r.find_by_name("nio.Buffer.invariants").is_some());
}

#[test]
fn s48_nio_channel_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("nio.FileChannel.readWrite").is_some());
    assert!(r.find_by_name("nio.FileChannel.positionAndSize").is_some());
    assert!(r.find_by_name("nio.FileChannel.transferToFrom").is_some());
    assert!(r.find_by_name("nio.FileLock.lockAndRelease").is_some());
}

#[test]
fn s48_nio_files_path_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("nio.files.Path.resolveNormalize").is_some());
    assert!(r
        .find_by_name("nio.files.Files.createDeleteExists")
        .is_some());
    assert!(r
        .find_by_name("nio.files.Files.readWriteAllBytes")
        .is_some());
    assert!(r.find_by_name("nio.files.Files.walkCopyMove").is_some());
}

#[test]
fn s48_nio_selector_datagram_tests_registered() {
    let r = TckRegistry::with_standard_tests();
    assert!(r.find_by_name("nio.Selector.openAndClose").is_some());
    assert!(r
        .find_by_name("nio.DatagramChannel.openBindClose")
        .is_some());
}

#[test]
fn s48_total_test_count_increased() {
    let r = TckRegistry::with_standard_tests();
    // 30 original + 30 IO + 25 NIO = 85+
    assert!(
        r.count() >= 85,
        "Expected >=85 total tests, got {}",
        r.count()
    );
}

#[test]
fn s48_executor_runs_io_category() {
    let r = TckRegistry::with_standard_tests();
    let mut executor = TckExecutor::new(&r);
    let report = executor.run_category(TckCategory::IO);
    assert!(report.total >= 33);
    assert_eq!(
        report.pass_rate, 1.0,
        "All IO tests should pass in simulation"
    );
}

#[test]
fn s48_executor_runs_nio_category() {
    let r = TckRegistry::with_standard_tests();
    let mut executor = TckExecutor::new(&r);
    let report = executor.run_category(TckCategory::Nio);
    assert!(report.total >= 25);
    assert_eq!(
        report.pass_rate, 1.0,
        "All NIO tests should pass in simulation"
    );
}

#[test]
fn s48_compatibility_checker_io_checks_pass() {
    let checker = CompatibilityChecker::new();
    assert_eq!(checker.check_file_separator_semantics(), CheckResult::Pass);
    assert_eq!(checker.check_byte_stream_round_trip(), CheckResult::Pass);
    assert_eq!(checker.check_data_stream_byte_order(), CheckResult::Pass);
    assert_eq!(checker.check_buffer_invariants(), CheckResult::Pass);
    assert_eq!(checker.check_buffer_initial_state(), CheckResult::Pass);
    assert_eq!(checker.check_eof_semantics(), CheckResult::Pass);
    assert_eq!(checker.check_close_idempotent(), CheckResult::Pass);
}

#[test]
fn s48_compatibility_checker_path_resolve() {
    let checker = CompatibilityChecker::new();
    assert_eq!(checker.check_path_resolve_semantics(), CheckResult::Pass);
}

#[test]
fn s48_compatibility_checker_total_check_count() {
    let checker = CompatibilityChecker::new();
    let results = checker.run_all_checks();
    // 7 original + 8 S48 = 15
    assert_eq!(
        results.len(),
        15,
        "Expected 15 checks, got {}",
        results.len()
    );
    let pass_count = results
        .iter()
        .filter(|(_, r)| *r == CheckResult::Pass)
        .count();
    assert!(pass_count >= 13, "Expected >=13 passes, got {}", pass_count);
}

// --- IoNioConformanceSuite tests ---

#[test]
fn s48_conformance_suite_runs_all() {
    let mut suite = IoNioConformanceSuite::new();
    let results = suite.run_all();
    assert!(!results.is_empty());
    let (pass, total) = suite.summary();
    assert!(
        total >= 70,
        "Expected >=70 conformance checks, got {}",
        total
    );
    assert_eq!(pass, total, "All conformance checks should pass");
}

#[test]
fn s48_conformance_suite_format_summary() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let summary = suite.format_summary();
    assert!(summary.contains("I/O & NIO Conformance"));
    assert!(summary.contains("100.0%"));
}

#[test]
fn s48_conformance_io_hierarchy_checks() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let hierarchy_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("io.hierarchy"))
        .collect();
    assert!(
        hierarchy_checks.len() >= 15,
        "Expected >=15 hierarchy checks, got {}",
        hierarchy_checks.len()
    );
    for (name, result) in &hierarchy_checks {
        assert_eq!(
            *result,
            IoConformanceResult::Pass,
            "Hierarchy check {} should pass",
            name
        );
    }
}

#[test]
fn s48_conformance_nio_buffer_checks() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let buffer_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("nio.buffer"))
        .collect();
    assert!(
        buffer_checks.len() >= 20,
        "Expected >=20 buffer checks, got {}",
        buffer_checks.len()
    );
}

#[test]
fn s48_conformance_data_stream_encoding() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let encoding_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("io.data_stream"))
        .collect();
    assert!(
        encoding_checks.len() >= 6,
        "Expected >=6 encoding checks, got {}",
        encoding_checks.len()
    );
    for (name, result) in &encoding_checks {
        assert_eq!(
            *result,
            IoConformanceResult::Pass,
            "Encoding check {} should pass",
            name
        );
    }
}

#[test]
fn s48_conformance_file_operations() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let file_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("io.file"))
        .collect();
    assert!(
        file_checks.len() >= 10,
        "Expected >=10 file operation checks, got {}",
        file_checks.len()
    );
    for (name, result) in &file_checks {
        assert_eq!(
            *result,
            IoConformanceResult::Pass,
            "File check {} should pass",
            name
        );
    }
}

#[test]
fn s48_conformance_path_operations() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let path_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("nio.path"))
        .collect();
    assert!(
        path_checks.len() >= 5,
        "Expected >=5 path checks, got {}",
        path_checks.len()
    );
}

#[test]
fn s48_conformance_channel_operations() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let channel_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("nio.channel"))
        .collect();
    assert!(
        channel_checks.len() >= 4,
        "Expected >=4 channel checks, got {}",
        channel_checks.len()
    );
}

#[test]
fn s48_conformance_fd_semantics() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let fd_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("io.fd"))
        .collect();
    assert_eq!(fd_checks.len(), 5);
}

#[test]
fn s48_conformance_stream_chaining() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let chain_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("io.chain"))
        .collect();
    assert_eq!(chain_checks.len(), 4);
}

#[test]
fn s48_conformance_scanner() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let scanner_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.starts_with("io.scanner"))
        .collect();
    assert_eq!(scanner_checks.len(), 4);
}

#[test]
fn s48_conformance_typed_buffer_element_sizes() {
    let mut suite = IoNioConformanceSuite::new();
    suite.run_all();
    let size_checks: Vec<_> = suite
        .results
        .iter()
        .filter(|(name, _)| name.contains("element_size"))
        .collect();
    assert_eq!(size_checks.len(), 7);
    for (name, result) in &size_checks {
        assert_eq!(
            *result,
            IoConformanceResult::Pass,
            "{} element size should be correct",
            name
        );
    }
}

#[test]
fn s48_conformance_default_constructor() {
    let suite = IoNioConformanceSuite::default();
    assert!(suite.results.is_empty());
}
