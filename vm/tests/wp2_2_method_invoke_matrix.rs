// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.2 — `Method.invoke` 50-case matrix.
//!
//! End-to-end coverage for `java.lang.reflect.Method.invoke` and supporting
//! reflection natives (getDeclaredMethod, getReturnType, getParameterTypes,
//! getModifiers). The probe at `apps/method_invoke_probe/MethodInvokeProbe.java`
//! defines 50 cases each printing a `PASS-N: ...` or `FAIL-N: ...` line plus
//! a final `OK 50/50` / `FAIL` summary. We drive that probe through the
//! `cratonvm.exe` CLI binary and assert on the captured stdout.
//!
//! The test layout is:
//! - 9 registry-gate tests (carried over from the staging contract): these
//!   verify the relevant natives are registered with the correct JDK 25
//!   signatures and that the probe class artifacts are staged on disk.
//! - 1 composite end-to-end test that runs the probe once and pins the
//!   `OK 50/50` line as the acceptance gate.
//! - 10 per-category subtests that re-parse the same captured probe output
//!   and assert on each case-range. The subtests share a one-shot probe run
//!   (memoized via `OnceLock`) so the whole file finishes in seconds.
//!
//! When Agent W's `Method.invoke` fix lands, these tests should all PASS.
//! Until then, the per-category subtests will report exactly which categories
//! still regress so triage can be focused. We do NOT mark the failing
//! sub-categories `#[ignore]` — they are the acceptance gates for the WP.

use cratonvm_native_api::NativeMethodRegistry;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Existing registry-gate tests (kept as-is)
// ---------------------------------------------------------------------------

#[test]
fn method_invoke_native_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(
        r.find(
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
        )
        .is_some(),
        "Method.invoke must be registered with the JDK 25 signature"
    );
}

#[test]
fn method_introspection_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let m = "java/lang/reflect/Method";
    let cases: &[(&str, &str)] = &[
        ("getName", "()Ljava/lang/String;"),
        ("getReturnType", "()Ljava/lang/Class;"),
        ("getParameterTypes", "()[Ljava/lang/Class;"),
        ("getDeclaringClass", "()Ljava/lang/Class;"),
        ("getModifiers", "()I"),
        ("getParameterCount", "()I"),
        ("setAccessible", "(Z)V"),
    ];
    for (name, sig) in cases {
        assert!(
            r.find(m, name, sig).is_some(),
            "Method.{name}{sig} must be registered"
        );
    }
}

#[test]
fn class_get_declared_method_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let public_get = r.find(
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    );
    let internal_get0 = r.find(
        "java/lang/Class",
        "getDeclaredMethods0",
        "(Z)[Ljava/lang/reflect/Method;",
    );
    assert!(
        public_get.is_some() || internal_get0.is_some(),
        "either Class.getDeclaredMethod or getDeclaredMethods0 must be registered"
    );
}

#[test]
fn invocation_target_exception_class_resolvable() {
    let target = "java/lang/reflect/InvocationTargetException";
    assert_eq!(
        target.replace('/', "."),
        "java.lang.reflect.InvocationTargetException"
    );
}

#[test]
fn method_invoke_probe_class_files_exist_when_staged() {
    let probe = probe_classes_dir();
    if !probe.exists() {
        let _ = common::require_fixture(
            "wp2_2_method_invoke_matrix",
            "the WP2.2 fixture directory `method_invoke_probe/classes` (built from \
             MethodInvokeProbe.java)",
            &[probe.clone()],
        );
        return;
    }
    assert!(
        probe.join("MethodInvokeProbe.class").exists(),
        "MethodInvokeProbe.class must be staged"
    );
    for inner in &[
        "MethodInvokeProbe$Targets.class",
        "MethodInvokeProbe$Custom.class",
        "MethodInvokeProbe$Sub.class",
        "MethodInvokeProbe$AbstractBase.class",
        "MethodInvokeProbe$IFace.class",
        "MethodInvokeProbe$WithIface.class",
    ] {
        assert!(probe.join(inner).exists(), "{inner} must be staged");
    }
}

