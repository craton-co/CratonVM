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

    /// Divergence ledger path (default: `bench/differential-divergences.json`).
    #[arg(long)]
    ledger: Option<PathBuf>,

    /// Write newly-confirmed divergences back into the ledger.
    #[arg(long)]
    update_ledger: bool,
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
    let mode = config.modes.first().copied().unwrap_or(Mode::JitOn);
    println!(
        "difftest run — corpus {} | cratonvm[{}] vs java | timeout {}s",
        config.corpus.display(),
        mode.label(),
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
    let corpus = args.corpus.clone().unwrap_or_else(default_corpus);
    let ledger_path = args
        .ledger
        .clone()
        .unwrap_or_else(ledger::default_ledger_path);

    let program_count = harness::discover_programs(&corpus).len();
    if program_count == 0 {
        eprintln!(
            "difftest gate: corpus {} is empty — bootstrap, exit {} (non-fatal).",
            corpus.display(),
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    if !runner::java_available(args.jdk.as_deref()) {
        eprintln!(
            "difftest gate: `java` not found (set --jdk / DIFFTEST_JAVA_HOME / PATH) — \
             bootstrap, exit {} (non-fatal).",
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    // Load the ledger purely to report known-divergence counts; the actual
    // comparison that could return exit 1 / 2 lands in Step 3. A file that does
    // not parse as the promoted `Ledger` schema is *not* fatal here: the
    // default path still holds the legacy §2.1 `DivergenceReport` shape until
    // Step 3 migrates it, so we treat any non-promoted/unreadable ledger as
    // "0 known" and proceed rather than red-flagging the gate.
    let known = match Ledger::load(&ledger_path) {
        Ok(Some(l)) => l.entries.len(),
        Ok(None) => 0,
        Err(e) => {
            eprintln!(
                "difftest gate: note — {} is not yet in the promoted ledger schema \
                 ({e}); treating as 0 known (migration lands in Step 3).",
                ledger_path.display()
            );
            0
        }
    };

    println!(
        "difftest gate: {program_count} program(s) staged, ledger has {known} known \
         divergence(s); comparison not wired yet (Step 3) — no new divergences. exit {}.",
        exit::OK
    );
    ExitCode::from(exit::OK)
}
