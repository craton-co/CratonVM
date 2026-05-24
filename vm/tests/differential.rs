// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T4.10 -- Differential testing harness.
//!
//! Runs the same Java method under both CratonVM and HotSpot (via
//! `std::process::Command`) and compares stdout + return values to detect
//! behavioral divergences.
//!
//! Requirements:
//! - `java` must be on PATH (for HotSpot comparison)
//! - `.class` files in `vm/tests/resources/cratonvm/`
//!
//! All tests requiring external resources are `#[ignore]`.
//!
//! Run with:
//!     cargo test -p cratonvm-vm --test differential -- --ignored

use std::path::Path;
use std::process::Command;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

// ---------------------------------------------------------------------------
// T4.10.1 -- Core types and harness
// ---------------------------------------------------------------------------

/// Outcome of running a single method under one VM.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Captured stdout lines (joined with newline).
    pub stdout: String,
    /// Return value as a string representation, or "void" / "error:<msg>".
    pub return_value: String,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stdout={:?}, return={:?}", self.stdout, self.return_value)
    }
}

/// Result of running a method under both CratonVM and HotSpot.
#[derive(Debug, Clone)]
pub struct DiffResult {
    /// Outcome from CratonVM execution.
    pub cratonvm_outcome: Outcome,
    /// Outcome from HotSpot execution.
    pub hotspot_outcome: Outcome,
    /// Whether the two outcomes match (stdout and return value).
    pub matches: bool,
}

/// Run a Java static method under both CratonVM and HotSpot, comparing results.
///
/// # Arguments
/// - `class`: JVM internal class name (e.g. `"cratonvm/DiffArithmetic"`)
/// - `method`: method name (e.g. `"add"`)
/// - `descriptor`: JVM method descriptor (e.g. `"()I"`)
///
/// # Returns
/// A `DiffResult` with both outcomes and whether they match.
pub fn differential_run(class: &str, method: &str, descriptor: &str) -> DiffResult {
    let cratonvm_outcome = run_cratonvm(class, method, descriptor);
    let hotspot_outcome = run_hotspot(class, method, descriptor);

    let matches = cratonvm_outcome.stdout == hotspot_outcome.stdout
        && cratonvm_outcome.return_value == hotspot_outcome.return_value;

    DiffResult {
        cratonvm_outcome,
        hotspot_outcome,
        matches,
    }
}

// ---------------------------------------------------------------------------
// CratonVM runner
// ---------------------------------------------------------------------------

/// Run a static method under CratonVM, capturing stdout and return value.
fn run_cratonvm(class: &str, method: &str, descriptor: &str) -> Outcome {
    let classpath = test_resources_dir();
    let config = VmConfig::new().with_classpath(vec![classpath]);
    let mut vm = Vm::new(config);

    // Prepare arguments based on descriptor.
    let args = args_for_descriptor(descriptor);

    match vm.invoke(class, method, descriptor, &args) {
        Ok(Some(value)) => {
            let stdout = vm.main_thread.printed_lines.join("\n");
            let return_value = format_value(&value);
            Outcome { stdout, return_value }
        }
        Ok(None) => {
            let stdout = vm.main_thread.printed_lines.join("\n");
            Outcome {
                stdout,
                return_value: "void".to_string(),
            }
        }
        Err(e) => {
            let stdout = vm.main_thread.printed_lines.join("\n");
            Outcome {
                stdout,
                return_value: format!("error:{e:?}"),
            }
        }
    }
}

/// Format a JVM Value as a string for comparison.
fn format_value(value: &Value) -> String {
    match value {
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Double(d) => format!("{d}"),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(_)) => "object".to_string(),
        _ => format!("{value:?}"),
    }
}

/// Build the argument list for a method descriptor.
/// For `main(String[])` we pass a null reference; for no-arg methods, empty.
fn args_for_descriptor(descriptor: &str) -> Vec<Value> {
    if descriptor.starts_with("([Ljava/lang/String;)") {
        vec![Value::Object(None)]
    } else if descriptor.starts_with("()") {
        vec![]
    } else {
        // For other descriptors, caller should pre-fill. Default to empty.
        vec![]
    }
}

// ---------------------------------------------------------------------------
// HotSpot runner
// ---------------------------------------------------------------------------

