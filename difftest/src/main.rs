// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `difftest` — the semantic differential fuzzer CLI (design
//! `docs/feature-designs/differential-fuzzer.md`).
//!
//! Subcommands: `run` (A/B the corpus), `gen` (generate programs), `min`
//! (minimize a repro), `gate` (CI gate). As of **Step 1**, `run` is wired —
//! it compiles each seed once, runs it on CratonVM and HotSpot, diffs the four
//! channels, and (with `--update-ledger`) writes the ledger. `gen` / `min` are
//! still stubs; `gate` honors the §3.5 exit-code contract (the committed-ledger
//! new-vs-known verdict lands in Step 3).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use cratonvm_difftest::harness;
use cratonvm_difftest::ledger::{self, Ledger};
use cratonvm_difftest::runner::{self, Mode, RunError, RunnerConfig, DEFAULT_TIMEOUT};

/// Gate / run exit-code contract (design §3.5). These describe the *divergence
/// verdict* of a completed run; structural CLI errors are clap's domain.
mod exit {
    /// No **new** divergences (known ledger entries are allowed).
    pub const OK: u8 = 0;
    /// A new divergence appeared, or a `known` entry's CratonVM side changed.
    pub const NEW_DIVERGENCE: u8 = 1;
    /// A `fixed` entry diverged again — a true regression of a closed bug.
    pub const FIXED_REGRESSED: u8 = 2;
    /// Bootstrap / non-fatal: `java` unavailable or the corpus is empty.
    pub const BOOTSTRAP: u8 = 3;
}

#[derive(Parser)]
#[command(
    name = "difftest",
    version,
    about = "Semantic differential fuzzer: diff Java behavior on CratonVM vs a real JDK",
    long_about = "Runs Java programs on both CratonVM and a real JDK and diffs observable \
                  behavior (stdout, stderr, exception type+message, exit code).\n\n\
                  Step 0 scaffold: subcommands print their plan; `gate` already honors the \
                  exit-code contract (0 ok / 1 new divergence / 2 fixed-regressed / 3 bootstrap)."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the corpus on both VMs and report divergences (Step 1).
    Run(RunArgs),
    /// Generate corpus programs biased toward the bug history (Step 4).
    Gen(GenArgs),
    /// Minimize a confirmed divergence to a small repro (Step 6).
    Min(MinArgs),
    /// CI gate: exit non-zero on a new or regressed divergence (Step 3).
    Gate(RunArgs),
}

/// Flags shared by `run` and `gate` (the A/B-executing subcommands).
#[derive(Args, Clone)]
struct RunArgs {
    /// Directory of programs to run (default: `difftest/corpus`).
    #[arg(long)]
    corpus: Option<PathBuf>,

    /// Comma-separated CratonVM modes to fan out across.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "jit-on,nojit",
        value_parser = parse_one_mode
    )]
    modes: Vec<Mode>,

    /// Hard per-run timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
    timeout_secs: u64,

    /// Explicit JDK home (else `DIFFTEST_JAVA_HOME`, else PATH).
    #[arg(long)]
    jdk: Option<PathBuf>,

    /// Allow a non-pinned JDK build for the HotSpot oracle.
    #[arg(long)]
    allow_jdk_downgrade: bool,

    /// Divergence ledger path (default: the committed `difftest/ledger.json`).
    #[arg(long)]
    ledger: Option<PathBuf>,

    /// Write newly-confirmed divergences back into the ledger.
    #[arg(long)]
    update_ledger: bool,

    /// Determinism pre-flight: run each program twice on HotSpot and skip any
    /// whose two runs disagree (`gate` forces this on).
    #[arg(long)]
    check_determinism: bool,

    /// Re-run a diverging mode to filter transient (non-reproducing)
    /// divergences (`gate` forces this on).
    #[arg(long)]
    reconfirm: bool,
}

#[derive(Args)]
struct GenArgs {
    /// Number of programs to generate.
    #[arg(long, default_value_t = 100)]
    count: usize,

    /// Where to write generated programs (default: `difftest/corpus`).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
struct MinArgs {
    /// The confirmed-divergent program to shrink.
    program: PathBuf,
}

/// clap per-value parser for `--modes`.
fn parse_one_mode(s: &str) -> Result<Mode, String> {
    Mode::from_label(s.trim()).ok_or_else(|| format!("unknown mode {s:?}"))
}

/// Default corpus directory: this crate's `corpus/`.
fn default_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

impl RunArgs {
    /// Resolve the flags into a [`RunnerConfig`], filling defaults.
    fn to_runner_config(&self) -> RunnerConfig {
        RunnerConfig {
            corpus: self.corpus.clone().unwrap_or_else(default_corpus),
            modes: if self.modes.is_empty() {
                vec![Mode::JitOn]
            } else {
                self.modes.clone()
            },
            timeout: Duration::from_secs(self.timeout_secs),
            jdk_home: self.jdk.clone(),
            allow_jdk_downgrade: self.allow_jdk_downgrade,
            ledger: self
                .ledger
                .clone()
                .unwrap_or_else(ledger::default_ledger_path),
            update_ledger: self.update_ledger,
            determinism_check: self.check_determinism,
            reconfirm: self.reconfirm,
        }
    }
}

/// Host tag (`os-arch`) for the ledger header.
fn host_tag() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Unix-epoch seconds as a string, for the ledger `captured_at` (Step 3 aligns
/// this with `capture-hotspot-baseline`'s ISO-8601 convention).
fn captured_at() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run(args) => cmd_run(&args),
        Cmd::Gen(args) => cmd_gen(&args),
        Cmd::Min(args) => cmd_min(&args),
        Cmd::Gate(args) => cmd_gate(&args),
    }
}

