// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The two-VM A/B executor (design §3.2).
//!
//! Builds **one** `java`-compatible argv and dispatches it to two executables:
//! the `cratonvm` binary (once per behavioral [`Mode`], because the per-run
//! `CRATONVM_*` env cache is a process-lifetime `OnceLock`) and a real `java`.
//! Each run captures stdout / stderr / exit-code / wall-time under a hard
//! timeout into an [`Observation`](crate::ledger::Observation).
//!
//! ## Status: Step 1
//!
//! The binary-resolution helpers and the [`Mode`] matrix are lifted from
//! `vm/tests/intrinsic_diff.rs` / `vm/tests/differential.rs`. [`run_subprocess`]
//! captures both pipes on drain threads (so a child that fills a pipe buffer
//! can't deadlock the timeout poll) and kills + flags `timed_out` on overrun.
//! The per-mode fan-out and `Classification` join are still Step 2 — Step 1
//! drives one CratonVM mode vs HotSpot (see [`crate::harness`]).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ledger::Observation;
use crate::oracle::normalize_line_endings;

/// Default hard per-run timeout, matching `vm/tests/intrinsic_diff.rs`'s
/// `RUN_TIMEOUT`. A CratonVM run that exceeds this while HotSpot finishes is
/// itself a divergence (`Hang`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Path / binary resolution (lifted from intrinsic_diff.rs / differential.rs)
// ---------------------------------------------------------------------------