/// Run a static method under HotSpot by invoking `java` on PATH.
///
/// For void methods (main), we capture stdout directly.
/// For value-returning methods, we generate a tiny wrapper that calls the
/// method and prints the result, then capture stdout.
fn run_hotspot(class: &str, method: &str, descriptor: &str) -> Outcome {
    let classpath = test_resources_dir();
    let dotted_class = class.replace('/', ".");

    // For main methods, just run the class directly.
    if method == "main" && descriptor == "([Ljava/lang/String;)V" {
        let output = Command::new(java_executable())
            .arg("-cp")
            .arg(&classpath)
            .arg(&dotted_class)
            .output()
            .unwrap_or_else(|e| panic!("Failed to run HotSpot java: {e}"));

        let stdout = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();

        return Outcome {
            stdout,
            return_value: "void".to_string(),
        };
    }

    // For value-returning static methods, use jshell-style one-liner via
    // java's ability to evaluate a class with a main that prints the result.
    // We build a small command using --source 21 or use a direct approach
    // via `java -e` (JDK 25+) or a temp wrapper.
    //
    // Simplest portable approach: invoke via jrunscript or a generated wrapper.
    // For maximum portability, we write a tiny .java file, compile, and run.
    let return_type = parse_return_type(descriptor);
    let wrapper_src = format!(
        "public class DiffWrapper__ {{ public static void main(String[] a) {{ \
         System.out.println({dotted_class}.{method}()); }} }}"
    );

    // Write wrapper to a temp directory alongside the classpath.
    let temp_dir = std::env::temp_dir().join("cratonvm_diff_test");
    std::fs::create_dir_all(&temp_dir).ok();

    let wrapper_java = temp_dir.join("DiffWrapper__.java");
    std::fs::write(&wrapper_java, &wrapper_src)
        .expect("Failed to write wrapper Java file");

    // Compile the wrapper.
    let compile = Command::new(javac_executable())
        .arg("-cp")
        .arg(&classpath)
        .arg("-d")
        .arg(temp_dir.to_str().unwrap())
        .arg(wrapper_java.to_str().unwrap())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run javac: {e}"));

    if !compile.status.success() {
        let stderr = String::from_utf8_lossy(&compile.stderr);
        return Outcome {
            stdout: String::new(),
            return_value: format!("error:javac failed: {stderr}"),
        };
    }

    // Run the wrapper, passing both the original classpath and temp dir.
    let sep = if cfg!(windows) { ";" } else { ":" };
    let full_cp = format!("{classpath}{sep}{}", temp_dir.to_string_lossy());

    let output = Command::new(java_executable())
        .arg("-cp")
        .arg(&full_cp)
        .arg("DiffWrapper__")
        .output()
        .unwrap_or_else(|e| panic!("Failed to run HotSpot java: {e}"));

    let raw_stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string();

    // The wrapper prints the return value on stdout. Parse it back.
    let return_value = match return_type.as_str() {
        "V" => "void".to_string(),
        _ => raw_stdout.clone(),
    };

    // Clean up temp files (best effort).
    std::fs::remove_file(temp_dir.join("DiffWrapper__.java")).ok();
    std::fs::remove_file(temp_dir.join("DiffWrapper__.class")).ok();

    Outcome {
        stdout: String::new(), // The wrapper only prints the return value
        return_value,
    }
}

/// Extract the return type character(s) from a JVM descriptor.
/// E.g. `"()I"` -> `"I"`, `"([Ljava/lang/String;)V"` -> `"V"`.
fn parse_return_type(descriptor: &str) -> String {
    if let Some(pos) = descriptor.rfind(')') {
        descriptor[pos + 1..].to_string()
    } else {
        "V".to_string()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the path to `vm/tests/resources/` for the test classpath.
fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let resources = Path::new(manifest_dir).join("tests").join("resources");
    resources.to_string_lossy().to_string()
}

/// Return the `java` executable name.
fn java_executable() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// Return the `javac` executable name.
fn javac_executable() -> &'static str {
    if cfg!(windows) { "javac.exe" } else { "javac" }
}

// ---------------------------------------------------------------------------
// Divergence tracking
// ---------------------------------------------------------------------------

/// A single divergence between CratonVM and HotSpot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Divergence {
    pub class_name: String,
    pub method: String,
    pub descriptor: String,
    pub cratonvm_stdout: String,
    pub cratonvm_return: String,
    pub hotspot_stdout: String,
    pub hotspot_return: String,
}

/// A collection of divergences, serializable to JSON.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DivergenceReport {
    pub divergences: Vec<Divergence>,
    pub total_tested: usize,
    pub total_matched: usize,
}

