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

use crate::census;
use crate::crossmode::{self, PathDisagreement};
use crate::ledger::{
    self, Channel, Classification, Ledger, LedgerEntry, LedgerStatus, Observation, StrictCensus,
};
use crate::oracle::{self, ModeVerdict, Normalizer, Verdict};
use crate::runner::{self, DumpPaths, Mode, RunError, RunnerConfig};

/// One CratonVM mode's outcome for a single program.
#[derive(Debug, Clone)]
pub struct ModeOutcome {
    pub mode: Mode,
    pub cratonvm: Observation,
    pub verdict: Verdict,
    /// The JDK-only census this run wrote, for a
    /// [`collects_census`](Mode::collects_census) mode. `None` for the
    /// historical five, which pass no dump flags at all.
    pub census: Option<StrictCensus>,
}

impl ModeOutcome {
    fn to_mode_verdict(&self) -> ModeVerdict {
        ModeVerdict {
            mode: self.mode,
            diverged: !self.verdict.agrees(),
            timed_out: self.cratonvm.timed_out,
            crashed: self.cratonvm.exit_code.is_none() && !self.cratonvm.timed_out,
            // Only a run that actually *enforced* the strict policy can violate
            // it. A compatible run's census records what a strict run would
            // have rejected — a measurement, not a finding against this run.
            jdk_only_violation: self.mode.is_jdk_only()
                && self
                    .census
                    .as_ref()
                    .is_some_and(StrictCensus::has_violations),
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
    /// JDK feature version of the reference runtime, stamped on every ledger
    /// row this result produces.
    pub jdk_feature: Option<u32>,
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

    /// Pairs of CratonVM execution paths that disagreed **with each other** on
    /// this program (see [`crate::crossmode`]).
    ///
    /// Strictly stronger evidence than a divergence against HotSpot: no
    /// reference, no environment difference and no normalization rule can
    /// explain two of the VM's own executors printing different things for the
    /// same deterministic program. Reported alongside the gate verdict and
    /// deliberately outside its exit code — a `JitOnly` row already on the
    /// ledger *is* a path split by construction, so gating here would fail the
    /// frozen baseline on findings that are already tracked.
    pub fn path_disagreements(&self) -> Vec<PathDisagreement> {
        let runs: Vec<(Mode, &Observation)> =
            self.modes.iter().map(|m| (m.mode, &m.cratonvm)).collect();
        crossmode::disagreements(&runs)
    }

    // -- per-profile views ---------------------------------------------------
    //
    // A ledger row is keyed by `(class, jdk_profile)`, so one run of one
    // program can produce two rows: `--modes jit-on,jdk-only-jit` measures the
    // compatible policy and the strict one, and they are separate findings. The
    // accessors below are the per-profile slice of the whole-matrix ones above.

    /// The distinct JDK profiles this result covers, in mode order. Always
    /// non-empty (a result with no modes reads as `compatible`).
    pub fn profiles(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for m in &self.modes {
            let p = m.mode.jdk_profile();
            if !out.contains(&p) {
                out.push(p);
            }
        }
        if out.is_empty() {
            out.push(ledger::PROFILE_COMPATIBLE);
        }
        out
    }

    /// The mode outcomes belonging to `profile`.
    pub fn modes_in(&self, profile: &str) -> Vec<&ModeOutcome> {
        self.modes
            .iter()
            .filter(|m| m.mode.jdk_profile() == profile)
            .collect()
    }

    /// True when at least one of `profile`'s modes disagreed with HotSpot.
    pub fn diverged_in(&self, profile: &str) -> bool {
        self.modes_in(profile).iter().any(|m| !m.verdict.agrees())
    }

    /// The triage label computed from `profile`'s modes alone, so a strict
    /// row's classification is never coloured by a compatible run's verdict.
    pub fn classification_in(&self, profile: &str) -> Option<Classification> {
        let verdicts: Vec<ModeVerdict> = self
            .modes_in(profile)
            .into_iter()
            .map(ModeOutcome::to_mode_verdict)
            .collect();
        oracle::classify(&verdicts)
    }

    /// The observation to record on `profile`'s ledger row.
    pub fn representative_in(&self, profile: &str) -> Option<&ModeOutcome> {
        let modes = self.modes_in(profile);
        modes
            .iter()
            .find(|m| !m.verdict.agrees())
            .or_else(|| modes.first())
            .copied()
    }

    /// The census to record on `profile`'s ledger row: the representative's,
    /// else the first one any of `profile`'s modes produced.
    pub fn census_in(&self, profile: &str) -> Option<&StrictCensus> {
        self.representative_in(profile)
            .and_then(|m| m.census.as_ref())
            .or_else(|| {
                self.modes_in(profile)
                    .into_iter()
                    .find_map(|m| m.census.as_ref())
            })
    }
}

/// Detect a **silent policy fallback**: a child launched with `--jdk-only` that
/// reports having run under the compatible profile.
///
/// This is the one failure the harness cannot see any other way. The contract
/// (§1.1) says strict mode has *no fallback* — it must refuse to start rather
/// than quietly downgrade — so a mismatch here means either the launcher
/// ignored the flag or the VM fell back. Either way every measurement from that
/// run describes a policy nobody asked for, and reading it as a clean strict
/// run would be the most expensive kind of wrong.
///
/// Recorded as a `profile-mismatch` violation rather than an error, because
/// wave 1 is measurement: the run's numbers stay on the ledger, tagged with the
/// reason they are not to be trusted. The census keeps the profile it
/// *reported* (the violation sample carries the discrepancy); the ledger row is
/// keyed by the profile that was *requested*, so a fallback can never quietly
/// re-key a strict row onto the compatible baseline.
///
/// Returns `true` when a mismatch was recorded. A mode that did not ask for
/// strictness, and a run whose report was absent (the census then carries the
/// requested profile as its fallback), record nothing.
pub fn guard_profile(mode: Mode, census: &mut StrictCensus) -> bool {
    if !mode.is_jdk_only() || census.jdk_profile == mode.jdk_profile() {
        return false;
    }
    let observed = census.jdk_profile.clone();
    census.record_violation(
        "profile-mismatch",
        Some(format!(
            "mode {} launched with --jdk-only but the run reported profile {observed:?}",
            mode.label()
        )),
    );
    true
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

    /// Diverging `(program, jdk_profile)` rows — the number of ledger entries
    /// this run materializes, which exceeds [`diverged`](Self::diverged) when a
    /// program was run under both policies.
    pub fn diverged_rows(&self) -> usize {
        self.results
            .iter()
            .map(|r| {
                r.profiles()
                    .into_iter()
                    .filter(|p| r.diverged_in(p))
                    .count()
            })
            .sum()
    }

    /// Build a fresh `Ledger` (every divergence a `New` entry) from this run.
    /// Step 3 will diff this against the committed ledger to compute the gate
    /// verdict; here we simply materialize what this run observed.
    ///
    /// One entry per diverging `(program, jdk_profile)` pair.
    pub fn to_ledger(&self, host: String, captured_at: String, jdk: String) -> Ledger {
        let mut ledger = Ledger::new(host, captured_at.clone(), jdk);
        let mut next = 0usize;
        for r in &self.results {
            for profile in r.profiles() {
                if !r.diverged_in(profile) {
                    continue;
                }
                ledger.entries.push(Self::ledger_entry_for_result(
                    r,
                    profile,
                    format!("div-{next:04}"),
                    LedgerStatus::New,
                    captured_at.clone(),
                    None,
                ));
                next += 1;
            }
        }
        ledger
    }

    /// Build an updated ledger from this run while preserving human triage in
    /// an existing ledger. Existing entries keep their `id`, `status`,
    /// `first_seen`, and `linked_doc`; observed rows get fresh observations and
    /// classification, and brand-new divergences append as `New`.
    pub fn to_merged_ledger(
        &self,
        existing: Option<&Ledger>,
        host: String,
        captured_at: String,
        jdk: String,
    ) -> Ledger {
        let Some(existing) = existing else {
            return self.to_ledger(host, captured_at, jdk);
        };

        use std::collections::{HashMap, HashSet};

        let mut ledger = Ledger::new(host, captured_at.clone(), jdk);
        ledger.entries = existing.entries.clone();
        ledger.migrate();

        let mut used_ids: HashSet<String> = HashSet::new();
        // Keyed by `(class, jdk_profile)` — the same key `gate` looks rows up
        // by. Keying on the class alone would let a strict run overwrite the
        // compatible row's triage (and vice versa) with the other policy's
        // observation.
        let mut by_key: HashMap<(String, String), usize> = HashMap::new();
        for (idx, entry) in ledger.entries.iter().enumerate() {
            used_ids.insert(entry.id.clone());
            let (class, profile) = entry.key();
            by_key
                .entry((class.to_string(), profile.to_string()))
                .or_insert(idx);
        }

        for r in &self.results {
            for profile in r.profiles() {
                if !r.diverged_in(profile) {
                    continue;
                }
                let key = (r.program.clone(), profile.to_string());
                if let Some(&idx) = by_key.get(&key) {
                    let previous = ledger.entries[idx].clone();
                    ledger.entries[idx] = Self::ledger_entry_for_result(
                        r,
                        profile,
                        previous.id,
                        previous.status,
                        previous.first_seen,
                        previous.linked_doc,
                    );
                } else {
                    let id = next_divergence_id(&used_ids);
                    used_ids.insert(id.clone());
                    by_key.insert(key, ledger.entries.len());
                    ledger.entries.push(Self::ledger_entry_for_result(
                        r,
                        profile,
                        id,
                        LedgerStatus::New,
                        captured_at.clone(),
                        None,
                    ));
                }
            }
        }

        ledger
    }

    fn ledger_entry_for_result(
        r: &ProgramResult,
        profile: &str,
        id: String,
        status: LedgerStatus,
        first_seen: String,
        linked_doc: Option<String>,
    ) -> LedgerEntry {
        // Store timing-stripped observations so the committed ledger is stable
        // across regenerations and drift checks use plain equality.
        let cratonvm = r
            .representative_in(profile)
            .map(|m| m.cratonvm.canonical())
            .unwrap_or_else(Observation::empty);
        let mut entry = LedgerEntry {
            id,
            class: r.program.clone(),
            repro_path: r.source.to_string_lossy().into_owned(),
            classification: r
                .classification_in(profile)
                .unwrap_or(Classification::Universal),
            cratonvm,
            hotspot: r.hotspot.canonical(),
            status,
            first_seen,
            linked_doc,
            jdk_profile: profile.to_string(),
            jdk_feature: r.jdk_feature,
            class_origins: Default::default(),
            native_invocations: Default::default(),
            jdk_only_violations: Vec::new(),
        };
        if let Some(census) = r.census_in(profile) {
            entry.set_census(census);
            // The census's own feature version wins (the VM is the authority on
            // the image it booted), but must not erase what the probe knew.
            entry.jdk_feature = entry.jdk_feature.or(r.jdk_feature);
        }
        entry
    }
}

fn next_divergence_id(used_ids: &std::collections::HashSet<String>) -> String {
    let mut i = 0usize;
    loop {
        let id = format!("div-{i:04}");
        if !used_ids.contains(&id) {
            return id;
        }
        i = i
            .checked_add(1)
            .expect("divergence id counter exhausted usize");
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

    // Probe the JDK feature version **once** per corpus, not once per run: it
    // costs a subprocess and cannot change mid-corpus.
    let mut owned = config.clone();
    if owned.jdk_feature.is_none() {
        let feature = runner::jdk_feature_version(owned.jdk_home.as_deref());
        owned.jdk_feature = feature;
    }
    let config = &owned;

    let jdk = config.jdk_home.as_deref();

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
    // The census scratch dir is created lazily by `run_one`; each run cleans up
    // its own files, so this only removes the (now empty) directory.
    let _ = std::fs::remove_dir(census_dir());
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
/// [`run_corpus`] and the bytecode-mutation tier (`cratonvm-difftest mutate`).
pub fn run_one(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    modes: &[Mode],
    config: &RunnerConfig,
    source: std::path::PathBuf,
) -> Result<RunOne, RunError> {
    let jdk = config.jdk_home.as_deref();

    // HotSpot is the deterministic reference — run it once, reuse across modes.
    // Its own determinism is judged strictly: HotSpot emits no CratonVM
    // diagnostics, so there is nothing for the profile normalizer to strip and
    // relaxing here would only weaken the pre-flight.
    let strict = Normalizer::strict();
    let hotspot = hotspot_obs(classpath, main_class, jdk, config.timeout)?;

    // Determinism pre-flight (design §3.3): reject a program whose two HotSpot
    // runs disagree, or the gate flaps.
    if config.determinism_check {
        let hotspot2 = hotspot_obs(classpath, main_class, jdk, config.timeout)?;
        if !oracle::compare(&hotspot, &hotspot2, &strict).agrees() {
            return Ok(RunOne::Nondeterministic(
                "two HotSpot runs disagree under the normalizer".to_string(),
            ));
        }
    }

    let mut outcomes = Vec::with_capacity(modes.len());
    for &mode in modes {
        // Each census-collecting mode gets its own dump file names, so a
        // second mode's run can never be read as the first's measurement.
        let dumps = mode.collects_census().then(|| {
            let dir = census_dir();
            let _ = std::fs::create_dir_all(&dir);
            DumpPaths::tagged(dir, &format!("{main_class}-{}", mode.label()))
        });
        let normalizer = Normalizer::for_mode(mode);

        let mut cratonvm = cratonvm_obs(
            bin,
            classpath,
            main_class,
            mode,
            dumps.as_ref(),
            config.timeout,
        )?;
        let mut census = collect_census(mode, dumps.as_ref(), config.jdk_feature);
        let mut verdict = oracle::compare(&cratonvm, &hotspot, &normalizer);

        // Re-confirm a divergence: a transient won't reproduce. The re-run uses
        // the **same mode** — there is no fallback to a laxer policy, because a
        // strict divergence that disappears under `--real-jdk` is the finding.
        if config.reconfirm && !verdict.agrees() {
            let cratonvm2 = cratonvm_obs(
                bin,
                classpath,
                main_class,
                mode,
                dumps.as_ref(),
                config.timeout,
            )?;
            let census2 = collect_census(mode, dumps.as_ref(), config.jdk_feature);
            let verdict2 = oracle::compare(&cratonvm2, &hotspot, &normalizer);
            if verdict2.agrees() {
                cratonvm = cratonvm2;
                census = census2;
                verdict = verdict2;
            }
        }

        if let Some(dumps) = &dumps {
            dumps.cleanup();
        }

        outcomes.push(ModeOutcome {
            mode,
            cratonvm,
            verdict,
            census,
        });
    }

    Ok(RunOne::Result(ProgramResult {
        program: main_class.to_string(),
        source,
        hotspot,
        modes: outcomes,
        jdk_feature: config.jdk_feature,
    }))
}

/// Scratch directory for the census dumps, namespaced by pid so concurrent
/// difftest processes can't read each other's files. Created lazily, only by a
/// mode that actually collects a census.
fn census_dir() -> PathBuf {
    std::env::temp_dir().join(format!("difftest_census_{}", std::process::id()))
}

/// Read back one run's census and check it for a silent policy fallback.
///
/// The requested profile is the fallback when the report is missing or does not
/// say — [`census::collect`] never invents one, and [`guard_profile`] therefore
/// only fires on a report that positively claims the *wrong* profile.
fn collect_census(
    mode: Mode,
    dumps: Option<&DumpPaths>,
    jdk_feature: Option<u32>,
) -> Option<StrictCensus> {
    let dumps = dumps?;
    let mut census = census::collect(dumps, mode.jdk_profile(), jdk_feature);
    guard_profile(mode, &mut census);
    Some(census)
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
/// exception, optionally requesting the JDK-only census dumps.
fn cratonvm_obs(
    bin: &Path,
    classpath: &Path,
    main_class: &str,
    mode: Mode,
    dumps: Option<&DumpPaths>,
    timeout: std::time::Duration,
) -> Result<Observation, RunError> {
    let mut o = runner::run_cratonvm_with_dumps(bin, classpath, main_class, mode, timeout, dumps)?;
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
        // A run that agreed everywhere can still have measured a policy
        // violation; say so rather than printing a bare OK.
        let violations: u64 = r
            .modes
            .iter()
            .filter(|m| m.mode.is_jdk_only())
            .filter_map(|m| m.census.as_ref())
            .map(StrictCensus::violation_count)
            .sum();
        if !r.diverged() {
            if violations > 0 {
                let _ = writeln!(
                    s,
                    "  OK    {} ({violations} jdk-only violation(s) recorded)",
                    r.program
                );
            } else {
                let _ = writeln!(s, "  OK    {}", r.program);
            }
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
    // Path splits are printed for every program, including ones that agreed
    // with HotSpot everywhere: two executors can be wrong in the same direction
    // against the reference and still disagree with each other on a *third*
    // program, and that is precisely the case the reference cannot see.
    for r in &summary.results {
        let splits = r.path_disagreements();
        s.push_str(&crossmode::render(&r.program, &splits));
    }
    for (prog, reason) in &summary.skipped {
        let _ = writeln!(s, "  SKIP  {prog} — {reason}");
    }
    let split_programs = summary
        .results
        .iter()
        .filter(|r| !r.path_disagreements().is_empty())
        .count();
    let _ = writeln!(
        s,
        "{}/{} programs diverged from the reference, {split_programs} split across CratonVM's own \
         execution paths ({} skipped)",
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

/// The stable label for a channel. Delegates to [`Channel::label`] so the
/// summary, the gate report and the cross-path report can never disagree about
/// what a dimension is called.
fn channel_label(c: Channel) -> &'static str {
    c.label()
}

fn classification_label(c: Classification) -> &'static str {
    match c {
        Classification::JitOnly => "jit-only",
        Classification::GcMode => "gc-mode",
        Classification::Universal => "universal",
        Classification::Hang => "hang",
        Classification::Crash => "crash",
        Classification::JdkOnlyViolation => "jdk-only-violation",
    }
}

// ---------------------------------------------------------------------------
// Gate (design §3.5)
// ---------------------------------------------------------------------------

/// One recorded JDK-only policy violation, attributed to the run that produced
/// it. Reported by the gate for **every** census-collecting mode, including
/// modes that agreed with HotSpot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictViolation {
    /// The program under test.
    pub program: String,
    /// The mode label the violation was recorded under.
    pub mode: &'static str,
    /// That mode's `CompatibilityMode::as_str()` profile.
    pub jdk_profile: String,
    /// `JdkOnlyViolation::kind()`, e.g. `"missing-native"`.
    pub kind: String,
    /// How many of this kind the run recorded.
    pub count: u64,
    /// A representative, when the report supplied one.
    pub sample: Option<String>,
}

impl std::fmt::Display for StrictViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}[{}] {} ×{}",
            self.program, self.mode, self.kind, self.count
        )?;
        match &self.sample {
            Some(s) => write!(f, " ({s})"),
            None => Ok(()),
        }
    }
}

/// The gate's verdict: a run's divergences diffed against the committed
/// known-divergence ledger. Drives the §3.5 exit-code contract.
///
/// Every program list is keyed by **row**, not by class: a strict row is
/// labelled `Class@jdk-only` (see [`crate::ledger::row_label`]), a compatible
/// one is the bare class name it has always been.
#[derive(Debug, Clone, Default)]
pub struct GateReport {
    /// Diverging rows with **no** ledger entry — newly appeared (exit 1).
    pub new: Vec<String>,
    /// Diverging rows matching a `fixed` entry — a closed bug re-opened
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
    /// Every policy violation any census-collecting run recorded, including
    /// runs whose behaviour matched HotSpot exactly.
    ///
    /// **Informational — this list does not move the exit code.** Two reasons,
    /// both from the contract: wave 1 is measurement, not enforcement (§10), so
    /// a recorded violation is the distance-to-strict being counted rather than
    /// a failure; and a violation that *did* change behaviour has already shown
    /// up as a divergence on its own row and gates there. Making it gate here
    /// too would double-count it and would make the first strict run fail
    /// wholesale, which is the outcome the measurement wave exists to avoid.
    pub strict_violations: Vec<StrictViolation>,
    /// Programs where two of CratonVM's **own** execution paths disagreed, as
    /// `"Program: nojit≠ir-jit[stdout]"` (see [`crate::crossmode`]).
    ///
    /// **Informational — this list does not move the exit code**, for the same
    /// reason `strict_violations` does not: a `JitOnly` divergence already on
    /// the ledger *is* a path split by construction (`jit-on` disagrees with
    /// HotSpot while `nojit` agrees, so the two disagree with each other), and
    /// gating here would fail the frozen baseline on findings that are already
    /// tracked and triaged. It is reported because it is the strongest evidence
    /// the harness produces: it needs no reference JDK, so neither the
    /// reference nor the normalization can be blamed for it.
    pub path_splits: Vec<String>,
}

impl GateReport {
    /// The §3.5 exit code: 2 (a `fixed` bug re-diverged) > 1 (new/drift) > 0.
    /// [`strict_violations`](Self::strict_violations) is deliberately not
    /// consulted.
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

