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

use std::collections::BTreeMap;
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
    /// An empty observation placeholder for tests and synthetic ledger rows.
    /// Never produced by a real run.
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

    /// A copy with run-specific timing zeroed, so a committed ledger entry is
    /// stable across regenerations and a drift comparison can use plain
    /// equality without `wall_ms` jitter tripping it.
    pub fn canonical(&self) -> Observation {
        Observation {
            wall_ms: 0,
            ..self.clone()
        }
    }
}

// ===========================================================================
// Classification & status (design §3.3 / §3.5)
// ===========================================================================

/// An observable channel the oracle compares (design §3.3). Each disagreement
/// a [`compare`](crate::oracle::compare) reports is tagged with the channel it
/// came from, so the ledger and the run summary can say *what* diverged.
///
/// ## Why the exception channel is four channels
///
/// "The exception differed" is not an actionable report: a wrong *type* is a
/// dispatch or resolution bug, a wrong *message* is usually a formatting or
/// helpful-NPE gap, and wrong *frames* are an attribution bug in the unwinder.
/// The committed `ExceptionId` ledger row is exactly this case — its CCE
/// message lacks HotSpot's module/loader detail while the type and frames are
/// right — and collapsing all three into one string made that read as a single
/// opaque "exception" diff. Each is now its own dimension, reported
/// independently, so a divergence names which one moved.
///
/// [`Channel::Exception`] survives as the **presence** dimension: one VM threw
/// and the other did not, which is a different finding from the two throwing
/// differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    /// Process exit code (a CratonVM timeout is reported here too).
    ExitCode,
    /// Uncaught-exception **presence**: one side threw, the other did not.
    Exception,
    /// Uncaught-exception **type** (`fqcn`), compared exactly.
    ExceptionType,
    /// Uncaught-exception **message**, compared exactly after normalization.
    ExceptionMessage,
    /// Uncaught-exception **stack frames**, compared in order (so a reversed
    /// stack trace surfaces as an ordering diff).
    ExceptionFrames,
    /// Program stdout (strict after normalization).
    Stdout,
    /// Program stderr (contextual; not gated by default).
    Stderr,
    /// The program's own declared checksums (`crate::checksum`), compared on
    /// **un-normalized** stdout so no normalization rule can launder them.
    Checksum,
}

impl Channel {
    /// Every channel the oracle can report, in report order.
    pub fn all() -> &'static [Channel] {
        &[
            Channel::ExitCode,
            Channel::Exception,
            Channel::ExceptionType,
            Channel::ExceptionMessage,
            Channel::ExceptionFrames,
            Channel::Stdout,
            Channel::Stderr,
            Channel::Checksum,
        ]
    }

    /// The stable kebab-case label used in run/gate output.
    pub fn label(self) -> &'static str {
        match self {
            Channel::ExitCode => "exit-code",
            Channel::Exception => "exception",
            Channel::ExceptionType => "exception-type",
            Channel::ExceptionMessage => "exception-message",
            Channel::ExceptionFrames => "exception-frames",
            Channel::Stdout => "stdout",
            Channel::Stderr => "stderr",
            Channel::Checksum => "checksum",
        }
    }
}

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
    /// A `--jdk-only` run recorded a policy violation
    /// (`docs/feature-designs/jdk-only-mode.md` §3's `JdkOnlyViolation`): a
    /// compatibility class was requested, a `SyntheticStub` was registered or
    /// invoked, a native was missing, or the child silently fell back to the
    /// compatible profile. Outranks `Crash` because it names the *cause* a
    /// crash or a wrong answer would otherwise only hint at.
    JdkOnlyViolation,
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

// ===========================================================================
// JDK-only census (schema 2; docs/feature-designs/jdk-only-mode.md §9)
// ===========================================================================

/// The `"compatible"` profile tag — `CompatibilityMode::as_str()`'s spelling
/// for the default policy, and the value a schema-1 ledger row migrates to.
pub const PROFILE_COMPATIBLE: &str = "compatible";

/// The `"jdk-only"` profile tag (`CompatibilityMode::as_str()`).
pub const PROFILE_JDK_ONLY: &str = "jdk-only";

