// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration tests for the bytecode interpreter.
//!
//! These tests compile Java test classes and run them through the VM,
//! verifying that the interpreter produces correct results.
//!
//! **Prerequisites:**
//! - Java test classes are compiled automatically by `build.rs` if `javac`
//!   is on the PATH. If not, tests will be skipped at runtime.
//! - Extended session/TCK blocks are opt-in because several are compatibility
//!   corpus probes rather than stable default CI gates. Set
//!   `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1` to run them.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

/// Path to the test resources directory.
fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

/// Check if compiled .class files are available.
fn class_files_available() -> bool {
    let dir = test_resources_dir();
    let class_path = format!("{dir}/cratonvm/SimpleReturn.class");
    std::path::Path::new(&class_path).exists()
}

fn extended_interpreter_tests_enabled() -> bool {
    matches!(
        std::env::var("CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn require_extended_interpreter_tests(test_name: &str) -> bool {
    if !extended_interpreter_tests_enabled() {
        eprintln!(
            "Skipping {test_name}: set CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 to run extended interpreter corpus tests"
        );
        return false;
    }
    assert!(
        cratonvm_vm::config::SYNTHETIC_JDK_COMPILED_IN,
        "the extended interpreter corpus was opted into, but this test binary was \
         built WITHOUT the `synthetic-jdk` Cargo feature.\n\
         \n\
         `test_vm()` builds a `VmConfig::default()`, whose JDK mode is \
         `EMBEDDED_DEFAULT_JDK_MODE` = synthetic. In a default-feature build that \
         registers none of the ~5,200 synthetic stubs AND suppresses boot-classpath \
         discovery, so the corpus measures a VM with no class library at all — \
         `config::require_synthetic_jdk` rejects exactly this state for the CLI. \
         Run it and 214 of the 924 tests fail; with the feature, 40 do. Failing here \
         rather than reporting that number is the whole point.\n\
         \n\
         Re-run as:\n  \
         CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release \\\n    \
         -p cratonvm-vm --features synthetic-jdk --test interpreter_tests"
    );
    // The second gate must NOT stay a silent skip once the corpus has been
    // asked for. `vm/build.rs` compiles every fixture in one `javac`
    // invocation per pass, so a single unbuildable source leaves the whole
    // staging directory empty — which is what happened, undetected, until
    // `f715d1367`: opting in still printed a green `924 passed` that had run
    // none of the corpus. An explicit request for the corpus that cannot be
    // served is a failure, not a pass.
    assert!(
        class_files_available(),
        "the extended interpreter corpus was opted into, but no compiled fixtures \
         were staged: {}/cratonvm/SimpleReturn.class does not exist.\n\
         \n\
         `vm/build.rs` needs a `javac` (from `$JAVA_HOME/bin` if set, else PATH) \
         and compiles all fixtures of a pass in ONE invocation, so one broken \
         source stages nothing at all. Re-run the build and read the \
         `cargo:warning=javac failed …` lines it prints.",
        test_resources_dir()
    );
    true
}

/// Create a VM configured for testing (classpath pointing to test resources).
fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

/// `(class, method)` pairs that do not produce the JDK-correct answer under
/// CratonVM's synthetic class library — the pinned baseline of this corpus.
///
/// **This list is EMPTY, and that is its finished state, not an unset one.**
/// All 924 fixtures produce the JDK-correct answer. The eleven entries it
/// carried from 2026-08-02 were closed on 2026-08-11; see
/// fixed-bugs/synthetic-jdk-class-library-gaps-FIXED-20260811.md for what each
/// one turned out to be. Every one of the eleven was *reachable* code that had
/// been switched off, mis-shaped or unlinked — a feature gate the synthetic
/// library could not satisfy, a reified `Type` no `instanceof` could
/// recognise, a fixed-size list class fabricated as an interface so its
/// backing array had nowhere to go, and one fixture that asked CratonVM a
/// question its own harness could not pose.
///
/// Keep the constant, and keep the gate around it. An empty list still fails
/// the run on any mismatch; deleting it would turn the corpus back into
/// something whose failures are a number nobody reads. Adding an entry is
/// therefore a deliberate act with a price, and the price is the point.
///
/// **Before adding one**, measure it twice — both measurements are
/// load-bearing, and the first one has changed three fixtures' minds already:
///
/// 1. Re-run under a real **JDK 25** (`probes/CorpusOracle`) — HotSpot must
///    produce the value the test expects, so the expectation is right and
///    CratonVM is what is missing. Fixture expectations HotSpot *disagreed*
///    with were corrected in the fixtures instead and never belonged here
///    (`ReflectionComplete.testFieldGetPrivate`,
///    `ReflectionComplete.testMethodInvokePrivateViaReflection`,
///    `PropertiesComplete.testPropertiesLoadSpaces`,
///    `ScopedValueComplete.testThreadVisibility`). Note the oracle constructs
///    a receiver for a non-static fixture method and `Vm::invoke` cannot, so a
///    fixture that is not `static` asks the two sides different questions —
///    that mismatch was filed as a class-library gap for nine days.
/// 2. Re-run **alone** in a fresh process (`--exact <test>`) — it must still
///    fail, so it is a real gap and not an artefact of the ~900 VMs that
///    precede it in a full run. Two of the eleven were exactly that artefact:
///    `SerializeBasic`'s side tables are keyed by the stream object's raw
///    address, and a recycled address inherited a dead stream's wire-handle
///    table.
///
/// **Reading check 2 correctly matters.** Running `--exact` on a test that is
/// already listed here reports `FAILED` when it *passes*, because the
/// unexpected-pass arm below fires. Grepping the run for `result: FAILED` and
/// calling that "still broken" inverts the answer for every listed entry —
/// which is exactly the mistake that briefly filed the eleven as merely
/// "order dependent" and the five proxy/annotation tests (which pass) as gaps.
/// Read the panic text, not the exit status.
///
/// The list is a two-way gate, which is the whole point of pinning it:
/// a mismatch that is NOT listed fails the run, and a listed pair that starts
/// PASSING also fails the run, telling you to delete the entry. A corpus whose
/// known gaps close silently is how this one went dark for as long as it did —
/// and the gate has now paid for itself twice, first on five proxy/annotation
/// tests and then on `FinalizerTest.testNoFinalizeOnLive`, which it reported
/// as passing before anyone went looking for its cause.
const KNOWN_SYNTHETIC_JDK_GAPS: &[(&str, &str)] = &[];

fn listed(list: &[(&str, &str)], class: &str, method: &str) -> bool {
    list.iter().any(|&(c, m)| c == class && m == method)
}

/// Decide what a corpus result means, given the pinned baseline.
///
/// `matched` is whether the call produced the expected value; `expected` is a
/// human label for it. A mismatch is reported through
/// [`Vm::describe_result`] rather than `{result:?}` — `MethodCallResult`'s own
/// `Debug` prints a thrown exception as
/// `Err(ExceptionThrown(ObjectRef { ptr: 0x… }))`, an address with no class,
/// no message and no stack, which is why 175 of this corpus's 214 failures
/// were indistinguishable from one another and had to be re-run one at a time
/// through a hand-written Java probe to be grouped at all.
fn corpus_check(
    vm: &Vm,
    class: &str,
    method: &str,
    expected: &str,
    matched: bool,
    result: &cratonvm_vm::error::MethodCallResult,
) {
    match (matched, listed(KNOWN_SYNTHETIC_JDK_GAPS, class, method)) {
        (true, false) => {}
        (false, true) => {}
        (false, false) => panic!(
            "{class}::{method} — expected {expected}, {}",
            vm.describe_result(result)
        ),
        (true, true) => panic!(
            "{class}::{method} now produces the expected {expected}, but it is still              listed in KNOWN_SYNTHETIC_JDK_GAPS. Delete that entry: the list is the              pinned synthetic-JDK baseline, and a gap that closes silently leaves the              next regression with nothing to fail against."
        ),
    }
}

/// Skip guard — returns early if .class files are not available.
macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: .class files not available (javac not on PATH?)");
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Basic arithmetic and control flow tests
// ---------------------------------------------------------------------------

#[test]
fn test_simple_return() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SimpleReturn", "test", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SimpleReturn",
        "test",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_arithmetic_add() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/Arithmetic", "test", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/Arithmetic",
        "test",
        "Int(30)",
        matches!(result, Ok(Some(Value::Int(30)))),
        &result,
    );
}

#[test]
fn test_arithmetic_mul() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/Arithmetic", "testMul", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/Arithmetic",
        "testMul",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_arithmetic_div() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/Arithmetic", "testDiv", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/Arithmetic",
        "testDiv",
        "Int(25)",
        matches!(result, Ok(Some(Value::Int(25)))),
        &result,
    );
}

#[test]
fn test_arithmetic_neg() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/Arithmetic", "testNeg", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/Arithmetic",
        "testNeg",
        "Int(-42)",
        matches!(result, Ok(Some(Value::Int(-42)))),
        &result,
    );
}

#[test]
fn test_control_flow_if_else() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ControlFlow", "testIfElse", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ControlFlow",
        "testIfElse",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_control_flow_loop() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ControlFlow", "testLoop", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ControlFlow",
        "testLoop",
        "Int(45)",
        matches!(result, Ok(Some(Value::Int(45)))),
        &result,
    );
}

#[test]
fn test_control_flow_while() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ControlFlow", "testWhile", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ControlFlow",
        "testWhile",
        "Int(1024)",
        matches!(result, Ok(Some(Value::Int(1024)))),
        &result,
    );
}

#[test]
fn test_control_flow_switch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ControlFlow",
        "testSwitch",
        "(I)I",
        &[Value::Int(2)],
    );
    corpus_check(
        &vm,
        "cratonvm/ControlFlow",
        "testSwitch",
        "Int(20)",
        matches!(result, Ok(Some(Value::Int(20)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Exception tests (now work without RT_JAR via synthetic class hierarchy)
// ---------------------------------------------------------------------------

#[test]
fn test_division_by_zero() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/Arithmetic", "testDivByZero", "()I", &[]);
    assert!(
        result.is_err(),
        "Division by zero should fail, got: {result:?}"
    );
}

#[test]
fn test_basic_exception_catch() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ExceptionTest", "testBasicCatch", "()V", &[]);
    assert!(result.is_ok(), "testBasicCatch failed: {result:?}");

    let printed_ints: Vec<i32> = vm
        .main_thread
        .printed
        .iter()
        .filter_map(|v| v.as_int())
        .collect();
    assert_eq!(printed_ints, vec![1, 2]);
}

#[test]
fn test_exception_propagation() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ExceptionTest", "testPropagation", "()V", &[]);
    assert!(result.is_ok(), "testPropagation failed: {result:?}");

    let printed_ints: Vec<i32> = vm
        .main_thread
        .printed
        .iter()
        .filter_map(|v| v.as_int())
        .collect();
    assert_eq!(printed_ints, vec![1, 3]);
}

#[test]
fn test_finally_block() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ExceptionTest", "testFinally", "()V", &[]);
    assert!(result.is_ok(), "testFinally failed: {result:?}");

    let printed_ints: Vec<i32> = vm
        .main_thread
        .printed
        .iter()
        .filter_map(|v| v.as_int())
        .collect();
    assert_eq!(printed_ints, vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// Phase 83: Pattern Matching Completeness
// ---------------------------------------------------------------------------

// -- 83.1: Primitive Patterns in Switch --

#[test]
fn test_pattern_switch_exact_match() {
    if !require_extended_interpreter_tests("test_pattern_switch_exact_match") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testExactMatch", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testExactMatch",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_pattern_switch_widening() {
    if !require_extended_interpreter_tests("test_pattern_switch_widening") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testWidening", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testWidening",
        "Int(107)",
        matches!(result, Ok(Some(Value::Int(107)))),
        &result,
    );
}

#[test]
fn test_pattern_switch_narrowing() {
    if !require_extended_interpreter_tests("test_pattern_switch_narrowing") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testNarrowing", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testNarrowing",
        "Int(15)",
        matches!(result, Ok(Some(Value::Int(15)))),
        &result,
    );
}

#[test]
fn test_pattern_switch_out_of_range() {
    if !require_extended_interpreter_tests("test_pattern_switch_out_of_range") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testOutOfRange", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testOutOfRange",
        "Int(314)",
        matches!(result, Ok(Some(Value::Int(314)))),
        &result,
    );
}

#[test]
fn test_pattern_switch_null() {
    if !require_extended_interpreter_tests("test_pattern_switch_null") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testNull", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testNull",
        "Int(99)",
        matches!(result, Ok(Some(Value::Int(99)))),
        &result,
    );
}

// -- 83.2: Record Patterns --

#[test]
fn test_record_pattern_simple() {
    if !require_extended_interpreter_tests("test_record_pattern_simple") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordPatterns", "testSimpleRecord", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordPatterns",
        "testSimpleRecord",
        "Int(7)",
        matches!(result, Ok(Some(Value::Int(7)))),
        &result,
    );
}

#[test]
fn test_record_pattern_nested() {
    if !require_extended_interpreter_tests("test_record_pattern_nested") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordPatterns", "testNestedRecord", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordPatterns",
        "testNestedRecord",
        "Int(10)",
        matches!(result, Ok(Some(Value::Int(10)))),
        &result,
    );
}

#[test]
fn test_record_pattern_with_guard() {
    if !require_extended_interpreter_tests("test_record_pattern_with_guard") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordPatterns", "testRecordWithGuard", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordPatterns",
        "testRecordWithGuard",
        "Int(2)",
        matches!(result, Ok(Some(Value::Int(2)))),
        &result,
    );
}

#[test]
fn test_record_pattern_null() {
    if !require_extended_interpreter_tests("test_record_pattern_null") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordPatterns", "testRecordNull", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordPatterns",
        "testRecordNull",
        "Int(77)",
        matches!(result, Ok(Some(Value::Int(77)))),
        &result,
    );
}

#[test]
fn test_record_pattern_in_box() {
    if !require_extended_interpreter_tests("test_record_pattern_in_box") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordPatterns", "testRecordInBox", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordPatterns",
        "testRecordInBox",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

// -- 83.3: Guard Expressions --

#[test]
fn test_guard_true() {
    if !require_extended_interpreter_tests("test_guard_true") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testGuardTrue", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testGuardTrue",
        "Int(43)",
        matches!(result, Ok(Some(Value::Int(43)))),
        &result,
    );
}

#[test]
fn test_guard_false() {
    if !require_extended_interpreter_tests("test_guard_false") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testGuardFalse", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testGuardFalse",
        "Int(103)",
        matches!(result, Ok(Some(Value::Int(103)))),
        &result,
    );
}

#[test]
fn test_guard_side_effect() {
    if !require_extended_interpreter_tests("test_guard_side_effect") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/PatternSwitch", "testGuardSideEffect", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/PatternSwitch",
        "testGuardSideEffect",
        "Int(56)",
        matches!(result, Ok(Some(Value::Int(56)))),
        &result,
    );
}

// -- 83.4: Sealed Class Exhaustiveness --

#[test]
fn test_sealed_exhaustive_circle() {
    if !require_extended_interpreter_tests("test_sealed_exhaustive_circle") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SealedSwitch", "testExhaustive", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SealedSwitch",
        "testExhaustive",
        "Int(5)",
        matches!(result, Ok(Some(Value::Int(5)))),
        &result,
    );
}

#[test]
fn test_sealed_exhaustive_rect() {
    if !require_extended_interpreter_tests("test_sealed_exhaustive_rect") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SealedSwitch", "testExhaustiveRect", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SealedSwitch",
        "testExhaustiveRect",
        "Int(12)",
        matches!(result, Ok(Some(Value::Int(12)))),
        &result,
    );
}