/// Workspace root (the parent of this crate's directory).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Resolve the `cratonvm` CLI binary, mirroring `intrinsic_diff.rs`:
///   1. `CRATONVM_BIN` env var, if it points at an existing file,
///   2. `target/release/cratonvm[.exe]`,
///   3. `target/debug/cratonvm[.exe]`.
pub fn cratonvm_binary() -> Option<PathBuf> {
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
    for profile in ["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// The `java` executable. When `jdk_home` (`--jdk`) or `DIFFTEST_JAVA_HOME` is
/// set, resolves to `<home>/bin/java[.exe]`; otherwise relies on PATH.
pub fn java_executable(jdk_home: Option<&Path>) -> PathBuf {
    resolve_jdk_tool(jdk_home, "java")
}

/// The `javac` executable (used to compile a `.java` seed once before running
/// the resulting `.class` on both VMs).
pub fn javac_executable(jdk_home: Option<&Path>) -> PathBuf {
    resolve_jdk_tool(jdk_home, "javac")
}

/// Whether a usable `java` is reachable. Spawns `<java> -version` (fast and
/// self-terminating) and treats any spawn failure as "unavailable". Shared by
/// the gate's bootstrap check and the harness's prerequisite gate.
pub fn java_available(jdk_home: Option<&Path>) -> bool {
    Command::new(java_executable(jdk_home))
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The JDK identity string (`java -version`'s first line), for the ledger
/// header. Returns `None` if `java` can't be run. HotSpot prints the version
/// banner to stderr.
pub fn jdk_version(jdk_home: Option<&Path>) -> Option<String> {
    let out = Command::new(java_executable(jdk_home))
        .arg("-version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let banner = String::from_utf8_lossy(&out.stderr);
    banner
        .lines()
        .next()
        .map(|l| l.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve a JDK tool by name under `<jdk_home>/bin`, falling back to the bare
/// name (PATH lookup). `DIFFTEST_JAVA_HOME` is consulted when no explicit home
/// is supplied.
fn resolve_jdk_tool(jdk_home: Option<&Path>, tool: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    };
    let home = jdk_home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("DIFFTEST_JAVA_HOME").map(PathBuf::from));
    match home {
        Some(h) => h.join("bin").join(exe),
        None => PathBuf::from(exe),
    }
}

// ---------------------------------------------------------------------------
// Behavioral mode matrix (design §3.2)
// ---------------------------------------------------------------------------

/// A CratonVM behavioral mode. Each is its own subprocess (one row of the diff
/// matrix), distinguished by the `CRATONVM_*` env it sets — which is why a JIT
/// divergence shows up as `jit-on ≠ java` while `nojit = java` and auto-
/// classifies as a JIT bug (design §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default execution (JIT enabled).
    JitOn,
    /// `CRATONVM_DISABLE_JIT=1` — interpreter only.
    NoJit,
    /// `CRATONVM_DISABLE_INTRINSICS=1` — bypass the interpreter intrinsic table.
    NoIntrinsics,
    /// Moving young-gen GC mode (vs the default selective-promote).
    MovingGc,
    /// `CRATONVM_JIT_THRESHOLD=1` — force early compile to surface OSR/deopt.
    LowJitThreshold,
}

impl Mode {
    /// The stable kebab-case label used on the CLI (`--modes`) and in ledger
    /// rows.
    pub fn label(self) -> &'static str {
        match self {
            Mode::JitOn => "jit-on",
            Mode::NoJit => "nojit",
            Mode::NoIntrinsics => "no-intrinsics",
            Mode::MovingGc => "moving-gc",
            Mode::LowJitThreshold => "low-jit-threshold",
        }
    }

    /// The `(key, value)` `CRATONVM_*` env pairs this mode *sets* on the child.
    /// `JitOn` is the default and sets nothing. The runner additionally
    /// *clears* the opposing knobs (see [`apply_mode_env`]) so a child's
    /// environment is deterministic regardless of what the harness inherited.
    pub fn env_overrides(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Mode::JitOn => &[],
            Mode::NoJit => &[("CRATONVM_DISABLE_JIT", "1")],
            Mode::NoIntrinsics => &[("CRATONVM_DISABLE_INTRINSICS", "1")],
            // The moving young-gen default is selected by *disabling* selective
            // promotion (see MEMORY.md's GC knobs).
            Mode::MovingGc => &[("CRATONVM_NO_SELECTIVE_PROMOTE", "1")],
            Mode::LowJitThreshold => &[("CRATONVM_JIT_THRESHOLD", "1")],
        }
    }

    /// Parse a single mode label. Returns `None` for an unknown label.
    pub fn from_label(s: &str) -> Option<Mode> {
        match s {
            "jit-on" => Some(Mode::JitOn),
            "nojit" | "no-jit" => Some(Mode::NoJit),
            "no-intrinsics" => Some(Mode::NoIntrinsics),
            "moving-gc" => Some(Mode::MovingGc),
            "low-jit-threshold" => Some(Mode::LowJitThreshold),
            _ => None,
        }
    }
}

/// Every `CRATONVM_*` knob any mode can set — cleared on each child before the
/// active mode re-sets its own, so an inherited env can't leak across modes.
const ALL_MODE_KNOBS: &[&str] = &[
    "CRATONVM_DISABLE_JIT",
    "CRATONVM_DISABLE_INTRINSICS",
    "CRATONVM_NO_SELECTIVE_PROMOTE",
    "CRATONVM_JIT_THRESHOLD",
];

/// Set `cmd`'s environment to exactly `mode`'s knobs: clear every knob, then
/// apply the active mode's overrides. Deterministic regardless of inherited env.
fn apply_mode_env(cmd: &mut Command, mode: Mode) {
    for knob in ALL_MODE_KNOBS {
        cmd.env_remove(knob);
    }
    for (k, v) in mode.env_overrides() {
        cmd.env(k, v);
    }
}

/// Parse a comma-separated `--modes` list into [`Mode`]s, erroring on any
/// unknown label so a typo can't silently drop a matrix row.
pub fn parse_modes(s: &str) -> Result<Vec<Mode>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|label| Mode::from_label(label).ok_or_else(|| format!("unknown mode: {label:?}")))
        .collect()
}

// ---------------------------------------------------------------------------
// Runner configuration
// ---------------------------------------------------------------------------