    /// Total recorded violations across every run.
    pub fn violation_count(&self) -> u64 {
        self.strict_violations.iter().map(|v| v.count).sum()
    }
}

/// Compute the gate verdict by diffing a run's `summary` against the committed
/// `ledger` (design §3.5), keyed by **`(class, jdk_profile)`**.
///
/// - a divergence with no ledger entry → **new**;
/// - a divergence matching a `fixed` entry → **regressed**;
/// - a divergence matching a `known`/`new` entry whose CratonVM observation
///   changed → **drifted**, else **known** (allowed);
/// - a row that now agrees but has a `known`/`new` entry → **resolved**.
///
/// The profile is half the key on purpose: `Foo` diverging under `--jdk-only`
/// is a *different finding* from `Foo` diverging under the compatible policy,
/// and a `known` compatible row must not silently excuse the strict one. A run
/// that uses only the historical modes sees exactly the old behaviour, since
/// every row it produces and every migrated ledger row keys as `compatible`.
pub fn gate(summary: &RunSummary, ledger: &Ledger) -> GateReport {
    let mut report = GateReport {
        nondeterministic: summary
            .nondeterministic
            .iter()
            .map(|(p, _)| p.clone())
            .collect(),
        ..Default::default()
    };

    for r in &summary.results {
        // Path splits are collected for every program, diverging or not: two
        // executors can agree with HotSpot on one program and disagree with
        // each other on another, and the reference cannot see that.
        for split in r.path_disagreements() {
            report
                .path_splits
                .push(format!("{}: {}", r.program, split.label()));
        }

        // Violations are collected from every census-collecting mode, whether
        // or not that mode diverged — an enforced refusal that happened to
        // leave behaviour unchanged is still the measurement wave's output.
        for m in &r.modes {
            let Some(census) = &m.census else { continue };
            for v in &census.violations {
                if v.count == 0 {
                    continue;
                }
                report.strict_violations.push(StrictViolation {
                    program: r.program.clone(),
                    mode: m.mode.label(),
                    jdk_profile: m.mode.jdk_profile().to_string(),
                    kind: v.kind.clone(),
                    count: v.count,
                    sample: v.sample.clone(),
                });
            }
        }

        for profile in r.profiles() {
            let label = ledger::row_label(&r.program, profile);
            let entry = ledger.find(&r.program, profile);
            if r.diverged_in(profile) {
                match entry {
                    None => report.new.push(label),
                    Some(e) if e.status == LedgerStatus::Fixed => report.regressed.push(label),
                    Some(e) => {
                        // `known` / `new`: allowed unless the CratonVM side
                        // drifted, judged under the representative mode's own
                        // normalizer.
                        let drifted = match r.representative_in(profile) {
                            Some(m) => !oracle::gated_eq(
                                &m.cratonvm,
                                &e.cratonvm,
                                &Normalizer::for_mode(m.mode),
                            ),
                            None => true,
                        };
                        if drifted {
                            report.drifted.push(label);
                        } else {
                            report.known.push(label);
                        }
                    }
                }
            } else if let Some(e) = entry {
                if matches!(e.status, LedgerStatus::Known | LedgerStatus::New) {
                    report.resolved.push(label);
                }
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
    if !report.path_splits.is_empty() {
        let _ = writeln!(
            s,
            "  CratonVM execution paths disagreed with each other (reported, not gated): {}",
            report.path_splits.len()
        );
        for split in &report.path_splits {
            let _ = writeln!(s, "      {split}");
        }
    }
    if !report.strict_violations.is_empty() {
        let _ = writeln!(
            s,
            "  jdk-only violations (measured, not gated): {} across {} run(s)",
            report.violation_count(),
            report.strict_violations.len()
        );
        for v in &report.strict_violations {
            let _ = writeln!(s, "      {v}");
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::ChannelDiff;

    use crate::ledger::{ViolationTally, PROFILE_COMPATIBLE, PROFILE_JDK_ONLY};

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
            census: None,
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
            census: None,
        }
    }

    /// A census carrying one violation of `kind`.
    fn census_with(profile: &str, kind: &str) -> StrictCensus {
        let mut c = StrictCensus::profile_only(profile, Some(25));
        c.violations.push(ViolationTally {
            kind: kind.into(),
            count: 1,
            sample: Some("java/foo/Bar.baz()V".into()),
        });
        c
    }

    fn temp_program_dir(test_name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "cratonvm_difftest_{test_name}_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).expect("create temp program dir");
        dir
    }

    #[test]
    fn discover_finds_java_and_class_sorted() {
        let dir = temp_program_dir("discover");
        std::fs::write(dir.join("B.class"), b"class bytes").expect("write class");
        std::fs::write(dir.join("A.java"), "public class A {}\n").expect("write java");
        std::fs::write(dir.join("ignore.txt"), "not runnable\n").expect("write txt");

        let progs = discover_programs(&dir);
        assert!(progs.iter().all(|p| {
            matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("java") | Some("class")
            )
        }));
        let mut sorted = progs.clone();
        sorted.sort();
        assert_eq!(progs, sorted);
        assert_eq!(progs.len(), 2);
        let names: Vec<_> = progs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["A.java".to_string(), "B.class".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
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
            jdk_feature: Some(25),
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
            jdk_feature: Some(25),
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
            jdk_feature: Some(25),
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
        // A legacy-mode run writes plain compatible rows, as it always has.
        assert_eq!(ledger.entries[0].jdk_profile, PROFILE_COMPATIBLE);
        assert_eq!(ledger.entries[0].jdk_feature, Some(25));
        assert!(ledger.entries[0].jdk_only_violations.is_empty());
        assert_eq!(ledger.schema_version, crate::ledger::LEDGER_SCHEMA_VERSION);

        let rendered = render_summary(&summary);
        assert!(rendered.contains("OK    A"));
        assert!(rendered.contains("DIFF  B [universal]"));
        assert!(rendered.contains("jit-on:stdout"));
        assert!(rendered.contains("nojit:stdout"));
    }

    // -- gate (design §3.5) -------------------------------------------------

    /// A program that diverges in both modes (the Universal shape).
    fn diverging_program(name: &str) -> ProgramResult {
        program_with(
            name,
            vec![
                diverging_outcome(Mode::JitOn),
                diverging_outcome(Mode::NoJit),
            ],
        )
    }

    fn agreeing_program(name: &str) -> ProgramResult {
        program_with(
            name,
            vec![agreeing_outcome(Mode::JitOn), agreeing_outcome(Mode::NoJit)],
        )
    }

    fn program_with(name: &str, modes: Vec<ModeOutcome>) -> ProgramResult {
        ProgramResult {
            program: name.into(),
            source: PathBuf::from(format!("seeds/{name}.java")),
            hotspot: Observation::empty(),
            modes,
            jdk_feature: Some(25),
        }
    }

    /// A ledger holding one `compatible` entry for `class`.
    fn ledger_with(class: &str, status: LedgerStatus, cratonvm_stdout: &str) -> Ledger {
        ledger_row(class, PROFILE_COMPATIBLE, status, cratonvm_stdout)
    }

    /// A ledger holding one entry for `(class, profile)` with the given status;
    /// its recorded CratonVM stdout is `cratonvm_stdout`.
    fn ledger_row(
        class: &str,
        profile: &str,
        status: LedgerStatus,
        cratonvm_stdout: &str,
    ) -> Ledger {
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
            jdk_profile: profile.into(),
            jdk_feature: Some(25),
            class_origins: Default::default(),
            native_invocations: Default::default(),
            jdk_only_violations: Vec::new(),
        });
        l
    }

    #[test]
    fn update_ledger_preserves_existing_triage_and_links() {
        let mut existing = ledger_with("X", LedgerStatus::Known, "old");
        existing.host = "old-host".into();
        existing.captured_at = "old-capture".into();
        existing.jdk = "old-jdk".into();
        existing.entries[0].first_seen = "old-first-seen".into();
        existing.entries[0].linked_doc = Some("x.md".into());
        existing.entries.push(LedgerEntry {
            id: "div-0001".into(),
            class: "Z".into(),
            repro_path: "seeds/Z.java".into(),
            classification: Classification::Universal,
            cratonvm: Observation {
                stdout: "stale".into(),
                exit_code: Some(0),
                ..Observation::empty()
            },
            hotspot: Observation::empty(),
            status: LedgerStatus::Fixed,
            first_seen: "fixed-first-seen".into(),
            linked_doc: Some("z.md".into()),
            jdk_profile: PROFILE_COMPATIBLE.into(),
            jdk_feature: Some(25),
            class_origins: Default::default(),
            native_invocations: Default::default(),
            jdk_only_violations: Vec::new(),
        });

        let summary = summary_of(vec![diverging_program("X"), diverging_program("Y")]);
        let merged = summary.to_merged_ledger(
            Some(&existing),
            "new-host".into(),
            "new-capture".into(),
            "new-jdk".into(),
        );

        assert_eq!(merged.host, "new-host");
        assert_eq!(merged.captured_at, "new-capture");
        assert_eq!(merged.jdk, "new-jdk");
        assert_eq!(merged.entries.len(), 3);

        let x = merged.entries.iter().find(|e| e.class == "X").unwrap();
        assert_eq!(x.id, "div-0000");
        assert_eq!(x.status, LedgerStatus::Known);
        assert_eq!(x.first_seen, "old-first-seen");
        assert_eq!(x.linked_doc.as_deref(), Some("x.md"));
        assert_eq!(x.cratonvm.stdout, "42");

        let z = merged.entries.iter().find(|e| e.class == "Z").unwrap();
        assert_eq!(z.status, LedgerStatus::Fixed);
        assert_eq!(z.linked_doc.as_deref(), Some("z.md"));

        let y = merged.entries.iter().find(|e| e.class == "Y").unwrap();
        assert_eq!(y.id, "div-0002");
        assert_eq!(y.status, LedgerStatus::New);
        assert_eq!(y.first_seen, "new-capture");
        assert!(y.linked_doc.is_none());
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

    // -- (class, jdk_profile) keying ----------------------------------------

    /// A program run under the two strict modes, diverging in both.
    fn strict_diverging_program(name: &str) -> ProgramResult {
        program_with(
            name,
            vec![
                diverging_outcome(Mode::JdkOnlyJit),
                diverging_outcome(Mode::JdkOnlyNoJit),
            ],
        )
    }

    #[test]
    fn a_known_compatible_row_does_not_excuse_a_strict_divergence() {
        // The whole reason the profile is half the key. Before this, a `known`
        // compatible entry for `X` would be found by class name and the strict
        // divergence would gate as "known (allowed)" — the exact silent pass
        // this feature exists to prevent.
        let summary = summary_of(vec![strict_diverging_program("X")]);
        let ledger = ledger_with("X", LedgerStatus::Known, "42");
        let report = gate(&summary, &ledger);
        assert_eq!(report.new, vec!["X@jdk-only".to_string()]);
        assert!(report.known.is_empty(), "the compatible row must not match");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn a_strict_row_matches_only_its_own_profile() {
        let summary = summary_of(vec![strict_diverging_program("X")]);
        let ledger = ledger_row("X", PROFILE_JDK_ONLY, LedgerStatus::Known, "42");
        let report = gate(&summary, &ledger);
        assert_eq!(report.known, vec!["X@jdk-only".to_string()]);
        assert!(report.new.is_empty());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn one_program_under_both_policies_produces_two_rows() {
        // `--modes nojit,jdk-only-jit`: the compatible row is known, the strict
        // one is new. Two findings, judged independently.
        let r = program_with(
            "X",
            vec![
                diverging_outcome(Mode::NoJit),
                diverging_outcome(Mode::JdkOnlyJit),
            ],
        );
        assert_eq!(r.profiles(), vec![PROFILE_COMPATIBLE, PROFILE_JDK_ONLY]);
        assert!(r.diverged_in(PROFILE_COMPATIBLE) && r.diverged_in(PROFILE_JDK_ONLY));

        let summary = summary_of(vec![r]);
        assert_eq!(summary.diverged(), 1, "one program");
        assert_eq!(summary.diverged_rows(), 2, "two ledger rows");

        let report = gate(&summary, &ledger_with("X", LedgerStatus::Known, "42"));
        assert_eq!(report.known, vec!["X".to_string()]);
        assert_eq!(report.new, vec!["X@jdk-only".to_string()]);
        assert_eq!(report.exit_code(), 1);

        let ledger = summary.to_ledger("h".into(), "t".into(), "25".into());
        assert_eq!(ledger.entries.len(), 2);
        let profiles: Vec<&str> = ledger
            .entries
            .iter()
            .map(|e| e.jdk_profile.as_str())
            .collect();
        assert_eq!(profiles, vec![PROFILE_COMPATIBLE, PROFILE_JDK_ONLY]);
        // Ids stay unique across rows of the same class.
        assert_ne!(ledger.entries[0].id, ledger.entries[1].id);
    }

    #[test]
    fn merged_ledger_updates_each_profile_row_independently() {
        let mut existing = ledger_row("X", PROFILE_JDK_ONLY, LedgerStatus::Known, "old");
        existing.entries[0].linked_doc = Some("x.md".into());
        existing.entries[0].first_seen = "old-first-seen".into();

        let summary = summary_of(vec![program_with(
            "X",
            vec![
                diverging_outcome(Mode::NoJit),
                diverging_outcome(Mode::JdkOnlyJit),
            ],
        )]);
        let merged =
            summary.to_merged_ledger(Some(&existing), "h".into(), "cap".into(), "25".into());

        assert_eq!(merged.entries.len(), 2);
        let strict = merged.find("X", PROFILE_JDK_ONLY).expect("strict row");
        // The pre-existing strict row keeps its triage and gets fresh data.
        assert_eq!(strict.status, LedgerStatus::Known);
        assert_eq!(strict.first_seen, "old-first-seen");
        assert_eq!(strict.linked_doc.as_deref(), Some("x.md"));
        assert_eq!(strict.cratonvm.stdout, "42");
        // The compatible row is brand new and did not inherit the strict row's
        // triage.
        let compat = merged
            .find("X", PROFILE_COMPATIBLE)
            .expect("compatible row");
        assert_eq!(compat.status, LedgerStatus::New);
        assert_eq!(compat.first_seen, "cap");
        assert!(compat.linked_doc.is_none());
    }

    #[test]
    fn legacy_only_runs_key_exactly_as_before() {
        // A run using only the historical modes must be judged against the
        // migrated schema-1 rows with no change in verdict.
        let mut ledger = ledger_with("X", LedgerStatus::Known, "42");
        ledger.schema_version = 1;
        ledger.migrate();
        let summary = summary_of(vec![diverging_program("X")]);
        let report = gate(&summary, &ledger);
        assert_eq!(report.known, vec!["X".to_string()], "bare class label");
        assert_eq!(report.exit_code(), 0);
    }

    // -- strict violations ---------------------------------------------------

    #[test]
    fn strict_violations_are_reported_even_when_the_run_agreed() {
        let mut outcome = agreeing_outcome(Mode::JdkOnlyJit);
        outcome.census = Some(census_with(PROFILE_JDK_ONLY, "missing-native"));
        let summary = summary_of(vec![program_with("X", vec![outcome])]);
        let report = gate(&summary, &Ledger::new("h".into(), "t".into(), "25".into()));

        assert_eq!(report.strict_violations.len(), 1);
        let v = &report.strict_violations[0];
        assert_eq!(v.program, "X");
        assert_eq!(v.mode, "jdk-only-jit");
        assert_eq!(v.jdk_profile, PROFILE_JDK_ONLY);
        assert_eq!(v.kind, "missing-native");
        assert_eq!(report.violation_count(), 1);

        // Measurement, not enforcement (contract §10): the exit code is
        // untouched and the gate still passes.
        assert_eq!(report.exit_code(), 0);
        assert!(report.is_clean());
        assert!(report.new.is_empty() && report.drifted.is_empty());
        assert!(render_gate(&report).contains("measured, not gated"));
    }

    #[test]
    fn a_violation_that_changed_behaviour_gates_as_the_divergence_it_is() {
        let mut outcome = diverging_outcome(Mode::JdkOnlyJit);
        outcome.census = Some(census_with(
            PROFILE_JDK_ONLY,
            "compatibility-class-requested",
        ));
        let summary = summary_of(vec![program_with("X", vec![outcome])]);
        let report = gate(&summary, &Ledger::new("h".into(), "t".into(), "25".into()));
        // It gates once, as a new divergence — not twice.
        assert_eq!(report.new, vec!["X@jdk-only".to_string()]);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.strict_violations.len(), 1);
    }

    #[test]
    fn compatible_mode_violations_are_measured_but_not_a_violation_verdict() {
        // `real-compatible-*` records what a strict run *would* have refused.
        // It must be reported, but it must not classify the run as violating.
        let mut outcome = agreeing_outcome(Mode::RealCompatibleJit);
        outcome.census = Some(census_with(
            PROFILE_COMPATIBLE,
            "compatibility-class-requested",
        ));
        let r = program_with("X", vec![outcome]);
        assert_eq!(
            r.classification(),
            None,
            "a would-be refusal is not a finding"
        );

        let report = gate(
            &summary_of(vec![r]),
            &Ledger::new("h".into(), "t".into(), "25".into()),
        );
        assert_eq!(report.strict_violations.len(), 1, "still measured");
        assert_eq!(report.strict_violations[0].jdk_profile, PROFILE_COMPATIBLE);
        assert!(report.is_clean());
    }

    #[test]
    fn a_strict_violation_classifies_and_lands_on_the_ledger_row() {
        let mut outcome = diverging_outcome(Mode::JdkOnlyJit);
        let mut census = census_with(PROFILE_JDK_ONLY, "missing-native");
        census.class_origins.insert("boot-image".into(), 312);
        census.native_invocations.insert("synthetic-stub".into(), 0);
        outcome.census = Some(census);
        let summary = summary_of(vec![program_with("X", vec![outcome])]);

        let ledger = summary.to_ledger("h".into(), "t".into(), "25".into());
        let e = ledger.find("X", PROFILE_JDK_ONLY).expect("strict row");
        assert_eq!(e.classification, Classification::JdkOnlyViolation);
        assert_eq!(e.jdk_feature, Some(25));
        assert_eq!(e.class_origins["boot-image"], 312);
        assert_eq!(e.native_invocations["synthetic-stub"], 0);
        assert_eq!(e.jdk_only_violations.len(), 1);
        assert_eq!(e.jdk_only_violations[0].kind, "missing-native");
    }

    // -- guard_profile -------------------------------------------------------

    #[test]
    fn guard_profile_flags_a_silent_fallback() {
        // A `--jdk-only` child that reports `compatible` fell back; the whole
        // run described a policy nobody asked for.
        let mut census = StrictCensus::profile_only(PROFILE_COMPATIBLE, Some(25));
        assert!(guard_profile(Mode::JdkOnlyJit, &mut census));
        assert!(census.has_violations());
        assert_eq!(census.violations[0].kind, "profile-mismatch");
        assert!(census.violations[0]
            .sample
            .as_deref()
            .unwrap()
            .contains("compatible"));
        // The census keeps what the run *reported*; the row is keyed by what
        // was requested.
        assert_eq!(census.jdk_profile, PROFILE_COMPATIBLE);
    }

    #[test]
    fn guard_profile_is_silent_when_the_profile_matches() {
        let mut census = StrictCensus::profile_only(PROFILE_JDK_ONLY, Some(25));
        assert!(!guard_profile(Mode::JdkOnlyJit, &mut census));
        assert!(!census.has_violations());
    }

    #[test]
    fn guard_profile_ignores_modes_that_did_not_ask_for_strictness() {
        // `real-compatible-*` and the historical five report `compatible`
        // because that is what they asked for.
        for mode in [Mode::RealCompatibleJit, Mode::JitOn, Mode::NoJit] {
            let mut census = StrictCensus::profile_only(PROFILE_COMPATIBLE, Some(25));
            assert!(!guard_profile(mode, &mut census), "{}", mode.label());
            assert!(!census.has_violations());
        }
    }

    #[test]
    fn a_fallback_makes_the_run_a_violation_verdict() {
        // End to end: the mismatch guard is what turns an otherwise-clean
        // strict run into a reported finding.
        let mut outcome = agreeing_outcome(Mode::JdkOnlyJit);
        let mut census = StrictCensus::profile_only(PROFILE_COMPATIBLE, Some(25));
        guard_profile(Mode::JdkOnlyJit, &mut census);
        outcome.census = Some(census);
        let r = program_with("X", vec![outcome]);
        assert_eq!(
            r.classification_in(PROFILE_JDK_ONLY),
            Some(Classification::JdkOnlyViolation)
        );
        let rendered = render_summary(&summary_of(vec![r]));
        assert!(
            rendered.contains("jdk-only violation(s) recorded"),
            "{rendered}"
        );
    }

    // -- census-free modes are untouched -------------------------------------

    // -- cross-path (reference-free) findings --------------------------------

    #[test]
    fn two_execution_paths_disagreeing_is_reported_but_does_not_gate() {
        // `nojit` printed "42" and `ir-jit` printed nothing: one of the VM's own
        // executors is wrong, and no reference JDK is needed to know it. It is
        // surfaced on the gate report and deliberately left out of the exit
        // code — a `JitOnly` row already on the ledger is a path split by
        // construction, so gating here would fail the frozen baseline.
        let r = program_with(
            "X",
            vec![
                diverging_outcome(Mode::NoJit),
                agreeing_outcome(Mode::IrJit),
            ],
        );
        let splits = r.path_disagreements();
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].label(), "nojit≠ir-jit[stdout]");

        // The ledger says this program is a known divergence, so the gate is
        // clean — and the split is still reported.
        let report = gate(
            &summary_of(vec![r]),
            &ledger_with("X", LedgerStatus::Known, "42"),
        );
        assert_eq!(
            report.path_splits,
            vec!["X: nojit≠ir-jit[stdout]".to_string()]
        );
        assert_eq!(report.exit_code(), 0, "a path split must not move the gate");
        assert!(report.is_clean());
        assert!(render_gate(&report).contains("reported, not gated"));
    }

    #[test]
    fn paths_that_agree_produce_no_split_even_when_all_diverge_from_hotspot() {
        // The `Universal` shape: every executor is wrong in the same way. There
        // is no path split, because the paths agree with each other — which is
        // itself the useful signal (the bug is shared, not JIT-specific).
        let r = diverging_program("X");
        assert!(r.path_disagreements().is_empty());
        let report = gate(
            &summary_of(vec![r]),
            &Ledger::new("h".into(), "t".into(), "25".into()),
        );
        assert!(report.path_splits.is_empty());
        assert!(!render_gate(&report).contains("reported, not gated"));
    }

    #[test]
    fn a_cross_policy_pair_is_never_reported_as_a_path_split() {
        // `--jdk-only` refusing something the compatible run allowed is a policy
        // finding with its own ledger row, not two executors disagreeing.
        let r = program_with(
            "X",
            vec![
                diverging_outcome(Mode::JitOn),
                agreeing_outcome(Mode::JdkOnlyJit),
            ],
        );
        assert!(
            r.path_disagreements().is_empty(),
            "the policy axis must not masquerade as an execution-path split"
        );
    }

    #[test]
    fn the_summary_names_the_dimension_that_moved() {
        // The whole point of splitting the exception channel: a report says
        // `exception-message`, not `exception`.
        let mut outcome = agreeing_outcome(Mode::JitOn);
        outcome.verdict = Verdict::Diverge(vec![ChannelDiff {
            channel: Channel::ExceptionMessage,
            cratonvm: "no detail".into(),
            hotspot: "Index 9 out of bounds for length 3".into(),
        }]);
        let rendered = render_summary(&summary_of(vec![program_with("X", vec![outcome])]));
        assert!(rendered.contains("jit-on:exception-message"), "{rendered}");
    }

    #[test]
    fn legacy_modes_carry_no_census_and_no_violation() {
        let r = diverging_program("X");
        for m in &r.modes {
            assert!(
                m.census.is_none(),
                "{} must not collect a census",
                m.mode.label()
            );
        }
        assert_eq!(r.profiles(), vec![PROFILE_COMPATIBLE]);
        assert_eq!(r.classification(), Some(Classification::Universal));
        let report = gate(
            &summary_of(vec![r]),
            &Ledger::new("h".into(), "t".into(), "25".into()),
        );
        assert!(report.strict_violations.is_empty());
        assert_eq!(report.new, vec!["X".to_string()]);
    }
}
