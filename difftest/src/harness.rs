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
//! ## Status: Step 2
//!
//! For each program HotSpot runs **once** (the deterministic reference) and
//! CratonVM runs **once per configured [`Mode`]**; the per-mode agree/diverge
//! map feeds [`oracle::classify`] to auto-label the divergence
//! (`JitOnly`/`GcMode`/`Universal`/`Hang`/`Crash`) — automating the manual
//! `--nojit` bisection.

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
            let cratonvm = r
                .representative()
                .map(|m| m.cratonvm.clone())
                .unwrap_or_else(Observation::empty);
            ledger.entries.push(LedgerEntry {
                id: format!("div-{i:04}"),
                class: r.program.clone(),
                repro_path: r.source.to_string_lossy().into_owned(),
                classification: r.classification().unwrap_or(Classification::Universal),
                cratonvm,
                hotspot: r.hotspot.clone(),
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

        // HotSpot is the deterministic reference — run it once and reuse it
        // across every CratonVM mode.
        let mut hotspot = runner::run_hotspot(&classpath, &main_class, jdk, config.timeout)?;
        hotspot.exception = oracle::parse_exception(&hotspot.stderr);

        let mut outcomes = Vec::with_capacity(modes.len());
        for &mode in &modes {
            let mut cratonvm =
                runner::run_cratonvm(&bin, &classpath, &main_class, mode, config.timeout)?;
            cratonvm.exception = oracle::parse_exception(&cratonvm.stderr);
            let verdict = oracle::compare(&cratonvm, &hotspot, &normalizer);
            outcomes.push(ModeOutcome {
                mode,
                cratonvm,
                verdict,
            });
        }

        summary.results.push(ProgramResult {
            program: main_class,
            source,
            hotspot,
            modes: outcomes,
        });
    }

    let _ = std::fs::remove_dir_all(&workdir);
    Ok(summary)
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
            skipped: vec![],
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
}
