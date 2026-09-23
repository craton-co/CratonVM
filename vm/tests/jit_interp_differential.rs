// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Continuous JIT-vs-interpreter (-vs-HotSpot) differential suite.
//!
//! Feature: feat-runtime — "expand the differential harness into a continuous
//! JIT-vs-interpreter-vs-HotSpot suite".
//!
//! ## What this verifies
//!
//! The JIT must be **behavior-identical to the interpreter** for the core
//! bytecode kernels: integer/long arithmetic (incl. overflow and MIN_VALUE),
//! float/double (incl. NaN / -0.0), shifts with spec masking, idiv/ldiv on the
//! mandated `MIN_VALUE / -1` overflow case, array load/store + bounds, simple
//! counted loops, and method calls that pass a freshly-allocated object as a
//! call argument (the bug-25 escape-analysis class).
//!
//! The acceptance criterion is the same shape as the intrinsic-table
//! differential (`vm/tests/intrinsic_diff.rs`): run the *same* Java program
//! once interpreted and once JIT'd and assert **byte-for-byte identical**
//! stdout + identical exit code. A divergence here is a JIT miscompile of the
//! kind that historically slipped through (bug-25 call-arg scalar-replacement,
//! the `ir_lower` SETcc condition lowering, and `aastore` covariance/bounds).
//!
//! ## How the two modes are driven (so the orchestrator can verify)
//!
//! The JIT kill-switch (`CRATONVM_DISABLE_JIT`) and the warmup threshold
//! (`CRATONVM_JIT_THRESHOLD`) are each read **once per process** (an
//! `OnceLock` in `vm/src/runtime/env_cache.rs`), so an in-process `Vm::new`
//! cannot exercise both modes in one test binary. We therefore launch the
//! **`cratonvm` CLI binary as a subprocess** — exactly like
//! `vm/tests/intrinsic_diff.rs` and `vm/tests/cluster_c_constructor.rs`:
//!
//! ```text
//!   <cratonvm-bin>  -c  <vm/tests/resources>  cratonvm.JitDifferential
//! ```
//!
//! with `stdout`/`stderr` piped and a per-child environment:
//!
//! | mode        | env                                                                |
//! |-------------|--------------------------------------------------------------------|
//! | interpreter | `CRATONVM_DISABLE_JIT=1`                                            |
//! | jit         | `CRATONVM_JIT_THRESHOLD=1`                                          |
//!
//! The JIT mode lowers the warmup threshold to 1 so the kernels in
//! `JitDifferential.java` are compiled before their results are recorded — the
//! fixture warms every kernel in a loop *before* replaying the edge-case matrix
//! for exactly this reason.
//!
//! This mode also used to pass `CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/` to lift
//! a blanket `cratonvm/*` skip-list ban. `d1979bec5` deleted the static ban
//! machinery outright, taking the `package_allowed` gate and the variable's
//! only reader with it, so there is no ban to lift and the variable reached
//! nothing.
//!
//! ## Optional HotSpot triangulation
//!
//! When the `CRATONVM_DIFF_HOTSPOT` env var is set (to anything non-empty),
//! the suite also runs `JitDifferential` under a real `java` on PATH and
//! asserts the `r:` observation lines match CratonVM. This part is **off by
//! default** so the JIT-vs-interpreter net runs in CI with no JDK present; it
//! is the "-vs-HotSpot" third leg of the differential when a JDK is available.
//!
//! ## Prerequisite gating
//!
//! Mirrors every other subprocess test in `vm/tests/`: if `javac` did not
//! compile the fixture, or the `cratonvm` binary has not been built, the test
//! prints a skip notice and returns (it does **not** fail). Build the binary
//! with `cargo build -p cratonvm-cli` (or set `CRATONVM_BIN`).
//!
//! Run with:
//!     cargo test -p cratonvm-vm --test jit_interp_differential -- --nocapture
//!
//! Run including HotSpot triangulation (needs `java`/`javac` on PATH):
//!     CRATONVM_DIFF_HOTSPOT=1 cargo test -p cratonvm-vm \
//!         --test jit_interp_differential -- --nocapture

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hard per-subprocess timeout. The differential program is short; a hang here
/// means a JIT livelock / infinite OSR re-entry, which we want surfaced as a
/// loud failure rather than a stalled CI job.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