/// Serde default for [`LedgerEntry::jdk_profile`]: a row written before schema
/// 2 was necessarily captured under the compatible policy.
fn default_jdk_profile() -> String {
    PROFILE_COMPATIBLE.to_string()
}

/// One `JdkOnlyViolation::kind()` bucket from a run's `--jdk-only-report`,
/// with a single human-readable example. The full violation list is *not*
/// stored: the ledger is a committed artifact, and a per-violation dump would
/// churn on every unrelated JDK detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViolationTally {
    /// Stable kind tag, e.g. `"missing-native"` (contract §3).
    pub kind: String,
    /// How many violations of this kind the run recorded.
    pub count: u64,
    /// One representative, e.g. `"java/foo/Bar.baz()V"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<String>,
}

/// What one census-collecting run measured about the policy it ran under.
///
/// Produced by [`crate::census::collect`] from the three dump files and folded
/// onto the ledger row for the `(class, jdk_profile)` it belongs to. Counter
/// maps are **sparse on purpose**: an absent key means "not measured", which is
/// a different claim from a recorded zero (see `census.rs`'s parsing
/// discipline).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrictCensus {
    /// `CompatibilityMode::as_str()` — the profile the run *reported*, which is
    /// what makes a silent fallback detectable.
    pub jdk_profile: String,
    /// JDK feature version (`25`, `21`, `8`, …).
    pub jdk_feature: Option<u32>,
    /// Loaded-class counts keyed by `ClassOrigin::as_str()`.
    pub class_origins: BTreeMap<String, u64>,
    /// Native dispatch counts keyed by `NativeKind::as_str()`.
    pub native_invocations: BTreeMap<String, u64>,
    /// Recorded policy violations, tallied by kind.
    pub violations: Vec<ViolationTally>,
}

impl StrictCensus {
    /// A census that knows only which profile was *requested* — the shape a
    /// run with no readable dumps yields.
    pub fn profile_only(jdk_profile: &str, jdk_feature: Option<u32>) -> Self {
        Self {
            jdk_profile: jdk_profile.to_string(),
            jdk_feature,
            ..Self::default()
        }
    }

    /// Classes fabricated with no real bytes (`ClassOrigin::CompatibilityStub`).
    /// `0` when the census says zero *and* when it says nothing — callers that
    /// need the distinction read `class_origins` directly.
    pub fn compatibility_classes(&self) -> u64 {
        self.class_origins
            .get("compatibility-stub")
            .copied()
            .unwrap_or(0)
    }

    /// Dispatches through a `NativeKind::SyntheticStub` slot.
    pub fn synthetic_stub_invocations(&self) -> u64 {
        self.native_invocations
            .get("synthetic-stub")
            .copied()
            .unwrap_or(0)
    }

    /// Whether this run recorded any policy violation.
    pub fn has_violations(&self) -> bool {
        self.violations.iter().any(|v| v.count > 0)
    }

    /// Total recorded violations across every kind.
    pub fn violation_count(&self) -> u64 {
        self.violations.iter().map(|v| v.count).sum()
    }

    /// Whether the run reported the strict profile.
    pub fn is_jdk_only(&self) -> bool {
        self.jdk_profile == PROFILE_JDK_ONLY
    }

    /// Record one violation, merging into an existing tally of the same kind.
    pub fn record_violation(&mut self, kind: &str, sample: Option<String>) {
        if let Some(t) = self.violations.iter_mut().find(|t| t.kind == kind) {
            t.count += 1;
            if t.sample.is_none() {
                t.sample = sample;
            }
            return;
        }
        self.violations.push(ViolationTally {
            kind: kind.to_string(),
            count: 1,
            sample,
        });
    }
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
    /// Cross-link to the `*` / `MEMORY.md` note, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_doc: Option<String>,

