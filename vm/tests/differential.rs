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
//!
//! # Adding a divergence
//!
//! This harness *is* the divergence record — the standing divergence log was
//! retired on 2026-07-28 once its last entry was fixed, and the JSON report
//! below replaced it. To pin a newly found divergence: add a fixture class under
//! `vm/tests/resources/cratonvm/` whose `main` prints everything it checks, then
//! either an `#[ignore]`d `assert_main_matches_hotspot("cratonvm/YourFixture")`
//! test (for a permanent regression) or a `DIFFERENTIAL_CLASSES=cratonvm.Your…`
//! run of [`diff_batch_from_env`] (for an ad-hoc sweep). Mismatches land in
//! `bench/differential-divergences.json`.
//!
//! The three fixtures already here — `DiffLocaleCase`, `DiffFloatFormat`,
//! `DiffNpeMessage` — pin the three divergences that log recorded:
//! locale-sensitive case mapping, `Double`/`Float` layout, and JEP 358 NPE
//! messages.
//!
//! # Two ways to get a false divergence
//!
//! Both of these were live bugs in this file, and both make the *harness* wrong
//! rather than the VM:
//!
//! 1. **Configuration skew.** The CratonVM side must adopt the launcher's
//!    VM-flag defaults. `vm-cli` resolves an absent
//!    `-XX:±ShowCodeDetailsInExceptionMessages` to HotSpot's `on`, but an
//!    in-process embedder that never calls the setter keeps the pre-JEP-358
//!    strings — so every NPE fixture "diverged" purely from configuration.
//! 2. **Formatting skew.** Whatever this file uses to render a return value has
//!    to match what `System.out.println` prints on the HotSpot side. Rust's
//!    `Display` for `f64` never emits scientific notation, so `1e7` compared as
//!    `10000000` against HotSpot's `1.0E7`.

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
        write!(
            f,
            "stdout={:?}, return={:?}",
            self.stdout, self.return_value
        )
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
/// Both sides use the real JDK: CratonVM boots the launcher configuration
/// (`VmConfig::for_launcher`), which loads `java.base` from the same
/// `$JAVA_HOME` the `java` subprocess runs.
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
    // `vm-cli` resolves an absent `-XX:±ShowCodeDetailsInExceptionMessages` to
    // HotSpot's `on` default and publishes it before `Vm::new`; an in-process
    // embedder that never calls the setter keeps the legacy (non-JEP-358)
    // strings instead. A harness comparing against a stock `java` has to adopt
    // the launcher's defaults, or an NPE-message fixture "diverges" purely
    // because the two sides were configured differently.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(true);
    let classpath = test_resources_dir();
    // `for_launcher()`, not `VmConfig::new()`: the latter is the *embedded*
    // default, which boots the synthetic JDK. Comparing a synthetic-stdlib
    // CratonVM against a real-JDK HotSpot charges every synthetic-mode gap to
    // the VM as a "divergence" — a `new Locale("en")` that the synthetic
    // library cannot construct is not a case-mapping bug. The shipping
    // `cratonvm` launcher boots real-JDK mode, and this harness already
    // requires a JDK on PATH for the HotSpot side, so both sides can and should
    // use the same class library.
    let config = VmConfig::for_launcher().with_classpath(vec![classpath]);
    let mut vm = Vm::new(config);

    // Prepare arguments based on descriptor.
    let args = args_for_descriptor(descriptor);

    match vm.invoke(class, method, descriptor, &args) {
        Ok(Some(value)) => {
            let stdout = vm.main_thread.printed_lines.join("\n");
            let return_value = format_value(&value, descriptor, &vm);
            Outcome {
                stdout,
                return_value,
            }
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

/// Format a JVM Value the way `System.out.println` prints it on the HotSpot
/// side, so the two strings are comparable.
///
/// Three things have to be undone to get there, and each of them used to
/// manufacture a divergence out of a correct result:
///
/// * **Floating point.** HotSpot prints `Double.toString` / `Float.toString`.
///   Rust's `Display` for `f64`/`f32` picks the same shortest round-tripping
///   digits but never switches to scientific notation, so `1e7` formatted as
///   `10000000` here against `1.0E7` there.
///   `cratonvm_types::java_{double,float}_to_string` implements the JLS layout
///   rule and is what the VM's own `Double.toString` native uses.
/// * **`char` / `boolean`.** Both live in a `Value::Int` on the operand stack,
///   but `println` prints `d` and `true`, not `100` and `1`. The method
///   descriptor's return type is the only thing that distinguishes them from an
///   `int`.
/// * **References.** A returned `String` printed as the literal `"object"`,
///   which no HotSpot output can ever equal. Read the actual characters when the
///   reference is a String; anything else keeps `object` (its HotSpot spelling
///   would be an identity hash, which is not comparable anyway).
fn format_value(value: &Value, descriptor: &str, vm: &Vm) -> String {
    let return_type = parse_return_type(descriptor);
    match value {
        Value::Int(i) => match return_type.as_str() {
            "Z" => (*i != 0).to_string(),
            "C" => char::from_u32(*i as u32)
                .map(String::from)
                .unwrap_or_else(|| i.to_string()),
            _ => i.to_string(),
        },
        Value::Long(l) => l.to_string(),
        Value::Float(f) => cratonvm_types::java_float_to_string(*f),
        Value::Double(d) => cratonvm_types::java_double_to_string(*d),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(obj)) => cratonvm_vm::vm::read_java_string(&vm.shared.mem.heap, *obj)
            .unwrap_or_else(|| "object".to_string()),
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

/// Distinguishes concurrent `run_hotspot` calls' scratch directories.
static WRAPPER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

    // Write the wrapper to a directory of its own. A single shared path would
    // be raced by concurrently running tests (the default `cargo test` layout):
    // each call compiles the wrapper and then deletes it, so a neighbour's
    // `java` invocation can find the class gone and produce empty stdout, which
    // reads as a divergence against a VM that was right all along.
    let temp_dir = std::env::temp_dir()
        .join("cratonvm_diff_test")
        .join(format!(
            "{}-{}",
            std::process::id(),
            WRAPPER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&temp_dir).ok();

    let wrapper_java = temp_dir.join("DiffWrapper__.java");
    std::fs::write(&wrapper_java, &wrapper_src).expect("Failed to write wrapper Java file");

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

    // The wrapper prints the tested method's own output first and the return
    // value on the LAST line. Splitting them keeps a method that both prints and
    // returns comparable: the CratonVM runner always reports `printed_lines`, so
    // discarding HotSpot's stdout here made every such method "diverge".
    let (stdout, return_value) = match return_type.as_str() {
        "V" => (raw_stdout, "void".to_string()),
        _ => match raw_stdout.rfind('\n') {
            Some(cut) => (
                raw_stdout[..cut].trim_end_matches('\r').to_string(),
                raw_stdout[cut + 1..].to_string(),
            ),
            None => (String::new(), raw_stdout),
        },
    };

    // Clean up (best effort) — the whole per-run directory, not just the two
    // files, so nothing accumulates under the shared parent.
    std::fs::remove_dir_all(&temp_dir).ok();

    Outcome {
        stdout,
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
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// Return the `javac` executable name.
fn javac_executable() -> &'static str {
    if cfg!(windows) {
        "javac.exe"
    } else {
        "javac"
    }
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
            eprintln!(
                "Warning: could not write divergence report to {}: {e}",
                path.display()
            );
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
            result.cratonvm_outcome, result.hotspot_outcome
        );
    }

    // Also test the main method which prints all results to stdout.
    let main_result = differential_run("cratonvm/DiffArithmetic", "main", "([Ljava/lang/String;)V");
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
        // Asserted since 2026-07-28. This used to only warn, on the premise
        // that "String operations may diverge due to incomplete String
        // support" — but with the harness rendering `char`, `boolean` and
        // `String` returns the way `println` does, and both sides booting the
        // real JDK, all nine match. A regression here is a real one.
        assert!(
            result.matches,
            "String divergence in DiffString.{method}!\n  \
             CratonVM: {}\n  HotSpot: {}",
            result.cratonvm_outcome, result.hotspot_outcome
        );
    }

    // Also test main which prints all results to stdout.
    let main_result = differential_run("cratonvm/DiffString", "main", "([Ljava/lang/String;)V");
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
}

// ---------------------------------------------------------------------------
// Divergence-log regression suites
// ---------------------------------------------------------------------------

/// Run one fixture's `main` under both VMs and assert byte-identical stdout.
///
/// These three classes pin the divergences the log recorded: locale-sensitive
/// case mapping (DIV-001), `Double`/`Float` formatting (DIV-002) and JEP 358
/// NullPointerException messages (DIV-003). Each prints everything it checks, so
/// a single stdout comparison covers the whole matrix.
fn assert_main_matches_hotspot(class: &str) {
    let result = differential_run(class, "main", "([Ljava/lang/String;)V");
    assert!(
        result.matches,
        "{class}.main diverged from HotSpot.\n  CratonVM: {}\n  HotSpot:  {}",
        result.cratonvm_outcome, result.hotspot_outcome
    );
}

/// DIV-001: `String.to{Lower,Upper}Case(Locale)` must honour the Turkish /
/// Azeri dotted-I and Lithuanian retained-dot rules, not just the root mapping.
#[test]
#[ignore = "requires java/javac on PATH"]
fn diff_locale_case_mapping() {
    assert_main_matches_hotspot("cratonvm/DiffLocaleCase");
}

/// DIV-002: `Double.toString` / `Float.toString` layout (the `10^-3 .. 10^7`
/// plain-decimal window, subnormals, specials) through every printing path.
#[test]
#[ignore = "requires java/javac on PATH"]
fn diff_float_formatting() {
    assert_main_matches_hotspot("cratonvm/DiffFloatFormat");
}

/// DIV-003: JEP 358 helpful NullPointerException messages, including the
/// `because "<expr>" is null` clause inside control-flow merge blocks.
#[test]
#[ignore = "requires java/javac on PATH"]
fn diff_npe_messages() {
    assert_main_matches_hotspot("cratonvm/DiffNpeMessage");
}

// ---------------------------------------------------------------------------
// Batch runs via the DIFFERENTIAL_CLASSES env var
// ---------------------------------------------------------------------------

/// Run `main` under both VMs for every class named in `DIFFERENTIAL_CLASSES`
/// (comma- or space-separated, internal or dotted form), writing the report to
/// `bench/differential-divergences.json`.
///
/// This is the ad-hoc entry point for widening the sweep without editing the
/// test: point the classpath fixtures directory at new `.class` files and list
/// them. It reports rather than asserts, so an exploratory batch always
/// produces a full report instead of stopping at the first divergence.
#[test]
#[ignore = "requires java/javac on PATH and DIFFERENTIAL_CLASSES"]
fn diff_batch_from_env() {
    let Ok(raw) = std::env::var("DIFFERENTIAL_CLASSES") else {
        eprintln!("DIFFERENTIAL_CLASSES not set — nothing to do");
        return;
    };
    let classes: Vec<String> = raw
        .split([',', ' ', ';'])
        .map(|c| c.trim().replace('.', "/"))
        .filter(|c| !c.is_empty())
        .collect();
    assert!(
        !classes.is_empty(),
        "DIFFERENTIAL_CLASSES was set but named no classes: {raw:?}"
    );

    let mut report = DivergenceReport::new();
    for class in &classes {
        let result = differential_run(class, "main", "([Ljava/lang/String;)V");
        eprintln!(
            "diff_batch: {class}.main match={}\n  CratonVM: {}\n  HotSpot:  {}",
            result.matches, result.cratonvm_outcome, result.hotspot_outcome
        );
        report.record(class, "main", "([Ljava/lang/String;)V", &result);
    }
    report.write_to_file();
    eprintln!(
        "\ndiff_batch_from_env: {}/{} matched ({} divergences)",
        report.total_matched,
        report.total_tested,
        report.divergences.len()
    );
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
        assert_eq!(
            parse_return_type("()Ljava/lang/String;"),
            "Ljava/lang/String;"
        );
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

    /// A `Vm` is only needed to resolve reference returns; the scalar
    /// renderings are pure and are asserted through this helper.
    fn scalar(value: Value, descriptor: &str) -> String {
        let vm = Vm::new(VmConfig::new());
        format_value(&value, descriptor, &vm)
    }

    #[test]
    fn format_value_int() {
        assert_eq!(scalar(Value::Int(42), "()I"), "42");
    }

    #[test]
    fn format_value_null() {
        assert_eq!(scalar(Value::Object(None), "()Ljava/lang/String;"), "null");
    }

    /// `char` and `boolean` share `Value::Int` with `int`; only the descriptor
    /// says which, and `println` prints them as `d` / `true`.
    #[test]
    fn format_value_char_and_boolean_follow_the_descriptor() {
        assert_eq!(scalar(Value::Int(100), "()C"), "d");
        assert_eq!(scalar(Value::Int(1), "()Z"), "true");
        assert_eq!(scalar(Value::Int(0), "()Z"), "false");
        assert_eq!(scalar(Value::Int(100), "()I"), "100");
    }

    /// `format_value` must produce what `System.out.println` prints on the
    /// HotSpot side — `Double.toString`, not Rust's `Display` (which never uses
    /// scientific notation and so mismatched on every value outside the
    /// `10^-3 .. 10^7` window).
    #[test]
    fn format_value_doubles_use_java_tostring() {
        assert_eq!(scalar(Value::Double(1e7), "()D"), "1.0E7");
        assert_eq!(scalar(Value::Double(3.0), "()D"), "3.0");
        assert_eq!(scalar(Value::Double(1e-4), "()D"), "1.0E-4");
        assert_eq!(scalar(Value::Float(1e8), "()F"), "1.0E8");
        assert_eq!(scalar(Value::Float(3.0), "()F"), "3.0");
    }
}
