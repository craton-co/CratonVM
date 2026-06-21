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
//! ## Status: Step 0
//!
//! The binary-resolution helpers and the [`Mode`] matrix are **real** (lifted
//! from `vm/tests/intrinsic_diff.rs` and `vm/tests/differential.rs`). The
//! actual subprocess fan-out ([`Runner::run_program`]) is a documented stub
//! that returns [`RunError::Unimplemented`] — wired in Step 1.

use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// The `java` executable name. When `DIFFTEST_JAVA_HOME` (or `--jdk`, threaded
/// in by the caller as `jdk_home`) is set, resolves to `<home>/bin/java[.exe]`;
/// otherwise relies on PATH.
pub fn java_executable(jdk_home: Option<&Path>) -> PathBuf {
    resolve_jdk_tool(jdk_home, "java")
}

/// The `javac` executable name (used only by the legacy value-returning
/// micro-method wrapper tier — generated whole programs print from `main`).
pub fn javac_executable(jdk_home: Option<&Path>) -> PathBuf {
    resolve_jdk_tool(jdk_home, "javac")
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

    /// The `(key, value)` `CRATONVM_*` env pairs this mode sets on the child.
    /// `JitOn` is the default and sets nothing. The runner additionally
    /// *clears* the opposing knobs so a child's environment is deterministic
    /// regardless of what the harness inherited (Step 1).
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
// Runner configuration + (stubbed) executor
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

/// Why the (stubbed) runner could not produce observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The subprocess executor is not wired yet (Step 0).
    Unimplemented,
    /// The `cratonvm` binary could not be resolved (build it / set
    /// `CRATONVM_BIN`).
    CratonvmBinaryMissing,
    /// `java` is not on PATH / under the configured JDK home.
    JavaMissing,
    /// The corpus directory contains no runnable programs.
    EmptyCorpus,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            RunError::Unimplemented => "two-VM runner not yet wired (Step 1)",
            RunError::CratonvmBinaryMissing => {
                "cratonvm binary not found — build it or set CRATONVM_BIN"
            }
            RunError::JavaMissing => "java not found on PATH / under the configured JDK",
            RunError::EmptyCorpus => "corpus contains no runnable programs",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for RunError {}

/// The two-VM executor.
#[derive(Debug, Clone)]
pub struct Runner {
    config: RunnerConfig,
}

impl Runner {
    pub fn new(config: RunnerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &RunnerConfig {
        &self.config
    }

    /// Run a single program across all configured modes plus HotSpot.
    ///
    /// **Step 0 stub:** always returns [`RunError::Unimplemented`]. The Step 1
    /// implementation will spawn `cratonvm` once per [`Mode`] and `java` once,
    /// each under [`RunnerConfig::timeout`], and return their observations.
    pub fn run_program(&self, _program: &Path) -> Result<(), RunError> {
        Err(RunError::Unimplemented)
    }
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
    fn run_program_is_unimplemented_in_step0() {
        let cfg = RunnerConfig {
            corpus: PathBuf::from("corpus"),
            modes: vec![Mode::JitOn],
            timeout: DEFAULT_TIMEOUT,
            jdk_home: None,
            allow_jdk_downgrade: false,
            ledger: PathBuf::from("bench/differential-divergences.json"),
            update_ledger: false,
        };
        let runner = Runner::new(cfg);
        assert_eq!(
            runner.run_program(Path::new("seeds/Hello.java")),
            Err(RunError::Unimplemented)
        );
    }
}