    // -- schema 2: the JDK-only census ------------------------------------
    /// The compatibility policy this row was captured under
    /// (`CompatibilityMode::as_str()`). Half of the row's identity: the same
    /// class diverging under `compatible` and under `jdk-only` is **two**
    /// findings, and a `known` compatible row must not excuse a strict one.
    #[serde(default = "default_jdk_profile")]
    pub jdk_profile: String,
    /// JDK feature version of the runtime image the row was captured against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jdk_feature: Option<u32>,
    /// Loaded-class counts by `ClassOrigin::as_str()`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub class_origins: BTreeMap<String, u64>,
    /// Native dispatch counts by `NativeKind::as_str()`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native_invocations: BTreeMap<String, u64>,
    /// Policy violations recorded on this row's run, tallied by kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jdk_only_violations: Vec<ViolationTally>,
}

impl LedgerEntry {
    /// The row's identity: `(class, jdk_profile)`. Lookups in
    /// [`crate::harness::gate`] and `to_merged_ledger` go through this so the
    /// two can never drift into keying on different things.
    pub fn key(&self) -> (&str, &str) {
        (self.class.as_str(), self.jdk_profile.as_str())
    }

    /// Attach a run's census to this row.
    pub fn set_census(&mut self, census: &StrictCensus) {
        self.jdk_feature = census.jdk_feature;
        self.class_origins = census.class_origins.clone();
        self.native_invocations = census.native_invocations.clone();
        self.jdk_only_violations = census.violations.clone();
    }
}

