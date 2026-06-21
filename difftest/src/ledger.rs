// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Divergence records and the committed divergence ledger.
//!
//! Two layers live here:
//!
//! 1. The **legacy `DivergenceReport`** shape, lifted verbatim from
//!    `vm/tests/differential.rs` so the existing integration test can migrate
//!    onto the shared types without a behavior change (design §6.3). It is a
//!    flat `stdout` + `return_value` comparison.
//!
//! 2. The **first-class [`Ledger`]** (design §3.5): a richer,
//!    `bench/hotspot-baseline.json`-style committed artifact with a
//!    `schema_version` / `host` / `captured_at` / JDK header and a list of
//!    [`LedgerEntry`] rows carrying the full [`Observation`] from each VM, a
//!    [`Classification`], and a `known | fixed | new` [`LedgerStatus`]. The CI
//!    gate (design §3.5) reads this to decide whether a run introduced a *new*
//!    or *regressed* divergence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ===========================================================================
// Legacy report shape (lifted from vm/tests/differential.rs)
// ===========================================================================

/// Outcome of running a single method under one VM (legacy micro-method tier).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    /// Captured stdout lines (joined with newline).
    pub stdout: String,
    /// Return value as a string, or `"void"` / `"error:<msg>"`.
    pub return_value: String,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stdout={:?}, return={:?}",
            self.stdout, self.return_value
        )
    }
}

/// A single divergence between CratonVM and HotSpot (legacy flat shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Divergence {
    pub class_name: String,
    pub method: String,
    pub descriptor: String,
    pub cratonvm_stdout: String,
    pub cratonvm_return: String,
    pub hotspot_stdout: String,
    pub hotspot_return: String,
}

/// A collection of divergences, serializable to JSON. Carries the running
/// `total_tested` / `total_matched` tallies used by the seed-tier summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DivergenceReport {
    pub divergences: Vec<Divergence>,
    pub total_tested: usize,
    pub total_matched: usize,
}

impl DivergenceReport {
    /// An empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one micro-method comparison: bump the tallies and, when the two
    /// outcomes differ, push a [`Divergence`].
    pub fn record(
        &mut self,
        class: &str,
        method: &str,
        descriptor: &str,
        cratonvm: &Outcome,
        hotspot: &Outcome,
    ) {
        self.total_tested += 1;
        if cratonvm.stdout == hotspot.stdout && cratonvm.return_value == hotspot.return_value {
            self.total_matched += 1;
        } else {
            self.divergences.push(Divergence {
                class_name: class.to_string(),
                method: method.to_string(),
                descriptor: descriptor.to_string(),
                cratonvm_stdout: cratonvm.stdout.clone(),
                cratonvm_return: cratonvm.return_value.clone(),
                hotspot_stdout: hotspot.stdout.clone(),
                hotspot_return: hotspot.return_value.clone(),
            });
        }
    }

    /// Serialize the report (pretty JSON) to `path`. Returns the IO/serde
    /// error rather than panicking, so callers decide how loud to be.
    pub fn write_to_file(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

// ===========================================================================
// Rich observation (design §3.2)
// ===========================================================================

/// A parsed uncaught JVM exception, extracted from the standard
/// `Exception in thread "main" <fqcn>: <message>` stderr banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JvmException {
    /// Fully-qualified class name, e.g. `java.lang.NullPointerException`.
    pub fqcn: String,
    /// The message text following the colon (after normalization).
    pub message: String,
    /// The `at <frame>` lines, in source order (so reversed-stacktrace bugs
    /// surface as an ordering diff).
    pub top_frames: Vec<String>,
}

/// Everything observed from one process run of one program under one VM/mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub stdout: String,
    pub stderr: String,
    /// Process exit code, or `None` if the process was killed by a signal.
    pub exit_code: Option<i32>,
    /// The uncaught exception parsed out of `stderr`, when present.
    pub exception: Option<JvmException>,
    /// Set when the run hit the per-run timeout (a `Hang` classification).
    pub timed_out: bool,
    /// Wall-clock time of the run, in milliseconds.
    pub wall_ms: u64,
}

impl Observation {
    /// An empty observation placeholder (used by stubs until the runner is
    /// wired). Never produced by a real run.
    pub fn empty() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            exception: None,
            timed_out: false,
            wall_ms: 0,
        }
    }
}

// ===========================================================================
// Classification & status (design §3.3 / §3.5)
// ===========================================================================

/// How a confirmed divergence is triaged — this is the bisection the human
/// currently does by hand (`--nojit` to isolate JIT bugs, GC-mode flips, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Classification {
    /// `jit-on` diverges but `--nojit` agrees — a JIT correctness bug.
    JitOnly,
    /// Only a moving / selective-promote GC mode diverges.
    GcMode,
    /// Every CratonVM mode diverges equally — an interpreter/native gap.
    Universal,
    /// CratonVM times out while HotSpot finishes.
    Hang,
    /// CratonVM exits via signal/abort while HotSpot exits cleanly.
    Crash,
}