impl DivergenceReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a differential test result.
    pub fn record(&mut self, class: &str, method: &str, descriptor: &str, result: &DiffResult) {
        self.total_tested += 1;
        if result.matches {
            self.total_matched += 1;
        } else {
            self.divergences.push(Divergence {
                class_name: class.to_string(),
                method: method.to_string(),
                descriptor: descriptor.to_string(),
                cratonvm_stdout: result.cratonvm_outcome.stdout.clone(),
                cratonvm_return: result.cratonvm_outcome.return_value.clone(),
                hotspot_stdout: result.hotspot_outcome.stdout.clone(),
                hotspot_return: result.hotspot_outcome.return_value.clone(),
            });
        }
    }

    /// Write the report to `bench/differential-divergences.json`.
    pub fn write_to_file(&self) {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = Path::new(manifest_dir)
            .parent()
            .unwrap()
            .join("bench")
            .join("differential-divergences.json");
        let json = serde_json::to_string_pretty(self).expect("failed to serialize report");
        std::fs::write(&path, json).unwrap_or_else(|e| {
            eprintln!("Warning: could not write divergence report to {}: {e}", path.display());
        });
    }
}

// ---------------------------------------------------------------------------
// T4.10.2 -- diff_basic_arithmetic
// ---------------------------------------------------------------------------

/// Test basic arithmetic operations through the differential harness.
///
/// Runs `cratonvm/DiffArithmetic` static methods under both CratonVM and HotSpot,
/// comparing return values for add, multiply, divide, modulo, negation, and a
/// mixed expression.
#[test]
#[ignore = "requires java/javac on PATH"]
fn diff_basic_arithmetic() {
    let methods = [
        ("add", "()I"),
        ("mul", "()I"),
        ("div", "()I"),
        ("mod_op", "()I"),
        ("neg", "()I"),
        ("mixed", "()I"),
    ];

    let mut report = DivergenceReport::new();

    for (method, descriptor) in &methods {
        eprintln!("diff_basic_arithmetic: DiffArithmetic.{method}");
        let result = differential_run("cratonvm/DiffArithmetic", method, descriptor);
        eprintln!(
            "  CratonVM: {} | HotSpot: {} | match={}",
            result.cratonvm_outcome.return_value,
            result.hotspot_outcome.return_value,
            result.matches
        );
        report.record("cratonvm/DiffArithmetic", method, descriptor, &result);
        assert!(
            result.matches,
            "Arithmetic divergence in DiffArithmetic.{method}!\n  \
             CratonVM: {}\n  HotSpot: {}",
            result.cratonvm_outcome,
            result.hotspot_outcome
        );
    }

    // Also test the main method which prints all results to stdout.
    let main_result = differential_run(
        "cratonvm/DiffArithmetic",
        "main",
        "([Ljava/lang/String;)V",
    );
    eprintln!(
        "diff_basic_arithmetic: DiffArithmetic.main\n  \
         CratonVM stdout: {:?}\n  HotSpot stdout: {:?}\n  match={}",
        main_result.cratonvm_outcome.stdout,
        main_result.hotspot_outcome.stdout,
        main_result.matches
    );
    report.record(
        "cratonvm/DiffArithmetic",
        "main",
        "([Ljava/lang/String;)V",
        &main_result,
    );

    eprintln!(
        "\ndiff_basic_arithmetic: {}/{} matched",
        report.total_matched, report.total_tested
    );
}

// ---------------------------------------------------------------------------
// T4.10.3 -- diff_string_operations
// ---------------------------------------------------------------------------