#[test]
fn primitive_descriptor_set_complete() {
    let primitives: &[&str] = &["Z", "B", "C", "S", "I", "J", "F", "D"];
    assert_eq!(primitives.len(), 8);
    let v: &str = "V";
    assert!(!primitives.contains(&v));
}

#[test]
fn boxed_wrapper_class_names_complete() {
    let wrappers: &[&str] = &[
        "java/lang/Byte",
        "java/lang/Short",
        "java/lang/Character",
        "java/lang/Integer",
        "java/lang/Long",
        "java/lang/Float",
        "java/lang/Double",
        "java/lang/Boolean",
    ];
    assert_eq!(wrappers.len(), 8);
}

#[test]
fn essential_natives_register_no_duplicates() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    assert!(r.len() > 200, "registry too small, got {}", r.len());
}

#[test]
fn modifier_flags_match_jdk_constants() {
    const ACC_STATIC: i32 = 0x0008;
    const ACC_PUBLIC: i32 = 0x0001;
    const ACC_PRIVATE: i32 = 0x0002;
    const ACC_PROTECTED: i32 = 0x0004;
    assert_eq!(ACC_STATIC, 8);
    assert_eq!(ACC_PUBLIC | ACC_PRIVATE | ACC_PROTECTED, 7);
}

#[test]
fn descriptor_parsing_round_trip() {
    let descriptors: &[(&str, usize, &str)] = &[
        ("(I)I", 1, "I"),
        ("(BSC)Z", 3, "Z"),
        ("([I)I", 1, "I"),
        (
            "([Ljava/lang/String;)Ljava/lang/String;",
            1,
            "Ljava/lang/String;",
        ),
        ("()V", 0, "V"),
        (
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            1,
            "Ljava/lang/Object;",
        ),
        ("(JJ)D", 2, "D"),
    ];
    for (desc, expected_arity, expected_ret) in descriptors {
        let close = desc.find(')').expect("descriptor must have ')'");
        let _params_part = &desc[1..close];
        let ret_part = &desc[close + 1..];
        assert_eq!(
            ret_part, *expected_ret,
            "ret mismatch for {desc}: expected {expected_ret}, got {ret_part}"
        );
        let mut count = 0;
        let mut chars = desc[1..close].chars();
        while let Some(c) = chars.next() {
            match c {
                'L' => {
                    while chars.next() != Some(';') {}
                    count += 1;
                }
                '[' => {
                    let mut k = chars.next();
                    while k == Some('[') {
                        k = chars.next();
                    }
                    if k == Some('L') {
                        while chars.next() != Some(';') {}
                    }
                    count += 1;
                }
                _ => count += 1,
            }
        }
        assert_eq!(
            count, *expected_arity,
            "arity mismatch for {desc}: expected {expected_arity}, got {count}"
        );
    }
}

// ---------------------------------------------------------------------------
// End-to-end probe driver
// ---------------------------------------------------------------------------

/// The `apps/method_invoke_probe/classes` directory that holds the compiled
/// MethodInvokeProbe.class plus inner classes.
fn probe_classes_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("method_invoke_probe")
        .join("classes")
}

/// Source `.java` file for the probe. Used by the on-demand javac fallback.
fn probe_source_file() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("method_invoke_probe")
        .join("MethodInvokeProbe.java")
}

