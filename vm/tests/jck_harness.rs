// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JCK harness — discovers and runs JCK test classes through the CratonVM.
//!
//! This test is `#[ignore]` by default because it requires a valid JCK
//! installation pointed to by the `JCK_HOME` environment variable. Run it
//! explicitly with:
//!
//!   cargo test --test jck_harness -- --ignored
//!
//! The harness walks the JCK test tree, finds compiled test classes, invokes
//! each one through the VM, and collects pass/fail results. Failures are
//! written to `bench/jck-failures.json` via the `jck_capture` module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

// ---------------------------------------------------------------------------
// JCK discovery
// ---------------------------------------------------------------------------

/// Resolve the JCK home directory from the environment.
fn jck_home() -> Option<PathBuf> {
    std::env::var("JCK_HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Recursively discover `.class` files under the given directory.
fn discover_classes(root: &Path, base: &Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            discover_classes(&path, base, out);
        } else if path.extension().map_or(false, |e| e == "class") {
            // Convert file path to JVM class name (forward slashes, no .class).
            if let Ok(rel) = path.strip_prefix(base) {
                let class_name = rel.with_extension("").to_string_lossy().replace('\\', "/");
                // Skip inner classes (contain '$') — only top-level test classes.
                if !class_name.contains('$') {
                    out.push(class_name);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test runner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TestResult {
    class_name: String,
    module: String,
    passed: bool,
    error_type: String,
    message: String,
}

/// Derive a module name from a JCK class path.
/// e.g. "api/java_lang/String/EqualsTest" -> "api/java_lang"
fn module_from_class(class_name: &str) -> String {
    let parts: Vec<&str> = class_name.split('/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        class_name.to_string()
    }
}

/// Run a single JCK test class through the VM.
/// JCK tests follow the convention of having a `public static void main(String[])`
/// method. A successful test exits normally (return code 0); a failing test
/// throws an exception or calls `System.exit(non-zero)`.
fn run_jck_test(classpath: &str, class_name: &str) -> TestResult {
    let module = module_from_class(class_name);

    let config = VmConfig::new().with_classpath(vec![classpath.to_string()]);
    let mut vm = Vm::new(config);

    // JCK tests use main(String[]) as entry point.
    match vm.invoke(
        class_name,
        "main",
        "([Ljava/lang/String;)V",
        &[Value::Object(None)],
    ) {
        Ok(_) => TestResult {
            class_name: class_name.to_string(),
            module,
            passed: true,
            error_type: String::new(),
            message: String::new(),
        },
        Err(e) => {
            let err_str = format!("{e:?}");
            let error_type = if err_str.contains("ClassNotFound") {
                "ClassNotFound"
            } else if err_str.contains("NoSuchMethod") {
                "NoSuchMethod"
            } else if err_str.contains("NativeMethod") {
                "NativeMethodNotFound"
            } else {
                "RuntimeError"
            };
            TestResult {
                class_name: class_name.to_string(),
                module,
                passed: false,
                error_type: error_type.to_string(),
                message: err_str,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON report generation (manual serialization, serde-free for this test crate)
// ---------------------------------------------------------------------------

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn results_to_json(results: &[TestResult]) -> String {
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();
    let failed = total - passed;

    let failures: Vec<String> = results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| {
            // Truncate message to 500 chars to keep the JSON manageable.
            let msg = if r.message.len() > 500 {
                format!("{}...", &r.message[..500])
            } else {
                r.message.clone()
            };
            format!(
                concat!(
                    "    {{\n",
                    "      \"test_name\": \"{}\",\n",
                    "      \"module\": \"{}\",\n",
                    "      \"error_type\": \"{}\",\n",
                    "      \"message\": \"{}\",\n",
                    "      \"stack_trace\": \"\"\n",
                    "    }}"
                ),
                escape_json(&r.class_name),
                escape_json(&r.module),
                escape_json(&r.error_type),
                escape_json(&msg),
            )
        })
        .collect();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    format!(
        concat!(
            "{{\n",
            "  \"timestamp\": {},\n",
            "  \"total\": {},\n",
            "  \"passed\": {},\n",
            "  \"failed\": {},\n",
            "  \"failures\": [\n",
            "{}\n",
            "  ]\n",
            "}}\n"
        ),
        timestamp,
        total,
        passed,
        failed,
        failures.join(",\n"),
    )
}

fn write_report(results: &[TestResult]) {
    let json = results_to_json(results);
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let report_path = format!("{manifest_dir}/../bench/jck-failures.json");
    if let Err(e) = std::fs::write(&report_path, &json) {
        eprintln!("[jck-harness] failed to write report to {report_path}: {e}");
    } else {
        eprintln!("[jck-harness] wrote failure report to {report_path}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The main JCK harness test. Discovers and runs all JCK test classes.
#[test]
#[ignore]
fn jck_harness_full_run() {
    let jck_root = match jck_home() {
        Some(p) => p,
        None => {
            eprintln!("[jck-harness] JCK_HOME not set or not a directory; skipping.");
            return;
        }
    };

    let tests_dir = jck_root.join("tests");
    if !tests_dir.is_dir() {
        eprintln!(
            "[jck-harness] {}/tests not found; skipping.",
            jck_root.display()
        );
        return;
    }

    // Discover test classes under the api/ subdirectories we care about.
    let target_modules = [
        "api/java_lang",
        "api/java_util",
        "api/java_io",
        "api/java_nio",
        "api/java_net",
        "api/java_security",
        "api/java_math",
    ];

    let mut class_names = Vec::new();
    for module in &target_modules {
        let module_dir = tests_dir.join(module);
        if module_dir.is_dir() {
            discover_classes(&module_dir, &tests_dir, &mut class_names);
        }
    }
    class_names.sort();

    eprintln!(
        "[jck-harness] discovered {} test classes",
        class_names.len()
    );
    if class_names.is_empty() {
        eprintln!("[jck-harness] no test classes found; check JCK_HOME/tests layout.");
        return;
    }

    // Run each test class.
    let classpath = tests_dir.to_string_lossy().to_string();
    let mut results = Vec::new();
    for class_name in &class_names {
        let result = run_jck_test(&classpath, class_name);
        if !result.passed {
            eprintln!(
                "[jck-harness] FAIL {} [{}]: {}",
                result.class_name,
                result.error_type,
                if result.message.len() > 120 {
                    format!("{}...", &result.message[..120])
                } else {
                    result.message.clone()
                }
            );
        }
        results.push(result);
    }

    // Summary.
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();
    let failed = total - passed;

    // Group by module.
    let mut by_module: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in &results {
        let entry = by_module.entry(r.module.clone()).or_default();
        if r.passed {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }

    eprintln!();
    eprintln!("=== JCK Harness Summary ===");
    for (module, (p, f)) in &by_module {
        eprintln!("  {:<30} pass={:>5}  fail={:>5}", module, p, f);
    }
    eprintln!("  {:<30} pass={:>5}  fail={:>5}", "TOTAL", passed, failed);
    eprintln!();

    // Write failure report.
    write_report(&results);

    // The harness does not assert pass rates — it captures the current state.
    // Regression gates are enforced by jck_conformance.rs and CI thresholds.
    eprintln!(
        "[jck-harness] complete: {passed}/{total} passed ({:.1}%)",
        if total > 0 {
            passed as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    );
}