/// Configuration for one A/B run, assembled from CLI flags.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Directory of programs to run.
    pub corpus: PathBuf,
    /// CratonVM modes to fan out across.
    pub modes: Vec<Mode>,
    /// Hard per-run timeout.
    pub timeout: Duration,
    /// Explicit JDK home (`--jdk`), else `DIFFTEST_JAVA_HOME` / PATH.
    pub jdk_home: Option<PathBuf>,
    /// Allow a non-pinned JDK (`--allow-jdk-downgrade`).
    pub allow_jdk_downgrade: bool,
    /// Ledger path (`--ledger`).
    pub ledger: PathBuf,
    /// Whether to write discovered divergences back into the ledger.
    pub update_ledger: bool,
}

/// Why a run could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The `cratonvm` binary could not be resolved (build it / set
    /// `CRATONVM_BIN`).
    CratonvmBinaryMissing,
    /// `java` / `javac` is not on PATH / under the configured JDK home.
    JavaMissing,
    /// The corpus directory contains no runnable programs.
    EmptyCorpus,
    /// `javac` failed to compile a seed (its stderr is captured).
    CompileFailed { program: String, stderr: String },
    /// A spawn / IO error launching one of the VMs.
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::CratonvmBinaryMissing => {
                f.write_str("cratonvm binary not found — build it or set CRATONVM_BIN")
            }
            RunError::JavaMissing => {
                f.write_str("java/javac not found on PATH / under the configured JDK")
            }
            RunError::EmptyCorpus => f.write_str("corpus contains no runnable programs"),
            RunError::CompileFailed { program, stderr } => {
                write!(f, "javac failed for {program}: {stderr}")
            }
            RunError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

// ---------------------------------------------------------------------------
// Subprocess capture
// ---------------------------------------------------------------------------

/// Spawn `cmd` and capture stdout/stderr/exit-code under `timeout`.
///
/// Both pipes are drained on dedicated threads so a child that fills a pipe
/// buffer (64 KiB) cannot deadlock the timeout poll. On overrun the child is
/// killed and `timed_out` is set. Captured streams are line-ending-normalized
/// (CRLF→LF, trailing-newline trim) at the boundary so a platform `\r` can't
/// masquerade as a divergence. `exception` is left `None` here — the harness
/// fills it via [`crate::oracle::parse_exception`].
pub fn run_subprocess(mut cmd: Command, timeout: Duration) -> Result<Observation, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| RunError::Io(e.to_string()))?;

    // Drain both pipes concurrently so neither can block the child on a full
    // buffer while we poll for exit.
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| RunError::Io(e.to_string()))? {
            Some(st) => break st,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let st = child.wait().map_err(|e| RunError::Io(e.to_string()))?;
                    timed_out = true;
                    break st;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    let wall_ms = start.elapsed().as_millis() as u64;

    Ok(Observation {
        stdout: normalize_line_endings(&String::from_utf8_lossy(&stdout)),
        stderr: normalize_line_endings(&String::from_utf8_lossy(&stderr)),
        exit_code: status.code(),
        exception: None,
        timed_out,
        wall_ms,
    })
}

/// Run `<main_class>` on CratonVM in `mode` with `classpath`.
pub fn run_cratonvm(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    mode: Mode,
    timeout: Duration,
) -> Result<Observation, RunError> {
    let mut cmd = Command::new(bin);
    cmd.arg("-cp").arg(classpath).arg(main_class);
    apply_mode_env(&mut cmd, mode);
    run_subprocess(cmd, timeout)
}

/// Run `<main_class>` on HotSpot with `classpath`.
pub fn run_hotspot(
    classpath: &Path,
    main_class: &str,
    jdk_home: Option<&Path>,
    timeout: Duration,
) -> Result<Observation, RunError> {
    let mut cmd = Command::new(java_executable(jdk_home));
    cmd.arg("-cp").arg(classpath).arg(main_class);
    run_subprocess(cmd, timeout)
}