/// Resolve the cratonvm CLI binary. Prefer release (faster), fall back to
/// debug; honor the `CRATONVM_BIN` env var for hermetic CI builds.
mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Compile the probe via `javac` if the classes directory is missing or stale.
/// Best-effort: returns true if the classes dir contains MethodInvokeProbe.class
/// after the call.
fn ensure_probe_compiled() -> bool {
    let classes = probe_classes_dir();
    if classes.join("MethodInvokeProbe.class").exists() {
        return true;
    }
    let src = probe_source_file();
    if !src.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`. `apps/` is gitignored (.gitignore line 12),
        // which is why this fixture was never tracked and is absent here.
        let _ = common::require_fixture(
            "wp2_2_method_invoke_matrix",
            "the WP2.2 fixture `MethodInvokeProbe.java` (this file's per-case tests pin its \
             MethodInvokeProbe$Targets / $Custom / $Sub / $AbstractBase / $IFace / $WithIface \
             inner classes)",
            &[src.clone()],
        );
        return false;
    }
    let _ = std::fs::create_dir_all(&classes);
    let status = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output();
    match status {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[wp2_2_method_invoke_matrix] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wp2_2_method_invoke_matrix] the checked-in probe fixture failed to compile \
                 — fix the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            classes.join("MethodInvokeProbe.class").exists()
        }
    }
}

/// Captured probe output (stdout). Returns `None` when we cannot run the probe
/// (binary missing, classes missing, javac unavailable, etc.) — callers
/// should `eprintln!` and short-circuit so the test reports `skip` rather
/// than misattributing failure.
fn run_probe_once() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if !ensure_probe_compiled() {
                eprintln!("[wp2_2] probe class files unavailable; skipping");
                return None;
            }
            let bin = match cratonvm_binary() {
                Some(b) => b,
                None => {
                    eprintln!("[wp2_2] cratonvm binary not found; build with `cargo build --release -p cratonvm-cli`");
                    return None;
                }
            };
            let classes = probe_classes_dir();
            let output = Command::new(&bin)
                .arg("-c")
                .arg(&classes)
                .arg("MethodInvokeProbe")
                .output();
            match output {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                    // Return both joined so callers can grep either; the
                    // probe prints PASS/FAIL on stdout but the System.exit
                    // tracer line goes to stderr.
                    Some(format!("{stdout}\n--- STDERR ---\n{stderr}"))
                }
                Err(e) => {
                    eprintln!("[wp2_2] failed to spawn cratonvm: {e}");
                    None
                }
            }
        })
        .clone()
}

/// Returns true if the probe output contains `PASS-{n}:`.
fn case_passed(output: &str, n: u32) -> bool {
    output.contains(&format!("PASS-{n}:"))
}

/// Returns the FAIL message for the case if it failed, else None.
fn fail_message(output: &str, n: u32) -> Option<String> {
    let needle = format!("FAIL-{n}:");
    output
        .lines()
        .find(|l| l.contains(&needle))
        .map(|l| l.to_string())
}

/// Assert that every case in `range` PASSes; if any fails, the panic
/// message lists every failing case + its FAIL line for triage.
fn assert_cases_pass(category: &str, output: &str, cases: &[u32]) {
    let mut failures = Vec::new();
    for &n in cases {
        if !case_passed(output, n) {
            let msg = fail_message(output, n)
                .unwrap_or_else(|| format!("(no PASS-{n} or FAIL-{n} line found in probe output)"));
            failures.push(msg);
        }
    }
    if !failures.is_empty() {
        panic!(
            "WP2.2 [{category}] {} of {} cases failed:\n  {}",
            failures.len(),
            cases.len(),
            failures.join("\n  ")
        );
    }
}

// ---------------------------------------------------------------------------
// Composite acceptance gate
// ---------------------------------------------------------------------------

/// Composite end-to-end test: runs the probe once and pins the final
/// `OK 50/50` line. This is the WP2.2 acceptance gate — failing this
/// means the probe didn't reach 50/50 PASS, regardless of category.
///
/// Until Agent W's interpreter fix lands this test is expected to FAIL —
/// that is by design. We do NOT `#[ignore]` it so the gap stays visible
/// in `cargo test` runs.
#[test]
fn method_invoke_probe_composite_50_of_50() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return, // skipped (no binary / no classes / no javac)
    };
    if !output.contains("OK 50/50") {
        // Pull out the summary line for a clean panic message.
        let summary: String = output
            .lines()
            .filter(|l| l.starts_with("PASSED:") || l.starts_with("FAILED:") || *l == "FAIL")
            .collect::<Vec<_>>()
            .join(" | ");
        panic!("WP2.2 composite probe did not reach 50/50. Summary: {summary}");
    }
}

