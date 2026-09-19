// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JCK failure capture and JSON report generation.
//!
//! Provides structured recording of JCK test outcomes. Each failure is captured
//! with its test name, module, error classification, message, and optional stack
//! trace. The accumulated report is serialized to JSON for consumption by CI
//! dashboards and regression tracking.
//!
//! The canonical output file is `bench/jck-failures.json`.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single JCK test failure record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JckFailure {
    /// Fully qualified test class/method name (e.g. "api/java_lang/String/EqualsTest").
    pub test_name: String,
    /// Module or API area (e.g. "api/java_lang").
    pub module: String,
    /// Classification of the error (e.g. "ClassNotFound", "RuntimeError", "NativeMethodNotFound").
    pub error_type: String,
    /// Human-readable error message.
    pub message: String,
    /// Stack trace at point of failure (may be empty).
    pub stack_trace: String,
}

/// Aggregate JCK test report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JckReport {
    /// Unix timestamp (seconds since epoch) when the report was generated.
    pub timestamp: u64,
    /// Total number of tests executed.
    pub total: usize,
    /// Number of tests that passed.
    pub passed: usize,
    /// Number of tests that failed.
    pub failed: usize,
    /// Detailed failure records.
    pub failures: Vec<JckFailure>,
}

impl JckReport {
    /// Create a new empty report with the current timestamp.
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            timestamp,
            total: 0,
            passed: 0,
            failed: 0,
            failures: Vec::new(),
        }
    }

    /// Record a passing test (increments total and passed counters).
    pub fn record_pass(&mut self) {
        self.total += 1;
        self.passed += 1;
    }

    /// Capture a test failure and add it to the report.
    pub fn capture_failure(
        &mut self,
        test_name: String,
        module: String,
        error_type: String,
        message: String,
        stack_trace: String,
    ) {
        self.total += 1;
        self.failed += 1;
        self.failures.push(JckFailure {
            test_name,
            module,
            error_type,
            message,
            stack_trace,
        });
    }

    /// Serialize the report to a pretty-printed JSON string.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|e| format!("{{\"error\": \"serialization failed: {e}\"}}"))
    }

    /// Write the report to the specified file path as JSON.
    ///
    /// Creates parent directories if they do not exist. Returns an I/O error
    /// if the write fails.
    pub fn write_report(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = self.to_json();
        std::fs::write(path, json)
    }
}

impl Default for JckReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: capture a single failure into a one-entry report and return it.
///
/// Useful for ad-hoc capture outside a running harness loop.
pub fn capture_failure(
    test_name: String,
    module: String,
    error_type: String,
    message: String,
    stack_trace: String,
) -> JckFailure {
    JckFailure {
        test_name,
        module,
        error_type,
        message,
        stack_trace,
    }
}

/// Write a report to the given path. Shorthand for `report.write_report(path)`.
pub fn write_report(report: &JckReport, path: &Path) -> std::io::Result<()> {
    report.write_report(path)
}

#[cfg(test)]
mod tests {
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
}
