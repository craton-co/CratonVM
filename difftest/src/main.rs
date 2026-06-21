// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `difftest` — the semantic differential fuzzer CLI (design
//! `docs/feature-designs/differential-fuzzer.md`).
//!
//! Subcommands: `run` (A/B the corpus), `gen` (generate programs), `min`
//! (minimize a repro), `gate` (CI gate). In **Step 0** every subcommand is a
//! documented stub that prints its plan and exits cleanly; only the gate's
//! **exit-code contract** is already live so CI can adopt the step from day one
//! and it just passes on an empty corpus.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Args, Parser, Subcommand};

use cratonvm_difftest::ledger::{self, Ledger};
use cratonvm_difftest::runner::{self, Mode, DEFAULT_TIMEOUT};

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
    let corpus = args.corpus.clone().unwrap_or_else(default_corpus);
    let modes: Vec<&str> = args.modes.iter().map(|m| m.label()).collect();
    println!("difftest run — planned, not yet wired (Step 1).");
    println!("  corpus : {}", corpus.display());
    println!("  modes  : {}", modes.join(", "));
    println!("  timeout: {}s", args.timeout_secs);
    println!(
        "  ledger : {}",
        args.ledger
            .clone()
            .unwrap_or_else(ledger::default_ledger_path)
            .display()
    );
    println!("  -> Step 1 will A/B each program (cratonvm per-mode vs java) and write the ledger.");
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

    let program_count = count_programs(&corpus);
    if program_count == 0 {
        eprintln!(
            "difftest gate: corpus {} is empty — bootstrap, exit {} (non-fatal).",
            corpus.display(),
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    if !java_available(args.jdk.as_deref()) {
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

/// Count runnable programs (`*.java` / `*.class`) directly under `dir`.
/// Missing/unreadable directory ⇒ 0 (treated as an empty corpus).
fn count_programs(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| {
            matches!(
                e.path().extension().and_then(|s| s.to_str()),
                Some("java") | Some("class")
            )
        })
        .count()
}

/// Whether a usable `java` is reachable. Spawns `<java> -version` (fast and
/// self-terminating) and treats any spawn failure as "unavailable".
fn java_available(jdk_home: Option<&Path>) -> bool {
    let java = runner::java_executable(jdk_home);
    Command::new(java)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