/// Compile a `.java` source into `out_dir` with `javac`, returning the main
/// class name to run (the source's file stem — our seeds declare a public
/// class of that name in the default package).
pub fn compile_java(
    file: &Path,
    out_dir: &Path,
    jdk_home: Option<&Path>,
    timeout: Duration,
) -> Result<String, RunError> {
    let program = file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut cmd = Command::new(javac_executable(jdk_home));
    cmd.arg("-d").arg(out_dir).arg(file);
    let obs = run_subprocess(cmd, timeout)?;
    let ok = obs.exit_code == Some(0) && !obs.timed_out;
    if !ok {
        return Err(RunError::CompileFailed {
            program,
            stderr: obs.stderr,
        });
    }
    Ok(file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes_round_trips_labels() {
        let modes = parse_modes("jit-on, nojit ,no-intrinsics").unwrap();
        assert_eq!(modes, vec![Mode::JitOn, Mode::NoJit, Mode::NoIntrinsics]);
        let labels: Vec<_> = modes.iter().map(|m| m.label()).collect();
        assert_eq!(labels, vec!["jit-on", "nojit", "no-intrinsics"]);
    }

    #[test]
    fn parse_modes_rejects_unknown() {
        let err = parse_modes("jit-on,bogus").unwrap_err();
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn parse_modes_skips_empties() {
        assert!(parse_modes("").unwrap().is_empty());
        assert_eq!(parse_modes("nojit,").unwrap(), vec![Mode::NoJit]);
    }

    #[test]
    fn nojit_sets_disable_jit_env() {
        assert_eq!(
            Mode::NoJit.env_overrides(),
            &[("CRATONVM_DISABLE_JIT", "1")]
        );
        assert!(Mode::JitOn.env_overrides().is_empty());
    }

    #[test]
    fn all_mode_knobs_cover_every_override() {
        // The clear-list must be a superset of every mode's set-keys, or an
        // inherited knob could leak into a mode that doesn't set it.
        for mode in [
            Mode::JitOn,
            Mode::NoJit,
            Mode::NoIntrinsics,
            Mode::MovingGc,
            Mode::LowJitThreshold,
        ] {
            for (k, _) in mode.env_overrides() {
                assert!(
                    ALL_MODE_KNOBS.contains(k),
                    "{k} missing from ALL_MODE_KNOBS"
                );
            }
        }
    }

    #[test]
    fn java_executable_honors_explicit_home() {
        let home = Path::new("/opt/jdk-25");
        let java = java_executable(Some(home));
        assert!(java.ends_with(if cfg!(windows) {
            "bin/java.exe"
        } else {
            "bin/java"
        }));
        assert!(java.starts_with(home));
    }

    #[test]
    fn subprocess_captures_stdout_and_exit_code() {
        // A trivial, always-present child: the host's own shell echo. This
        // exercises the capture/timeout plumbing without needing a JVM.
        let cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "echo difftest-ok"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "echo difftest-ok"]);
            c
        };
        let obs = run_subprocess(cmd, Duration::from_secs(30)).expect("spawn");
        assert_eq!(obs.stdout, "difftest-ok");
        assert_eq!(obs.exit_code, Some(0));
        assert!(!obs.timed_out);
    }

    #[test]
    fn subprocess_times_out_and_flags() {
        // A child that sleeps far longer than the timeout must be killed and
        // flagged, not block the test.
        // Spawn the long sleeper as a DIRECT child (no shell wrapper) so
        // killing it closes our captured pipes immediately — mirroring how a
        // real cratonvm/java timeout is handled. A `cmd /C ping` grandchild
        // would keep the inherited pipe open until ping exits and stall the
        // drain thread.
        let cmd = if cfg!(windows) {
            // `ping -n 30 127.0.0.1` sleeps ~29s without extra deps.
            let mut c = Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        let obs = run_subprocess(cmd, Duration::from_millis(300)).expect("spawn");
        assert!(obs.timed_out, "expected timeout flag");
    }
}
