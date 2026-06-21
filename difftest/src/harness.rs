// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Run orchestration: discover the corpus, compile each program once with
//! `javac`, run the resulting `.class` on CratonVM (across the mode matrix) and
//! HotSpot, diff, and classify.
//!
//! This is the seam between the process mechanics ([`crate::runner`]) and the
//! comparison ([`crate::oracle`]). The same compiled `.class` runs on both VMs
//! (design §3.1, the wrapper-free path).
//!
//! ## Status: Step 3
//!
//! For each program HotSpot runs **once** (the deterministic reference) and
//! CratonVM runs **once per configured [`Mode`]**; the per-mode agree/diverge
//! map feeds [`oracle::classify`] to auto-label the divergence
//! (`JitOnly`/`GcMode`/`Universal`/`Hang`/`Crash`) — automating the manual
//! `--nojit` bisection. Step 3 adds the **determinism pre-flight** (run HotSpot
//! twice, reject programs whose runs disagree), **divergence re-confirmation**
//! (re-run a diverging mode to drop transients), and [`gate`] — the verdict
//! that diffs a run against the committed [`Ledger`] for the §3.5 exit codes.

use std::path::{Path, PathBuf};

use crate::ledger::{Channel, Classification, Ledger, LedgerEntry, LedgerStatus, Observation};
use crate::oracle::{self, ModeVerdict, Normalizer, Verdict};
use crate::runner::{self, Mode, RunError, RunnerConfig};

/// One CratonVM mode's outcome for a single program.
#[derive(Debug, Clone)]
pub struct ModeOutcome {
    pub mode: Mode,
    pub cratonvm: Observation,
    pub verdict: Verdict,
}

impl ModeOutcome {
    fn to_mode_verdict(&self) -> ModeVerdict {
        ModeVerdict {
            mode: self.mode,
            diverged: !self.verdict.agrees(),
            timed_out: self.cratonvm.timed_out,
            crashed: self.cratonvm.exit_code.is_none() && !self.cratonvm.timed_out,
        }
    }
}

/// One program's A/B result across the whole mode matrix.
#[derive(Debug, Clone)]
pub struct ProgramResult {
    /// The main class name that was run.
    pub program: String,
    /// The corpus source file (`.java` / `.class`).
    pub source: PathBuf,
    /// The HotSpot reference observation (run once).
    pub hotspot: Observation,
    /// One outcome per configured CratonVM mode.
    pub modes: Vec<ModeOutcome>,
}

impl ProgramResult {
    /// True when at least one mode disagreed with HotSpot.
    pub fn diverged(&self) -> bool {
        self.modes.iter().any(|m| !m.verdict.agrees())
    }

    /// The triage label from the per-mode verdict map (`None` = all agreed).
    pub fn classification(&self) -> Option<Classification> {
        let verdicts: Vec<ModeVerdict> = self
            .modes
            .iter()
            .map(ModeOutcome::to_mode_verdict)
            .collect();
        oracle::classify(&verdicts)
    }

    /// The CratonVM observation to record in the ledger: the first *diverging*
    /// mode (the one that exhibits the bug), else the first mode.
    pub fn representative(&self) -> Option<&ModeOutcome> {
        self.modes
            .iter()
            .find(|m| !m.verdict.agrees())
            .or_else(|| self.modes.first())
    }
}

/// Outcome of running a whole corpus.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    /// One entry per program that ran on both VMs.
    pub results: Vec<ProgramResult>,
    /// `(program, reason)` for programs that could not be run (e.g. javac
    /// failure) — surfaced, never silently dropped.
    pub skipped: Vec<(String, String)>,
    /// `(program, reason)` for programs rejected by the determinism pre-flight
    /// (the two HotSpot runs disagreed). These are *not* judged — admitting a
    /// nondeterministic program would make the gate flap (design §3.3).
    pub nondeterministic: Vec<(String, String)>,
}

impl RunSummary {
    /// Programs that ran on both VMs.
    pub fn total(&self) -> usize {
        self.results.len()
    }

    /// Programs whose VMs disagreed in at least one mode.
    pub fn diverged(&self) -> usize {
        self.results.iter().filter(|r| r.diverged()).count()
    }

