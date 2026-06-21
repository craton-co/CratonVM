// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Run orchestration: discover the corpus, compile each program once with
//! `javac`, run the resulting `.class` on CratonVM and HotSpot, and diff.
//!
//! This is the seam between the process mechanics ([`crate::runner`]) and the
//! comparison ([`crate::oracle`]). The same compiled `.class` runs on both VMs
//! (design §3.1, the wrapper-free path).
//!
//! ## Status: Step 1
//!
//! Runs the **first configured CratonVM mode** (default `jit-on`) vs HotSpot
//! and diffs the four channels. The per-mode fan-out + `Classification` join is
//! Step 2; until then a divergence is classified coarsely
//! (`Hang`/`Crash`/`Universal`).

use std::path::{Path, PathBuf};

use crate::ledger::{Channel, Classification, Ledger, LedgerEntry, LedgerStatus, Observation};
use crate::oracle::{self, Normalizer, Verdict};
use crate::runner::{self, Mode, RunError, RunnerConfig};

/// One program's A/B result.
#[derive(Debug, Clone)]
pub struct ProgramResult {
    /// The main class name that was run.
    pub program: String,
    /// The corpus source file (`.java` / `.class`).
    pub source: PathBuf,
    /// The CratonVM mode used (Step 1: the first configured mode).
    pub mode: Mode,
    pub cratonvm: Observation,
    pub hotspot: Observation,
    pub verdict: Verdict,
}

impl ProgramResult {
    /// Coarse single-mode classification (refined by Step 2's mode matrix).
    pub fn classification(&self) -> Option<Classification> {
        oracle::classify(&self.cratonvm, !self.verdict.agrees(), &[])
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

    /// Programs whose VMs disagreed.
    pub fn diverged(&self) -> usize {
        self.results.iter().filter(|r| !r.verdict.agrees()).count()
    }

    /// Build a fresh `Ledger` (every divergence a `New` entry) from this run.
    /// Step 3 will diff this against the committed ledger to compute the gate
    /// verdict; here we simply materialize what this run observed.
    pub fn to_ledger(&self, host: String, captured_at: String, jdk: String) -> Ledger {
        let mut ledger = Ledger::new(host, captured_at.clone(), jdk);
        for (i, r) in self.results.iter().enumerate() {
            if r.verdict.agrees() {
                continue;
            }
            ledger.entries.push(LedgerEntry {
                id: format!("div-{i:04}"),
                class: r.program.clone(),
                repro_path: r.source.to_string_lossy().into_owned(),
                classification: r.classification().unwrap_or(Classification::Universal),
                cratonvm: r.cratonvm.clone(),
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

/// Run the whole corpus: compile each program once, run it on the first
/// configured CratonVM mode and HotSpot, and diff.
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
    let mode = config.modes.first().copied().unwrap_or(Mode::JitOn);
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

        let mut cratonvm =
            runner::run_cratonvm(&bin, &classpath, &main_class, mode, config.timeout)?;
        cratonvm.exception = oracle::parse_exception(&cratonvm.stderr);
        let mut hotspot = runner::run_hotspot(&classpath, &main_class, jdk, config.timeout)?;
        hotspot.exception = oracle::parse_exception(&hotspot.stderr);

        let verdict = oracle::compare(&cratonvm, &hotspot, &normalizer);
        summary.results.push(ProgramResult {
            program: main_class,
            source,
            mode,
            cratonvm,
            hotspot,
            verdict,
        });
    }

    let _ = std::fs::remove_dir_all(&workdir);
    Ok(summary)
}

/// Render a one-line-per-program human summary of a run (used by `difftest
/// run`). Divergences list the channels that disagreed.
pub fn render_summary(summary: &RunSummary) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for r in &summary.results {
        match &r.verdict {
            Verdict::Agree => {
                let _ = writeln!(s, "  OK    {} [{}]", r.program, r.mode.label());
            }
            Verdict::Diverge(diffs) => {
                let channels: Vec<&str> = diffs.iter().map(|d| channel_label(d.channel)).collect();
                let _ = writeln!(
                    s,
                    "  DIFF  {} [{}] — {}",
                    r.program,
                    r.mode.label(),
                    channels.join(", ")
                );
            }
        }
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

fn channel_label(c: Channel) -> &'static str {
    match c {
        Channel::ExitCode => "exit-code",
        Channel::Exception => "exception",
        Channel::Stdout => "stdout",
        Channel::Stderr => "stderr",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_java_and_class_sorted() {
        // The committed seeds dir is a stable fixture.
        let seeds = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
        let progs = discover_programs(&seeds);
        assert!(!progs.is_empty(), "seeds dir should have programs");
        assert!(progs.iter().all(|p| {
            matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("java") | Some("class")
            )
        }));
        // Sorted order is deterministic.
        let mut sorted = progs.clone();
        sorted.sort();
        assert_eq!(progs, sorted);
    }

    #[test]
    fn discover_missing_dir_is_empty() {
        assert!(discover_programs(Path::new("does/not/exist")).is_empty());
    }

    #[test]
    fn summary_counts_and_ledger() {
        // A synthetic summary with one agreement and one divergence.
        let agree = ProgramResult {
            program: "A".into(),
            source: PathBuf::from("seeds/A.java"),
            mode: Mode::JitOn,
            cratonvm: Observation::empty(),
            hotspot: Observation::empty(),
            verdict: Verdict::Agree,
        };
        let diverge = ProgramResult {
            program: "B".into(),
            source: PathBuf::from("seeds/B.java"),
            mode: Mode::JitOn,
            cratonvm: Observation {
                stdout: "42".into(),
                ..Observation::empty()
            },
            hotspot: Observation {
                stdout: "43".into(),
                ..Observation::empty()
            },
            verdict: Verdict::Diverge(vec![crate::oracle::ChannelDiff {
                channel: Channel::Stdout,
                cratonvm: "42".into(),
                hotspot: "43".into(),
            }]),
        };
        let summary = RunSummary {
            results: vec![agree, diverge],
            skipped: vec![],
        };
        assert_eq!(summary.total(), 2);
        assert_eq!(summary.diverged(), 1);

        let ledger = summary.to_ledger("host".into(), "t".into(), "25".into());
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].class, "B");
        assert_eq!(ledger.entries[0].status, LedgerStatus::New);

        let rendered = render_summary(&summary);
        assert!(rendered.contains("OK    A"));
        assert!(rendered.contains("DIFF  B"));
        assert!(rendered.contains("stdout"));
    }
}