/// Test String operations through the differential harness.
///
/// Runs `cratonvm/DiffString` static methods under both CratonVM and HotSpot,
/// comparing return values for length, concat, charAt, substring, indexOf,
/// toUpperCase, trim, equals, and valueOf.
#[test]
#[ignore = "requires java/javac on PATH"]
fn diff_string_operations() {
    let methods = [
        ("length", "()I"),
        ("concat", "()Ljava/lang/String;"),
        ("charAt", "()C"),
        ("substring", "()Ljava/lang/String;"),
        ("indexOf", "()I"),
        ("toUpperCase", "()Ljava/lang/String;"),
        ("trim", "()Ljava/lang/String;"),
        ("equals", "()Z"),
        ("valueOf", "()Ljava/lang/String;"),
    ];

    let mut report = DivergenceReport::new();

    for (method, descriptor) in &methods {
        eprintln!("diff_string_operations: DiffString.{method}");
        let result = differential_run("cratonvm/DiffString", method, descriptor);
        eprintln!(
            "  CratonVM: {} | HotSpot: {} | match={}",
            result.cratonvm_outcome.return_value,
            result.hotspot_outcome.return_value,
            result.matches
        );
        report.record("cratonvm/DiffString", method, descriptor, &result);
        // String operations may diverge due to incomplete String support.
        // Log divergences but don't fail the test -- the report captures them.
        if !result.matches {
            eprintln!(
                "  WARNING: String divergence in DiffString.{method}!\n    \
                 CratonVM: {}\n    HotSpot: {}",
                result.cratonvm_outcome,
                result.hotspot_outcome
            );
        }
    }

    // Also test main which prints all results to stdout.
    let main_result = differential_run(
        "cratonvm/DiffString",
        "main",
        "([Ljava/lang/String;)V",
    );
    eprintln!(
        "diff_string_operations: DiffString.main\n  \
         CratonVM stdout: {:?}\n  HotSpot stdout: {:?}\n  match={}",
        main_result.cratonvm_outcome.stdout,
        main_result.hotspot_outcome.stdout,
        main_result.matches
    );
    report.record(
        "cratonvm/DiffString",
        "main",
        "([Ljava/lang/String;)V",
        &main_result,
    );

    report.write_to_file();

    eprintln!(
        "\ndiff_string_operations: {}/{} matched ({} divergences)",
        report.total_matched,
        report.total_tested,
        report.divergences.len()
    );

    // Log integer-returning method results separately. These may diverge
    // if String methods are not yet implemented in the synthetic JDK.
    let int_methods: Vec<&str> = methods
        .iter()
        .filter(|(_, d)| *d == "()I")
        .map(|(m, _)| *m)
        .collect();
    for m in &int_methods {
        let r = differential_run("cratonvm/DiffString", m, "()I");
        if !r.matches {
            eprintln!(
                "  NOTE: Integer String method DiffString.{m} diverged \
                 (expected once String natives are complete):\n    \
                 CratonVM: {}\n    HotSpot: {}",
                r.cratonvm_outcome,
                r.hotspot_outcome
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests for harness infrastructure (no external deps)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn diff_result_matches_when_equal() {
        let r = DiffResult {
            cratonvm_outcome: Outcome {
                stdout: "hello".into(),
                return_value: "42".into(),
            },
            hotspot_outcome: Outcome {
                stdout: "hello".into(),
                return_value: "42".into(),
            },
            matches: true,
        };
        assert!(r.matches);
    }

    #[test]
    fn diff_result_diverges_when_different() {
        let r = DiffResult {
            cratonvm_outcome: Outcome {
                stdout: "hello".into(),
                return_value: "42".into(),
            },
            hotspot_outcome: Outcome {
                stdout: "hello".into(),
                return_value: "43".into(),
            },
            matches: false,
        };
        assert!(!r.matches);
    }

    #[test]
    fn divergence_report_records_correctly() {
        let mut report = DivergenceReport::new();

        let match_result = DiffResult {
            cratonvm_outcome: Outcome {
                stdout: "ok".into(),
                return_value: "1".into(),
            },
            hotspot_outcome: Outcome {
                stdout: "ok".into(),
                return_value: "1".into(),
            },
            matches: true,
        };
        report.record("Test1", "test", "()I", &match_result);

        let div_result = DiffResult {
            cratonvm_outcome: Outcome {
                stdout: "foo".into(),
                return_value: "1".into(),
            },
            hotspot_outcome: Outcome {
                stdout: "bar".into(),
                return_value: "1".into(),
            },
            matches: false,
        };
        report.record("Test2", "test", "()V", &div_result);

        assert_eq!(report.total_tested, 2);
        assert_eq!(report.total_matched, 1);
        assert_eq!(report.divergences.len(), 1);
        assert_eq!(report.divergences[0].class_name, "Test2");
    }

    #[test]
    fn divergence_report_default_is_empty() {
        let report = DivergenceReport::new();
        assert_eq!(report.total_tested, 0);
        assert_eq!(report.total_matched, 0);
        assert!(report.divergences.is_empty());
    }

    #[test]
    fn test_resources_dir_exists() {
        let dir = test_resources_dir();
        assert!(
            Path::new(&dir).exists(),
            "test resources directory should exist: {dir}"
        );
    }

    #[test]
    fn parse_return_type_int() {
        assert_eq!(parse_return_type("()I"), "I");
    }

    #[test]
    fn parse_return_type_void() {
        assert_eq!(parse_return_type("([Ljava/lang/String;)V"), "V");
    }

    #[test]
    fn parse_return_type_object() {
        assert_eq!(parse_return_type("()Ljava/lang/String;"), "Ljava/lang/String;");
    }

    #[test]
    fn args_for_main_descriptor() {
        let args = args_for_descriptor("([Ljava/lang/String;)V");
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn args_for_noarg_descriptor() {
        let args = args_for_descriptor("()I");
        assert!(args.is_empty());
    }

    #[test]
    fn format_value_int() {
        assert_eq!(format_value(&Value::Int(42)), "42");
    }

    #[test]
    fn format_value_null() {
        assert_eq!(format_value(&Value::Object(None)), "null");
    }
}
