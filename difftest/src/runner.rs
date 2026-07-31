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
    jdk_version_banner(jdk_home).and_then(|banner| {
        banner
            .lines()
            .next()
            .map(|l| l.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
    })
}

/// The raw `java -version` banner (stderr), or `None` if `java` can't be run.
fn jdk_version_banner(jdk_home: Option<&Path>) -> Option<String> {
    let out = Command::new(java_executable(jdk_home))
        .arg("-version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The JDK **feature version** (`25`, `21`, `8`, …) of the configured runtime.
///
/// Recorded on every schema-2 ledger row (`docs/feature-designs/jdk-only-mode.md`
/// §9's `jdk_feature`), because a strict-mode violation is only meaningful
/// against the JDK it was measured on: a `MissingNative` on 25 and the same one
/// on 21 are different findings. Probed **once per corpus** (into
/// [`RunnerConfig::jdk_feature`]), never per run.
pub fn jdk_feature_version(jdk_home: Option<&Path>) -> Option<u32> {
    jdk_version_banner(jdk_home).and_then(|b| parse_jdk_feature(&b))
}

/// Extract the feature version from a `java -version` banner.
///
/// Handles both the modern and the legacy spellings:
///
/// ```text
/// openjdk version "25.0.3" 2026-04-21 LTS   ⇒ 25
/// java version "1.8.0_412"                  ⇒  8   (1.x is the pre-9 scheme)
/// openjdk version "17"                      ⇒ 17
/// ```
///
/// Returns `None` rather than guessing when no quoted version is present — an
/// unknown feature version must read as "not measured", not as `0`.
pub fn parse_jdk_feature(banner: &str) -> Option<u32> {
    // The version is the first double-quoted token on the banner's first
    // non-empty line (`openjdk version "25.0.3" ...`).
    let line = banner.lines().map(str::trim).find(|l| l.contains('"'))?;
    let quoted = line.split('"').nth(1)?;
    let mut parts = quoted.split(['.', '-', '+', '_']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        // `1.8.0_412` — the feature version is the *second* component.
        return parts.next()?.parse().ok();
    }
    Some(first)
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
///
/// ## Two families
///
/// The **historical five** (`jit-on` … `low-jit-threshold`) pass *no* CLI flags
/// at all and run under the launcher's default compatibility policy. Their
/// argv, env and ledger rows are frozen: the committed `difftest/seeds` gate
/// baseline is measured with them, so anything this feature adds must be
/// invisible to them.
///
/// The **four profile modes** additionally select a compatibility policy on the
/// command line (`docs/feature-designs/jdk-only-mode.md` §9) and collect the
/// three census dumps. They are a second axis, not a replacement: a program can
/// be run under both, and lands on **two ledger rows** keyed by
/// `(class, jdk_profile)`.
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

    /// `--jdk-only`, JIT enabled: real class bytes are authoritative.
    JdkOnlyJit,
    /// `--jdk-only`, interpreter only — isolates a strict-mode divergence from
    /// a JIT one exactly the way `nojit` does for the compatible policy.
    JdkOnlyNoJit,
    /// `--real-jdk`, JIT enabled: today's compatibility behaviour, but with the
    /// census dumps on, so the strict rows have a same-JDK control to diff
    /// against. **Not** the same as `jit-on`: it names the policy explicitly and
    /// records what a strict run *would* have rejected (contract §10).
    RealCompatibleJit,
    /// `--real-jdk`, interpreter only.
    RealCompatibleNoJit,
}

impl Mode {
    /// Every mode, historical five first, in `--modes` order.
    pub fn all() -> &'static [Mode] {
        &[
            Mode::JitOn,
            Mode::NoJit,
            Mode::NoIntrinsics,
            Mode::MovingGc,
            Mode::LowJitThreshold,
            Mode::JdkOnlyJit,
            Mode::JdkOnlyNoJit,
            Mode::RealCompatibleJit,
            Mode::RealCompatibleNoJit,
        ]
    }

    /// The stable kebab-case label used on the CLI (`--modes`) and in ledger
    /// rows.
    pub fn label(self) -> &'static str {
        match self {
            Mode::JitOn => "jit-on",
            Mode::NoJit => "nojit",
            Mode::NoIntrinsics => "no-intrinsics",
            Mode::MovingGc => "moving-gc",
            Mode::LowJitThreshold => "low-jit-threshold",
            Mode::JdkOnlyJit => "jdk-only-jit",
            Mode::JdkOnlyNoJit => "jdk-only-nojit",
            Mode::RealCompatibleJit => "real-compatible-jit",
            Mode::RealCompatibleNoJit => "real-compatible-nojit",
        }
    }

    /// The `(key, value)` `CRATONVM_*` env pairs this mode *sets* on the child.
    /// `JitOn` is the default and sets nothing. The runner additionally
    /// *clears* the opposing knobs (see [`apply_mode_env`]) so a child's
    /// environment is deterministic regardless of what the harness inherited.
    ///
    /// The policy modes reuse the same JIT knob: strictness is a *launcher
    /// flag* (see [`cli_args`](Mode::cli_args)), never an env var, so a stale
    /// inherited `CRATONVM_*` can't turn a compatible run strict or back.
    pub fn env_overrides(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Mode::JitOn => &[],
            Mode::NoJit => &[("CRATONVM_DISABLE_JIT", "1")],
            Mode::NoIntrinsics => &[("CRATONVM_DISABLE_INTRINSICS", "1")],
            // The moving young-gen default is selected by *disabling* selective
            // promotion (see MEMORY.md's GC knobs).
            Mode::MovingGc => &[("CRATONVM_NO_SELECTIVE_PROMOTE", "1")],
            Mode::LowJitThreshold => &[("CRATONVM_JIT_THRESHOLD", "1")],
            Mode::JdkOnlyJit | Mode::RealCompatibleJit => &[],
            Mode::JdkOnlyNoJit | Mode::RealCompatibleNoJit => &[("CRATONVM_DISABLE_JIT", "1")],
        }
    }

    /// Launcher flags this mode passes, ahead of `-cp` (contract §9).
    ///
    /// The historical five return `&[]` — they must keep producing the exact
    /// argv the committed gate baseline was captured with. `--jdk-only` and
    /// `--real-jdk` are mutually exclusive by construction: no mode emits both.
    pub fn cli_args(self) -> &'static [&'static str] {
        match self {
            Mode::JdkOnlyJit | Mode::JdkOnlyNoJit => &["--jdk-only"],
            Mode::RealCompatibleJit | Mode::RealCompatibleNoJit => &["--real-jdk"],
            _ => &[],
        }
    }

    /// The `CompatibilityMode::as_str()` spelling of the policy this mode runs
    /// under — the second half of a ledger row's key.
    ///
    /// `real-compatible-*` reports `"compatible"`, the same as the historical
    /// five: it differs in *observability*, not in policy, so it must share
    /// their ledger rows rather than forking a parallel set.
    pub fn jdk_profile(self) -> &'static str {
        if self.is_jdk_only() {
            crate::ledger::PROFILE_JDK_ONLY
        } else {
            crate::ledger::PROFILE_COMPATIBLE
        }
    }

    /// Whether this mode asks the launcher for the strict policy.
    pub fn is_jdk_only(self) -> bool {
        matches!(self, Mode::JdkOnlyJit | Mode::JdkOnlyNoJit)
    }

    /// Whether this mode passes the three census dump flags.
    ///
    /// True for all four profile modes — including the compatible pair, since
    /// wave 1 is measurement (contract §10) and the compatible census is the
    /// baseline the strict one is read against. **False for the historical
    /// five**, whose argv is frozen.
    pub fn collects_census(self) -> bool {
        matches!(
            self,
            Mode::JdkOnlyJit
                | Mode::JdkOnlyNoJit
                | Mode::RealCompatibleJit
                | Mode::RealCompatibleNoJit
        )
    }

    /// Parse a single mode label. Returns `None` for an unknown label.
    pub fn from_label(s: &str) -> Option<Mode> {
        match s {
            "jit-on" => Some(Mode::JitOn),
            "nojit" | "no-jit" => Some(Mode::NoJit),
            "no-intrinsics" => Some(Mode::NoIntrinsics),
            "moving-gc" => Some(Mode::MovingGc),
            "low-jit-threshold" => Some(Mode::LowJitThreshold),
            "jdk-only-jit" => Some(Mode::JdkOnlyJit),
            "jdk-only-nojit" | "jdk-only-no-jit" => Some(Mode::JdkOnlyNoJit),
            "real-compatible-jit" => Some(Mode::RealCompatibleJit),
            "real-compatible-nojit" | "real-compatible-no-jit" => Some(Mode::RealCompatibleNoJit),
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
    /// Ledger path (`--ledger`).
    pub ledger: PathBuf,
    /// Whether to write discovered divergences back into the ledger.
    pub update_ledger: bool,
    /// Determinism pre-flight: run each program **twice** on HotSpot and reject
    /// it (don't judge it) when the two runs disagree under the normalizer — the
    /// oracle's soundness guard (design §3.3). The single most important
    /// correctness property of the gate.
    pub determinism_check: bool,
    /// Re-confirm a divergence by re-running the diverging CratonVM mode.
    ///
    /// A divergence that doesn't reproduce was a transient (e.g. a concurrent
    /// rebuild overwriting the binary mid-run) and is dropped. The re-run uses
    /// the **same mode** — there is deliberately no fallback to a laxer policy,
    /// because a strict divergence that "goes away" under `--real-jdk` is the
    /// finding, not a flake.
    pub reconfirm: bool,
    /// JDK feature version of the configured runtime (`25`, `21`, `8`, …),
    /// probed once per corpus by [`crate::harness::run_corpus`] and stamped on
    /// every ledger row. `None` until probed / when `java` is unavailable.
    pub jdk_feature: Option<u32>,
}

impl RunnerConfig {
    /// A default config for `corpus`: `jit-on,nojit`, 120 s timeout, default
    /// ledger path, no determinism/reconfirm/ledger-write. Tests and callers
    /// tweak from here so adding a field is a one-line change, not a churn.
    pub fn for_corpus(corpus: PathBuf) -> Self {
        Self {
            corpus,
            modes: vec![Mode::JitOn, Mode::NoJit],
            timeout: DEFAULT_TIMEOUT,
            jdk_home: None,
            ledger: crate::ledger::default_ledger_path(),
            update_ledger: false,
            determinism_check: false,
            reconfirm: false,
            jdk_feature: None,
        }
    }
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

// ---------------------------------------------------------------------------
// JDK-only census dumps (contract §9)
// ---------------------------------------------------------------------------

/// Where one census-collecting child writes its three JSON artefacts.
///
/// The launcher writes these on shutdown; [`crate::census::collect`] reads them
/// back. All three work **with or without** `--jdk-only` (contract §9), which
/// is what makes `real-compatible-*` a usable control: it measures what a
/// strict run *would* have rejected while behaving exactly as today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpPaths {
    /// `--jdk-only-report <FILE>` — mode, jdk_feature, violations, counts.
    pub jdk_only_report: PathBuf,
    /// `--dump-class-origins <FILE>` — one row per loaded class.
    pub class_origins: PathBuf,
    /// `--dump-native-registry <FILE>` — schema-2 native census.
    pub native_registry: PathBuf,
}

impl DumpPaths {
    /// The three dumps under `dir`, with plain names.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self::tagged(dir, "")
    }

    /// The three dumps under `dir`, prefixed with `tag` so concurrent runs of
    /// different `(program, mode)` pairs in one scratch directory can't
    /// overwrite each other's census.
    pub fn tagged(dir: impl AsRef<Path>, tag: &str) -> Self {
        let dir = dir.as_ref();
        let name = |what: &str| {
            if tag.is_empty() {
                format!("{what}.json")
            } else {
                format!("{tag}-{what}.json")
            }
        };
        Self {
            jdk_only_report: dir.join(name("jdk-only-report")),
            class_origins: dir.join(name("class-origins")),
            native_registry: dir.join(name("native-registry")),
        }
    }

    /// The launcher flags that request these dumps, in contract §9 order.
    pub fn cli_args(&self) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = Vec::with_capacity(6);
        for (flag, path) in [
            ("--jdk-only-report", &self.jdk_only_report),
            ("--dump-class-origins", &self.class_origins),
            ("--dump-native-registry", &self.native_registry),
        ] {
            args.push(flag.into());
            args.push(path.clone().into_os_string());
        }
        args
    }

    /// Remove any files a previous run left behind, so a launcher that fails to
    /// write one cannot have a stale file read as this run's measurement.
    pub fn cleanup(&self) {
        for p in [
            &self.jdk_only_report,
            &self.class_origins,
            &self.native_registry,
        ] {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Run `<main_class>` on CratonVM in `mode` with `classpath`.
///
/// Signature unchanged from Step 1; delegates to
/// [`run_cratonvm_with_dumps`] with no census.
pub fn run_cratonvm(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    mode: Mode,
    timeout: Duration,
) -> Result<Observation, RunError> {
    run_cratonvm_with_dumps(bin, classpath, main_class, mode, timeout, None)
}

/// Run `<main_class>` on CratonVM in `mode`, optionally requesting the three
/// JDK-only census dumps.
///
/// Argv order is `<policy flag> <dump flags> -cp <classpath> <main class>`:
/// the launcher's own options must precede `-cp`, and everything after the main
/// class would be program arguments. A mode with no policy flag and no dumps
/// produces byte-identical argv to Step 1's.
pub fn run_cratonvm_with_dumps(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    mode: Mode,
    timeout: Duration,
    dumps: Option<&DumpPaths>,
) -> Result<Observation, RunError> {
    let mut cmd = Command::new(bin);
    cmd.args(mode.cli_args());
    if let Some(dumps) = dumps {
        dumps.cleanup();
        cmd.args(dumps.cli_args());
    }
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

/// The main class name and expected `.class` path for a default-package seed.
fn expected_class_output(file: &Path, out_dir: &Path) -> (String, PathBuf) {
    let class_name = file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let expected_class = out_dir.join(format!("{class_name}.class"));
    (class_name, expected_class)
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
    let (class_name, expected_class) = expected_class_output(file, out_dir);
    let _ = std::fs::remove_file(&expected_class);

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
    if !expected_class.is_file() {
        return Err(RunError::CompileFailed {
            program,
            stderr: format!(
                "javac exited successfully but did not produce {}",
                expected_class.display()
            ),
        });
    }
    Ok(class_name)
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
        for mode in Mode::all() {
            for (k, _) in mode.env_overrides() {
                assert!(
                    ALL_MODE_KNOBS.contains(k),
                    "{k} missing from ALL_MODE_KNOBS"
                );
            }
        }
    }

    // -- the historical five are frozen -------------------------------------

    /// The five modes the committed `difftest/seeds` gate baseline was captured
    /// with. Their labels, argv and env are a compatibility surface.
    const LEGACY_MODES: [Mode; 5] = [
        Mode::JitOn,
        Mode::NoJit,
        Mode::NoIntrinsics,
        Mode::MovingGc,
        Mode::LowJitThreshold,
    ];

    #[test]
    fn legacy_modes_contribute_no_cli_flags_and_no_census() {
        // The whole point of the new modes being *new*: adding a policy axis
        // must not change one byte of what the historical five run. A flag or a
        // dump leaking in here silently re-measures the CI baseline.
        for mode in LEGACY_MODES {
            assert!(
                mode.cli_args().is_empty(),
                "{} must pass no launcher flags, got {:?}",
                mode.label(),
                mode.cli_args()
            );
            assert!(
                !mode.collects_census(),
                "{} must not request census dumps",
                mode.label()
            );
            assert!(!mode.is_jdk_only(), "{} is not strict", mode.label());
            assert_eq!(mode.jdk_profile(), "compatible", "{}", mode.label());
        }
    }

    #[test]
    fn legacy_labels_and_env_are_unchanged() {
        let labels: Vec<&str> = LEGACY_MODES.iter().map(|m| m.label()).collect();
        assert_eq!(
            labels,
            vec![
                "jit-on",
                "nojit",
                "no-intrinsics",
                "moving-gc",
                "low-jit-threshold"
            ]
        );
        assert!(Mode::JitOn.env_overrides().is_empty());
        assert_eq!(
            Mode::NoJit.env_overrides(),
            &[("CRATONVM_DISABLE_JIT", "1")]
        );
        assert_eq!(
            Mode::NoIntrinsics.env_overrides(),
            &[("CRATONVM_DISABLE_INTRINSICS", "1")]
        );
        assert_eq!(
            Mode::MovingGc.env_overrides(),
            &[("CRATONVM_NO_SELECTIVE_PROMOTE", "1")]
        );
        assert_eq!(
            Mode::LowJitThreshold.env_overrides(),
            &[("CRATONVM_JIT_THRESHOLD", "1")]
        );
        // The default `--modes` list is still the historical pair.
        assert_eq!(
            RunnerConfig::for_corpus(PathBuf::from("x")).modes,
            vec![Mode::JitOn, Mode::NoJit]
        );
    }

    // -- the profile modes ---------------------------------------------------

    #[test]
    fn profile_modes_round_trip_and_carry_one_policy_flag() {
        let modes =
            parse_modes("jdk-only-jit,jdk-only-nojit,real-compatible-jit,real-compatible-nojit")
                .unwrap();
        assert_eq!(
            modes,
            vec![
                Mode::JdkOnlyJit,
                Mode::JdkOnlyNoJit,
                Mode::RealCompatibleJit,
                Mode::RealCompatibleNoJit
            ]
        );
        for m in &modes {
            assert!(m.collects_census(), "{} collects a census", m.label());
            assert_eq!(m.cli_args().len(), 1, "{}", m.label());
            assert_eq!(Mode::from_label(m.label()), Some(*m), "label round-trips");
        }
        assert_eq!(Mode::JdkOnlyJit.cli_args(), &["--jdk-only"]);
        assert_eq!(Mode::RealCompatibleNoJit.cli_args(), &["--real-jdk"]);
        // The nojit half of each pair disables the JIT the same way `nojit` does.
        assert_eq!(
            Mode::JdkOnlyNoJit.env_overrides(),
            &[("CRATONVM_DISABLE_JIT", "1")]
        );
        assert!(Mode::JdkOnlyJit.env_overrides().is_empty());
    }

    #[test]
    fn no_mode_mixes_the_two_policy_flags() {
        // `--jdk-only` and `--real-jdk` request opposite policies; a mode that
        // passed both would leave the verdict to the launcher's argument order.
        for &m in Mode::all() {
            let args = m.cli_args();
            if m.is_jdk_only() {
                assert!(args.contains(&"--jdk-only"), "{}", m.label());
                assert!(
                    !args.contains(&"--real-jdk"),
                    "{} is strict and must not ask for the compatible policy",
                    m.label()
                );
            } else {
                assert!(
                    !args.contains(&"--jdk-only"),
                    "{} is compatible and must not ask for the strict policy",
                    m.label()
                );
            }
        }
    }

    #[test]
    fn profile_maps_to_the_compatibility_mode_spelling() {
        assert_eq!(Mode::JdkOnlyJit.jdk_profile(), "jdk-only");
        assert_eq!(Mode::JdkOnlyNoJit.jdk_profile(), "jdk-only");
        // `real-compatible-*` shares the historical five's profile: it differs
        // in observability, not policy, so it keys onto the same ledger rows.
        assert_eq!(Mode::RealCompatibleJit.jdk_profile(), "compatible");
        assert_eq!(Mode::RealCompatibleNoJit.jdk_profile(), "compatible");
        assert!(!Mode::RealCompatibleJit.is_jdk_only());
    }

    #[test]
    fn all_lists_every_mode_exactly_once() {
        let labels: Vec<&str> = Mode::all().iter().map(|m| m.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "duplicate label in Mode::all()");
        assert_eq!(labels.len(), 9);
        for label in &labels {
            assert!(Mode::from_label(label).is_some(), "{label} must parse");
        }
    }

    // -- census dumps --------------------------------------------------------

    #[test]
    fn dump_paths_emit_the_three_contract_flags() {
        let dumps = DumpPaths::tagged(Path::new("scratch"), "Foo-jdk-only-jit");
        let args = dumps.cli_args();
        assert_eq!(args.len(), 6);
        assert_eq!(args[0], "--jdk-only-report");
        assert_eq!(args[2], "--dump-class-origins");
        assert_eq!(args[4], "--dump-native-registry");
        assert!(dumps
            .jdk_only_report
            .ends_with("Foo-jdk-only-jit-jdk-only-report.json"));
        // Distinct tags never collide in one scratch dir.
        let other = DumpPaths::tagged(Path::new("scratch"), "Foo-real-compatible-jit");
        assert_ne!(dumps.jdk_only_report, other.jdk_only_report);
    }

    #[test]
    fn dump_cleanup_removes_stale_files() {
        let dir = std::env::temp_dir().join(format!("difftest_dumps_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let dumps = DumpPaths::in_dir(&dir);
        std::fs::write(&dumps.jdk_only_report, "{}").expect("write");
        assert!(dumps.jdk_only_report.exists());
        // A stale report from a previous run must not be readable as this
        // run's measurement.
        dumps.cleanup();
        assert!(!dumps.jdk_only_report.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- JDK feature probe ---------------------------------------------------

    #[test]
    fn parses_the_modern_version_banner() {
        assert_eq!(
            parse_jdk_feature("openjdk version \"25.0.3\" 2026-04-21 LTS\nOpenJDK Runtime ...\n"),
            Some(25)
        );
        assert_eq!(parse_jdk_feature("openjdk version \"17\"\n"), Some(17));
        assert_eq!(
            parse_jdk_feature("openjdk version \"21.0.2\" 2024-01-16"),
            Some(21)
        );
        assert_eq!(
            parse_jdk_feature("openjdk version \"24-ea\" 2025-03-18"),
            Some(24)
        );
    }

    #[test]
    fn parses_the_legacy_1_8_banner() {
        // Pre-9 JDKs spell 8 as `1.8.0_x`; reading the leading component would
        // record every JDK 8 run as feature 1.
        assert_eq!(parse_jdk_feature("java version \"1.8.0_412\"\n"), Some(8));
        assert_eq!(parse_jdk_feature("java version \"1.7.0_80\"\n"), Some(7));
    }

    #[test]
    fn an_unreadable_banner_is_none_not_zero() {
        assert_eq!(parse_jdk_feature(""), None);
        assert_eq!(parse_jdk_feature("no version here\n"), None);
        assert_eq!(parse_jdk_feature("openjdk version \"weird\"\n"), None);
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
    fn expected_class_output_uses_source_stem() {
        let (class_name, class_file) =
            expected_class_output(Path::new("corpus/Found_0042.java"), Path::new("out"));
        assert_eq!(class_name, "Found_0042");
        assert_eq!(class_file, Path::new("out").join("Found_0042.class"));
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