    /// Build a fresh `Ledger` (every divergence a `New` entry) from this run.
    /// Step 3 will diff this against the committed ledger to compute the gate
    /// verdict; here we simply materialize what this run observed.
    pub fn to_ledger(&self, host: String, captured_at: String, jdk: String) -> Ledger {
        let mut ledger = Ledger::new(host, captured_at.clone(), jdk);
        for (i, r) in self.results.iter().enumerate() {
            if !r.diverged() {
                continue;
            }
            // Store timing-stripped observations so the committed ledger is
            // stable across regenerations and drift checks use plain equality.
            let cratonvm = r
                .representative()
                .map(|m| m.cratonvm.canonical())
                .unwrap_or_else(Observation::empty);
            ledger.entries.push(LedgerEntry {
                id: format!("div-{i:04}"),
                class: r.program.clone(),
                repro_path: r.source.to_string_lossy().into_owned(),
                classification: r.classification().unwrap_or(Classification::Universal),
                cratonvm,
                hotspot: r.hotspot.canonical(),
                status: LedgerStatus::New,
                first_seen: captured_at.clone(),
                linked_doc: None,
            });
        }
        ledger
    }
}

/// Discover runnable programs directly under `corpus`: `.java` (compiled here)
/// and `.class` (run as-is), sorted by name for a deterministic run order.
pub fn discover_programs(corpus: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(corpus) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("java") | Some("class")
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Run the whole corpus: compile each program once, run HotSpot once and
/// CratonVM in every configured mode, diff, and classify.
///
/// Prerequisites are checked up front so callers can skip cleanly:
/// the `cratonvm` binary must resolve, `java`/`javac` must be reachable, and
/// the corpus must be non-empty.
pub fn run_corpus(config: &RunnerConfig) -> Result<RunSummary, RunError> {
    let bin = runner::cratonvm_binary().ok_or(RunError::CratonvmBinaryMissing)?;
    if !runner::java_available(config.jdk_home.as_deref()) {
        return Err(RunError::JavaMissing);
    }
    let programs = discover_programs(&config.corpus);
    if programs.is_empty() {
        return Err(RunError::EmptyCorpus);
    }
    let modes = if config.modes.is_empty() {
        vec![Mode::JitOn]
    } else {
        config.modes.clone()
    };
    let jdk = config.jdk_home.as_deref();
    let normalizer = Normalizer::strict();

    // Per-run scratch dir for compiled `.class` files, namespaced by pid so
    // concurrent runs don't collide. Best-effort cleanup at the end.
    let workdir = std::env::temp_dir().join(format!("difftest_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&workdir);

    let mut summary = RunSummary::default();
    for source in programs {
        let is_java = source.extension().and_then(|s| s.to_str()) == Some("java");
        let (classpath, main_class) = if is_java {
            match runner::compile_java(&source, &workdir, jdk, config.timeout) {
                Ok(cls) => (workdir.clone(), cls),
                Err(e) => {
                    summary.skipped.push((
                        source
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        e.to_string(),
                    ));
                    continue;
                }
            }
        } else {
            // A bare `.class` runs from its own directory under its file stem.
            let dir = source.parent().unwrap_or(&config.corpus).to_path_buf();
            let cls = source
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            (dir, cls)
        };

        match run_one(&bin, &classpath, &main_class, &modes, config, source)? {
            RunOne::Result(r) => summary.results.push(r),
            RunOne::Nondeterministic(reason) => summary.nondeterministic.push((main_class, reason)),
        }
    }

    let _ = std::fs::remove_dir_all(&workdir);
    Ok(summary)
}

/// The outcome of running a single already-compiled program.
pub enum RunOne {
    /// The program ran on both VMs across the mode matrix.
    Result(ProgramResult),
    /// The determinism pre-flight rejected it (two HotSpot runs disagreed).
    Nondeterministic(String),
}

/// Run one already-compiled `main_class` (found on `classpath`) across every
/// `mode` against a single HotSpot reference, honoring the determinism
/// pre-flight and divergence re-confirmation from `config`. Shared by
/// [`run_corpus`] and the bytecode-mutation tier (`difftest mutate`).
pub fn run_one(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    modes: &[Mode],
    config: &RunnerConfig,
    source: std::path::PathBuf,
) -> Result<RunOne, RunError> {
    let jdk = config.jdk_home.as_deref();
    let normalizer = Normalizer::strict();

    // HotSpot is the deterministic reference — run it once, reuse across modes.
    let hotspot = hotspot_obs(classpath, main_class, jdk, config.timeout)?;

    // Determinism pre-flight (design §3.3): reject a program whose two HotSpot
    // runs disagree, or the gate flaps.
    if config.determinism_check {
        let hotspot2 = hotspot_obs(classpath, main_class, jdk, config.timeout)?;
        if !oracle::compare(&hotspot, &hotspot2, &normalizer).agrees() {
            return Ok(RunOne::Nondeterministic(
                "two HotSpot runs disagree under the normalizer".to_string(),
            ));
        }
    }

    let mut outcomes = Vec::with_capacity(modes.len());
    for &mode in modes {
        let mut cratonvm = cratonvm_obs(bin, classpath, main_class, mode, jdk, config.timeout)?;
        let mut verdict = oracle::compare(&cratonvm, &hotspot, &normalizer);

        // Re-confirm a divergence: a transient won't reproduce.
        if config.reconfirm && !verdict.agrees() {
            let cratonvm2 = cratonvm_obs(bin, classpath, main_class, mode, jdk, config.timeout)?;
            let verdict2 = oracle::compare(&cratonvm2, &hotspot, &normalizer);
            if verdict2.agrees() {
                cratonvm = cratonvm2;
                verdict = verdict2;
            }
        }

        outcomes.push(ModeOutcome {
            mode,
            cratonvm,
            verdict,
        });
    }

    Ok(RunOne::Result(ProgramResult {
        program: main_class.to_string(),
        source,
        hotspot,
        modes: outcomes,
    }))
}

/// Run one program on HotSpot and attach the parsed uncaught exception.
fn hotspot_obs(
    classpath: &Path,
    main_class: &str,
    jdk: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<Observation, RunError> {
    let mut o = runner::run_hotspot(classpath, main_class, jdk, timeout)?;
    o.exception = oracle::parse_exception(&o.stderr);
    Ok(o)
}

/// Run one program on CratonVM in `mode` and attach the parsed uncaught
/// exception.
fn cratonvm_obs(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    mode: Mode,
    _jdk: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<Observation, RunError> {
    let mut o = runner::run_cratonvm(bin, classpath, main_class, mode, timeout)?;
    o.exception = oracle::parse_exception(&o.stderr);
    Ok(o)
}

/// Render a one-line-per-program human summary of a run (used by `difftest
/// run`). A divergence shows its classification and the per-mode channels that
/// disagreed.
pub fn render_summary(summary: &RunSummary) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for r in &summary.results {
        if !r.diverged() {
            let _ = writeln!(s, "  OK    {}", r.program);
            continue;
        }
        let class = r
            .classification()
            .map(classification_label)
            .unwrap_or("divergent");
        let per_mode: Vec<String> = r
            .modes
            .iter()
            .filter(|m| !m.verdict.agrees())
            .map(|m| format!("{}:{}", m.mode.label(), verdict_channels(&m.verdict)))
            .collect();
        let _ = writeln!(
            s,
            "  DIFF  {} [{}] — {}",
            r.program,
            class,
            per_mode.join(" ")
        );
    }
    for (prog, reason) in &summary.skipped {
        let _ = writeln!(s, "  SKIP  {prog} — {reason}");
    }
    let _ = writeln!(
        s,
        "{}/{} programs diverged ({} skipped)",
        summary.diverged(),
        summary.total(),
        summary.skipped.len()
    );
    s
}

/// The channels that disagreed in one verdict, e.g. `stdout,exception`.
fn verdict_channels(v: &Verdict) -> String {
    match v {
        Verdict::Agree => String::new(),
        Verdict::Diverge(diffs) => diffs
            .iter()
            .map(|d| channel_label(d.channel))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn channel_label(c: Channel) -> &'static str {
    match c {
        Channel::ExitCode => "exit-code",
        Channel::Exception => "exception",
        Channel::Stdout => "stdout",
        Channel::Stderr => "stderr",
    }
}

fn classification_label(c: Classification) -> &'static str {
    match c {
        Classification::JitOnly => "jit-only",
        Classification::GcMode => "gc-mode",
        Classification::Universal => "universal",
        Classification::Hang => "hang",
        Classification::Crash => "crash",
    }
}

// ---------------------------------------------------------------------------
// Gate (design §3.5)
// ---------------------------------------------------------------------------

/// The gate's verdict: a run's divergences diffed against the committed
/// known-divergence ledger. Drives the §3.5 exit-code contract.
#[derive(Debug, Clone, Default)]
pub struct GateReport {
    /// Diverging programs with **no** ledger entry — newly appeared (exit 1).
    pub new: Vec<String>,
    /// Diverging programs matching a `fixed` entry — a closed bug re-opened
    /// (exit 2, the most severe).
    pub regressed: Vec<String>,
    /// `known` entries whose CratonVM side **changed** — silent drift (exit 1).
    pub drifted: Vec<String>,
    /// `known` entries that matched as recorded — allowed (no gate failure).
    pub known: Vec<String>,
    /// Ledger divergences that now **agree** — a fix landed (informational; the
    /// entry should be promoted to `fixed`).
    pub resolved: Vec<String>,
    /// Programs rejected by the determinism pre-flight (informational).
    pub nondeterministic: Vec<String>,
}

impl GateReport {
    /// The §3.5 exit code: 2 (a `fixed` bug re-diverged) > 1 (new/drift) > 0.
    pub fn exit_code(&self) -> u8 {
        if !self.regressed.is_empty() {
            2
        } else if !self.new.is_empty() || !self.drifted.is_empty() {
            1
        } else {
            0
        }
    }

    /// Whether the gate passes (no new/regressed/drifted divergence).
    pub fn is_clean(&self) -> bool {
        self.exit_code() == 0
    }
}

/// Compute the gate verdict by diffing a run's `summary` against the committed
/// `ledger` (design §3.5), keyed by program (class) name.
///
/// - a divergence with no ledger entry → **new**;
/// - a divergence matching a `fixed` entry → **regressed**;
/// - a divergence matching a `known`/`new` entry whose CratonVM observation
///   changed → **drifted**, else **known** (allowed);
/// - a program that now agrees but has a `known`/`new` entry → **resolved**.
pub fn gate(summary: &RunSummary, ledger: &Ledger) -> GateReport {
    use std::collections::HashMap;
    let normalizer = Normalizer::strict();
    let by_class: HashMap<&str, &LedgerEntry> = ledger
        .entries
        .iter()
        .map(|e| (e.class.as_str(), e))
        .collect();

    let mut report = GateReport {
        nondeterministic: summary
            .nondeterministic
            .iter()
            .map(|(p, _)| p.clone())
            .collect(),
        ..Default::default()
    };

    for r in &summary.results {
        let entry = by_class.get(r.program.as_str()).copied();
        if r.diverged() {
            match entry {
                None => report.new.push(r.program.clone()),
                Some(e) if e.status == LedgerStatus::Fixed => {
                    report.regressed.push(r.program.clone())
                }
                Some(e) => {
                    // `known` / `new`: allowed unless the CratonVM side drifted.
                    let drifted = match r.representative() {
                        Some(m) => !oracle::gated_eq(&m.cratonvm, &e.cratonvm, &normalizer),
                        None => true,
                    };
                    if drifted {
                        report.drifted.push(r.program.clone());
                    } else {
                        report.known.push(r.program.clone());
                    }
                }
            }
        } else if let Some(e) = entry {
            if matches!(e.status, LedgerStatus::Known | LedgerStatus::New) {
                report.resolved.push(r.program.clone());
            }
        }
    }
    report
}

/// Render the gate verdict for the CLI.
pub fn render_gate(report: &GateReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let line = |s: &mut String, tag: &str, items: &[String]| {
        if !items.is_empty() {
            let _ = writeln!(s, "  {tag}: {}", items.join(", "));
        }
    };
    line(
        &mut s,
        "REGRESSED (fixed bug re-diverged)",
        &report.regressed,
    );
    line(&mut s, "NEW divergence", &report.new);
    line(&mut s, "DRIFT (known divergence changed)", &report.drifted);
    line(&mut s, "known (allowed)", &report.known);
    line(&mut s, "resolved (now agrees)", &report.resolved);
    line(
        &mut s,
        "nondeterministic (skipped)",
        &report.nondeterministic,
    );
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::ChannelDiff;

    // A completed run has a real exit code; `exit_code: None` would (correctly)
    // be read as a signal-kill ⇒ Crash, so these fixtures set `Some(0)`.
    fn diverging_outcome(mode: Mode) -> ModeOutcome {
        ModeOutcome {
            mode,
            cratonvm: Observation {
                stdout: "42".into(),
                exit_code: Some(0),
                ..Observation::empty()
            },
            verdict: Verdict::Diverge(vec![ChannelDiff {
                channel: Channel::Stdout,
                cratonvm: "42".into(),
                hotspot: "43".into(),
            }]),
        }
    }

    fn agreeing_outcome(mode: Mode) -> ModeOutcome {
        ModeOutcome {
            mode,
            cratonvm: Observation {
                exit_code: Some(0),
                ..Observation::empty()
            },
            verdict: Verdict::Agree,
        }
    }

    #[test]
    fn discover_finds_java_and_class_sorted() {
        let seeds = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
        let progs = discover_programs(&seeds);
        assert!(!progs.is_empty(), "seeds dir should have programs");
        assert!(progs.iter().all(|p| {
            matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("java") | Some("class")
            )
        }));
        let mut sorted = progs.clone();
        sorted.sort();
        assert_eq!(progs, sorted);
    }

    #[test]
    fn discover_missing_dir_is_empty() {
        assert!(discover_programs(Path::new("does/not/exist")).is_empty());
    }

    #[test]
    fn program_classifies_jit_only() {
        // jit-on diverges, nojit agrees ⇒ JitOnly + representative is jit-on.
        let r = ProgramResult {
            program: "J".into(),
            source: PathBuf::from("seeds/J.java"),
            hotspot: Observation::empty(),
            modes: vec![
                diverging_outcome(Mode::JitOn),
                agreeing_outcome(Mode::NoJit),
            ],
        };
        assert!(r.diverged());
        assert_eq!(r.classification(), Some(Classification::JitOnly));
        assert_eq!(r.representative().unwrap().mode, Mode::JitOn);
    }

    #[test]
    fn summary_counts_classifies_and_ledger() {
        let agree = ProgramResult {
            program: "A".into(),
            source: PathBuf::from("seeds/A.java"),
            hotspot: Observation::empty(),
            modes: vec![agreeing_outcome(Mode::JitOn), agreeing_outcome(Mode::NoJit)],
        };
        // Both modes diverge ⇒ Universal (the ExceptionId shape).
        let universal = ProgramResult {
            program: "B".into(),
            source: PathBuf::from("seeds/B.java"),
            hotspot: Observation::empty(),
            modes: vec![
                diverging_outcome(Mode::JitOn),
                diverging_outcome(Mode::NoJit),
            ],
        };
        let summary = RunSummary {
            results: vec![agree, universal],
            ..Default::default()
        };
        assert_eq!(summary.total(), 2);
        assert_eq!(summary.diverged(), 1);

        let ledger = summary.to_ledger("host".into(), "t".into(), "25".into());
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].class, "B");
        assert_eq!(ledger.entries[0].classification, Classification::Universal);
        assert_eq!(ledger.entries[0].status, LedgerStatus::New);

        let rendered = render_summary(&summary);
        assert!(rendered.contains("OK    A"));
        assert!(rendered.contains("DIFF  B [universal]"));
        assert!(rendered.contains("jit-on:stdout"));
        assert!(rendered.contains("nojit:stdout"));
    }

    // -- gate (design §3.5) -------------------------------------------------

    /// A program that diverges in both modes (the Universal shape).
    fn diverging_program(name: &str) -> ProgramResult {
        ProgramResult {
            program: name.into(),
            source: PathBuf::from(format!("seeds/{name}.java")),
            hotspot: Observation::empty(),
            modes: vec![
                diverging_outcome(Mode::JitOn),
                diverging_outcome(Mode::NoJit),
            ],
        }
    }

    fn agreeing_program(name: &str) -> ProgramResult {
        ProgramResult {
            program: name.into(),
            source: PathBuf::from(format!("seeds/{name}.java")),
            hotspot: Observation::empty(),
            modes: vec![agreeing_outcome(Mode::JitOn), agreeing_outcome(Mode::NoJit)],
        }
    }

    /// A ledger holding one entry for `class` with the given status; its
    /// recorded CratonVM stdout is `cratonvm_stdout`.
    fn ledger_with(class: &str, status: LedgerStatus, cratonvm_stdout: &str) -> Ledger {
        let mut l = Ledger::new("host".into(), "t".into(), "25".into());
        l.entries.push(LedgerEntry {
            id: "div-0000".into(),
            class: class.into(),
            repro_path: format!("seeds/{class}.java"),
            classification: Classification::Universal,
            cratonvm: Observation {
                stdout: cratonvm_stdout.into(),
                exit_code: Some(0),
                ..Observation::empty()
            },
            hotspot: Observation::empty(),
            status,
            first_seen: "t".into(),
            linked_doc: None,
        });
        l
    }

    fn summary_of(results: Vec<ProgramResult>) -> RunSummary {
        RunSummary {
            results,
            ..Default::default()
        }
    }

    #[test]
    fn gate_new_divergence_fails_exit_1() {
        // Diverges, but nothing in the (empty) ledger ⇒ new.
        let summary = summary_of(vec![diverging_program("X")]);
        let ledger = Ledger::new("h".into(), "t".into(), "25".into());
        let report = gate(&summary, &ledger);
        assert_eq!(report.new, vec!["X".to_string()]);
        assert_eq!(report.exit_code(), 1);
        assert!(!report.is_clean());
    }

    #[test]
    fn gate_known_divergence_is_allowed_exit_0() {
        // The diverging_outcome's recorded stdout is "42"; a Known entry with
        // the same stdout matches ⇒ allowed.
        let summary = summary_of(vec![diverging_program("X")]);
        let ledger = ledger_with("X", LedgerStatus::Known, "42");
        let report = gate(&summary, &ledger);
        assert_eq!(report.known, vec!["X".to_string()]);
        assert!(report.new.is_empty());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn gate_drift_fails_exit_1() {
        // Known entry recorded a different CratonVM stdout ⇒ drift.
        let summary = summary_of(vec![diverging_program("X")]);
        let ledger = ledger_with("X", LedgerStatus::Known, "99-was-different");
        let report = gate(&summary, &ledger);
        assert_eq!(report.drifted, vec!["X".to_string()]);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn gate_fixed_redivergence_fails_exit_2() {
        let summary = summary_of(vec![diverging_program("X")]);
        let ledger = ledger_with("X", LedgerStatus::Fixed, "42");
        let report = gate(&summary, &ledger);
        assert_eq!(report.regressed, vec!["X".to_string()]);
        assert_eq!(report.exit_code(), 2);
    }

    #[test]
    fn gate_resolved_is_clean() {
        // A Known divergence that now agrees ⇒ resolved (a fix), exit 0.
        let summary = summary_of(vec![agreeing_program("X")]);
        let ledger = ledger_with("X", LedgerStatus::Known, "42");
        let report = gate(&summary, &ledger);
        assert_eq!(report.resolved, vec!["X".to_string()]);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn gate_all_agree_empty_ledger_is_clean() {
        let summary = summary_of(vec![agreeing_program("A"), agreeing_program("B")]);
        let ledger = Ledger::new("h".into(), "t".into(), "25".into());
        let report = gate(&summary, &ledger);
        assert!(report.is_clean());
        assert_eq!(report.exit_code(), 0);
    }
}