/// The gate/summary label for a row: bare `Class` under the compatible policy
/// (so historical output is unchanged), `Class@jdk-only` under a strict one.
pub fn row_label(class: &str, jdk_profile: &str) -> String {
    if jdk_profile == PROFILE_COMPATIBLE {
        class.to_string()
    } else {
        format!("{class}@{jdk_profile}")
    }
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
///
/// | Version | Change |
/// |---|---|
/// | 1 | initial: id / class / repro / classification / observations / status |
/// | 2 | adds the JDK-only census: `jdk_profile` (row key), `jdk_feature`, `class_origins`, `native_invocations`, `jdk_only_violations` |
///
/// Schema 1 files load unmodified — every new field defaults — and are
/// migrated in memory by [`Ledger::migrate`].
pub const LEDGER_SCHEMA_VERSION: u32 = 2;

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
    /// exist (a first run / bootstrap), and an error on a malformed file **or a
    /// schema newer than this build understands**.
    ///
    /// A newer schema is a hard error rather than a best-effort read: serde
    /// would happily ignore fields it does not know, and a gate that silently
    /// dropped a future row's key would judge a strict divergence against a
    /// compatible baseline. Loud beats subtly wrong.
    ///
    /// A schema-1 file loads unchanged and is migrated in memory by
    /// [`Ledger::migrate`]; nothing is written back here.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut ledger: Ledger = serde_json::from_str(&text)?;
                if ledger.schema_version > LEDGER_SCHEMA_VERSION {
                    anyhow::bail!(
                        "ledger {} is schema_version {} but this build understands at most {} — \
                         upgrade cratonvm-difftest rather than reading it partially",
                        path.display(),
                        ledger.schema_version,
                        LEDGER_SCHEMA_VERSION
                    );
                }
                ledger.migrate();
                Ok(Some(ledger))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Bring a loaded ledger up to [`LEDGER_SCHEMA_VERSION`].
    ///
    /// Schema 1 → 2: every pre-existing row was captured under the default
    /// policy, so it keys as `compatible`. (`serde(default)` already supplies
    /// that; this also repairs a row whose `jdk_profile` was hand-edited to an
    /// empty string, which would otherwise key as `Class@`.) Returns `true`
    /// when anything changed.
    pub fn migrate(&mut self) -> bool {
        let mut changed = self.schema_version != LEDGER_SCHEMA_VERSION;
        for entry in &mut self.entries {
            if entry.jdk_profile.trim().is_empty() {
                entry.jdk_profile = default_jdk_profile();
                changed = true;
            }
        }
        self.schema_version = LEDGER_SCHEMA_VERSION;
        changed
    }

    /// The `schema_version` of the file at `path`, read without committing to
    /// the rest of the shape. Lets a caller refuse a future ledger *before*
    /// falling back to "treat an unreadable file as an empty baseline", which
    /// for a newer schema would silently judge every row as new.
    pub fn schema_version_of(path: &Path) -> Option<u32> {
        let text = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        value.get("schema_version")?.as_u64().map(|v| v as u32)
    }

    /// The row for `(class, jdk_profile)`, if any. The **only** supported way
    /// to look a row up: keying by class alone would let a `known` compatible
    /// divergence excuse a strict one for the same program.
    pub fn find(&self, class: &str, jdk_profile: &str) -> Option<&LedgerEntry> {
        self.entries
            .iter()
            .find(|e| e.key() == (class, jdk_profile))
    }

    /// Serialize the ledger (pretty JSON) to `path`.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

/// Default ledger path: the committed `difftest/ledger.json`.
///
/// The design (§3.5) nominally reused `bench/differential-divergences.json` for
/// continuity with the §2.1 harness, but `bench/` is **gitignored** in this
/// repo — a committed known-divergence ledger must be tracked, so it lives in
/// the crate dir (the design's own open-question #2 alternative).
pub fn default_ledger_path() -> PathBuf {
    crate::runner::workspace_root()
        .join("difftest")
        .join("ledger.json")
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
            jdk_profile: PROFILE_COMPATIBLE.into(),
            jdk_feature: None,
            class_origins: BTreeMap::new(),
            native_invocations: BTreeMap::new(),
            jdk_only_violations: Vec::new(),
        });
        let json = serde_json::to_string_pretty(&ledger).unwrap();
        let back: Ledger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, LEDGER_SCHEMA_VERSION);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].status, LedgerStatus::Known);
        // `linked_doc: None` is skipped on the wire.
        assert!(!json.contains("linked_doc"));
        // Empty schema-2 census fields are skipped too, so a row that measured
        // nothing looks exactly like a schema-1 row plus its profile.
        assert!(!json.contains("class_origins"));
        assert!(!json.contains("native_invocations"));
        assert!(!json.contains("jdk_only_violations"));
        assert!(!json.contains("jdk_feature"));
        assert!(json.contains("\"jdk_profile\": \"compatible\""));
    }

    // -- schema 2 -----------------------------------------------------------

    /// A schema-1 file, byte-for-byte the shape committed before this wave.
    const SCHEMA_1: &str = r#"{
      "schema_version": 1,
      "host": "windows-x86_64",
      "captured_at": "epoch:1785436557",
      "jdk": "openjdk version \"25.0.3\"",
      "entries": [
        {
          "id": "div-0001",
          "class": "ExceptionId",
          "repro_path": "difftest/seeds/ExceptionId.java",
          "classification": "universal",
          "cratonvm": {"stdout":"a","stderr":"","exit_code":0,"exception":null,"timed_out":false,"wall_ms":0},
          "hotspot": {"stdout":"b","stderr":"","exit_code":0,"exception":null,"timed_out":false,"wall_ms":0},
          "status": "known",
          "first_seen": "2026-06-21"
        }
      ]
    }"#;

    #[test]
    fn schema_1_loads_unchanged_and_keys_as_compatible() {
        let mut ledger: Ledger = serde_json::from_str(SCHEMA_1).expect("schema 1 still parses");
        assert_eq!(ledger.schema_version, 1);
        let e = &ledger.entries[0];
        assert_eq!(e.jdk_profile, PROFILE_COMPATIBLE);
        assert_eq!(e.key(), ("ExceptionId", "compatible"));
        assert!(e.jdk_feature.is_none());
        assert!(e.class_origins.is_empty());
        assert!(e.jdk_only_violations.is_empty());
        // Nothing about the schema-1 payload is reinterpreted.
        assert_eq!(e.status, LedgerStatus::Known);
        assert_eq!(e.cratonvm.stdout, "a");

        assert!(ledger.migrate(), "schema 1 → 2 is a change");
        assert_eq!(ledger.schema_version, LEDGER_SCHEMA_VERSION);
        assert!(!ledger.migrate(), "migrating twice is a no-op");
    }

    #[test]
    fn a_newer_schema_is_a_hard_error_not_a_partial_read() {
        let dir = std::env::temp_dir().join(format!("difftest_ledger_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("future.json");
        std::fs::write(
            &path,
            SCHEMA_1.replace("\"schema_version\": 1", "\"schema_version\": 99"),
        )
        .expect("write");
        let err = Ledger::load(&path).expect_err("a newer schema must fail loudly");
        assert!(err.to_string().contains("99"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_is_keyed_by_class_and_profile() {
        let mut ledger = Ledger::new("h".into(), "t".into(), "25".into());
        for profile in [PROFILE_COMPATIBLE, PROFILE_JDK_ONLY] {
            ledger.entries.push(LedgerEntry {
                id: format!("div-{profile}"),
                class: "Same".into(),
                repro_path: "seeds/Same.java".into(),
                classification: Classification::Universal,
                cratonvm: Observation::empty(),
                hotspot: Observation::empty(),
                status: LedgerStatus::Known,
                first_seen: "t".into(),
                linked_doc: None,
                jdk_profile: profile.into(),
                jdk_feature: Some(25),
                class_origins: BTreeMap::new(),
                native_invocations: BTreeMap::new(),
                jdk_only_violations: Vec::new(),
            });
        }
        assert_eq!(
            ledger.find("Same", PROFILE_JDK_ONLY).map(|e| e.id.as_str()),
            Some("div-jdk-only")
        );
        assert_eq!(
            ledger
                .find("Same", PROFILE_COMPATIBLE)
                .map(|e| e.id.as_str()),
            Some("div-compatible")
        );
        assert!(ledger.find("Same", "no-such-profile").is_none());
        assert!(ledger.find("Other", PROFILE_COMPATIBLE).is_none());
    }

    #[test]
    fn row_label_marks_only_strict_rows() {
        assert_eq!(row_label("X", PROFILE_COMPATIBLE), "X");
        assert_eq!(row_label("X", PROFILE_JDK_ONLY), "X@jdk-only");
    }

    #[test]
    fn strict_census_counters_distinguish_absent_from_zero() {
        let mut c = StrictCensus::profile_only(PROFILE_JDK_ONLY, Some(25));
        assert!(c.is_jdk_only());
        // Nothing measured yet: the accessors read 0, but the maps are empty —
        // callers that need "did we measure it" read the map.
        assert_eq!(c.compatibility_classes(), 0);
        assert!(c.class_origins.is_empty());
        assert!(!c.has_violations());

        c.class_origins.insert("compatibility-stub".into(), 3);
        c.native_invocations.insert("synthetic-stub".into(), 0);
        assert_eq!(c.compatibility_classes(), 3);
        assert_eq!(c.synthetic_stub_invocations(), 0);
        assert!(c.native_invocations.contains_key("synthetic-stub"));

        c.record_violation("missing-native", Some("a/B.c()V".into()));
        c.record_violation("missing-native", Some("d/E.f()V".into()));
        c.record_violation("profile-mismatch", None);
        assert!(c.has_violations());
        assert_eq!(c.violation_count(), 3);
        assert_eq!(c.violations.len(), 2);
        let mn = c
            .violations
            .iter()
            .find(|v| v.kind == "missing-native")
            .unwrap();
        assert_eq!(mn.count, 2);
        assert_eq!(mn.sample.as_deref(), Some("a/B.c()V"), "first sample wins");
    }

    #[test]
    fn classification_jdk_only_violation_serializes_kebab_case() {
        let json = serde_json::to_string(&Classification::JdkOnlyViolation).unwrap();
        assert_eq!(json, "\"jdk-only-violation\"");
    }

    #[test]
    fn every_channel_has_a_unique_stable_label() {
        // The labels are what a divergence report prints, so they are a
        // compatibility surface: a reader (and a CI log grep) must be able to
        // tell `exception-type` from `exception-message`.
        let labels: Vec<&str> = Channel::all().iter().map(|c| c.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "duplicate channel label");
        assert_eq!(labels.len(), 8);
        // The serde spelling and the report spelling must not drift apart.
        for c in Channel::all() {
            let json = serde_json::to_string(c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.label()), "{c:?}");
        }
    }

    #[test]
    fn observation_empty_is_inert() {
        let o = Observation::empty();
        assert!(!o.timed_out);
        assert!(o.exception.is_none());
        assert_eq!(o.exit_code, None);
    }
}