#[test]
fn test_sealed_with_default() {
    if !require_extended_interpreter_tests("test_sealed_with_default") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SealedSwitch", "testWithDefault", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SealedSwitch",
        "testWithDefault",
        "Int(12)",
        matches!(result, Ok(Some(Value::Int(12)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Phase 84: Records & Sealed Classes Runtime
// ---------------------------------------------------------------------------

// -- 84.1: Record Canonical Constructor & Accessors --

#[test]
fn test_record_canonical_ctor() {
    if !require_extended_interpreter_tests("test_record_canonical_ctor") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordRuntime", "testCanonicalCtor", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testCanonicalCtor",
        "Int(30)",
        matches!(result, Ok(Some(Value::Int(30)))),
        &result,
    );
}

#[test]
fn test_record_accessor_generation() {
    if !require_extended_interpreter_tests("test_record_accessor_generation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/RecordRuntime",
        "testAccessorGeneration",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testAccessorGeneration",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

// -- 84.2: Record equals/hashCode/toString --

#[test]
fn test_record_equals_true() {
    if !require_extended_interpreter_tests("test_record_equals_true") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordRuntime", "testEqualsTrue", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testEqualsTrue",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_record_equals_false() {
    if !require_extended_interpreter_tests("test_record_equals_false") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordRuntime", "testEqualsFalse", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testEqualsFalse",
        "Int(0)",
        matches!(result, Ok(Some(Value::Int(0)))),
        &result,
    );
}

#[test]
fn test_record_equals_null() {
    if !require_extended_interpreter_tests("test_record_equals_null") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordRuntime", "testEqualsNull", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testEqualsNull",
        "Int(0)",
        matches!(result, Ok(Some(Value::Int(0)))),
        &result,
    );
}

#[test]
fn test_record_hashcode_consistent() {
    if !require_extended_interpreter_tests("test_record_hashcode_consistent") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/RecordRuntime",
        "testHashCodeConsistent",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testHashCodeConsistent",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_record_hashcode_different() {
    if !require_extended_interpreter_tests("test_record_hashcode_different") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/RecordRuntime",
        "testHashCodeDifferent",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testHashCodeDifferent",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_record_tostring() {
    if !require_extended_interpreter_tests("test_record_tostring") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/RecordRuntime", "testToString", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/RecordRuntime",
        "testToString",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// -- 84.3: Sealed Class Verification --

#[test]
fn test_sealed_permitted_loads() {
    if !require_extended_interpreter_tests("test_sealed_permitted_loads") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/SealedVerify",
        "testPermittedSubclassLoads",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/SealedVerify",
        "testPermittedSubclassLoads",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_sealed_multiple_permitted() {
    if !require_extended_interpreter_tests("test_sealed_multiple_permitted") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SealedVerify", "testMultiplePermitted", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SealedVerify",
        "testMultiplePermitted",
        "Int(3)",
        matches!(result, Ok(Some(Value::Int(3)))),
        &result,
    );
}

#[test]
fn test_sealed_verify_with_default() {
    if !require_extended_interpreter_tests("test_sealed_verify_with_default") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SealedVerify", "testSealedWithDefault", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SealedVerify",
        "testSealedWithDefault",
        "Int(10)",
        matches!(result, Ok(Some(Value::Int(10)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Phase 88: Reflection Completeness
// ---------------------------------------------------------------------------

// -- 88.1: Method.invoke on User Bytecode --

#[test]
fn test_reflect_method_static() {
    if !require_extended_interpreter_tests("test_reflect_method_static") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectMethod", "testStaticMethod", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectMethod",
        "testStaticMethod",
        "Int(30)",
        matches!(result, Ok(Some(Value::Int(30)))),
        &result,
    );
}

#[test]
fn test_reflect_method_instance() {
    if !require_extended_interpreter_tests("test_reflect_method_instance") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectMethod", "testInstanceMethod", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectMethod",
        "testInstanceMethod",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_reflect_method_string_return() {
    if !require_extended_interpreter_tests("test_reflect_method_string_return") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectMethod", "testStringReturn", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectMethod",
        "testStringReturn",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_reflect_method_private_accessible() {
    if !require_extended_interpreter_tests("test_reflect_method_private_accessible") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectMethod",
        "testPrivateSetAccessible",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectMethod",
        "testPrivateSetAccessible",
        "Int(777)",
        matches!(result, Ok(Some(Value::Int(777)))),
        &result,
    );
}

#[test]
fn test_reflect_method_exception_wrapping() {
    if !require_extended_interpreter_tests("test_reflect_method_exception_wrapping") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectMethod",
        "testExceptionWrapping",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectMethod",
        "testExceptionWrapping",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// -- 88.2: Field Get/Set on User Classes --

#[test]
fn test_reflect_field_get_int() {
    if !require_extended_interpreter_tests("test_reflect_field_get_int") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectField", "testGetIntField", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectField",
        "testGetIntField",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_reflect_field_get_string() {
    if !require_extended_interpreter_tests("test_reflect_field_get_string") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectField", "testGetStringField", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectField",
        "testGetStringField",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_reflect_field_static() {
    if !require_extended_interpreter_tests("test_reflect_field_static") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectField", "testStaticField", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectField",
        "testStaticField",
        "Int(300)",
        matches!(result, Ok(Some(Value::Int(300)))),
        &result,
    );
}

#[test]
fn test_reflect_field_set_int() {
    if !require_extended_interpreter_tests("test_reflect_field_set_int") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectField", "testSetIntField", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectField",
        "testSetIntField",
        "Int(123)",
        matches!(result, Ok(Some(Value::Int(123)))),
        &result,
    );
}

// -- 88.3: Constructor.newInstance --

#[test]
fn test_reflect_constructor_noarg() {
    if !require_extended_interpreter_tests("test_reflect_constructor_noarg") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectConstructor",
        "testNoArgConstructor",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectConstructor",
        "testNoArgConstructor",
        "Int(10)",
        matches!(result, Ok(Some(Value::Int(10)))),
        &result,
    );
}

#[test]
fn test_reflect_constructor_param() {
    if !require_extended_interpreter_tests("test_reflect_constructor_param") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectConstructor",
        "testParamConstructor",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectConstructor",
        "testParamConstructor",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_reflect_constructor_exception() {
    if !require_extended_interpreter_tests("test_reflect_constructor_exception") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectConstructor",
        "testExceptionInConstructor",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectConstructor",
        "testExceptionInConstructor",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// -- 88.4: Annotation Runtime Retention --

#[test]
fn test_reflect_annotation_class() {
    if !require_extended_interpreter_tests("test_reflect_annotation_class") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectAnnotation",
        "testClassAnnotation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectAnnotation",
        "testClassAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_reflect_annotation_method() {
    if !require_extended_interpreter_tests("test_reflect_annotation_method") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectAnnotation",
        "testMethodAnnotation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectAnnotation",
        "testMethodAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_reflect_annotation_absent() {
    if !require_extended_interpreter_tests("test_reflect_annotation_absent") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectAnnotation", "testNoAnnotation", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectAnnotation",
        "testNoAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_reflect_annotation_is_present() {
    if !require_extended_interpreter_tests("test_reflect_annotation_is_present") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectAnnotation",
        "testIsAnnotationPresent",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectAnnotation",
        "testIsAnnotationPresent",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Phase 91: Serialization & ClassLoader
// ---------------------------------------------------------------------------

// 91.1+91.2: Simple object round-trip via OOS → byte[] → OIS
#[test]
fn test_serialize_simple_round_trip() {
    if !require_extended_interpreter_tests("test_serialize_simple_round_trip") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SerializeBasic", "testSimpleRoundTrip", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SerializeBasic",
        "testSimpleRoundTrip",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

// 91.1: Nested object serialization
#[test]
fn test_serialize_nested_object() {
    if !require_extended_interpreter_tests("test_serialize_nested_object") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SerializeBasic", "testNestedObject", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SerializeBasic",
        "testNestedObject",
        "Int(30)",
        matches!(result, Ok(Some(Value::Int(30)))),
        &result,
    );
}

// 91.1: Transient field is skipped during serialization
#[test]
fn test_serialize_transient_field() {
    if !require_extended_interpreter_tests("test_serialize_transient_field") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/SerializeBasic", "testTransientField", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/SerializeBasic",
        "testTransientField",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 91.1: Non-serializable class throws exception
#[test]
fn test_serialize_non_serializable_throws() {
    if !require_extended_interpreter_tests("test_serialize_non_serializable_throws") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/SerializeBasic",
        "testNonSerializableThrows",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/SerializeBasic",
        "testNonSerializableThrows",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 91.3: Thread.getContextClassLoader returns non-null
#[test]
fn test_context_class_loader() {
    if !require_extended_interpreter_tests("test_context_class_loader") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testContextClassLoader",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testContextClassLoader",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 91.3: Parent delegation chain
#[test]
fn test_parent_delegation() {
    if !require_extended_interpreter_tests("test_parent_delegation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testParentDelegation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testParentDelegation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 91.3: Set/get context class loader round-trip
#[test]
fn test_set_context_class_loader() {
    if !require_extended_interpreter_tests("test_set_context_class_loader") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testSetContextClassLoader",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testSetContextClassLoader",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 91.3: Class.getClassLoader for user classes
#[test]
fn test_class_get_class_loader() {
    if !require_extended_interpreter_tests("test_class_get_class_loader") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testClassGetClassLoader",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testClassGetClassLoader",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Session 3: ClassLoader Parent Delegation Fix
// ---------------------------------------------------------------------------

// Session 3: Bootstrap classes (Object, String) return null from getClassLoader()
#[test]
fn test_bootstrap_class_loader_is_null() {
    if !require_extended_interpreter_tests("test_bootstrap_class_loader_is_null") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testBootstrapClassLoaderIsNull",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testBootstrapClassLoaderIsNull",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_string_bootstrap_loader() {
    if !require_extended_interpreter_tests("test_string_bootstrap_loader") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testStringBootstrapLoader",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testStringBootstrapLoader",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: Loader name correctness
#[test]
fn test_loader_name() {
    if !require_extended_interpreter_tests("test_loader_name") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ClassLoaderTest", "testLoaderName", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testLoaderName",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_platform_loader_name() {
    if !require_extended_interpreter_tests("test_platform_loader_name") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testPlatformLoaderName",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testPlatformLoaderName",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: System class loader has correct parent chain (app → platform → null)
#[test]
fn test_system_class_loader_chain() {
    if !require_extended_interpreter_tests("test_system_class_loader_chain") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testSystemClassLoaderChain",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testSystemClassLoaderChain",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: loadClass delegation to parent works for bootstrap classes
#[test]
fn test_load_class_delegation() {
    if !require_extended_interpreter_tests("test_load_class_delegation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testLoadClassDelegation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testLoadClassDelegation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: loadClass works for user-defined classes
#[test]
fn test_load_class_for_user_class() {
    if !require_extended_interpreter_tests("test_load_class_for_user_class") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testLoadClassForUserClass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testLoadClassForUserClass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: getClassLoader() returns same object each call (singleton identity)
#[test]
fn test_class_loader_identity() {
    if !require_extended_interpreter_tests("test_class_loader_identity") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testClassLoaderIdentity",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testClassLoaderIdentity",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: getSystemClassLoader() returns same instance each call
#[test]
fn test_system_class_loader_identity() {
    if !require_extended_interpreter_tests("test_system_class_loader_identity") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testSystemClassLoaderIdentity",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testSystemClassLoaderIdentity",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 3: Loader isolation — bootstrap classes vs app classes have different loaders
#[test]
fn test_loader_isolation() {
    if !require_extended_interpreter_tests("test_loader_isolation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testLoaderIsolation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testLoaderIsolation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Regression (SC-custom-classloader-ignored): a custom loader that overrides
// loadClass(String,boolean) must have that override invoked by loadClass(String),
// and a fresh custom loader's findLoadedClass must be loader-scoped (return null
// for a class only the app loader has loaded). Previously loadClass reimplemented
// parent-first delegation in Rust and resolved through the global/app class store,
// ignoring the user loader entirely.
#[test]
fn test_custom_loader_override_invoked() {
    if !require_extended_interpreter_tests("test_custom_loader_override_invoked") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testCustomLoaderOverrideInvoked",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testCustomLoaderOverrideInvoked",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Regression (SC-custom-classloader-ignored): Class.forName(name, false, loader)
// must route through the user loader's loadClass override, not the global store.
#[test]
fn test_for_name_honors_custom_loader_override() {
    if !require_extended_interpreter_tests("test_for_name_honors_custom_loader_override") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ClassLoaderTest",
        "testForNameHonorsCustomLoaderOverride",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ClassLoaderTest",
        "testForNameHonorsCustomLoaderOverride",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Session 4: MethodHandle and VarHandle Completeness
// ---------------------------------------------------------------------------

// Session 4: Static method invocation via MethodHandle
#[test]
fn test_method_handle_static() {
    if !require_extended_interpreter_tests("test_method_handle_static") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testStaticMethodHandle",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testStaticMethodHandle",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: Virtual method invocation via MethodHandle
#[test]
fn test_method_handle_virtual() {
    if !require_extended_interpreter_tests("test_method_handle_virtual") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testVirtualMethodHandle",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testVirtualMethodHandle",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: Constructor invocation via MethodHandle
#[test]
fn test_method_handle_constructor() {
    if !require_extended_interpreter_tests("test_method_handle_constructor") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testConstructorMethodHandle",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testConstructorMethodHandle",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: MethodHandle.bindTo bound receiver
#[test]
fn test_method_handle_bind_to() {
    if !require_extended_interpreter_tests("test_method_handle_bind_to") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/MethodHandleTest", "testBindTo", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testBindTo",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: Lookup.in(targetClass)
#[test]
fn test_lookup_in() {
    if !require_extended_interpreter_tests("test_lookup_in") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/MethodHandleTest", "testLookupIn", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testLookupIn",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: VarHandle.get() and VarHandle.set() for instance fields
#[test]
fn test_var_handle_get_set() {
    if !require_extended_interpreter_tests("test_var_handle_get_set") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testVarHandleGetSet",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testVarHandleGetSet",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: VarHandle.compareAndSet()
#[test]
fn test_var_handle_cas() {
    if !require_extended_interpreter_tests("test_var_handle_cas") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/MethodHandleTest", "testVarHandleCAS", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testVarHandleCAS",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Session 4: MethodHandle.type() returns valid MethodType
#[test]
fn test_method_handle_type() {
    if !require_extended_interpreter_tests("test_method_handle_type") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testMethodHandleType",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testMethodHandleType",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// WP1.6 acceptance: MethodHandle.invokeExact strict-arity round-trip.
// `findVirtual` + `bindTo` + `invokeExact` is the literal acceptance text
// in `gaps/wildfly-ejbca-roadmap.md` Wave 1 §WP1.6. The pre-existing
// `test_method_handle_bind_to` exercises the loose `invoke` path; this
// closes the gap by driving the signature-polymorphic strict-arity path.
#[test]
fn test_method_handle_invoke_exact_round_trip() {
    if !require_extended_interpreter_tests("test_method_handle_invoke_exact_round_trip") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testInvokeExactRoundTrip",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testInvokeExactRoundTrip",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// WP1.6 acceptance: VarHandle.getAcquire / setRelease round-trip on a
// regular int field. Roadmap §WP1.6 calls out
// "VarHandle.acquire/release on volatile int field of a regular class"
// — this drives the access mode through the VarHandle native dispatch
// in `native-builtins/src/lang_invoke.rs::lang_invoke.rs:521-558`.
#[test]
fn test_var_handle_acquire_release() {
    if !require_extended_interpreter_tests("test_var_handle_acquire_release") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/MethodHandleTest",
        "testVarHandleAcquireRelease",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/MethodHandleTest",
        "testVarHandleAcquireRelease",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// ---------------------------------------------------------------------------
// Phase 97: OpenJDK TCK Preparation
// ---------------------------------------------------------------------------

// 97.1: JTReg directive parsing — compile test
#[test]
fn test_jtreg_parse_compile() {
    if !require_extended_interpreter_tests("test_jtreg_parse_compile") {
        return;
    }
    use cratonvm_vm::runtime::tck::{JtregDirective, JtregRunMode, JtregTestDescriptor};

    let source = r#"
/*
 * @test
 * @summary Tests basic compilation
 * @compile TestCompile.java
 * @run main TestCompile
 */
public class TestCompile {
    public static void main(String[] args) {
        System.out.println("OK");
    }
}
"#;
    let desc = JtregTestDescriptor::parse("TestCompile.java", source);
    assert!(desc.is_test, "Should detect @test");
    assert_eq!(desc.summary.as_deref(), Some("Tests basic compilation"));
    assert!(desc.compile_files.contains(&"TestCompile.java".to_string()));
    assert_eq!(desc.main_class.as_deref(), Some("TestCompile"));
    // Check that Run directive was parsed
    let has_run = desc.directives.iter().any(|d| {
        matches!(d,
            JtregDirective::Run { mode: JtregRunMode::Main, class_name, .. }
            if class_name == "TestCompile"
        )
    });
    assert!(has_run, "Should have @run main TestCompile directive");
}

// 97.1: JTReg directive parsing — run test with othervm
#[test]
fn test_jtreg_parse_run() {
    if !require_extended_interpreter_tests("test_jtreg_parse_run") {
        return;
    }
    use cratonvm_vm::runtime::tck::{JtregDirective, JtregRunMode, JtregTestDescriptor};

    let source = r#"
/* @test
 * @bug 1234567
 * @summary Tests othervm mode
 * @run main/othervm -Xmx256m TestOther arg1 arg2
 */
public class TestOther {
    public static void main(String[] args) {}
}
"#;
    let desc = JtregTestDescriptor::parse("TestOther.java", source);
    assert!(desc.is_test);
    // Check for othervm mode
    let has_othervm = desc.directives.iter().any(|d| {
        matches!(
            d,
            JtregDirective::Run {
                mode: JtregRunMode::OtherVm,
                ..
            }
        )
    });
    assert!(has_othervm, "Should detect main/othervm mode");
    // Check bug directive
    let has_bug = desc.directives.iter().any(|d| {
        matches!(d,
            JtregDirective::Bug(id) if id == "1234567"
        )
    });
    assert!(has_bug, "Should parse @bug directive");
}

// 97.1: JTReg output comparison
#[test]
fn test_jtreg_compare_output() {
    if !require_extended_interpreter_tests("test_jtreg_compare_output") {
        return;
    }
    use cratonvm_vm::runtime::tck::compare_output;

    let actual = vec!["hello".to_string(), "world".to_string()];
    let expected = vec!["hello".to_string(), "world".to_string()];
    assert!(compare_output(&actual, &expected));

    let mismatched = vec!["hello".to_string(), "WORLD".to_string()];
    assert!(!compare_output(&actual, &mismatched));

    let short = vec!["hello".to_string()];
    assert!(!compare_output(&actual, &short));

    // Trimming whitespace
    let padded = vec!["  hello  ".to_string(), " world ".to_string()];
    assert!(compare_output(&padded, &expected));
}

// 97.2: TCK Chapter 4 — Class file format (magic, version, constant pool, fields, methods)
#[test]
fn test_tck_class_file_magic() {
    if !require_extended_interpreter_tests("test_tck_class_file_magic") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckClassFile", "testMagicNumber", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckClassFile",
        "testMagicNumber",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_class_file_version() {
    if !require_extended_interpreter_tests("test_tck_class_file_version") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckClassFile", "testClassVersion", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckClassFile",
        "testClassVersion",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_class_file_constant_pool() {
    if !require_extended_interpreter_tests("test_tck_class_file_constant_pool") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckClassFile", "testConstantPool", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckClassFile",
        "testConstantPool",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_class_file_field_access() {
    if !require_extended_interpreter_tests("test_tck_class_file_field_access") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckClassFile", "testFieldAccess", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckClassFile",
        "testFieldAccess",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_class_file_method_access() {
    if !require_extended_interpreter_tests("test_tck_class_file_method_access") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckClassFile", "testMethodAccess", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckClassFile",
        "testMethodAccess",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 97.2: TCK Chapter 5 — Loading, Linking, Initialization
#[test]
fn test_tck_loading_class_loading() {
    if !require_extended_interpreter_tests("test_tck_loading_class_loading") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckLoading", "testClassLoading", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckLoading",
        "testClassLoading",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_loading_static_init() {
    if !require_extended_interpreter_tests("test_tck_loading_static_init") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckLoading", "testStaticInit", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckLoading",
        "testStaticInit",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_loading_interface_init() {
    if !require_extended_interpreter_tests("test_tck_loading_interface_init") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckLoading", "testInterfaceInit", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckLoading",
        "testInterfaceInit",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_loading_array_creation() {
    if !require_extended_interpreter_tests("test_tck_loading_array_creation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckLoading", "testArrayCreation", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckLoading",
        "testArrayCreation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_loading_inheritance() {
    if !require_extended_interpreter_tests("test_tck_loading_inheritance") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckLoading", "testInheritance", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckLoading",
        "testInheritance",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// 97.2: TCK Chapter 6 — Instruction Set
#[test]
fn test_tck_instructions_int_arithmetic() {
    if !require_extended_interpreter_tests("test_tck_instructions_int_arithmetic") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testIntArithmetic", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testIntArithmetic",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_long_arithmetic() {
    if !require_extended_interpreter_tests("test_tck_instructions_long_arithmetic") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testLongArithmetic", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testLongArithmetic",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_float_arithmetic() {
    if !require_extended_interpreter_tests("test_tck_instructions_float_arithmetic") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/TckInstructions",
        "testFloatArithmetic",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testFloatArithmetic",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_comparisons() {
    if !require_extended_interpreter_tests("test_tck_instructions_comparisons") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testComparisons", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testComparisons",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_tableswitch() {
    if !require_extended_interpreter_tests("test_tck_instructions_tableswitch") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testTableswitch", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testTableswitch",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_lookupswitch() {
    if !require_extended_interpreter_tests("test_tck_instructions_lookupswitch") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testLookupswitch", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testLookupswitch",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_field_ops() {
    if !require_extended_interpreter_tests("test_tck_instructions_field_ops") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testFieldOps", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testFieldOps",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_array_ops() {
    if !require_extended_interpreter_tests("test_tck_instructions_array_ops") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testArrayOps", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testArrayOps",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_invoke_virtual() {
    if !require_extended_interpreter_tests("test_tck_instructions_invoke_virtual") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testInvokeVirtual", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testInvokeVirtual",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_invoke_static() {
    if !require_extended_interpreter_tests("test_tck_instructions_invoke_static") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testInvokeStatic", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testInvokeStatic",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_exception_handling() {
    if !require_extended_interpreter_tests("test_tck_instructions_exception_handling") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/TckInstructions",
        "testExceptionHandling",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testExceptionHandling",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_checkcast() {
    if !require_extended_interpreter_tests("test_tck_instructions_checkcast") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testCheckcast", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testCheckcast",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_tck_instructions_instanceof() {
    if !require_extended_interpreter_tests("test_tck_instructions_instanceof") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/TckInstructions", "testInstanceof", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/TckInstructions",
        "testInstanceof",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// =========================================================================
// Session 17: Reflection Completeness
// =========================================================================

#[test]
fn test_s17_method_invoke_private() {
    if !require_extended_interpreter_tests("test_s17_method_invoke_private") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodInvokePrivateViaReflection",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodInvokePrivateViaReflection",
        "Int(49)",
        matches!(result, Ok(Some(Value::Int(49)))),
        &result,
    );
}

#[test]
fn test_s17_method_invoke_instance() {
    if !require_extended_interpreter_tests("test_s17_method_invoke_instance") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodInvokeInstance",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodInvokeInstance",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_s17_method_invoke_type_coercion() {
    if !require_extended_interpreter_tests("test_s17_method_invoke_type_coercion") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodInvokeTypeCoercion",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodInvokeTypeCoercion",
        "Int(36)",
        matches!(result, Ok(Some(Value::Int(36)))),
        &result,
    );
}

#[test]
fn test_s17_field_get_private() {
    if !require_extended_interpreter_tests("test_s17_field_get_private") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testFieldGetPrivate",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldGetPrivate",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_s17_field_get_own_private_final_reference_without_set_accessible() {
    if !require_extended_interpreter_tests(
        "test_s17_field_get_own_private_final_reference_without_set_accessible",
    ) {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_s17_field_set_private() {
    if !require_extended_interpreter_tests("test_s17_field_set_private") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testFieldSetPrivate",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldSetPrivate",
        "Int(999)",
        matches!(result, Ok(Some(Value::Int(999)))),
        &result,
    );
}

#[test]
fn test_s17_field_static_get_set() {
    if !require_extended_interpreter_tests("test_s17_field_static_get_set") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testFieldStaticGetSet",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldStaticGetSet",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_s17_constructor_noarg() {
    if !require_extended_interpreter_tests("test_s17_constructor_noarg") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testConstructorNoArg",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testConstructorNoArg",
        "Int(0)",
        matches!(result, Ok(Some(Value::Int(0)))),
        &result,
    );
}

#[test]
fn test_s17_constructor_with_args() {
    if !require_extended_interpreter_tests("test_s17_constructor_with_args") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testConstructorWithArgs",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testConstructorWithArgs",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_s17_constructor_set_accessible() {
    if !require_extended_interpreter_tests("test_s17_constructor_set_accessible") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testConstructorSetAccessible",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testConstructorSetAccessible",
        "Int(300)",
        matches!(result, Ok(Some(Value::Int(300)))),
        &result,
    );
}

#[test]
fn test_s17_proxy_basic() {
    if !require_extended_interpreter_tests("test_s17_proxy_basic") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectionComplete", "testProxyBasic", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testProxyBasic",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

#[test]
fn test_s17_proxy_is_proxy_class() {
    if !require_extended_interpreter_tests("test_s17_proxy_is_proxy_class") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testProxyIsProxyClass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testProxyIsProxyClass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_proxy_get_handler() {
    if !require_extended_interpreter_tests("test_s17_proxy_get_handler") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testProxyGetHandler",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testProxyGetHandler",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_get_declared_methods() {
    if !require_extended_interpreter_tests("test_s17_get_declared_methods") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testGetDeclaredMethods",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testGetDeclaredMethods",
        "Int(0)",
        matches!(result, Ok(Some(Value::Int(0)))),
        &result,
    );
}

#[test]
fn test_s17_get_declared_fields() {
    if !require_extended_interpreter_tests("test_s17_get_declared_fields") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testGetDeclaredFields",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testGetDeclaredFields",
        "Int(2)",
        matches!(result, Ok(Some(Value::Int(2)))),
        &result,
    );
}

#[test]
fn test_s17_get_declared_constructors() {
    if !require_extended_interpreter_tests("test_s17_get_declared_constructors") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testGetDeclaredConstructors",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testGetDeclaredConstructors",
        "Int(2)",
        matches!(result, Ok(Some(Value::Int(2)))),
        &result,
    );
}

#[test]
fn test_s17_get_declared_method_by_name() {
    if !require_extended_interpreter_tests("test_s17_get_declared_method_by_name") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testGetDeclaredMethodByName",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testGetDeclaredMethodByName",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_method_modifiers() {
    if !require_extended_interpreter_tests("test_s17_method_modifiers") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodModifiers",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodModifiers",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_field_modifiers() {
    if !require_extended_interpreter_tests("test_s17_field_modifiers") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testFieldModifiers",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldModifiers",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_method_return_type() {
    if !require_extended_interpreter_tests("test_s17_method_return_type") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodReturnType",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodReturnType",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_method_parameter_types() {
    if !require_extended_interpreter_tests("test_s17_method_parameter_types") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodParameterTypes",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodParameterTypes",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_method_parameter_count() {
    if !require_extended_interpreter_tests("test_s17_method_parameter_count") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodParameterCount",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodParameterCount",
        "Int(2)",
        matches!(result, Ok(Some(Value::Int(2)))),
        &result,
    );
}

#[test]
fn test_s17_field_type() {
    if !require_extended_interpreter_tests("test_s17_field_type") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ReflectionComplete", "testFieldType", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldType",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_field_declaring_class() {
    if !require_extended_interpreter_tests("test_s17_field_declaring_class") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testFieldDeclaringClass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testFieldDeclaringClass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s17_method_declaring_class() {
    if !require_extended_interpreter_tests("test_s17_method_declaring_class") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ReflectionComplete",
        "testMethodDeclaringClass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/ReflectionComplete",
        "testMethodDeclaringClass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// ============================================================
// Session 18: Annotation Processing
// ============================================================

#[test]
fn test_s18_custom_annotation_values() {
    if !require_extended_interpreter_tests("test_s18_custom_annotation_values") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testCustomAnnotationValues",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testCustomAnnotationValues",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_inherited_annotation() {
    if !require_extended_interpreter_tests("test_s18_inherited_annotation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testInheritedAnnotation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testInheritedAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_non_inherited_not_present() {
    if !require_extended_interpreter_tests("test_s18_non_inherited_not_present") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testNonInheritedNotPresent",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testNonInheritedNotPresent",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_get_inherited_annotation() {
    if !require_extended_interpreter_tests("test_s18_get_inherited_annotation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testGetInheritedAnnotation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testGetInheritedAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_declared_annotations_no_inherited() {
    if !require_extended_interpreter_tests("test_s18_declared_annotations_no_inherited") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testDeclaredAnnotationsNoInherited",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testDeclaredAnnotationsNoInherited",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_overriding_inherited_annotation() {
    if !require_extended_interpreter_tests("test_s18_overriding_inherited_annotation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testOverridingInheritedAnnotation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testOverridingInheritedAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_method_annotation_present() {
    if !require_extended_interpreter_tests("test_s18_method_annotation_present") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testMethodAnnotationPresent",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testMethodAnnotationPresent",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_method_annotation_identity() {
    if !require_extended_interpreter_tests("test_s18_method_annotation_identity") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testMethodAnnotationIdentity",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testMethodAnnotationIdentity",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_method_no_annotation() {
    if !require_extended_interpreter_tests("test_s18_method_no_annotation") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testMethodNoAnnotation",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testMethodNoAnnotation",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_parameter_annotation_count() {
    if !require_extended_interpreter_tests("test_s18_parameter_annotation_count") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testParameterAnnotationCount",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testParameterAnnotationCount",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s18_parameter_annotation_empty() {
    if !require_extended_interpreter_tests("test_s18_parameter_annotation_empty") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/AnnotationTest",
        "testParameterAnnotationEmpty",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/AnnotationTest",
        "testParameterAnnotationEmpty",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// ============================================================
// Session 19: Generics and Type Erasure Support
// ============================================================

#[test]
fn test_s19_class_type_params() {
    if !require_extended_interpreter_tests("test_s19_class_type_params") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testClassTypeParams",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testClassTypeParams",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_multiple_type_params() {
    if !require_extended_interpreter_tests("test_s19_multiple_type_params") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testMultipleTypeParams",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testMultipleTypeParams",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_bounded_type_param() {
    if !require_extended_interpreter_tests("test_s19_bounded_type_param") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testBoundedTypeParam",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testBoundedTypeParam",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_generic_superclass() {
    if !require_extended_interpreter_tests("test_s19_generic_superclass") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testGenericSuperclass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testGenericSuperclass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_non_generic_superclass() {
    if !require_extended_interpreter_tests("test_s19_non_generic_superclass") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testNonGenericSuperclass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testNonGenericSuperclass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_method_type_params() {
    if !require_extended_interpreter_tests("test_s19_method_type_params") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testMethodTypeParams",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testMethodTypeParams",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_method_generic_return_type() {
    if !require_extended_interpreter_tests("test_s19_method_generic_return_type") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testMethodGenericReturnType",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testMethodGenericReturnType",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_method_generic_param_types() {
    if !require_extended_interpreter_tests("test_s19_method_generic_param_types") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testMethodGenericParamTypes",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testMethodGenericParamTypes",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_field_generic_type() {
    if !require_extended_interpreter_tests("test_s19_field_generic_type") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testFieldGenericType",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testFieldGenericType",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

#[test]
fn test_s19_no_type_params() {
    if !require_extended_interpreter_tests("test_s19_no_type_params") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testNoTypeParams",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testNoTypeParams",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// WP2.8 acceptance: getGenericSuperclass on a class extending
// ArrayList<String> returns a real ParameterizedType whose raw type is
// ArrayList and getActualTypeArguments()[0] is String.class. This is the
// load-bearing roadmap acceptance criterion (Jackson List<User> /
// Hibernate List<OrderLine> patterns).
#[test]
fn test_s19_parameterized_superclass() {
    if !require_extended_interpreter_tests("test_s19_parameterized_superclass") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testParameterizedSuperclass",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testParameterizedSuperclass",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// WP2.8 acceptance: Field.getGenericType for a List<String> field
// materializes a ParameterizedType with the right raw + args.
#[test]
fn test_s19_parameterized_field() {
    if !require_extended_interpreter_tests("test_s19_parameterized_field") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testParameterizedField",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testParameterizedField",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// WP2.8 acceptance: two-argument ParameterizedType (Map<String, Integer>)
// — both type arguments must round-trip in declaration order.
#[test]
fn test_s19_two_arg_parameterized_field() {
    if !require_extended_interpreter_tests("test_s19_two_arg_parameterized_field") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testTwoArgParameterizedField",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testTwoArgParameterizedField",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// WP2.8 acceptance: wildcard with upper bound (`? extends Number`)
// materializes as a WildcardType whose getUpperBounds()[0] is Number.class.
#[test]
fn test_s19_wildcard_extends_number() {
    if !require_extended_interpreter_tests("test_s19_wildcard_extends_number") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testWildcardExtendsNumber",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testWildcardExtendsNumber",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// Spring GenericTypeResolver regression: same-named type variables declared by
// different interfaces must compare by their declaring GenericDeclaration.
#[test]
fn test_s19_same_named_interface_type_variables() {
    if !require_extended_interpreter_tests("test_s19_same_named_interface_type_variables") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/GenericReflectionTest",
        "testSameNamedInterfaceTypeVariables",
        "()I",
        &[],
    );
    corpus_check(
        &vm,
        "cratonvm/GenericReflectionTest",
        "testSameNamedInterfaceTypeVariables",
        "Int(1)",
        matches!(result, Ok(Some(Value::Int(1)))),
        &result,
    );
}

// =========================================================================
// Session 20: Full java.util.stream Support
// =========================================================================

macro_rules! s20_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/StreamComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/StreamComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s20_test!(
    test_s20_filter_map_collect_to_list,
    "testFilterMapCollectToList",
    3
);
s20_test!(test_s20_stream_of_count, "testStreamOfCount", 5);
s20_test!(test_s20_reduce_with_identity, "testReduceWithIdentity", 15);
s20_test!(
    test_s20_reduce_with_generic_accumulator,
    "testReduceWithGenericAccumulator",
    1
);
s20_test!(test_s20_for_each, "testForEach", 60);
s20_test!(test_s20_collect_to_set, "testCollectToSet", 3);
s20_test!(test_s20_find_first, "testFindFirst", 10);
s20_test!(test_s20_any_match, "testAnyMatch", 1);
s20_test!(test_s20_all_match, "testAllMatch", 1);
s20_test!(test_s20_none_match, "testNoneMatch", 1);
s20_test!(test_s20_sorted, "testSorted", 1134);
s20_test!(test_s20_distinct, "testDistinct", 3);
s20_test!(test_s20_limit_skip, "testLimitSkip", 12);
s20_test!(test_s20_to_array, "testToArray", 3);
s20_test!(test_s20_flat_map, "testFlatMap", 10);
s20_test!(test_s20_collectors_joining, "testCollectorsJoining", 1);
s20_test!(test_s20_collectors_to_map, "testCollectorsToMap", 5);
s20_test!(
    test_s20_collectors_grouping_by,
    "testCollectorsGroupingBy",
    32
);
s20_test!(test_s20_stream_empty, "testStreamEmpty", 0);
s20_test!(test_s20_reduce_optional, "testReduceOptional", 10);
s20_test!(test_s20_grouping_by_counting, "testGroupingByCounting", 3);
s20_test!(test_s20_stream_concat, "testStreamConcat", 4);
s20_test!(test_s20_stream_to_list, "testStreamToList", 2);
s20_test!(test_s20_min_max, "testMinMax", 15);
s20_test!(test_s20_peek, "testPeek", 33);
s20_test!(test_s20_chained_pipeline, "testChainedPipeline", 56);
s20_test!(test_s20_partitioning_by, "testPartitioningBy", 23);
s20_test!(test_s20_int_stream_range_sum, "testIntStreamRangeSum", 15);
s20_test!(test_s20_map_to_int_sum, "testMapToIntSum", 10);
s20_test!(test_s20_stream_of_single, "testStreamOfSingle", 1);
s20_test!(test_s20_parallel_stream, "testParallelStream", 1);

// ===========================================================================
// Session 21: String Concat and Formatting
// ===========================================================================

macro_rules! s21_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/StringFormatComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/StringFormatComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s21_test!(test_s21_string_concat, "testStringConcat", 1);
s21_test!(
    test_s21_concat_with_primitives,
    "testConcatWithPrimitives",
    1
);
s21_test!(test_s21_concat_with_null, "testConcatWithNull", 1);
s21_test!(test_s21_format_string, "testFormatString", 1);
s21_test!(test_s21_format_int, "testFormatInt", 1);
s21_test!(test_s21_format_float, "testFormatFloat", 1);
s21_test!(test_s21_format_hex, "testFormatHex", 1);
s21_test!(test_s21_format_octal, "testFormatOctal", 1);
s21_test!(test_s21_format_boolean, "testFormatBoolean", 1);
s21_test!(test_s21_format_char, "testFormatChar", 1);
s21_test!(test_s21_format_width, "testFormatWidth", 1);
s21_test!(test_s21_format_left_justify, "testFormatLeftJustify", 1);
s21_test!(test_s21_format_zero_pad, "testFormatZeroPad", 1);
s21_test!(test_s21_format_percent, "testFormatPercent", 1);
s21_test!(test_s21_format_newline, "testFormatNewline", 1);
s21_test!(test_s21_format_multiple_args, "testFormatMultipleArgs", 1);
s21_test!(test_s21_format_scientific, "testFormatScientific", 1);
s21_test!(test_s21_format_upper_hex, "testFormatUpperHex", 1);
s21_test!(test_s21_format_formatted, "testStringFormatted", 1);
s21_test!(test_s21_formatter_object, "testFormatterObject", 1);
s21_test!(test_s21_formatter_append, "testFormatterAppend", 1);
s21_test!(test_s21_message_format, "testMessageFormat", 1);
s21_test!(
    test_s21_message_format_multiple,
    "testMessageFormatMultiple",
    1
);
s21_test!(test_s21_decimal_format_basic, "testDecimalFormatBasic", 1);
s21_test!(
    test_s21_decimal_format_integer,
    "testDecimalFormatInteger",
    1
);
s21_test!(
    test_s21_decimal_format_no_grouping,
    "testDecimalFormatNoGrouping",
    1
);
s21_test!(test_s21_concat_with_char, "testConcatWithChar", 1);
s21_test!(test_s21_format_plus_sign, "testFormatPlusSign", 1);
s21_test!(test_s21_concat_in_loop, "testConcatInLoop", 1);
s21_test!(
    test_s21_string_builder_with_format,
    "testStringBuilderWithFormat",
    1
);

// ===========================================================================
// Session 22: Properties and Resource Loading
// ===========================================================================

macro_rules! s22_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/PropertiesComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/PropertiesComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s22_test!(test_s22_os_name, "testOsName", 1);
s22_test!(test_s22_file_separator, "testFileSeparator", 1);
s22_test!(test_s22_line_separator, "testLineSeparator", 1);
s22_test!(test_s22_path_separator, "testPathSeparator", 1);
s22_test!(test_s22_user_dir, "testUserDir", 1);
s22_test!(test_s22_user_home, "testUserHome", 1);
s22_test!(test_s22_file_encoding, "testFileEncoding", 1);
s22_test!(test_s22_java_version, "testJavaVersion", 1);
s22_test!(test_s22_java_vendor, "testJavaVendor", 1);
s22_test!(test_s22_get_property_default, "testGetPropertyDefault", 1);
s22_test!(test_s22_get_property_null, "testGetPropertyNull", 1);
s22_test!(test_s22_set_property, "testSetProperty", 1);
s22_test!(
    test_s22_set_property_returns_old,
    "testSetPropertyReturnsOld",
    1
);
s22_test!(test_s22_properties_basic, "testPropertiesBasic", 1);
s22_test!(test_s22_properties_default, "testPropertiesDefault", 1);
s22_test!(test_s22_properties_load, "testPropertiesLoad", 1);
s22_test!(
    test_s22_properties_load_comments,
    "testPropertiesLoadComments",
    1
);
s22_test!(test_s22_properties_load_colon, "testPropertiesLoadColon", 1);
s22_test!(test_s22_properties_size, "testPropertiesSize", 3);
s22_test!(
    test_s22_properties_contains_key,
    "testPropertiesContainsKey",
    1
);
s22_test!(test_s22_properties_remove, "testPropertiesRemove", 1);
s22_test!(test_s22_properties_overwrite, "testPropertiesOverwrite", 1);
s22_test!(test_s22_properties_empty, "testPropertiesEmpty", 1);
s22_test!(test_s22_properties_clear, "testPropertiesClear", 1);
s22_test!(test_s22_system_line_separator, "testSystemLineSeparator", 1);
s22_test!(test_s22_tmp_dir, "testTmpDir", 1);
s22_test!(test_s22_properties_load_empty, "testPropertiesLoadEmpty", 1);
s22_test!(
    test_s22_properties_load_spaces,
    "testPropertiesLoadSpaces",
    1
);
s22_test!(test_s22_get_env, "testGetEnv", 1);
s22_test!(test_s22_get_env_missing, "testGetEnvMissing", 1);
s22_test!(
    test_s22_plain_properties_get_ignores_system_properties,
    "testPlainPropertiesGetIgnoresSystemProperties",
    1
);
s22_test!(
    test_s22_string_split_whitespace_comma_regex,
    "testStringSplitWhitespaceCommaRegex",
    1
);

// ===========================================================================
// Session 23: Java Memory Model Compliance
// ===========================================================================

macro_rules! s23_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/MemoryModelTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/MemoryModelTest",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
}

#[test]
fn test_s23_thread_basic() {
    if !require_extended_interpreter_tests("test_s23_thread_basic") {
        return;
    }
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ThreadBasicTest", "testThreadBasic", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/ThreadBasicTest",
        "testThreadBasic",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

s23_test!(test_s23_volatile_visibility, "testVolatileVisibility");
s23_test!(test_s23_join_happens_before, "testJoinHappensBefore");
s23_test!(test_s23_start_happens_before, "testStartHappensBefore");
s23_test!(
    test_s23_synchronized_happens_before,
    "testSynchronizedHappensBefore"
);
s23_test!(test_s23_double_checked_locking, "testDoubleCheckedLocking");
s23_test!(
    test_s23_dekker_mutual_exclusion,
    "testDekkerMutualExclusion"
);
s23_test!(test_s23_volatile_counter, "testVolatileCounter");
s23_test!(test_s23_monitor_wait_notify, "testMonitorWaitNotify");
s23_test!(test_s23_synchronized_counter, "testSynchronizedCounter");
s23_test!(test_s23_volatile_store_load, "testVolatileStoreLoad");

// ===========================================================================
// Session 23b: JMM Complete (JmmComplete.java) — 30 tests
// ===========================================================================

macro_rules! s23b_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/JmmComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/JmmComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s23b_test!(test_s23b_thread_start_join, "testThreadStartJoin", 42);
s23b_test!(
    test_s23b_multiple_threads_join,
    "testMultipleThreadsJoin",
    30
);
s23b_test!(test_s23b_volatile_visibility, "testVolatileVisibility", 1);
s23b_test!(test_s23b_synchronized_mutex, "testSynchronizedMutex", 500);
s23b_test!(test_s23b_current_thread, "testCurrentThread", 1);
s23b_test!(test_s23b_thread_sleep, "testThreadSleep", 1);
s23b_test!(test_s23b_atomic_integer_basic, "testAtomicIntegerBasic", 10);
s23b_test!(test_s23b_atomic_integer_cas, "testAtomicIntegerCAS", 1);
s23b_test!(
    test_s23b_atomic_integer_increment,
    "testAtomicIntegerIncrement",
    6
);
s23b_test!(
    test_s23b_atomic_integer_get_and_add,
    "testAtomicIntegerGetAndAdd",
    25
);
s23b_test!(test_s23b_atomic_boolean, "testAtomicBoolean", 1);
s23b_test!(test_s23b_atomic_long, "testAtomicLong", 1);
s23b_test!(test_s23b_synchronized_method, "testSynchronizedMethod", 150);
s23b_test!(test_s23b_thread_computation, "testThreadComputation", 55);
s23b_test!(
    test_s23b_atomic_concurrent_increment,
    "testAtomicConcurrentIncrement",
    100
);
s23b_test!(
    test_s23b_double_checked_locking,
    "testDoubleCheckedLocking",
    1
);
s23b_test!(test_s23b_wait_notify, "testWaitNotify", 1);
s23b_test!(test_s23b_thread_name, "testThreadName", 1);
s23b_test!(test_s23b_thread_is_alive, "testThreadIsAlive", 1);
s23b_test!(test_s23b_volatile_ordering, "testVolatileOrdering", 1);
s23b_test!(test_s23b_atomic_reference, "testAtomicReference", 1);
s23b_test!(test_s23b_reentrant_sync, "testReentrantSync", 3);
s23b_test!(test_s23b_thread_with_runnable, "testThreadWithRunnable", 77);
s23b_test!(test_s23b_atomic_decrement, "testAtomicDecrement", 17);
s23b_test!(test_s23b_nano_time_monotonic, "testNanoTimeMonotonic", 1);
s23b_test!(test_s23b_volatile_array, "testVolatileArray", 99);
s23b_test!(test_s23b_atomic_get_and_set, "testAtomicGetAndSet", 142);
s23b_test!(test_s23b_join_timeout, "testJoinTimeout", 1);
s23b_test!(test_s23b_sync_block_return, "testSyncBlockReturn", 42);
s23b_test!(
    test_s23b_thread_start_join_multiple,
    "testThreadStartJoinMultiple",
    55
);

// =============================================================================
// Session 24: Thread.interrupt() and Timed Waits
// =============================================================================

macro_rules! s24_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/InterruptComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/InterruptComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s24_test!(
    test_s24_interrupt_sleeping_thread,
    "interrupt_sleeping_thread",
    1
);
s24_test!(
    test_s24_interrupt_waiting_thread,
    "interrupt_waiting_thread",
    1
);
s24_test!(
    test_s24_interrupt_parked_thread,
    "interrupt_parked_thread",
    1
);
s24_test!(
    test_s24_is_interrupted_no_clear,
    "is_interrupted_no_clear",
    1
);
s24_test!(
    test_s24_interrupted_clears_flag,
    "interrupted_clears_flag",
    1
);
s24_test!(test_s24_interrupt_before_sleep, "interrupt_before_sleep", 1);
s24_test!(test_s24_interrupt_before_wait, "interrupt_before_wait", 1);
s24_test!(test_s24_interrupt_before_park, "interrupt_before_park", 1);
s24_test!(test_s24_timed_wait_normal, "timed_wait_normal", 1);
s24_test!(test_s24_timed_wait_interrupted, "timed_wait_interrupted", 1);
s24_test!(test_s24_thread_interrupt_self, "thread_interrupt_self", 1);
s24_test!(test_s24_multiple_interrupts, "multiple_interrupts", 1);
s24_test!(
    test_s24_wait_notify_no_interrupt,
    "wait_notify_no_interrupt",
    1
);
s24_test!(
    test_s24_park_unpark_no_interrupt,
    "park_unpark_no_interrupt",
    1
);
s24_test!(test_s24_unpark_before_park, "unpark_before_park", 1);
s24_test!(test_s24_park_nanos_timeout, "park_nanos_timeout", 1);
s24_test!(test_s24_sleep_nanos_interrupt, "sleep_nanos_interrupt", 1);
s24_test!(
    test_s24_interrupt_clears_on_exception,
    "interrupt_clears_on_exception",
    1
);
s24_test!(
    test_s24_wait_reacquires_monitor,
    "wait_reacquires_monitor",
    1
);
s24_test!(
    test_s24_interrupt_during_timed_park,
    "interrupt_during_timed_park",
    1
);
s24_test!(
    test_s24_notify_all_wakes_waiters,
    "notify_all_wakes_waiters",
    2
);
s24_test!(
    test_s24_park_after_interrupt_repeated,
    "park_after_interrupt_repeated",
    1
);
s24_test!(
    test_s24_sleep_zero_no_interrupt_check,
    "sleep_zero_no_interrupt_check",
    1
);
s24_test!(test_s24_timed_join, "timed_join", 1);
s24_test!(test_s24_interrupt_not_alive, "interrupt_not_alive", 1);
s24_test!(
    test_s24_wait_with_notify_all_and_interrupt,
    "wait_with_notify_all_and_interrupt",
    1
);
s24_test!(
    test_s24_concurrent_interrupt_and_join,
    "concurrent_interrupt_and_join",
    1
);
s24_test!(
    test_s24_park_unpark_multiple_threads,
    "park_unpark_multiple_threads",
    5
);
s24_test!(
    test_s24_wait_interrupt_reacquires_monitor,
    "wait_interrupt_reacquires_monitor",
    1
);
s24_test!(
    test_s24_interrupt_flag_survives_park,
    "interrupt_flag_survives_park",
    1
);

// =============================================================================
// Session 25: Virtual Threads Integration
// =============================================================================

macro_rules! s25_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/VirtualThreadTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/VirtualThreadTest",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s25_test!(test_s25_single_virtual_thread, "testSingleVirtualThread", 1);
s25_test!(test_s25_is_virtual, "testIsVirtual", 1);
s25_test!(
    test_s25_is_not_virtual_platform,
    "testIsNotVirtualPlatform",
    1
);
s25_test!(
    test_s25_hundred_virtual_threads,
    "testHundredVirtualThreads",
    100
);
s25_test!(
    test_s25_thousand_virtual_threads,
    "testThousandVirtualThreads",
    1000
);
s25_test!(test_s25_builder_virtual_start, "testBuilderVirtualStart", 1);
s25_test!(
    test_s25_builder_platform_start,
    "testBuilderPlatformStart",
    1
);
s25_test!(
    test_s25_virtual_join_happens_before,
    "testVirtualJoinHappensBefore",
    42
);
s25_test!(
    test_s25_mixed_virtual_platform,
    "testMixedVirtualPlatform",
    4
);
s25_test!(
    test_s25_virtual_start_happens_before,
    "testVirtualStartHappensBefore",
    100
);

// =============================================================================
// Session 27: GC Finalizer Support
// =============================================================================

macro_rules! s27_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/FinalizerTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/FinalizerTest",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s27_test!(test_s27_finalize_runs, "testFinalizeRuns", 42);
s27_test!(test_s27_finalize_multiple, "testFinalizeMultiple", 3);
s27_test!(test_s27_resurrection, "testResurrection", 77);
s27_test!(test_s27_no_finalize_on_live, "testNoFinalizeOnLive", 1);
s27_test!(test_s27_no_finalize_on_plain, "testNoFinalizeOnPlain", 1);
s27_test!(test_s27_system_gc_collects, "testSystemGcCollects", 1);
s27_test!(test_s27_finalize_side_effect, "testFinalizeSideEffect", 60);

// =============================================================================
// Session 32: JIT Escape Analysis — Scalar Replacement
// =============================================================================

macro_rules! s32_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/EscapeAnalysisTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/EscapeAnalysisTest",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s32_test!(test_s32_point_sum, "testPointSum", 30);
s32_test!(
    test_s32_point_distance_squared,
    "testPointDistanceSquared",
    500
);
s32_test!(test_s32_multiple_objects, "testMultipleObjects", 60);
s32_test!(test_s32_point_3d, "testPoint3D", 600);
s32_test!(test_s32_field_overwrite, "testFieldOverwrite", 99);
s32_test!(test_s32_escaping_object, "testEscapingObject", 42);
s32_test!(test_s32_loop_local_object, "testLoopLocalObject", 285);
s32_test!(test_s32_default_zero, "testDefaultZero", 0);

// ---------------------------------------------------------------------------
// Session 31: JIT Method Inlining
// ---------------------------------------------------------------------------
macro_rules! s31_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/InlineComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/InlineComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s31_test!(test_s31_inline_const, "inline_const", 5);
s31_test!(test_s31_inline_const_neg, "inline_const_neg", 1);
s31_test!(test_s31_inline_identity, "inline_identity", 42);
s31_test!(test_s31_inline_add, "inline_add", 42);
s31_test!(test_s31_inline_sub, "inline_sub", 42);
s31_test!(test_s31_inline_mul, "inline_mul", 42);
s31_test!(test_s31_inline_negate, "inline_negate", 42);
s31_test!(test_s31_inline_chain, "inline_chain", 42);
s31_test!(test_s31_inline_nested, "inline_nested", 42);
s31_test!(test_s31_inline_square, "inline_square", 36);
s31_test!(test_s31_inline_double, "inline_double", 42);
s31_test!(test_s31_inline_incr_decr, "inline_incr_decr", 83);
s31_test!(test_s31_inline_max, "inline_max", 42);
s31_test!(test_s31_inline_min, "inline_min", 42);
s31_test!(test_s31_inline_abs_positive, "inline_abs_positive", 42);
s31_test!(test_s31_inline_abs_negative, "inline_abs_negative", 42);
s31_test!(test_s31_inline_clamp_in_range, "inline_clamp_in_range", 42);
s31_test!(test_s31_inline_clamp_below, "inline_clamp_below", 42);
s31_test!(test_s31_inline_clamp_above, "inline_clamp_above", 42);
s31_test!(test_s31_inline_bitand, "inline_bitand", 42);
s31_test!(test_s31_inline_bitor, "inline_bitor", 42);
s31_test!(test_s31_inline_bitxor, "inline_bitxor", 42);
s31_test!(test_s31_inline_shift_left, "inline_shift_left", 42);
s31_test!(test_s31_inline_shift_right, "inline_shift_right", 42);
s31_test!(test_s31_inline_getter_setter, "inline_getter_setter", 42);
s31_test!(test_s31_inline_multi_field, "inline_multi_field", 42);
s31_test!(test_s31_inline_static_field, "inline_static_field", 42);
s31_test!(test_s31_inline_long_add, "inline_long_add", 42);
s31_test!(
    test_s31_inline_loop_body,
    "inline_loop_with_inlined_body",
    42
);
s31_test!(test_s31_inline_complex_expr, "inline_complex_expr", 42);

// ---------------------------------------------------------------------------
// Session 33: JIT Inline Caching
// ---------------------------------------------------------------------------
macro_rules! s33_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/InlineCacheTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/InlineCacheTest",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s33_test!(test_s33_monomorphic_call_site, "testMonomorphicCallSite", 1);
s33_test!(test_s33_polymorphic_call_site, "testPolymorphicCallSite", 1);
s33_test!(test_s33_megamorphic_call_site, "testMegamorphicCallSite", 1);
s33_test!(
    test_s33_virtual_dispatch_correctness,
    "testVirtualDispatchCorrectness",
    1
);
s33_test!(test_s33_interface_dispatch, "testInterfaceDispatch", 1);
s33_test!(test_s33_cache_miss_recovery, "testCacheMissRecovery", 1);
s33_test!(test_s33_hot_loop_monomorphic, "testHotLoopMonomorphic", 1);
s33_test!(test_s33_inlined_getter_setter, "testInlinedGetterSetter", 1);
s33_test!(test_s33_concurrent_dispatch, "testConcurrentDispatch", 1);
s33_test!(test_s33_deep_hierarchy, "testDeepHierarchy", 1);

// ---------------------------------------------------------------------------
// Session 35: JIT OSR (On-Stack Replacement)
// ---------------------------------------------------------------------------
macro_rules! s35_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/OsrComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/OsrComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

s35_test!(test_s35_osr_simple_sum, "osr_simple_sum", 5000);
s35_test!(test_s35_osr_accumulator, "osr_accumulator", 1);
s35_test!(test_s35_osr_while_loop, "osr_while_loop", 1);
s35_test!(test_s35_osr_countdown, "osr_countdown", 5000);
s35_test!(
    test_s35_osr_multiply_accumulate,
    "osr_multiply_accumulate",
    1
);
s35_test!(test_s35_osr_nested_loop, "osr_nested_loop", 5000);
s35_test!(test_s35_osr_branch_in_loop, "osr_branch_in_loop", 1);
s35_test!(test_s35_osr_local_variables, "osr_local_variables", 1);
s35_test!(test_s35_osr_long_arithmetic, "osr_long_arithmetic", 1);
s35_test!(test_s35_osr_bitwise_ops, "osr_bitwise_ops", 1);
s35_test!(test_s35_osr_array_sum, "osr_array_sum", 1);
s35_test!(
    test_s35_osr_post_loop_correctness,
    "osr_post_loop_correctness",
    1
);
s35_test!(
    test_s35_osr_method_call_after,
    "osr_method_call_after",
    5000
);
s35_test!(test_s35_osr_shift_operations, "osr_shift_operations", 1);
s35_test!(test_s35_osr_comparison_loop, "osr_comparison_loop", 1);
s35_test!(test_s35_osr_do_while, "osr_do_while", 1);
s35_test!(
    test_s35_osr_fibonacci_iterative,
    "osr_fibonacci_iterative",
    1
);
s35_test!(
    test_s35_osr_static_field_in_loop,
    "osr_static_field_in_loop",
    1
);
s35_test!(test_s35_osr_negative_step, "osr_negative_step", 1);
s35_test!(test_s35_osr_multiple_exits, "osr_multiple_exits", 5000);
s35_test!(test_s35_osr_return_from_loop, "osr_return_from_loop", 1);
s35_test!(test_s35_osr_two_counters, "osr_two_counters", 1);
s35_test!(
    test_s35_osr_conditional_increment,
    "osr_conditional_increment",
    1
);
s35_test!(
    test_s35_osr_early_exit_not_taken,
    "osr_early_exit_not_taken",
    5000
);
s35_test!(test_s35_osr_gauss_sum, "osr_gauss_sum", 1);
s35_test!(test_s35_osr_min_max_tracking, "osr_min_max_tracking", 1);
s35_test!(test_s35_osr_char_loop, "osr_char_loop", 1);
s35_test!(test_s35_osr_modular_arithmetic, "osr_modular_arithmetic", 1);
s35_test!(test_s35_osr_large_iteration, "osr_large_iteration", 1);
s35_test!(test_s35_osr_triangular_number, "osr_triangular_number", 1);

// ---------------------------------------------------------------------------
// Session 37 — JIT Floating-Point Completeness
// ---------------------------------------------------------------------------

macro_rules! s37_test_int {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/FPCompletenessTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/FPCompletenessTest",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

macro_rules! s37_test_long {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/FPCompletenessTest", $method, "()J", &[]);
            corpus_check(
                &vm,
                "cratonvm/FPCompletenessTest",
                $method,
                &format!("Long({})", $expected),
                matches!(result, Ok(Some(Value::Long(v))) if v == $expected),
                &result,
            );
        }
    };
}

// NaN / overflow conversions
s37_test_int!(test_s37_d2i_nan, "testD2iNaN", 0);
s37_test_int!(test_s37_d2i_pos_overflow, "testD2iPosOverflow", 2147483647);
s37_test_int!(test_s37_d2i_neg_overflow, "testD2iNegOverflow", -2147483648);
s37_test_long!(test_s37_d2l_nan, "testD2lNaN", 0);
s37_test_long!(
    test_s37_d2l_pos_overflow,
    "testD2lPosOverflow",
    9223372036854775807i64
);
s37_test_long!(
    test_s37_d2l_neg_overflow,
    "testD2lNegOverflow",
    -9223372036854775808i64
);
s37_test_int!(test_s37_f2i_nan, "testF2iNaN", 0);
s37_test_int!(test_s37_f2i_pos_overflow, "testF2iPosOverflow", 2147483647);
s37_test_long!(test_s37_f2l_nan, "testF2lNaN", 0);

// Normal conversions
s37_test_int!(test_s37_d2i_normal, "testD2iNormal", 42);
s37_test_long!(test_s37_d2l_normal, "testD2lNormal", 123456789);
s37_test_int!(test_s37_f2i_normal, "testF2iNormal", -7);
s37_test_long!(test_s37_f2l_normal, "testF2lNormal", 100000);

// Math.floor / ceil / rint
s37_test_long!(test_s37_math_floor, "testMathFloor", 2);
s37_test_long!(test_s37_math_floor_neg, "testMathFloorNeg", -3);
s37_test_long!(test_s37_math_ceil, "testMathCeil", 3);
s37_test_long!(test_s37_math_ceil_neg, "testMathCeilNeg", -2);
s37_test_long!(test_s37_math_rint, "testMathRint", 2);
s37_test_long!(test_s37_math_rint_odd, "testMathRintOdd", 4);

// Math.abs
s37_test_long!(test_s37_abs_double, "testMathAbsDouble", 42);
s37_test_long!(test_s37_abs_double_pos, "testMathAbsDoublePos", 99);
s37_test_int!(test_s37_abs_int, "testMathAbsInt", 123);
s37_test_int!(test_s37_abs_int_pos, "testMathAbsIntPos", 456);
s37_test_long!(test_s37_abs_long, "testMathAbsLong", 9876543210i64);
s37_test_long!(test_s37_abs_long_pos, "testMathAbsLongPos", 1234567890);
s37_test_int!(test_s37_abs_float, "testMathAbsFloat", 314);

// Combined N-Body style
s37_test_long!(test_s37_nbody_style, "testNBodyStyle", 5);

// ---------------------------------------------------------------------------
// Session 39: JDWP Debugger Protocol
// ---------------------------------------------------------------------------
macro_rules! s39_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/JdwpComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/JdwpComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

// Local variable inspection targets
s39_test!(test_s39_locals_int, "locals_int", 30);
s39_test!(test_s39_locals_long, "locals_long", 3);
s39_test!(test_s39_locals_float, "locals_float", 4);
s39_test!(test_s39_locals_double, "locals_double", 31);
s39_test!(test_s39_locals_mixed, "locals_mixed", 20);

// Control flow
s39_test!(test_s39_control_if_else, "control_if_else", 1);
s39_test!(test_s39_control_switch, "control_switch", 20);
s39_test!(test_s39_control_for_loop, "control_for_loop", 55);
s39_test!(test_s39_control_while, "control_while", 6);
s39_test!(test_s39_control_nested, "control_nested", 25);

// Array access
s39_test!(test_s39_array_basic, "array_basic", 30);
s39_test!(test_s39_array_sum, "array_sum", 15);

// Object interaction
s39_test!(test_s39_object_string_len, "object_string_len", 12);
s39_test!(test_s39_object_string_concat, "object_string_concat", 11);
s39_test!(test_s39_object_null_ref, "object_null_ref", 1);

// Arithmetic
s39_test!(test_s39_arith_bitwise, "arith_bitwise", 510);
s39_test!(test_s39_arith_shifts, "arith_shifts", 256);
s39_test!(test_s39_arith_divmod, "arith_divmod", 142);

// Method calls
s39_test!(test_s39_method_call_loop, "method_call_loop", 30);
s39_test!(test_s39_method_recursive, "method_recursive", 55);

// Exception handling
s39_test!(test_s39_exception_trycatch, "exception_trycatch", 42);
s39_test!(test_s39_exception_finally, "exception_finally", 15);

// Ternary / conditional
s39_test!(test_s39_ternary_expr, "ternary_expr", 10);

// Stack depth / many locals
s39_test!(test_s39_many_locals, "many_locals", 55);
s39_test!(test_s39_deep_stack, "deep_stack", 21);

// ---------------------------------------------------------------------------
// Session 44: JNI Completeness — Remaining 71 Functions
// ---------------------------------------------------------------------------
macro_rules! s44_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/JniComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/JniComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

// Static field access
s44_test!(test_s44_static_int_field, "static_int_field", 100);
s44_test!(test_s44_static_long_field, "static_long_field", 200);
s44_test!(
    test_s44_static_string_field_len,
    "static_string_field_len",
    5
);

// Instance field/method access
s44_test!(test_s44_instance_create_get, "instance_create_get", 42);
s44_test!(test_s44_instance_set_get, "instance_set_get", 99);

// Array operations
s44_test!(test_s44_array_int_create, "array_int_create", 10);
s44_test!(test_s44_array_int_set_get, "array_int_set_get", 30);
s44_test!(test_s44_array_int_sum, "array_int_sum", 55);
s44_test!(test_s44_array_long_ops, "array_long_ops", 600);
s44_test!(test_s44_array_byte_ops, "array_byte_ops", 100);
s44_test!(test_s44_array_boolean_ops, "array_boolean_ops", 2);
s44_test!(test_s44_array_double_ops, "array_double_ops", 7);
s44_test!(test_s44_array_object_ops, "array_object_ops", 11);

// String operations
s44_test!(test_s44_string_new_utf, "string_new_utf", 9);
s44_test!(test_s44_string_concat, "string_concat", 11);
s44_test!(test_s44_string_char_at, "string_char_at", 67);
s44_test!(test_s44_string_index_of, "string_index_of", 6);

// Object creation and references
s44_test!(test_s44_object_alloc, "object_alloc", 1);
s44_test!(test_s44_object_class_check, "object_class_check", 1);
s44_test!(test_s44_object_null_check, "object_null_check", 1);

// Method call types
s44_test!(test_s44_call_static_method, "call_static_method", 42);
s44_test!(test_s44_call_instance_method, "call_instance_method", 42);

// Arithmetic
s44_test!(test_s44_arith_add, "arith_add", 1);
s44_test!(test_s44_arith_long_math, "arith_long_math", 3);
s44_test!(test_s44_arith_float_cast, "arith_float_cast", 31);
s44_test!(test_s44_arith_double_cast, "arith_double_cast", 271);

// Exception handling
s44_test!(test_s44_exception_throw_catch, "exception_throw_catch", 1);

// Monitor
s44_test!(test_s44_monitor_basic, "monitor_basic", 42);

// Multi-dimensional arrays
s44_test!(test_s44_array_2d, "array_2d", 5);

// Complex: Fibonacci
s44_test!(test_s44_fib_iterative, "fib_iterative", 610);

// ---------------------------------------------------------------------------
// Session 46: TCK — java.lang Tests
// ---------------------------------------------------------------------------
macro_rules! s46_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/TckLang", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/TckLang",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

// Object
s46_test!(
    test_s46_obj_hashCode_consistent,
    "obj_hashCode_consistent",
    1
);
s46_test!(test_s46_obj_equals_identity, "obj_equals_identity", 1);
s46_test!(test_s46_obj_equals_different, "obj_equals_different", 1);
s46_test!(test_s46_obj_getClass, "obj_getClass", 1);
s46_test!(test_s46_obj_toString, "obj_toString", 1);

// String
s46_test!(test_s46_str_length, "str_length", 5);
s46_test!(test_s46_str_charAt, "str_charAt", 68);
s46_test!(test_s46_str_equals, "str_equals", 1);
s46_test!(test_s46_str_compareTo, "str_compareTo", 1);
s46_test!(test_s46_str_substring, "str_substring", 5);
s46_test!(test_s46_str_indexOf, "str_indexOf", 6);
s46_test!(test_s46_str_contains, "str_contains", 1);
s46_test!(test_s46_str_isEmpty, "str_isEmpty", 1);
s46_test!(test_s46_str_trim, "str_trim", 5);
s46_test!(test_s46_str_toLowerCase, "str_toLowerCase", 1);
s46_test!(test_s46_str_toUpperCase, "str_toUpperCase", 1);
s46_test!(test_s46_str_startsEndsWith, "str_startsEndsWith", 1);
s46_test!(test_s46_str_replace, "str_replace", 1);
s46_test!(test_s46_str_toCharArray, "str_toCharArray", 1);
s46_test!(test_s46_str_valueOf_int, "str_valueOf_int", 1);
s46_test!(test_s46_str_valueOf_bool, "str_valueOf_bool", 1);
s46_test!(test_s46_str_concat_op, "str_concat_op", 11);

// Integer
s46_test!(test_s46_int_parseInt, "int_parseInt", 42);
s46_test!(test_s46_int_parseInt_neg, "int_parseInt_neg", -100);
s46_test!(test_s46_int_valueOf, "int_valueOf", 42);
s46_test!(test_s46_int_toString, "int_toString", 1);
s46_test!(test_s46_int_toHexString, "int_toHexString", 1);
s46_test!(test_s46_int_constants, "int_constants", 1);
s46_test!(test_s46_int_autobox_cache, "int_autobox_cache", 1);
s46_test!(test_s46_int_compareTo, "int_compareTo", 1);

// Long
s46_test!(test_s46_long_parseLong, "long_parseLong", 1);
s46_test!(test_s46_long_valueOf, "long_valueOf", 99);
s46_test!(test_s46_long_toString, "long_toString", 1);
s46_test!(test_s46_long_maxValue, "long_maxValue", 1);

// Double
s46_test!(test_s46_double_parseDouble, "double_parseDouble", 1);
s46_test!(test_s46_double_isNaN, "double_isNaN", 1);
s46_test!(test_s46_double_isInfinite, "double_isInfinite", 1);
s46_test!(test_s46_double_toString, "double_toString", 1);
s46_test!(test_s46_double_bits_roundtrip, "double_bits_roundtrip", 1);

// Float
s46_test!(test_s46_float_parseFloat, "float_parseFloat", 1);
s46_test!(test_s46_float_isNaN, "float_isNaN", 1);
s46_test!(test_s46_float_bits_roundtrip, "float_bits_roundtrip", 1);

// Boolean
s46_test!(test_s46_bool_parseBoolean, "bool_parseBoolean", 1);
s46_test!(test_s46_bool_valueOf, "bool_valueOf", 1);
s46_test!(test_s46_bool_toString, "bool_toString", 1);

// Byte
s46_test!(test_s46_byte_constants, "byte_constants", 1);
s46_test!(test_s46_byte_parseByte, "byte_parseByte", 42);

// Short
s46_test!(test_s46_short_constants, "short_constants", 1);
s46_test!(test_s46_short_parseShort, "short_parseShort", 1000);

// Character
s46_test!(test_s46_char_isDigit, "char_isDigit", 1);
s46_test!(test_s46_char_isLetter, "char_isLetter", 1);
s46_test!(test_s46_char_case, "char_case", 1);
s46_test!(test_s46_char_convert, "char_convert", 1);
s46_test!(test_s46_char_isWhitespace, "char_isWhitespace", 1);

// Math
s46_test!(test_s46_math_abs, "math_abs", 1);
s46_test!(test_s46_math_maxMin, "math_maxMin", 1);
s46_test!(test_s46_math_sqrt, "math_sqrt", 1);
s46_test!(test_s46_math_pow, "math_pow", 1);
s46_test!(test_s46_math_floorCeil, "math_floorCeil", 1);
s46_test!(test_s46_math_round, "math_round", 1);
s46_test!(test_s46_math_constants, "math_constants", 1);
s46_test!(test_s46_math_sinCos, "math_sinCos", 1);
s46_test!(test_s46_math_logExp, "math_logExp", 1);

// System
s46_test!(test_s46_sys_currentTimeMillis, "sys_currentTimeMillis", 1);
s46_test!(test_s46_sys_nanoTime, "sys_nanoTime", 1);
s46_test!(test_s46_sys_arraycopy, "sys_arraycopy", 1);
s46_test!(test_s46_sys_identityHashCode, "sys_identityHashCode", 1);

// StringBuilder
s46_test!(test_s46_sb_basic, "sb_basic", 11);
s46_test!(test_s46_sb_appendInt, "sb_appendInt", 1);
s46_test!(test_s46_sb_chain, "sb_chain", 1);
s46_test!(test_s46_sb_length, "sb_length", 5);
s46_test!(test_s46_sb_reverse, "sb_reverse", 1);
s46_test!(test_s46_sb_delete, "sb_delete", 1);

// Throwable / Exceptions
s46_test!(test_s46_exc_getMessage, "exc_getMessage", 1);
s46_test!(test_s46_exc_getCause, "exc_getCause", 1);
s46_test!(test_s46_exc_tryCatch, "exc_tryCatch", 1);
s46_test!(test_s46_exc_hierarchy, "exc_hierarchy", 1);
s46_test!(test_s46_exc_npe_class, "exc_npe_class", 1);
s46_test!(test_s46_exc_finally, "exc_finally", 15);

// Class
s46_test!(test_s46_cls_getName, "cls_getName", 1);
s46_test!(test_s46_cls_isInterface, "cls_isInterface", 1);
s46_test!(test_s46_cls_isPrimitive, "cls_isPrimitive", 1);
s46_test!(test_s46_cls_isArray, "cls_isArray", 1);
s46_test!(test_s46_cls_getSuperclass, "cls_getSuperclass", 1);

// Runtime
s46_test!(test_s46_rt_availableProcessors, "rt_availableProcessors", 1);
s46_test!(test_s46_rt_memory, "rt_memory", 1);

// Thread
s46_test!(test_s46_thread_currentThread, "thread_currentThread", 1);
s46_test!(test_s46_thread_isAlive, "thread_isAlive", 1);

// Type casting
s46_test!(test_s46_cast_int_to_long, "cast_int_to_long", 1);
s46_test!(test_s46_cast_long_to_int, "cast_long_to_int", 42);
s46_test!(test_s46_cast_int_to_float, "cast_int_to_float", 1);
s46_test!(test_s46_cast_double_to_int, "cast_double_to_int", 3);
s46_test!(test_s46_cast_char_to_int, "cast_char_to_int", 65);

// Autoboxing
s46_test!(test_s46_autobox_int, "autobox_int", 42);
s46_test!(test_s46_autobox_double, "autobox_double", 1);
s46_test!(test_s46_autobox_boolean, "autobox_boolean", 1);

// ---------------------------------------------------------------------------
// Session 50: TCK — Reflection and Annotation Tests
// ---------------------------------------------------------------------------
macro_rules! s50_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/TckReflect", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/TckReflect",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

// Class metadata
s50_test!(test_s50_cls_forName, "cls_forName", 1);
s50_test!(test_s50_cls_getName, "cls_getName", 1);
s50_test!(test_s50_cls_getSimpleName, "cls_getSimpleName", 1);
s50_test!(test_s50_cls_getSuperclass, "cls_getSuperclass", 1);
s50_test!(
    test_s50_cls_objectSuperclassNull,
    "cls_objectSuperclassNull",
    1
);
s50_test!(test_s50_cls_isInterface, "cls_isInterface", 1);
s50_test!(test_s50_cls_isPrimitive, "cls_isPrimitive", 1);
s50_test!(test_s50_cls_isArray, "cls_isArray", 1);
s50_test!(test_s50_cls_isEnum, "cls_isEnum", 1);
s50_test!(test_s50_cls_isAnnotation, "cls_isAnnotation", 1);
s50_test!(test_s50_cls_getModifiers, "cls_getModifiers", 1);
s50_test!(test_s50_cls_isAssignableFrom, "cls_isAssignableFrom", 1);
s50_test!(test_s50_cls_isInstance, "cls_isInstance", 1);
s50_test!(test_s50_cls_getInterfaces, "cls_getInterfaces", 1);
s50_test!(test_s50_cls_getComponentType, "cls_getComponentType", 1);
s50_test!(test_s50_cls_cast, "cls_cast", 1);
s50_test!(test_s50_cls_newInstance, "cls_newInstance", 1);

// Method reflection
s50_test!(test_s50_meth_getDeclaredMethod, "meth_getDeclaredMethod", 1);
s50_test!(test_s50_meth_invokeInstance, "meth_invokeInstance", 1);
s50_test!(test_s50_meth_invokeStatic, "meth_invokeStatic", 1);
s50_test!(test_s50_meth_invokePrivate, "meth_invokePrivate", 1);
s50_test!(test_s50_meth_getReturnType, "meth_getReturnType", 1);
s50_test!(test_s50_meth_getParameterTypes, "meth_getParameterTypes", 1);
s50_test!(test_s50_meth_getParameterCount, "meth_getParameterCount", 1);
s50_test!(test_s50_meth_getModifiers, "meth_getModifiers", 1);
s50_test!(test_s50_meth_getDeclaringClass, "meth_getDeclaringClass", 1);
s50_test!(
    test_s50_meth_getDeclaredMethods,
    "meth_getDeclaredMethods",
    1
);

// Field reflection
s50_test!(test_s50_fld_getDeclaredField, "fld_getDeclaredField", 1);
s50_test!(test_s50_fld_get, "fld_get", 1);
s50_test!(test_s50_fld_set, "fld_set", 1);
s50_test!(test_s50_fld_getPrivate, "fld_getPrivate", 1);
s50_test!(test_s50_fld_getInt, "fld_getInt", 1);
s50_test!(test_s50_fld_setInt, "fld_setInt", 1);
s50_test!(test_s50_fld_getType, "fld_getType", 1);
s50_test!(test_s50_fld_getModifiers, "fld_getModifiers", 1);
s50_test!(test_s50_fld_getDeclaringClass, "fld_getDeclaringClass", 1);
s50_test!(test_s50_fld_getDeclaredFields, "fld_getDeclaredFields", 1);

// Constructor reflection
s50_test!(
    test_s50_ctor_getDeclaredConstructor,
    "ctor_getDeclaredConstructor",
    1
);
s50_test!(test_s50_ctor_newInstanceNoArgs, "ctor_newInstanceNoArgs", 1);
s50_test!(
    test_s50_ctor_newInstanceWithArgs,
    "ctor_newInstanceWithArgs",
    1
);
s50_test!(
    test_s50_ctor_newInstancePrivate,
    "ctor_newInstancePrivate",
    1
);
s50_test!(test_s50_ctor_getParameterTypes, "ctor_getParameterTypes", 1);
s50_test!(test_s50_ctor_getModifiers, "ctor_getModifiers", 1);
s50_test!(test_s50_ctor_getDeclaringClass, "ctor_getDeclaringClass", 1);
s50_test!(
    test_s50_ctor_getDeclaredConstructors,
    "ctor_getDeclaredConstructors",
    1
);

// Annotations — class level
s50_test!(test_s50_ann_classPresent, "ann_classPresent", 1);
s50_test!(test_s50_ann_classAbsent, "ann_classAbsent", 1);
s50_test!(test_s50_ann_classValue, "ann_classValue", 1);
s50_test!(test_s50_ann_inherited, "ann_inherited", 1);
s50_test!(test_s50_ann_inheritedValue, "ann_inheritedValue", 1);
s50_test!(
    test_s50_ann_declaredExcludesInherited,
    "ann_declaredExcludesInherited",
    1
);
s50_test!(
    test_s50_ann_getAnnotationsIncludesInherited,
    "ann_getAnnotationsIncludesInherited",
    1
);

// Annotations — method level
s50_test!(test_s50_ann_methodPresent, "ann_methodPresent", 1);
s50_test!(test_s50_ann_methodValue, "ann_methodValue", 1);
s50_test!(test_s50_ann_methodDefault, "ann_methodDefault", 1);
s50_test!(test_s50_ann_methodAbsent, "ann_methodAbsent", 1);

// Annotations — field level
s50_test!(test_s50_ann_fieldPresent, "ann_fieldPresent", 1);
s50_test!(test_s50_ann_fieldValue, "ann_fieldValue", 1);

// Array reflection
s50_test!(test_s50_arr_newInstance, "arr_newInstance", 1);
s50_test!(test_s50_arr_getLength, "arr_getLength", 1);
s50_test!(test_s50_arr_getSet, "arr_getSet", 1);
s50_test!(test_s50_arr_getObject, "arr_getObject", 1);
s50_test!(test_s50_arr_setObject, "arr_setObject", 1);
s50_test!(test_s50_arr_newInstanceRef, "arr_newInstanceRef", 1);

// Proxy
s50_test!(test_s50_proxy_create, "proxy_create", 1);
s50_test!(test_s50_proxy_isProxyClass, "proxy_isProxyClass", 1);
s50_test!(test_s50_proxy_getHandler, "proxy_getHandler", 1);
s50_test!(test_s50_proxy_objectMethods, "proxy_objectMethods", 1);

// Modifier
s50_test!(test_s50_mod_isPublic, "mod_isPublic", 1);
s50_test!(test_s50_mod_isStatic, "mod_isStatic", 1);
s50_test!(test_s50_mod_isFinal, "mod_isFinal", 1);
s50_test!(test_s50_mod_isAbstract, "mod_isAbstract", 1);
s50_test!(test_s50_mod_isInterface, "mod_isInterface", 1);
s50_test!(test_s50_mod_isPrivate, "mod_isPrivate", 1);
s50_test!(test_s50_mod_toString, "mod_toString", 1);

// Hierarchy
s50_test!(test_s50_hier_isInstance, "hier_isInstance", 1);
s50_test!(
    test_s50_hier_isAssignableFromInterface,
    "hier_isAssignableFromInterface",
    1
);
s50_test!(test_s50_hier_superclassChain, "hier_superclassChain", 1);

// Miscellaneous
s50_test!(test_s50_misc_invokeReturnBoxed, "misc_invokeReturnBoxed", 1);
s50_test!(test_s50_misc_multiFieldRead, "misc_multiFieldRead", 1);
s50_test!(test_s50_misc_ctorThenInvoke, "misc_ctorThenInvoke", 1);
s50_test!(
    test_s50_misc_getMethodInherited,
    "misc_getMethodInherited",
    1
);
s50_test!(test_s50_misc_noSuchField, "misc_noSuchField", 1);
s50_test!(test_s50_misc_noSuchMethod, "misc_noSuchMethod", 1);
s50_test!(
    test_s50_misc_invocationTargetException,
    "misc_invocationTargetException",
    1
);
s50_test!(test_s50_misc_getPublicFields, "misc_getPublicFields", 1);
s50_test!(test_s50_misc_getPublicMethods, "misc_getPublicMethods", 1);
s50_test!(
    test_s50_misc_getPublicConstructors,
    "misc_getPublicConstructors",
    1
);
s50_test!(test_s50_misc_primitiveClass, "misc_primitiveClass", 1);
s50_test!(test_s50_misc_voidClass, "misc_voidClass", 1);

// ---------------------------------------------------------------------------
// Session 38 — JIT Profile-Guided Optimization
// ---------------------------------------------------------------------------

macro_rules! s38_test {
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/PgoTest", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/PgoTest",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

// Branch profiling
s38_test!(test_s38_gauss_sum, "testGaussSum", 4950);
s38_test!(test_s38_repeated_hot_loop, "testRepeatedHotLoop", 247500);
s38_test!(test_s38_biased_branch, "testBiasedBranch", 9135);

// Loop trip count profiling
s38_test!(test_s38_short_loop_repeated, "testShortLoopRepeated", 5600);
s38_test!(
    test_s38_medium_loop_repeated,
    "testMediumLoopRepeated",
    19000
);

// Receiver type profiling
s38_test!(
    test_s38_monomorphic_dispatch,
    "testMonomorphicDispatch",
    2500
);
s38_test!(test_s38_bimorphic_dispatch, "testBimorphicDispatch", 1450);

// Combined PGO scenario
s38_test!(test_s38_combined_pgo, "testCombinedPgo", 955);

// Correctness after JIT with PGO data
s38_test!(test_s38_gauss_sum_large, "testGaussSumLarge", 499500);
s38_test!(test_s38_nested_loops, "testNestedLoops", 2025);

// ---------------------------------------------------------------------------
// Session 45 — Run a Real Application (-jar launch, ManifestInfo, ClassPath)
// ---------------------------------------------------------------------------

/// Test that HelloWorld.check() returns 42 (basic sanity for the test class).
#[test]
fn test_s45_hello_world_check() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/HelloWorld", "check", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/HelloWorld",
        "check",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );
}

/// Test ManifestInfo parsing including Class-Path attribute.
#[test]
fn test_s45_manifest_class_path_parsing() {
    use cratonvm_vm::ManifestInfo;

    let manifest = b"Manifest-Version: 1.0\r\nMain-Class: com.example.Main\r\nClass-Path: lib/foo.jar lib/bar.jar\r\n";
    let info = ManifestInfo::parse(manifest);
    assert_eq!(info.main_class.as_deref(), Some("com.example.Main"));
    assert_eq!(info.class_path.as_deref(), Some("lib/foo.jar lib/bar.jar"));
}

/// Test resolve_class_path resolves relative to JAR parent directory.
#[test]
fn test_s45_resolve_class_path() {
    use cratonvm_vm::ManifestInfo;

    let manifest =
        b"Manifest-Version: 1.0\r\nMain-Class: Main\r\nClass-Path: lib/dep.jar other.jar\r\n";
    let info = ManifestInfo::parse(manifest);
    let jar_path = std::path::Path::new("/app/myapp.jar");
    let resolved = info.resolve_class_path(jar_path);
    // Should resolve relative to /app/
    assert_eq!(resolved.len(), 2);
    assert!(resolved[0].contains("lib"));
    assert!(resolved[0].contains("dep.jar"));
    assert!(resolved[1].contains("other.jar"));
}

/// Test resolve_class_path returns empty vec when no Class-Path attribute.
#[test]
fn test_s45_resolve_class_path_empty() {
    use cratonvm_vm::ManifestInfo;

    let manifest = b"Manifest-Version: 1.0\r\nMain-Class: Main\r\n";
    let info = ManifestInfo::parse(manifest);
    let jar_path = std::path::Path::new("/app/myapp.jar");
    let resolved = info.resolve_class_path(jar_path);
    assert!(resolved.is_empty());
}

/// Test read_jar_manifest reads Main-Class and Class-Path from a real JAR file.
#[test]
fn test_s45_read_jar_manifest() {
    use cratonvm_vm::ClassPath;
    use std::io::Write;

    let dir = std::env::temp_dir().join("cratonvm_test_s45_manifest");
    let _ = std::fs::create_dir_all(&dir);
    let jar_path = dir.join("test.jar");

    // Create a JAR with a manifest
    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
    zip.write_all(
        b"Manifest-Version: 1.0\r\nMain-Class: com.example.App\r\nClass-Path: lib/util.jar\r\n",
    )
    .unwrap();
    zip.start_file("com/example/App.class", opts).unwrap();
    zip.write_all(b"\xCA\xFE\xBA\xBE_fake").unwrap();
    zip.finish().unwrap();

    let manifest = ClassPath::read_jar_manifest(&jar_path);
    assert!(manifest.is_some());
    let manifest = manifest.unwrap();
    assert_eq!(manifest.main_class.as_deref(), Some("com.example.App"));
    assert_eq!(manifest.class_path.as_deref(), Some("lib/util.jar"));

    // resolve_class_path should resolve relative to the JAR's dir
    let resolved = manifest.resolve_class_path(&jar_path);
    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].ends_with("lib/util.jar") || resolved[0].ends_with("lib\\util.jar"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test loading and executing a class from a JAR file via ClassPath.
#[test]
fn test_s45_run_class_from_jar() {
    require_class_files!();
    use std::io::Write;

    let dir = std::env::temp_dir().join("cratonvm_test_s45_jar_run");
    let _ = std::fs::create_dir_all(&dir);
    let jar_path = dir.join("hello.jar");

    // Read the compiled HelloWorld.class from test resources
    let resources = test_resources_dir();
    let class_file = format!("{resources}/cratonvm/HelloWorld.class");
    let class_bytes = match std::fs::read(&class_file) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("Skipping: HelloWorld.class not found");
            return;
        }
    };

    // Create a JAR with the class and a manifest
    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
    zip.write_all(b"Manifest-Version: 1.0\r\nMain-Class: cratonvm.HelloWorld\r\n")
        .unwrap();
    zip.start_file("cratonvm/HelloWorld.class", opts).unwrap();
    zip.write_all(&class_bytes).unwrap();
    zip.finish().unwrap();

    // Load the class from the JAR and invoke check()
    let config = cratonvm_vm::config::VmConfig::new()
        .with_classpath(vec![jar_path.to_string_lossy().into_owned()]);
    let mut vm = cratonvm_vm::vm::Vm::new(config);
    let result = vm.invoke("cratonvm/HelloWorld", "check", "()I", &[]);
    corpus_check(
        &vm,
        "cratonvm/HelloWorld",
        "check",
        "Int(42)",
        matches!(result, Ok(Some(Value::Int(42)))),
        &result,
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test that read_jar_manifest returns None for a JAR with no manifest.
#[test]
fn test_s45_read_jar_no_manifest() {
    use cratonvm_vm::ClassPath;
    use std::io::Write;

    let dir = std::env::temp_dir().join("cratonvm_test_s45_no_manifest");
    let _ = std::fs::create_dir_all(&dir);
    let jar_path = dir.join("nomanifest.jar");

    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("com/Foo.class", opts).unwrap();
    zip.write_all(b"\xCA\xFE\xBA\xBE_fake").unwrap();
    zip.finish().unwrap();

    // read_jar_manifest should still return Some (with empty fields), since the
    // archive was readable — ManifestInfo::default() has main_class=None
    let manifest = ClassPath::read_jar_manifest(&jar_path);
    assert!(manifest.is_some());
    assert!(manifest.unwrap().main_class.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test that nonexistent JAR returns None from read_jar_manifest.
#[test]
fn test_s45_read_jar_nonexistent() {
    use cratonvm_vm::ClassPath;
    let manifest = ClassPath::read_jar_manifest(std::path::Path::new("/nonexistent/path.jar"));
    assert!(manifest.is_none());
}

// ---------------------------------------------------------------------------
// Session 47 — TCK java.util Tests
// ---------------------------------------------------------------------------

macro_rules! s47_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/TckUtil", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/TckUtil",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
}

// ArrayList
s47_test!(test_s47_arraylist_basic, "testArrayListBasic");
s47_test!(test_s47_arraylist_mutations, "testArrayListMutations");
s47_test!(test_s47_arraylist_grow, "testArrayListGrow");
s47_test!(test_s47_arraylist_iterator, "testArrayListIterator");
s47_test!(
    test_s47_arraylist_list_iterator_previous,
    "testArrayListListIteratorPrevious"
);
s47_test!(test_s47_arraylist_insert, "testArrayListInsert");
s47_test!(test_s47_arraylist_last_index_of, "testArrayListLastIndexOf");
s47_test!(test_s47_arraylist_capacity, "testArrayListCapacity");
s47_test!(test_s47_arraylist_to_array, "testArrayListToArray");

// HashMap
s47_test!(test_s47_hashmap_basic, "testHashMapBasic");
s47_test!(test_s47_hashmap_mutations, "testHashMapMutations");
s47_test!(test_s47_hashmap_integer_keys, "testHashMapIntegerKeys");
s47_test!(test_s47_hashmap_get_or_default, "testHashMapGetOrDefault");
s47_test!(test_s47_hashmap_put_if_absent, "testHashMapPutIfAbsent");
s47_test!(test_s47_hashmap_null_key, "testHashMapNullKey");
s47_test!(test_s47_hashmap_key_set, "testHashMapKeySet");
s47_test!(test_s47_hashmap_capacity, "testHashMapCapacity");

// HashSet
s47_test!(test_s47_hashset_basic, "testHashSetBasic");
s47_test!(test_s47_hashset_iterator, "testHashSetIterator");

// Arrays
s47_test!(test_s47_arrays_sort, "testArraysSort");
s47_test!(test_s47_arrays_copy_of, "testArraysCopyOf");
s47_test!(test_s47_arrays_as_list, "testArraysAsList");

// Collections utility
s47_test!(test_s47_collections_empty_list, "testCollectionsEmptyList");
s47_test!(
    test_s47_collections_singleton_list,
    "testCollectionsSingletonList"
);
s47_test!(test_s47_collections_reverse, "testCollectionsReverse");
s47_test!(test_s47_enumset_all_of_iterator, "testEnumSetAllOfIterator");
s47_test!(
    test_s47_large_enumset_all_of_iterator,
    "testLargeEnumSetAllOfIterator"
);
s47_test!(
    test_s47_collections_synchronized_collection_for_each,
    "testCollectionsSynchronizedCollectionForEach"
);

// Optional
s47_test!(test_s47_optional_basic, "testOptionalBasic");
s47_test!(test_s47_optional_or_else, "testOptionalOrElse");

// Integration / combined
s47_test!(test_s47_frequency_map, "testFrequencyMap");
s47_test!(test_s47_deduplication, "testDeduplication");
s47_test!(
    test_s47_linkedlist_remove_if_iterator_remove,
    "linkedlist_remove_if_iterator_remove"
);

// ===========================================================================
// Session 49 — TCK: java.util.concurrent
// ===========================================================================

macro_rules! s49_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/JucComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/JucComplete",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
    ($name:ident, $method:expr, $expected:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/JucComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/JucComplete",
                $method,
                &format!("Int({})", $expected),
                matches!(result, Ok(Some(Value::Int(v))) if v == $expected),
                &result,
            );
        }
    };
}

// --- Atomics ---
s49_test!(test_s49_atomic_int_cas, "testAtomicIntCas");
s49_test!(test_s49_atomic_int_incr_decr, "testAtomicIntIncrDecr");
s49_test!(
    test_s49_atomic_int_pre_incr_decr,
    "testAtomicIntPreIncrDecr"
);
s49_test!(test_s49_atomic_int_add_ops, "testAtomicIntAddOps");
s49_test!(test_s49_atomic_int_get_and_set, "testAtomicIntGetAndSet");
s49_test!(test_s49_atomic_long_basic, "testAtomicLongBasic");
s49_test!(test_s49_atomic_boolean_cas, "testAtomicBooleanCas");
s49_test!(
    test_s49_atomic_boolean_get_and_set,
    "testAtomicBooleanGetAndSet"
);
s49_test!(test_s49_atomic_ref_cas, "testAtomicRefCas");
s49_test!(test_s49_atomic_ref_get_and_set, "testAtomicRefGetAndSet");
s49_test!(
    test_s49_atomic_int_concurrent_incr,
    "testAtomicIntConcurrentIncr",
    100
);
s49_test!(test_s49_atomic_int_lazy_set, "testAtomicIntLazySet", 99);
s49_test!(test_s49_atomic_long_lazy_set, "testAtomicLongLazySet");

// --- ReentrantLock ---
s49_test!(test_s49_reentrant_lock_basic, "testReentrantLockBasic");
s49_test!(test_s49_reentrant_lock_try_lock, "testReentrantLockTryLock");
s49_test!(
    test_s49_reentrant_lock_reentrant,
    "testReentrantLockReentrant"
);
s49_test!(
    test_s49_reentrant_lock_condition,
    "testReentrantLockCondition"
);
s49_test!(test_s49_read_write_lock_basic, "testReadWriteLockBasic");

// --- CountDownLatch ---
s49_test!(test_s49_count_down_latch_basic, "testCountDownLatchBasic");
s49_test!(
    test_s49_count_down_latch_get_count,
    "testCountDownLatchGetCount"
);
s49_test!(
    test_s49_count_down_latch_extra_countdown,
    "testCountDownLatchExtraCountDown"
);
s49_test!(
    test_s49_count_down_latch_to_string,
    "testCountDownLatchToString"
);
s49_test!(
    test_s49_count_down_latch_await_timeout,
    "testCountDownLatchAwaitTimeout"
);

// --- Semaphore ---
s49_test!(test_s49_semaphore_basic, "testSemaphoreBasic");
s49_test!(test_s49_semaphore_try_acquire, "testSemaphoreTryAcquire");
s49_test!(test_s49_semaphore_drain, "testSemaphoreDrain");
s49_test!(
    test_s49_semaphore_release_above_init,
    "testSemaphoreReleaseAboveInit"
);
s49_test!(test_s49_semaphore_acquire_n, "testSemaphoreAcquireN");
s49_test!(
    test_s49_semaphore_acquire_uninterruptibly_n,
    "testSemaphoreAcquireUninterruptiblyN"
);
s49_test!(test_s49_semaphore_is_fair, "testSemaphoreIsFair");

// --- CyclicBarrier ---
s49_test!(
    test_s49_cyclic_barrier_get_parties,
    "testCyclicBarrierGetParties"
);
s49_test!(
    test_s49_cyclic_barrier_is_broken,
    "testCyclicBarrierIsBroken"
);
s49_test!(
    test_s49_cyclic_barrier_get_number_waiting,
    "testCyclicBarrierGetNumberWaiting"
);
s49_test!(test_s49_cyclic_barrier_reset, "testCyclicBarrierReset");

// --- ConcurrentHashMap ---
s49_test!(
    test_s49_concurrent_hashmap_put_get,
    "testConcurrentHashMapPutGet"
);
s49_test!(
    test_s49_concurrent_hashmap_contains_key,
    "testConcurrentHashMapContainsKey"
);
s49_test!(
    test_s49_concurrent_hashmap_remove,
    "testConcurrentHashMapRemove"
);
s49_test!(
    test_s49_concurrent_hashmap_put_if_absent,
    "testConcurrentHashMapPutIfAbsent"
);
s49_test!(
    test_s49_concurrent_hashmap_is_empty,
    "testConcurrentHashMapIsEmpty"
);
s49_test!(
    test_s49_concurrent_hashmap_get_or_default,
    "testConcurrentHashMapGetOrDefault"
);
s49_test!(
    test_s49_concurrent_hashmap_replace,
    "testConcurrentHashMapReplace"
);
s49_test!(
    test_s49_concurrent_hashmap_contains_value,
    "testConcurrentHashMapContainsValue"
);
s49_test!(
    test_s49_concurrent_hashmap_clear,
    "testConcurrentHashMapClear"
);

// --- CopyOnWriteArrayList ---
s49_test!(test_s49_cowal_add_get, "testCOWALAddGet");
s49_test!(test_s49_cowal_contains, "testCOWALContains");
s49_test!(test_s49_cowal_remove, "testCOWALRemove");
s49_test!(test_s49_cowal_is_empty, "testCOWALIsEmpty");

// --- LinkedBlockingQueue ---
s49_test!(test_s49_lbq_offer_poll, "testLinkedBlockingQueueOfferPoll");
s49_test!(test_s49_lbq_put_take, "testLinkedBlockingQueuePutTake");
s49_test!(test_s49_lbq_peek, "testLinkedBlockingQueuePeek");
s49_test!(
    test_s49_lbq_is_empty_size,
    "testLinkedBlockingQueueIsEmptySize"
);
s49_test!(test_s49_lbq_capacity, "testLinkedBlockingQueueCapacity");
s49_test!(test_s49_lbq_clear, "testLinkedBlockingQueueClear");

// --- ArrayBlockingQueue ---
s49_test!(test_s49_abq_offer_poll, "testArrayBlockingQueueOfferPoll");
s49_test!(test_s49_abq_capacity, "testArrayBlockingQueueCapacity");
s49_test!(
    test_s49_abq_remaining_capacity,
    "testArrayBlockingQueueRemainingCapacity"
);

// --- CompletableFuture ---
s49_test!(test_s49_cf_complete, "testCompletableFutureComplete");
s49_test!(
    test_s49_cf_completed_future,
    "testCompletableFutureCompletedFuture"
);
s49_test!(test_s49_cf_then_apply, "testCompletableFutureThenApply");
s49_test!(test_s49_cf_then_accept, "testCompletableFutureThenAccept");
s49_test!(test_s49_cf_state, "testCompletableFutureState");
s49_test!(test_s49_cf_cancel, "testCompletableFutureCancel");
s49_test!(
    test_s49_cf_exceptionally,
    "testCompletableFutureExceptionally"
);
s49_test!(
    test_s49_cf_is_completed_exceptionally,
    "testCompletableFutureIsCompletedExceptionally"
);

// --- Multi-threaded synchronizer tests ---
s49_test!(
    test_s49_count_down_latch_threaded,
    "testCountDownLatchThreaded",
    6
);
s49_test!(test_s49_semaphore_threaded, "testSemaphoreThreaded", 3);
s49_test!(
    test_s49_reentrant_lock_threaded,
    "testReentrantLockThreaded",
    100
);
s49_test!(
    test_s49_concurrent_hashmap_threaded,
    "testConcurrentHashMapThreaded",
    50
);
s49_test!(
    test_s49_blocking_queue_producer_consumer,
    "testBlockingQueueProducerConsumer",
    15
);
s49_test!(test_s49_cowal_threaded, "testCOWALThreaded", 4);
s49_test!(
    test_s49_synchronizer_composition,
    "testSynchronizerComposition",
    10
);

// ============================================================================
// Session 51 – JDK 25: Scoped Values (JEP 487)
// ============================================================================

macro_rules! s51_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/ScopedValueComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/ScopedValueComplete",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
}

// Basic operations
s51_test!(test_s51_new_instance, "testNewInstance");
s51_test!(test_s51_where_run, "testWhereRun");
s51_test!(test_s51_unbound_after_run, "testUnboundAfterRun");
s51_test!(test_s51_where_call, "testWhereCall");
s51_test!(test_s51_get_unbound_throws, "testGetUnboundThrows");
s51_test!(
    test_s51_is_bound_initially_false,
    "testIsBoundInitiallyFalse"
);
s51_test!(test_s51_is_bound_inside, "testIsBoundInside");

// orElse / orElseThrow
s51_test!(test_s51_or_else_bound, "testOrElseBound");
s51_test!(test_s51_or_else_unbound, "testOrElseUnbound");
s51_test!(test_s51_or_else_null, "testOrElseNull");
s51_test!(test_s51_or_else_throw_bound, "testOrElseThrowBound");
s51_test!(test_s51_or_else_throw_unbound, "testOrElseThrowUnbound");

// Nested / rebinding
s51_test!(test_s51_nested_rebinding, "testNestedRebinding");
s51_test!(
    test_s51_outer_restored_after_nested,
    "testOuterRestoredAfterNested"
);
s51_test!(test_s51_triple_nesting, "testTripleNesting");

// Multiple ScopedValues
s51_test!(test_s51_two_scoped_values, "testTwoScopedValues");
s51_test!(test_s51_chained_where, "testChainedWhere");
s51_test!(test_s51_partial_rebind, "testPartialRebind");

// Call with return values
s51_test!(test_s51_call_return, "testCallReturn");
s51_test!(test_s51_call_string_concat, "testCallStringConcat");

// Exception handling
s51_test!(
    test_s51_exception_in_run_unbinds,
    "testExceptionInRunUnbinds"
);
s51_test!(
    test_s51_exception_in_call_unbinds,
    "testExceptionInCallUnbinds"
);

// hashCode
s51_test!(test_s51_hash_code_stable, "testHashCodeStable");
s51_test!(test_s51_hash_code_different, "testHashCodeDifferent");

// Null binding
s51_test!(test_s51_bind_null, "testBindNull");

// Thread inheritance
s51_test!(test_s51_thread_visibility, "testThreadVisibility");
s51_test!(
    test_s51_child_rebind_no_affect_parent,
    "testChildRebindNoAffectParent"
);

// Carrier operations
s51_test!(test_s51_carrier_get, "testCarrierGet");
s51_test!(test_s51_multiple_runs, "testMultipleRuns");
s51_test!(
    test_s51_carrier_reuse_after_exception,
    "testCarrierReuseAfterException"
);

// ==========================================================================
// ---------------------------------------------------------------------------
// Session 48 — TCK: java.io / java.nio Tests
// ---------------------------------------------------------------------------
macro_rules! s48_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/TckIo", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/TckIo",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
}

// File operations
s48_test!(test_s48_file_createDeleteExists, "file_createDeleteExists");
s48_test!(test_s48_file_isFileIsDirectory, "file_isFileIsDirectory");
s48_test!(test_s48_file_mkdir, "file_mkdir");
s48_test!(test_s48_file_length, "file_length");
s48_test!(test_s48_file_absolutePath, "file_absolutePath");
s48_test!(test_s48_file_canReadWrite, "file_canReadWrite");

// FileOutputStream / FileInputStream
s48_test!(test_s48_fos_writeSingleByte, "fos_writeSingleByte");
s48_test!(test_s48_fos_writeBulk, "fos_writeBulk");
s48_test!(test_s48_fos_appendMode, "fos_appendMode");
s48_test!(test_s48_fis_readEof, "fis_readEof");
s48_test!(test_s48_fis_available, "fis_available");
s48_test!(test_s48_fis_skip, "fis_skip");
s48_test!(test_s48_fis_closeIdempotent, "fis_closeIdempotent");

// ByteArrayStreams
s48_test!(test_s48_baos_basic, "baos_basic");
s48_test!(test_s48_baos_size, "baos_size");
s48_test!(test_s48_baos_reset, "baos_reset");
s48_test!(test_s48_bais_readAll, "bais_readAll");
s48_test!(test_s48_bais_available, "bais_available");
s48_test!(test_s48_bais_skip, "bais_skip");
s48_test!(test_s48_baos_toString, "baos_toString");

// StringReader / StringWriter
s48_test!(test_s48_sw_basic, "sw_basic");
s48_test!(test_s48_sr_readChar, "sr_readChar");
s48_test!(
    test_s48_sr_readCharArrayMultiline,
    "sr_readCharArrayMultiline"
);

// ByteBuffer
s48_test!(test_s48_bb_allocateCapacity, "bb_allocateCapacity");
s48_test!(test_s48_bb_putGetFlip, "bb_putGetFlip");
s48_test!(test_s48_bb_putGetAbsolute, "bb_putGetAbsolute");
s48_test!(test_s48_bb_wrap, "bb_wrap");
s48_test!(test_s48_bb_clearRewind, "bb_clearRewind");
s48_test!(test_s48_bb_markReset, "bb_markReset");
s48_test!(test_s48_bb_putGetInt, "bb_putGetInt");
s48_test!(test_s48_bb_putGetLong, "bb_putGetLong");
s48_test!(test_s48_bb_putGetShort, "bb_putGetShort");
s48_test!(test_s48_bb_putGetFloat, "bb_putGetFloat");
s48_test!(test_s48_bb_putGetDouble, "bb_putGetDouble");
s48_test!(test_s48_bb_putGetChar, "bb_putGetChar");
s48_test!(test_s48_bb_hasArray, "bb_hasArray");
s48_test!(test_s48_bb_array, "bb_array");
s48_test!(test_s48_bb_remaining, "bb_remaining");
s48_test!(test_s48_bb_compact, "bb_compact");
s48_test!(test_s48_bb_slice, "bb_slice");
s48_test!(test_s48_bb_duplicate, "bb_duplicate");

// CharBuffer
s48_test!(test_s48_cb_allocatePutGet, "cb_allocatePutGet");
s48_test!(test_s48_cb_wrapCharSequence, "cb_wrapCharSequence");

// IntBuffer / LongBuffer
s48_test!(test_s48_ib_allocatePutGet, "ib_allocatePutGet");
s48_test!(test_s48_ib_wrapArray, "ib_wrapArray");
s48_test!(test_s48_lb_allocatePutGet, "lb_allocatePutGet");

// End-to-end
s48_test!(test_s48_e2e_writeReadRoundtrip, "e2e_writeReadRoundtrip");
s48_test!(test_s48_e2e_byteBufferToArray, "e2e_byteBufferToArray");
s48_test!(test_s48_e2e_baosToInputStream, "e2e_baosToInputStream");

// ==========================================================================
// NEW-14 — TCK: java.sql (JDBC) end-to-end tests.
// Exercises every NEW-14 JDBC native (DriverManager, Connection,
// Statement, PreparedStatement, CallableStatement, ResultSet, Blob,
// Clob, Savepoint, DatabaseMetaData) from real Java bytecode.
// ==========================================================================
macro_rules! new14_jdbc_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/TckJdbc", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/TckJdbc",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
}

new14_jdbc_test!(test_new14_jdbc_open_inmemory, "open_inmemory_connection");
new14_jdbc_test!(
    test_new14_jdbc_statement_ddl_dml_query,
    "statement_ddl_dml_query"
);
new14_jdbc_test!(
    test_new14_jdbc_prepared_statement_binds,
    "prepared_statement_binds_and_executes"
);
new14_jdbc_test!(
    test_new14_jdbc_rollback_discards,
    "rollback_discards_changes"
);
new14_jdbc_test!(
    test_new14_jdbc_savepoint_roundtrip,
    "savepoint_rollback_and_release"
);
new14_jdbc_test!(test_new14_jdbc_blob_round_trip, "blob_round_trip");
new14_jdbc_test!(test_new14_jdbc_clob_round_trip, "clob_round_trip");
new14_jdbc_test!(
    test_new14_jdbc_callable_inherits_prepared,
    "callable_inherits_prepared"
);
new14_jdbc_test!(
    test_new14_jdbc_metadata_identifies_sqlite,
    "metadata_identifies_sqlite"
);
new14_jdbc_test!(
    test_new14_jdbc_driver_register_list_deregister,
    "driver_register_list_deregister"
);
new14_jdbc_test!(test_new14_jdbc_e2e_mini_app, "e2e_mini_app");

// Session 53 — Pattern Matching Completeness (JEP 441, 395, 409, 507)
// ==========================================================================

macro_rules! s53_test {
    ($name:ident, $method:expr) => {
        #[test]
        fn $name() {
            if !require_extended_interpreter_tests(stringify!($name)) {
                return;
            }
            require_class_files!();
            let mut vm = test_vm();
            let result = vm.invoke("cratonvm/PatternComplete", $method, "()I", &[]);
            corpus_check(
                &vm,
                "cratonvm/PatternComplete",
                $method,
                "Int(1)",
                matches!(result, Ok(Some(Value::Int(1)))),
                &result,
            );
        }
    };
}

// Type patterns
s53_test!(test_s53_string_pattern, "testStringPattern");
s53_test!(test_s53_supertype_match, "testSupertypeMatch");
s53_test!(test_s53_default_case, "testDefaultCase");
s53_test!(test_s53_null_vs_default, "testNullVsDefault");
s53_test!(test_s53_null_in_middle, "testNullInMiddle");

// Guard expressions
s53_test!(test_s53_guard_pass, "testGuardPass");
s53_test!(test_s53_guard_fail, "testGuardFail");
s53_test!(test_s53_multiple_guards, "testMultipleGuards");

// Record patterns
s53_test!(test_s53_record_decon, "testRecordDecon");
s53_test!(test_s53_record_guard, "testRecordGuard");
s53_test!(
    test_s53_record_object_component,
    "testRecordObjectComponent"
);
s53_test!(test_s53_nested_records, "testNestedRecords");
s53_test!(test_s53_record_null, "testRecordNull");

// Sealed class patterns
s53_test!(test_s53_sealed_switch, "testSealedSwitch");
s53_test!(test_s53_sealed_rect, "testSealedRect");
s53_test!(test_s53_sealed_decon, "testSealedDecon");

// instanceof patterns
s53_test!(test_s53_instanceof_pattern, "testInstanceofPattern");
s53_test!(test_s53_instanceof_no_match, "testInstanceofNoMatch");
s53_test!(test_s53_instanceof_null, "testInstanceofNull");
s53_test!(test_s53_instanceof_chain, "testInstanceofChain");

// Mixed / integration
s53_test!(test_s53_mixed_dispatch, "testMixedDispatch");
s53_test!(test_s53_area_calc, "testAreaCalc");
