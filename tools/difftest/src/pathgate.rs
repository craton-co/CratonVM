// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The **execution-path gate**: every JIT mode must agree with the interpreter.
//!
//! `cratonvm-difftest gate` judges CratonVM against a HotSpot run and the
//! committed `difftest/ledger.json`. That is the right gate for VM-vs-JDK
//! parity, but it is the wrong shape for the JIT's execution paths:
//!
//! * a ledger row is keyed by `(class, jdk_profile)`, not by mode, so a new
//!   mode silently shares the rows captured under `jit-on`/`nojit` — including
//!   their recorded CratonVM observation, which the gate compares for drift;
//! * extending it to new modes would mean regenerating the ledger with a local
//!   HotSpot run for every mode added;
//! * and its cross-path report ([`crate::crossmode`]) is deliberately
//!   **not gated**, because a `known` `jit-only` ledger row is a path split by
//!   construction.
//!
//! This gate needs neither HotSpot nor the ledger. The reference is a CratonVM
//! mode — `nojit`, the interpreter — and the rule is the one
//! [`crate::crossmode`] states: two of the VM's own executors printing
//! different things for the same deterministic program is a defect, with no
//! reference or normalization rule to blame. So:
//!
//! 1. the reference runs **twice**; a program whose two reference runs disagree
//!    is reported as nondeterministic and not judged;
//! 2. every other mode runs once and is compared to the reference on the gated
//!    dimensions (exit code, uncaught exception, stdout, checksums);
//! 3. a split is re-run once — a split that does not reproduce is reported as
//!    **flaky** and does not move the exit code;
//! 4. a reproducible split fails the gate (exit 1) unless it is listed in the
//!    known-splits file (`difftest/path-gate-known.json`), which plays the role
//!    the ledger plays for `gate` but is keyed by `(program, mode)`;
//! 5. optionally, the IR verifier rejecting a method (only visible on stderr
//!    under `CRATONVM_DBG_IR_COMPILES=1`) fails the gate too.
//!
//! `javac` is still needed to compile `.java` seeds, but no program is ever run
//! on HotSpot.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::harness;
use crate::ledger::Observation;
use crate::oracle::{self, Normalizer, Verdict};
use crate::runner::{self, Mode, RunError};

/// The line `jit/src/lib.rs::ir_verify_reject` prints when the IR verifier
/// rejects a graph. It is printed only when `ir_stage_reporting()` is on, i.e.
/// `CRATONVM_DBG_IR_COMPILES` or `CRATONVM_DBG_JITC` is set; otherwise a
/// rejection is a silent fallback to the single-pass backend.
pub const IR_VERIFY_REJECT_MARKER: &str = "[ir] verifier rejected";

/// The `path-gate --modes` default: `Mode::execution_paths()` plus `moving-gc`.
/// `path_gate_default_is_every_execution_path_plus_moving_gc` pins it.
pub const DEFAULT_MODES: &str =
    "nojit,interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt,moving-gc";

/// Schema of the known-splits file.
pub const KNOWN_SCHEMA_VERSION: u32 = 1;

/// The committed known-splits file.
pub fn default_known_path() -> PathBuf {
    runner::workspace_root()
        .join("difftest")
        .join("path-gate-known.json")
}

/// One tracked, not-yet-fixed path split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownSplit {
    /// Main class name.
    pub program: String,
    /// Mode label, e.g. `ir-jit`.
    pub mode: String,
    /// Where the bug is tracked (a `docs/known-issues/` path) and why.
    #[serde(default)]
    pub note: String,
}

/// The known-splits file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownSplits {
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<KnownSplit>,
}