/// Ledger lifecycle of a divergence (design §3.5). The gate's exit code keys
/// off transitions between these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedgerStatus {
    /// Tracked-but-open: allowed by the gate (CratonVM is ~90% there).
    Known,
    /// A previously-closed divergence — re-tripping it is a true regression.
    Fixed,
    /// Newly observed this run — fails the gate.
    New,
}

/// One row of the committed divergence ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Stable short id, referenced by the minimized repro directory name.
    pub id: String,
    /// The program under test (class / seed name).
    pub class: String,
    /// Path (relative to the crate) to the minimized reproducer.
    pub repro_path: String,
    pub classification: Classification,
    pub cratonvm: Observation,
    pub hotspot: Observation,
    pub status: LedgerStatus,
    /// ISO-8601 timestamp the divergence was first seen.
    pub first_seen: String,
    /// Cross-link to the `docs/internal/*` / `MEMORY.md` note, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_doc: Option<String>,
}

/// The committed known-divergence ledger (design §3.5). Mirrors the
/// `bench/hotspot-baseline.json` header discipline so the gate can pin the
/// JDK and host it was captured against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ledger {
    /// Bumped on any breaking change to this schema.
    pub schema_version: u32,
    /// Host tag (OS / arch) the ledger was captured on.
    pub host: String,
    /// ISO-8601 capture timestamp.
    pub captured_at: String,
    /// The JDK build string the HotSpot side was pinned to.
    pub jdk: String,
    pub entries: Vec<LedgerEntry>,
}

/// The current ledger schema version.
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

impl Ledger {
    /// An empty ledger with the given header fields.
    pub fn new(host: String, captured_at: String, jdk: String) -> Self {
        Self {
            schema_version: LEDGER_SCHEMA_VERSION,
            host,
            captured_at,
            jdk,
            entries: Vec::new(),
        }
    }

    /// Load a ledger from `path`. Returns `Ok(None)` when the file does not
    /// exist (a first run / bootstrap), and an error only on a malformed file.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Serialize the ledger (pretty JSON) to `path`.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

/// Default ledger path: `bench/differential-divergences.json` at the workspace
/// root (continuity with the §2.1 harness, which already writes there).
pub fn default_ledger_path() -> PathBuf {
    crate::runner::workspace_root()
        .join("bench")
        .join("differential-divergences.json")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(stdout: &str, ret: &str) -> Outcome {
        Outcome {
            stdout: stdout.to_string(),
            return_value: ret.to_string(),
        }
    }

    #[test]
    fn report_records_match_and_divergence() {
        let mut report = DivergenceReport::new();
        report.record("A", "m", "()I", &outcome("ok", "1"), &outcome("ok", "1"));
        report.record("B", "m", "()I", &outcome("foo", "1"), &outcome("bar", "1"));
        assert_eq!(report.total_tested, 2);
        assert_eq!(report.total_matched, 1);
        assert_eq!(report.divergences.len(), 1);
        assert_eq!(report.divergences[0].class_name, "B");
    }

    #[test]
    fn report_default_is_empty() {
        let report = DivergenceReport::new();
        assert_eq!(report.total_tested, 0);
        assert!(report.divergences.is_empty());
    }

    #[test]
    fn classification_serializes_kebab_case() {
        let json = serde_json::to_string(&Classification::JitOnly).unwrap();
        assert_eq!(json, "\"jit-only\"");
    }

    #[test]
    fn ledger_round_trips_through_json() {
        let mut ledger = Ledger::new(
            "linux-x86_64".into(),
            "2026-06-21T00:00:00Z".into(),
            "25".into(),
        );
        ledger.entries.push(LedgerEntry {
            id: "div-0001".into(),
            class: "ArithEdge".into(),
            repro_path: "regression/div-0001/ArithEdge.java".into(),
            classification: Classification::Universal,
            cratonvm: Observation::empty(),
            hotspot: Observation::empty(),
            status: LedgerStatus::Known,
            first_seen: "2026-06-21T00:00:00Z".into(),
            linked_doc: None,
        });
        let json = serde_json::to_string_pretty(&ledger).unwrap();
        let back: Ledger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, LEDGER_SCHEMA_VERSION);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].status, LedgerStatus::Known);
        // `linked_doc: None` is skipped on the wire.
        assert!(!json.contains("linked_doc"));
    }

    #[test]
    fn observation_empty_is_inert() {
        let o = Observation::empty();
        assert!(!o.timed_out);
        assert!(o.exception.is_none());
        assert_eq!(o.exit_code, None);
    }
}