// ---------------------------------------------------------------------------
// run — Step 1
// ---------------------------------------------------------------------------

fn cmd_run(args: &RunArgs) -> ExitCode {
    let config = args.to_runner_config();
    let modes: Vec<&str> = config.modes.iter().map(|m| m.label()).collect();
    println!(
        "difftest run — corpus {} | cratonvm[{}] vs java | timeout {}s",
        config.corpus.display(),
        modes.join(","),
        config.timeout.as_secs()
    );

    let summary = match harness::run_corpus(&config) {
        Ok(s) => s,
        Err(
            e @ (RunError::CratonvmBinaryMissing | RunError::JavaMissing | RunError::EmptyCorpus),
        ) => {
            eprintln!(
                "difftest run: {e} — bootstrap, exit {} (non-fatal).",
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
        Err(e) => {
            eprintln!("difftest run: {e}");
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };

    print!("{}", harness::render_summary(&summary));

    if config.update_ledger {
        let jdk =
            runner::jdk_version(config.jdk_home.as_deref()).unwrap_or_else(|| "unknown".into());
        let ledger = summary.to_ledger(host_tag(), captured_at(), jdk);
        match ledger.save(&config.ledger) {
            Ok(()) => println!("wrote ledger: {}", config.ledger.display()),
            Err(e) => eprintln!(
                "difftest run: could not write ledger {}: {e}",
                config.ledger.display()
            ),
        }
    } else if summary.diverged() > 0 {
        println!(
            "(re-run with --update-ledger to record the {} divergence(s) in {})",
            summary.diverged(),
            config.ledger.display()
        );
    }

    // `run` is a report, not a gate: a successful run exits 0 regardless of
    // divergences (the `gate` subcommand is what fails CI on them).
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// gen — Step 4
// ---------------------------------------------------------------------------

fn cmd_gen(args: &GenArgs) -> ExitCode {
    let out = args.out.clone().unwrap_or_else(default_corpus);
    println!("difftest gen — planned, not yet wired (Step 4).");
    println!("  count: {}", args.count);
    println!("  out  : {}", out.display());
    println!(
        "  -> Step 4 will emit type-directed self-printing Java weighted toward the bug history."
    );
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// min — Step 6
// ---------------------------------------------------------------------------

fn cmd_min(args: &MinArgs) -> ExitCode {
    println!("difftest min — planned, not yet wired (Step 6).");
    println!("  program: {}", args.program.display());
    println!("  -> Step 6 will ddmin-shrink the repro and commit it under difftest/regression/.");
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// gate — Step 3 (exit-code contract live now so CI can adopt it)
// ---------------------------------------------------------------------------

fn cmd_gate(args: &RunArgs) -> ExitCode {
    let mut config = args.to_runner_config();
    // The gate MUST be sound: the determinism pre-flight and divergence
    // re-confirmation are forced on regardless of CLI flags.
    config.determinism_check = true;
    config.reconfirm = true;

    // Prerequisites → bootstrap (non-fatal exit 3), so CI can adopt the step
    // before a JDK / the cratonvm binary is provisioned.
    if harness::discover_programs(&config.corpus).is_empty() {
        eprintln!(
            "difftest gate: corpus {} is empty — bootstrap, exit {} (non-fatal).",
            config.corpus.display(),
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }
    if runner::cratonvm_binary().is_none() || !runner::java_available(config.jdk_home.as_deref()) {
        eprintln!(
            "difftest gate: cratonvm binary or java not found (build cratonvm / set CRATONVM_BIN, \
             --jdk / DIFFTEST_JAVA_HOME / PATH) — bootstrap, exit {} (non-fatal).",
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    // The committed known-divergence ledger. A missing file or a legacy-schema
    // file is treated as an empty baseline (every divergence is then "new").
    let ledger = match Ledger::load(&config.ledger) {
        Ok(Some(l)) => l,
        Ok(None) => Ledger::new(host_tag(), captured_at(), "unknown".into()),
        Err(e) => {
            eprintln!(
                "difftest gate: note — {} is not in the promoted ledger schema ({e}); \
                 treating as an empty baseline.",
                config.ledger.display()
            );
            Ledger::new(host_tag(), captured_at(), "unknown".into())
        }
    };

    let summary = match harness::run_corpus(&config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "difftest gate: {e} — bootstrap, exit {} (non-fatal).",
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };

    let report = harness::gate(&summary, &ledger);
    print!("{}", harness::render_gate(&report));
    let code = report.exit_code();
    let verdict = match code {
        exit::OK => "clean — no new or regressed divergence",
        exit::NEW_DIVERGENCE => "FAIL — a new or drifted divergence appeared",
        exit::FIXED_REGRESSED => "FAIL — a fixed divergence re-opened",
        _ => "bootstrap",
    };
    println!("difftest gate: {verdict}. exit {code}.");
    ExitCode::from(code)
}