impl Default for KnownSplits {
    fn default() -> Self {
        Self {
            schema_version: KNOWN_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl KnownSplits {
    /// Load `path`; a missing file is an empty list (every split is new).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let k: KnownSplits = serde_json::from_str(&text)?;
                if k.schema_version > KNOWN_SCHEMA_VERSION {
                    anyhow::bail!(
                        "{} is schema_version {}, newer than this build's {KNOWN_SCHEMA_VERSION}",
                        path.display(),
                        k.schema_version
                    );
                }
                Ok(k)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn contains(&self, program: &str, mode: Mode) -> bool {
        self.entries
            .iter()
            .any(|e| e.program == program && e.mode == mode.label())
    }
}

/// Configuration for one gate run.
#[derive(Debug, Clone)]
pub struct PathGateConfig {
    pub corpus: PathBuf,
    pub modes: Vec<Mode>,
    pub reference: Mode,
    pub timeout: Duration,
    pub jdk_home: Option<PathBuf>,
    pub known: KnownSplits,
    pub fail_on_ir_verify_reject: bool,
}

/// One mode's disagreement with the reference on one program.
#[derive(Debug, Clone)]
pub struct Split {
    pub mode: Mode,
    /// `channel: mode-side != reference-side`, one per differing dimension.
    pub details: Vec<String>,
    /// Did not reproduce on the immediate re-run.
    pub flaky: bool,
}

/// The gate verdict.
#[derive(Debug, Clone, Default)]
pub struct PathGateReport {
    /// Programs judged.
    pub programs: usize,
    /// Reproducible splits not on the known list — **gated**.
    pub new_splits: Vec<String>,
    /// Reproducible splits on the known list — allowed.
    pub known_splits: Vec<String>,
    /// Known entries whose mode now agrees — informational (remove the entry).
    pub resolved_known: Vec<String>,
    /// Splits that did not reproduce — informational.
    pub flaky: Vec<String>,
    /// `Program[mode]` runs in which the IR verifier rejected a method —
    /// gated only under `fail_on_ir_verify_reject`.
    pub ir_verify_rejects: Vec<String>,
    /// Programs whose two reference runs disagreed — not judged.
    pub nondeterministic: Vec<String>,
    /// `(program, reason)` for programs that could not be compiled.
    pub skipped: Vec<(String, String)>,
    /// Human-readable detail lines for every split, in report order.
    pub details: Vec<String>,
    pub fail_on_ir_verify_reject: bool,
}

impl PathGateReport {
    /// 0 clean, 1 a new reproducible split (or a gated verifier rejection).
    pub fn exit_code(&self) -> u8 {
        if !self.new_splits.is_empty()
            || (self.fail_on_ir_verify_reject && !self.ir_verify_rejects.is_empty())
        {
            1
        } else {
            0
        }
    }

    /// Fold one program's outcome into the report. Pure, so the gate's policy
    /// is unit-testable without a VM.
    pub fn record(
        &mut self,
        program: &str,
        reference: Mode,
        judged_modes: &[Mode],
        splits: Vec<Split>,
        rejects: &[Mode],
        known: &KnownSplits,
    ) {
        self.programs += 1;
        for s in &splits {
            let label = format!("{program}: {}≠{}", reference.label(), s.mode.label());
            if s.flaky {
                self.flaky.push(label.clone());
            } else if known.contains(program, s.mode) {
                self.known_splits.push(label.clone());
            } else {
                self.new_splits.push(label.clone());
            }
            self.details.push(label);
            for d in &s.details {
                self.details.push(format!("    {d}"));
            }
        }
        for &m in judged_modes {
            let split_here = splits.iter().any(|s| s.mode == m && !s.flaky);
            if !split_here && known.contains(program, m) {
                self.resolved_known
                    .push(format!("{program}: {}≠{}", reference.label(), m.label()));
            }
        }
        for m in rejects {
            self.ir_verify_rejects
                .push(format!("{program}[{}]", m.label()));
        }
    }
}

/// Compare `obs` against the reference; `None` when they agree.
pub fn split_details(
    obs: &Observation,
    reference: &Observation,
    mode: Mode,
) -> Option<Vec<String>> {
    match oracle::compare(obs, reference, &Normalizer::for_mode(mode)) {
        Verdict::Agree => None,
        Verdict::Diverge(diffs) => Some(
            diffs
                .iter()
                .map(|d| {
                    format!(
                        "{}: {} != {}",
                        d.channel.label(),
                        one_line(&d.cratonvm),
                        one_line(&d.hotspot)
                    )
                })
                .collect(),
        ),
    }
}

fn one_line(s: &str) -> String {
    let flat = s.replace('\n', " / ");
    if flat.chars().count() <= 120 {
        flat
    } else {
        format!("{}...", flat.chars().take(117).collect::<String>())
    }
}

/// Run the gate over `cfg.corpus`.
pub fn run_path_gate(cfg: &PathGateConfig) -> Result<PathGateReport, RunError> {
    let bin = runner::cratonvm_binary().ok_or(RunError::CratonvmBinaryMissing)?;
    let programs = harness::discover_programs(&cfg.corpus);
    if programs.is_empty() {
        return Err(RunError::EmptyCorpus);
    }
    let needs_javac = programs
        .iter()
        .any(|p| p.extension().and_then(|s| s.to_str()) == Some("java"));
    if needs_javac && !runner::java_available(cfg.jdk_home.as_deref()) {
        return Err(RunError::JavaMissing);
    }

    let workdir = std::env::temp_dir().join(format!("difftest_pathgate_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&workdir);
    let mut report = PathGateReport {
        fail_on_ir_verify_reject: cfg.fail_on_ir_verify_reject,
        ..Default::default()
    };
    let judged: Vec<Mode> = cfg
        .modes
        .iter()
        .copied()
        .filter(|&m| m != cfg.reference && crate::crossmode::comparable(cfg.reference, m))
        .collect();

    for source in programs {
        let label = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let (classpath, main_class) = if source.extension().and_then(|s| s.to_str()) == Some("java")
        {
            match runner::compile_java(&source, &workdir, cfg.jdk_home.as_deref(), cfg.timeout) {
                Ok(cls) => (workdir.clone(), cls),
                Err(e) => {
                    report.skipped.push((label, e.to_string()));
                    continue;
                }
            }
        } else {
            let dir = source.parent().unwrap_or(&cfg.corpus).to_path_buf();
            let cls = source
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            (dir, cls)
        };

        let run = |mode: Mode| {
            runner::run_cratonvm_with_env(&bin, &classpath, &main_class, mode, cfg.timeout, &[])
        };
        let reference = run(cfg.reference)?;
        let reference2 = run(cfg.reference)?;
        if split_details(&reference2, &reference, cfg.reference).is_some() {
            report.nondeterministic.push(main_class.clone());
            continue;
        }

        let mut splits = Vec::new();
        let mut rejects = Vec::new();
        for &mode in &judged {
            let obs = run(mode)?;
            if obs.stderr.contains(IR_VERIFY_REJECT_MARKER) {
                rejects.push(mode);
            }
            if let Some(details) = split_details(&obs, &reference, mode) {
                let again = run(mode)?;
                let flaky = split_details(&again, &reference, mode).is_none();
                splits.push(Split {
                    mode,
                    details,
                    flaky,
                });
            }
        }
        report.record(
            &main_class,
            cfg.reference,
            &judged,
            splits,
            &rejects,
            &cfg.known,
        );
    }
    let _ = std::fs::remove_dir_all(&workdir);
    Ok(report)
}

/// Render the verdict for the CLI.
pub fn render(report: &PathGateReport, reference: Mode) -> String {
    let mut s = String::new();
    let line = |s: &mut String, tag: &str, items: &[String]| {
        if !items.is_empty() {
            let _ = writeln!(s, "  {tag}: {}", items.join(", "));
        }
    };
    line(&mut s, "NEW path split (gated)", &report.new_splits);
    line(&mut s, "known path split (allowed)", &report.known_splits);
    line(
        &mut s,
        "resolved known split (remove it from the known file)",
        &report.resolved_known,
    );
    line(
        &mut s,
        "flaky split (did not reproduce; not gated)",
        &report.flaky,
    );
    let reject_tag = if report.fail_on_ir_verify_reject {
        "IR verifier rejected a method (gated)"
    } else {
        "IR verifier rejected a method (not gated)"
    };
    line(&mut s, reject_tag, &report.ir_verify_rejects);
    line(
        &mut s,
        "nondeterministic under the reference (not judged)",
        &report.nondeterministic,
    );
    for (p, why) in &report.skipped {
        let _ = writeln!(s, "  SKIP  {p} — {why}");
    }
    for d in &report.details {
        let _ = writeln!(s, "  {d}");
    }
    let _ = writeln!(
        s,
        "{} program(s) judged against {}: {} new, {} known, {} flaky split(s)",
        report.programs,
        reference.label(),
        report.new_splits.len(),
        report.known_splits.len(),
        report.flaky.len()
    );
    s
}

#[cfg(test)]
mod tests;