// ---------------------------------------------------------------------------
// Per-category subtests
//
// Each subtest re-parses the same memoized probe output (`run_probe_once`)
// and asserts on a contiguous range of cases from MethodInvokeProbe.java.
// Failures show exactly which case in the category regressed.
// ---------------------------------------------------------------------------

/// Cases 1-8: 8 primitive arg+return types (byte/short/int/long/float/double/char/boolean).
#[test]
fn method_invoke_cat_primitives_arg_return() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("primitives 1-8", &output, &[1, 2, 3, 4, 5, 6, 7, 8]);
}

/// Cases 9-16: 8 boxed-primitive args (Byte/Short/Integer/Long/Float/Double/Character/Boolean)
/// — Method.invoke must auto-unbox these to feed primitive-typed parameters.
#[test]
fn method_invoke_cat_boxed_primitive_unbox() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass(
        "boxed unbox 9-16",
        &output,
        &[9, 10, 11, 12, 13, 14, 15, 16],
    );
}

/// Cases 17-19: object args (String identity, custom-class pass-through, null arg).
#[test]
fn method_invoke_cat_object_and_null_args() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("object/null args 17-19", &output, &[17, 18, 19]);
}

/// Cases 20-21: array args — int[] sum and String[] join.
#[test]
fn method_invoke_cat_array_args() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("array args 20-21", &output, &[20, 21]);
}

/// Cases 22-25: dispatch shape — static (22), instance (23), interface default (24),
/// interface static (25).
#[test]
fn method_invoke_cat_dispatch_shape() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("dispatch shape 22-25", &output, &[22, 23, 24, 25]);
}

/// Cases 26-27: access-control + abstract dispatch.
/// 26: private + setAccessible(true). 27: abstract method on concrete subclass.
#[test]
fn method_invoke_cat_access_and_abstract() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("access+abstract 26-27", &output, &[26, 27]);
}

/// Cases 28-30: exception/access edge cases — ITE wrapping (28), lenient
/// private invoke without setAccessible (29), varargs Object[] flattening (30).
#[test]
fn method_invoke_cat_exception_and_varargs() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("exception/varargs 28-30", &output, &[28, 29, 30]);
}

/// Cases 31-34: void return (31), virtual dispatch on subclass receiver (32),
/// instance method with String arg (33), NPE on null receiver of instance
/// method (34).
#[test]
fn method_invoke_cat_void_virtual_npe() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("void/virtual/npe 31-34", &output, &[31, 32, 33, 34]);
}

/// Cases 35-37: argument-validation errors — NoSuchMethodException on wrong
/// param types (35), IllegalArgumentException on wrong arg count (36) and
/// type mismatch (37).
#[test]
fn method_invoke_cat_arg_validation() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("arg validation 35-37", &output, &[35, 36, 37]);
}

/// Cases 38-41: Method introspection round-trips — getReturnType (38),
/// getParameterTypes (39), getParameterCount (40), getModifiers + Modifier.isStatic (41).
#[test]
fn method_invoke_cat_introspection() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("introspection 38-41", &output, &[38, 39, 40, 41]);
}

/// Cases 42-49: round-trip primitive boxing through invoke for all 8
/// primitive types — verifies that boxing on the call boundary preserves
/// the value (boundary cases like Integer.MAX_VALUE, Long.MAX_VALUE).
#[test]
fn method_invoke_cat_primitive_round_trip_boundaries() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass(
        "primitive round-trip 42-49",
        &output,
        &[42, 43, 44, 45, 46, 47, 48, 49],
    );
}

/// Case 50: invoke triggers class init for the declaring class on first use.
#[test]
fn method_invoke_cat_init_on_first_invoke() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    assert_cases_pass("init on invoke 50", &output, &[50]);
}
