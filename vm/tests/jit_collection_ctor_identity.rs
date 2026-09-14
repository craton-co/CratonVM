// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT-vs-interpreter identity for collections allocated inside a compiled
//! method.
//!
//! ## The bug this pins
//!
//! `is_elidable_construction` (`vm/src/runtime/interpreter.rs`) decided whether
//! an `invokespecial C.<init>()V` could be dropped by looking ONLY at the
//! constructor's bytecode body (`aload_0; invokespecial Object.<init>;
//! return`). That ignores native shadowing: `invokespecial` prefers a
//! registered native over bytecode, and `java/util/HashMap.<init>()V` is
//! exactly the dangerous shape — an empty bytecode constructor plus
//! `native_map_init`, which allocates the 16-bucket table.
//! `java/util/concurrent/ConcurrentHashMap.<init>()V` is similarly tiny, while
//! its native installs the segmented backing store. The inline resolver must
//! reject both native-shadowed classfile bodies, not only constructor elision.
//!
//! With the call elided, a JIT-compiled `new HashMap<>()` produced a map with
//! no table; the first `put` then materialised one through `map_resize`, which
//! DOUBLED the assumed default capacity to 32. Same keys, different bucket
//! count, different iteration order — so a JIT-created map and an
//! interpreter-created map holding identical entries iterated differently.
//! Found as a json-smart parse -> serialize -> re-parse round-trip mismatch
//! (`jsonsmart-parser-jit-retired-20260727.md`), which only
//! misfired in the narrow window where one map predated the tier-up of
//! `JSONParserBase.readObject` and the other followed it.
//!
//! ## Method
//!
//! Same shape as `vm/tests/jit_interp_differential.rs`: run the same fixture
//! twice as a `cratonvm` CLI subprocess — once with `CRATONVM_DISABLE_JIT=1`
//! (the oracle) and once with the JIT forced on for the fixture's package —
//! and require byte-identical `r:` observation lines. The fixture prints
//! iteration orders, which are a direct readout of each collection's table
//! capacity, and exercises `ArrayList.equals` with allocating element
//! comparisons so native collection loops cannot retain stale backing-array
//! references across a moving GC.
//!
//! Prerequisites are gated exactly like the other subprocess tests: a missing
//! `cratonvm` binary or uncompiled fixture prints a skip notice instead of
//! failing. Build with `cargo build -p cratonvm-cli` (or set `CRATONVM_BIN`).
//!
//! Run with:
//!     cargo test -p cratonvm-vm --test jit_collection_ctor_identity -- --nocapture

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hard per-subprocess timeout: the fixture is a short warm-up loop plus a
/// handful of prints, so a hang means a JIT livelock, not slow hardware.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

/// Simple name of the fixture (package `cratonvm`).
const FIXTURE: &str = "JitCollectionCtorIdentity";

/// Completion marker the fixture prints last.
const OK_MARKER: &str = "JIT_COLLECTION_CTOR_OK";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// The committed fixture classpath directory: `vm/tests/resources/`.
fn classpath_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
}

/// Resolve the `cratonvm` CLI binary (`CRATONVM_BIN`, then release, then
/// debug) — same resolution order as the sibling subprocess tests.
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