/// Simple name of the differential fixture (package `cratonvm`).
const FIXTURE: &str = "JitDifferential";

/// Completion marker `JitDifferential.main` prints last. Its absence means the
/// program aborted mid-replay (e.g. a JIT `#DE` on `idiv(MIN_VALUE, -1)`).
const OK_MARKER: &str = "JIT_DIFFERENTIAL_OK ";

// ---------------------------------------------------------------------------
// Path / binary resolution (mirrors intrinsic_diff.rs)
// ---------------------------------------------------------------------------

/// Workspace root (parent of the `vm` crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// The committed fixture classpath directory: `vm/tests/resources/`.
/// `vm/build.rs` stages freshly compiled classes under
/// `CRATONVM_TEST_CLASSES_DIR`; this test still uses committed fixtures.
fn classpath_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
}

/// Resolve the `cratonvm` CLI binary. Mirrors the helper used by the other
/// subprocess tests (`intrinsic_diff.rs`, `cluster_c_constructor.rs`, ...).
///
/// Resolution order:
///   1. `CRATONVM_BIN` env var, if it points to an existing file.
///   2. `target/release/cratonvm[.exe]`
///   3. `target/debug/cratonvm[.exe]`
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
    let target = workspace_root().join("target");
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

/// True if the compiled `.class` for `cratonvm/<simple_name>` exists.
fn class_file_present(simple_name: &str) -> bool {
    classpath_dir()
        .join("cratonvm")
        .join(format!("{simple_name}.class"))
        .exists()
}

// ---------------------------------------------------------------------------
// Execution mode
// ---------------------------------------------------------------------------