fn class_file_present(simple_name: &str) -> bool {
    classpath_dir()
        .join("cratonvm")
        .join(format!("{simple_name}.class"))
        .exists()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Pure interpreter — the oracle.
    Interpreter,
    /// JIT on, forced to compile the fixture's allocators immediately.
    Jit,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Interpreter => "interpreter",
            Mode::Jit => "jit",
        }
    }

    /// `CRATONVM_JIT_ALLOW_PACKAGES` used to be arranged here on both arms. It
    /// was the escape hatch for the static package-ban machinery, and
    /// `d1979bec5` ("delete the static ban machinery outright") removed the
    /// last reader — so setting it had become a no-op that still read like a
    /// precondition. Dropped rather than re-declared: the flag no longer
    /// exists, and a test arranging a variable nothing consumes is how a
    /// green run stops meaning what its author thought it meant.
    fn apply_env(self, cmd: &mut Command) {
        match self {
            Mode::Interpreter => {
                cmd.env("CRATONVM_DISABLE_JIT", "1");
                cmd.env_remove("CRATONVM_JIT_THRESHOLD");
            }
            Mode::Jit => {
                cmd.env_remove("CRATONVM_DISABLE_JIT");
                cmd.env("CRATONVM_JIT_THRESHOLD", "1");
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Run {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

fn observation_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| l.starts_with("r:")).collect()
}

fn run_fixture(mode: Mode) -> Option<Run> {
    if !class_file_present(FIXTURE) {
        eprintln!(
            "[jit_collection_ctor] {FIXTURE}.class not found under {} — javac \
             unavailable at build time? skipping.",
            classpath_dir().display()
        );
        return None;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[jit_collection_ctor] cratonvm binary not found; build it with \
                 `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
            );
            return None;
        }
    };

    let mut cmd = Command::new(&bin);
    cmd.arg("--Xmx")
        .arg("64m")
        .arg("-c")
        .arg(classpath_dir())
        .arg(format!("cratonvm.{FIXTURE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    mode.apply_env(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[jit_collection_ctor] failed to spawn cratonvm ({}): {e}",
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
                        "[jit_collection_ctor] {FIXTURE} ({}) timed out after \
                         {RUN_TIMEOUT:?} — likely a JIT livelock.",
                        mode.label(),
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                eprintln!("[jit_collection_ctor] try_wait failed: {e}");
                return None;
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_collection_ctor] wait_with_output failed: {e}");
            return None;
        }
    };

    Some(Run {
        stdout: normalize(&String::from_utf8_lossy(&output.stdout)),
        stderr: normalize(&String::from_utf8_lossy(&output.stderr)),
        exit_code: output.status.code(),
    })
}

/// A collection allocated inside a JIT-compiled method must be
/// indistinguishable from one the interpreter allocated — same entries AND the
/// same iteration order, i.e. the same table capacity.
#[test]
fn jit_allocated_collections_iterate_like_interpreted_ones() {
    let interp = match run_fixture(Mode::Interpreter) {
        Some(r) => r,
        None => return, // prerequisite missing; skip (see module docs)
    };
    let jit = match run_fixture(Mode::Jit) {
        Some(r) => r,
        None => return,
    };

    for (run, mode) in [(&interp, Mode::Interpreter), (&jit, Mode::Jit)] {
        assert!(
            run.stdout.contains(OK_MARKER),
            "{} run did not reach the {OK_MARKER:?} marker.\nexit={:?}\nstdout:\n{}\nstderr:\n{}",
            mode.label(),
            run.exit_code,
            run.stdout,
            run.stderr,
        );
    }

    let interp_obs = observation_lines(&interp.stdout);
    let jit_obs = observation_lines(&jit.stdout);
    assert_eq!(
        interp_obs.len(),
        8,
        "fixture should emit 8 'r:' observations; got {}.\nstdout:\n{}",
        interp_obs.len(),
        interp.stdout,
    );
    assert!(
        interp_obs
            .iter()
            .any(|l| *l == "r: roundTripEqualOrder=true"),
        "the interpreter oracle itself disagreed on round-trip order:\n{}",
        interp.stdout,
    );
    assert_eq!(
        interp_obs, jit_obs,
        "JIT MISCOMPILE: a collection allocated in JIT-compiled code does not \
         match the interpreter's. A differing `hashMapNoArg=` line means the \
         table capacity differs — the trivial-constructor elision skipped a \
         native-shadowed `<init>`; a missing `concurrentHashMap=` line means \
         the inline resolver bypassed that constructor (see this test's module \
         docs).\n\
         --- INTERP ---\n{}\n--- JIT ---\n{}\n--- JIT stderr ---\n{}",
        interp.stdout, jit.stdout, jit.stderr,
    );
    assert_eq!(
        interp.exit_code, jit.exit_code,
        "exit code diverged: interpreter={:?} jit={:?}",
        interp.exit_code, jit.exit_code,
    );

    eprintln!(
        "[jit_collection_ctor] {} observations identical interpreter vs JIT.",
        interp_obs.len()
    );
}