/// Which execution engine the child process should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Pure interpreter — `CRATONVM_DISABLE_JIT=1`. This is the oracle.
    Interpreter,
    /// JIT enabled and forced to compile the fixture's `cratonvm/*` kernels
    /// eagerly (`CRATONVM_JIT_THRESHOLD=1`).
    Jit,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Interpreter => "interpreter",
            Mode::Jit => "jit",
        }
    }

    /// Apply this mode's environment to a child `Command`. We set OR clear
    /// every relevant var explicitly so the child's environment is
    /// deterministic regardless of what the test harness inherited.
    fn apply_env(self, cmd: &mut Command) {
        match self {
            Mode::Interpreter => {
                cmd.env("CRATONVM_DISABLE_JIT", "1");
                cmd.env_remove("CRATONVM_JIT_THRESHOLD");
            }
            Mode::Jit => {
                cmd.env_remove("CRATONVM_DISABLE_JIT");
                // The `CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/` line that stood
                // here lifted the blanket `cratonvm/*` JIT ban. `d1979bec5`
                // deleted the static ban machinery along with its last reader,
                // so there is no blanket left to lift and the variable had
                // become a no-op dressed as a precondition. Same removal as in
                // `jit_collection_ctor_identity.rs`.
                // Compile as soon as a method is warm so the replayed kernels
                // run JIT'd, not interpreted.
                cmd.env("CRATONVM_JIT_THRESHOLD", "1");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Subprocess runner
// ---------------------------------------------------------------------------

/// Outcome of one subprocess run of a Java class on the cratonvm CLI.
#[derive(Debug, Clone)]
struct Run {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

/// Run `cratonvm -c <classpath> cratonvm.<FIXTURE>` as a child process under
/// the given execution `mode`.
///
/// Returns `Some(Run)` on a clean spawn+wait, or `None` if a prerequisite is
/// missing (binary not built / class not compiled) so callers can skip.
fn run_fixture(mode: Mode) -> Option<Run> {
    if !class_file_present(FIXTURE) {
        eprintln!(
            "[jit_interp_diff] {FIXTURE}.class not found under {} — javac \
             unavailable at build time? skipping.",
            classpath_dir().display()
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[jit_interp_diff] cratonvm binary not found; build it with \
                 `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
            );
            return None;
        }
    };

    let mut cmd = Command::new(&bin);
    cmd.arg("-c")
        .arg(classpath_dir())
        .arg(format!("cratonvm.{FIXTURE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    mode.apply_env(&mut cmd);

    let child = cmd.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[jit_interp_diff] failed to spawn cratonvm ({}): {e}",
                mode.label()
            );
            return None;
        }
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > RUN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[jit_interp_diff] {FIXTURE} ({}) timed out after {RUN_TIMEOUT:?} \
                         — likely a JIT livelock / infinite OSR re-entry.",
                        mode.label(),
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                eprintln!("[jit_interp_diff] try_wait failed: {e}");
                return None;
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_interp_diff] wait_with_output failed: {e}");
            return None;
        }
    };

    Some(Run {
        stdout: normalize(&String::from_utf8_lossy(&output.stdout)),
        stderr: normalize(&String::from_utf8_lossy(&output.stderr)),
        exit_code: output.status.code(),
    })
}

/// Normalize line endings (CRLF -> LF) and strip a trailing newline so a stray
/// platform `\r` cannot masquerade as a behavioral divergence.
fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// Extract the `r:` observation lines (the per-kernel results emitted by
/// `JitDifferential.java`) from a stdout blob, dropping incidental log noise.
fn observation_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| l.starts_with("r:")).collect()
}

/// Locate the first observation line on which two runs differ — used to
/// localise a divergence in the failure message.
fn first_obs_diff(a: &[&str], b: &[&str]) -> Option<(usize, String, String)> {
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or("<missing>");
        let y = b.get(i).copied().unwrap_or("<missing>");
        if x != y {
            return Some((i + 1, x.to_string(), y.to_string()));
        }
    }
    None
}

/// Assert a run reached the OK marker; return a descriptive panic otherwise.
fn assert_reached_marker(run: &Run, mode: Mode) {
    assert!(
        run.stdout.contains(OK_MARKER),
        "{} run did not reach the {OK_MARKER:?} marker — the program aborted \
         mid-replay (a JIT miscompile such as `#DE` on idiv MIN/-1, or a crash \
         in a JIT'd kernel).\nexit={:?}\nstdout:\n{}\nstderr:\n{}",
        mode.label(),
        run.exit_code,
        run.stdout,
        run.stderr,
    );
}

// ===========================================================================
// Test 1 — the core JIT-vs-interpreter differential
// ===========================================================================

/// Run `JitDifferential` interpreted, then JIT'd, and assert byte-for-byte
/// identical `r:` observations, identical exit codes, and that both reached
/// the completion marker. This is the regression net that would have caught
/// bug-25 (call-arg scalar-replacement), the `ir_lower` SETcc lowering, and the
/// `aastore` covariance/bounds miscompiles.
#[test]
fn jit_matches_interpreter() {
    let interp = match run_fixture(Mode::Interpreter) {
        Some(r) => r,
        None => return, // prerequisite missing; skip (see module docs)
    };
    let jit = match run_fixture(Mode::Jit) {
        Some(r) => r,
        None => return,
    };

    assert_reached_marker(&interp, Mode::Interpreter);
    assert_reached_marker(&jit, Mode::Jit);

    let interp_obs = observation_lines(&interp.stdout);
    let jit_obs = observation_lines(&jit.stdout);

    // The decisive check: identical per-kernel observations.
    if interp_obs != jit_obs {
        let where_ = first_obs_diff(&interp_obs, &jit_obs)
            .map(|(n, x, y)| {
                format!("first divergence at observation {n}:\n  INTERP: {x}\n  JIT   : {y}")
            })
            .unwrap_or_else(|| "(divergence in observation count / trailing content)".to_string());
        panic!(
            "JIT MISCOMPILE: stdout observations differ between the interpreter \
             and the JIT.\n{where_}\n\
             The JIT is NOT behavior-identical to the interpreter.\n\
             --- INTERP ({} obs) ---\n{}\n--- JIT ({} obs) ---\n{}\n\
             --- JIT stderr ---\n{}",
            interp_obs.len(),
            interp.stdout,
            jit_obs.len(),
            jit.stdout,
            jit.stderr,
        );
    }

    // Exit codes must also match, and a clean main() return must not be
    // nonzero in either mode.
    assert_eq!(
        interp.exit_code, jit.exit_code,
        "exit code diverged: interpreter={:?} jit={:?}\ninterp stderr:\n{}\njit stderr:\n{}",
        interp.exit_code, jit.exit_code, interp.stderr, jit.stderr,
    );
    assert!(
        matches!(interp.exit_code, Some(0) | None),
        "interpreter run exited nonzero ({:?}) despite the OK marker.\nstderr:\n{}",
        interp.exit_code,
        interp.stderr,
    );

    // Sanity: the matrix actually ran (not aborted after one or two lines).
    assert!(
        interp_obs.len() >= 50,
        "expected the differential program to emit many 'r:' observations \
         (one per edge case); got only {}. Did it abort early?\nstdout:\n{}",
        interp_obs.len(),
        interp.stdout,
    );

    eprintln!(
        "[jit_interp_diff] jit_matches_interpreter: {} kernel observations \
         identical interpreter vs JIT; exit codes match ({:?}).",
        interp_obs.len(),
        interp.exit_code,
    );
}

// ===========================================================================
// Test 2 — focused checks on the historically-miscompiled kernel families
// ===========================================================================

/// Re-run the differential and assert parity *specifically* on the kernel
/// families that have regressed before, so a family-local miscompile is
/// reported with a focused message even though `jit_matches_interpreter` would
/// also catch it. Also proves those families' edge cases actually executed.
#[test]
fn miscompile_family_parity() {
    let interp = match run_fixture(Mode::Interpreter) {
        Some(r) => r,
        None => return,
    };
    let jit = match run_fixture(Mode::Jit) {
        Some(r) => r,
        None => return,
    };
    assert_reached_marker(&interp, Mode::Interpreter);
    assert_reached_marker(&jit, Mode::Jit);

    // Each entry: (human label, observation-prefix, expected-min-count).
    // The prefixes match the `r:<label>=...` lines emitted by the fixture.
    let families: &[(&str, &str, usize)] = &[
        ("call-arg escape (bug-25)", "r:viaCall.", 3),
        ("SETcc / comparison lowering", "r:cmp", 4),
        ("aastore covariance + bounds", "r:arrStore.", 3),
        ("array load bounds", "r:arrIntBounds.", 3),
        ("idiv/irem MIN/-1 overflow", "r:idiv.min_by_-1", 1),
        ("ldiv MIN/-1 overflow", "r:ldiv.min_by_-1", 1),
        ("shift masking", "r:ishl.", 2),
        ("float/double NaN / -0.0", "r:dcmp", 2),
    ];

    for (label, prefix, min_count) in families {
        let i_lines: Vec<&str> = interp
            .stdout
            .lines()
            .filter(|l| l.starts_with(prefix))
            .collect();
        let j_lines: Vec<&str> = jit
            .stdout
            .lines()
            .filter(|l| l.starts_with(prefix))
            .collect();

        assert!(
            i_lines.len() >= *min_count,
            "{label}: expected >= {min_count} '{prefix}*' observations in the \
             interpreter run; got {}. The fixture did not reach this family.\n\
             stdout:\n{}",
            i_lines.len(),
            interp.stdout,
        );

        assert_eq!(
            i_lines, j_lines,
            "{label}: JIT diverged from interpreter on '{prefix}*'.\n\
             INTERP: {i_lines:#?}\nJIT   : {j_lines:#?}\njit stderr:\n{}",
            jit.stderr,
        );
    }

    eprintln!(
        "[jit_interp_diff] miscompile_family_parity: all {} historically-risky \
         kernel families identical interpreter vs JIT.",
        families.len(),
    );
}

// ===========================================================================
// Test 3 — optional HotSpot triangulation (the third leg)
// ===========================================================================

/// When `CRATONVM_DIFF_HOTSPOT` is set, run `JitDifferential` under a real
/// `java` on PATH and assert the `r:` observations match CratonVM's
/// interpreter (which test 1 already pins to the JIT). This closes the
/// "-vs-HotSpot" leg of the suite. Off by default so CI runs the
/// JIT-vs-interpreter net with no JDK present.
#[test]
fn hotspot_triangulation() {
    if std::env::var_os("CRATONVM_DIFF_HOTSPOT").is_none() {
        eprintln!(
            "[jit_interp_diff] hotspot_triangulation: CRATONVM_DIFF_HOTSPOT \
             unset — skipping HotSpot leg (JIT-vs-interpreter still covered by \
             jit_matches_interpreter)."
        );
        return;
    }
    if !class_file_present(FIXTURE) {
        eprintln!("[jit_interp_diff] hotspot_triangulation: fixture not compiled; skipping.");
        return;
    }

    let cratonvm = match run_fixture(Mode::Interpreter) {
        Some(r) => r,
        None => return,
    };
    assert_reached_marker(&cratonvm, Mode::Interpreter);

    let hotspot = match run_hotspot() {
        Some(r) => r,
        None => {
            eprintln!(
                "[jit_interp_diff] hotspot_triangulation: `java` not runnable; \
                 skipping despite CRATONVM_DIFF_HOTSPOT (no JDK on PATH?)."
            );
            return;
        }
    };

    assert!(
        hotspot.stdout.contains(OK_MARKER),
        "HotSpot did not reach the OK marker — fixture itself may be broken.\n\
         stdout:\n{}\nstderr:\n{}",
        hotspot.stdout,
        hotspot.stderr,
    );

    let c_obs = observation_lines(&cratonvm.stdout);
    let h_obs = observation_lines(&hotspot.stdout);

    if c_obs != h_obs {
        let where_ = first_obs_diff(&c_obs, &h_obs)
            .map(|(n, x, y)| {
                format!("first divergence at observation {n}:\n  CRATONVM: {x}\n  HOTSPOT : {y}")
            })
            .unwrap_or_else(|| "(divergence in observation count)".to_string());
        panic!(
            "HOTSPOT DIVERGENCE: CratonVM (interpreter) differs from HotSpot.\n{where_}\n\
             --- CRATONVM ({} obs) ---\n{}\n--- HOTSPOT ({} obs) ---\n{}",
            c_obs.len(),
            cratonvm.stdout,
            h_obs.len(),
            hotspot.stdout,
        );
    }

    eprintln!(
        "[jit_interp_diff] hotspot_triangulation: {} observations identical \
         CratonVM vs HotSpot.",
        c_obs.len(),
    );
}

/// Run the fixture under a real `java` on PATH (HotSpot). Returns `None` if
/// `java` cannot be launched (so the caller skips gracefully).
fn run_hotspot() -> Option<Run> {
    let java = if cfg!(windows) { "java.exe" } else { "java" };
    let out = Command::new(java)
        .arg("-cp")
        .arg(classpath_dir())
        .arg(format!("cratonvm.{FIXTURE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) => Some(Run {
            stdout: normalize(&String::from_utf8_lossy(&o.stdout)),
            stderr: normalize(&String::from_utf8_lossy(&o.stderr)),
            exit_code: o.status.code(),
        }),
        Err(e) => {
            eprintln!("[jit_interp_diff] failed to spawn java: {e}");
            None
        }
    }
}

// ===========================================================================
// Unit tests for the harness helpers (no external deps — always run)
// ===========================================================================

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn normalize_strips_crlf_and_trailing() {
        assert_eq!(normalize("a\r\nb\r\n"), "a\nb");
        assert_eq!(normalize("x   \n"), "x");
    }

    #[test]
    fn observation_lines_filters_prefix() {
        let s = "noise\nr:a=1\nlog\nr:b=2\n";
        assert_eq!(observation_lines(s), vec!["r:a=1", "r:b=2"]);
    }

    #[test]
    fn first_obs_diff_finds_first_mismatch() {
        let a = vec!["r:a=1", "r:b=2", "r:c=3"];
        let b = vec!["r:a=1", "r:b=9", "r:c=3"];
        assert_eq!(
            first_obs_diff(&a, &b),
            Some((2, "r:b=2".to_string(), "r:b=9".to_string()))
        );
    }

    #[test]
    fn first_obs_diff_detects_length_mismatch() {
        let a = vec!["r:a=1"];
        let b = vec!["r:a=1", "r:b=2"];
        assert_eq!(
            first_obs_diff(&a, &b),
            Some((2, "<missing>".to_string(), "r:b=2".to_string()))
        );
    }

    #[test]
    fn first_obs_diff_none_when_equal() {
        let a = vec!["r:a=1", "r:b=2"];
        assert_eq!(first_obs_diff(&a, &a.clone()), None);
    }

    #[test]
    fn mode_labels_are_distinct() {
        assert_ne!(Mode::Interpreter.label(), Mode::Jit.label());
    }

    #[test]
    fn classpath_dir_exists() {
        assert!(
            classpath_dir().exists(),
            "test resources directory should exist: {}",
            classpath_dir().display()
        );
    }
}
