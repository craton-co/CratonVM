// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `cratonvm-difftest` — the semantic differential fuzzer CLI (design
//! `docs/feature-designs/differential-fuzzer.md`).
//!
//! Subcommands:
//! * `run` — A/B the corpus across the CratonVM mode matrix vs HotSpot, diff
//!   the four channels, auto-classify, optionally write the ledger.
//! * `gen` — emit a seeded, reproducible, type-directed corpus (Step 4).
//! * `mutate` — perturb a compiled seed's constant pool and A/B each mutant
//!   (Step 5).
//! * `min` — ddmin-shrink a confirmed divergence to a minimal repro (Step 6).
//! * `gate` — diff a fresh run against the committed ledger; exit per the §3.5
//!   contract (0 ok / 1 new-or-drift / 2 fixed-reopened / 3 bootstrap).
//! * `gen-opcodes` — emit the generated opcode corpus: one deterministic,
//!   checksum-declaring Java program per JVMS opcode (C2 review P0).
//! * `matrix` — generate the opcode / execution-path coverage matrix from the
//!   corpus's own class files (C2 review P0).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use cratonvm_difftest::generate::{self, Rng, TargetFamily};
use cratonvm_difftest::harness::{self, RunOne};
use cratonvm_difftest::ledger::{self, Ledger};
use cratonvm_difftest::matrix::{self, CoverageMatrix, GeneratorIndex};
use cratonvm_difftest::minimize;
use cratonvm_difftest::mutate;
use cratonvm_difftest::opcorpus::{self, OpcodeCorpusConfig};
use cratonvm_difftest::oracle::Normalizer;
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
    name = "cratonvm-difftest",
    version,
    about = "Semantic differential fuzzer: diff Java behavior on CratonVM vs a real JDK",
    long_about = "Runs Java programs on both CratonVM and a real JDK and diffs observable \
                  behavior (stdout, stderr, exception type+message, exit code).\n\n\
                  `run` and `gate` execute the configured corpus; `gen`, `mutate`, and `min` \
                  provide the corpus-growth and reproducer workflows. `gate` honors the \
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
    /// Mutate a compiled seed's constants and run each mutant differentially (Step 5).
    Mutate(MutateArgs),
    /// Minimize a confirmed divergence to a small repro (Step 6).
    Min(MinArgs),
    /// CI gate: exit non-zero on a new or regressed divergence (Step 3).
    Gate(RunArgs),
    /// Emit one Java program per JVMS opcode (C2 review P0).
    GenOpcodes(GenOpcodesArgs),
    /// Generate the opcode / execution-path coverage matrix (C2 review P0).
    Matrix(MatrixArgs),
}

/// Flags shared by `run` and `gate` (the A/B-executing subcommands).
#[derive(Args, Clone)]
struct RunArgs {
    /// Directory of programs to run (default: `difftest/corpus`).
    #[arg(long)]
    corpus: Option<PathBuf>,

    /// Comma-separated CratonVM modes to fan out across: `jit-on`, `nojit`,
    /// `interp-decoded`, `no-intrinsics`, `moving-gc`, `low-jit-threshold`,
    /// `jdk-only-jit`, `jdk-only-nojit`, `real-compatible-jit`,
    /// `real-compatible-nojit`.
    ///
    /// ## Why `interp-decoded` is in the default (ARCH-2026-08-04 A4b)
    ///
    /// The interpreter has **two** implementations of the same opcodes: a
    /// raw-byte fast path carrying 122 opcode arms, and a complete decoded
    /// path. `--noverify` switches which one runs for all of them at once
    /// (`interpreter.rs`: `let use_fast_path = !shared.config.skip_verification`).
    /// Every fix to those 122 opcodes has to land twice, and a divergence
    /// between them is a bug that appears under one flag only.
    ///
    /// `Mode::InterpDecoded` is the axis covering the second implementation —
    /// it is the only mode that passes `--noverify`, and `matrix.rs` maps it to
    /// `PathAxis::InterpreterDecoded`, which nothing else claims. It was
    /// defined, labelled, listed in `Mode::all()` *and* in `execution_paths()`,
    /// and **left out of this default** — so the CI gate
    /// (`cargo run ... -- gate --corpus difftest/seeds`, which passes no
    /// `--modes`) ran `jit-on,nojit` and never exercised the decoded path.
    ///
    /// `nojit` is the *fast* path with the JIT out of the picture, not the
    /// decoded one (`matrix.rs`: `Mode::NoJit => &[InterpreterFast, Exception]`
    /// against `Mode::InterpDecoded => &[InterpreterDecoded, Exception]`). The
    /// two are easy to confuse, and confusing them is what left the gap.
    ///
    /// This repository has been here before: it has already lost 1,522 tests to
    /// a configuration nothing compiled. An axis that exists, is documented, and
    /// is never run is the same failure in a different costume.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "jit-on,nojit,interp-decoded",
        value_parser = parse_one_mode
    )]
    modes: Vec<Mode>,

    /// Hard per-run timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
    timeout_secs: u64,

    /// Explicit JDK home (else `DIFFTEST_JAVA_HOME`, else PATH).
    #[arg(long)]
    jdk: Option<PathBuf>,

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
    /// Number of programs to generate (per family).
    #[arg(long, default_value_t = 20)]
    count: usize,

    /// Target family: `arith` | `concat` | `exceptions` | `all`.
    #[arg(long, default_value = "arith")]
    family: String,

    /// PRNG seed — the same seed reproduces the exact corpus.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Where to write generated `.java` programs (default: `difftest/corpus`).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
struct MutateArgs {
    /// Seed program to mutate: a `.java` (compiled here) or a `.class`.
    program: PathBuf,

    /// Number of mutants to generate and run.
    #[arg(long, default_value_t = 50)]
    count: usize,

    /// PRNG seed — the same seed reproduces the exact mutant sequence.
    #[arg(long, default_value_t = 1)]
    seed: u64,

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
}

#[derive(Args)]
struct MinArgs {
    /// The confirmed-divergent `.java` program to shrink.
    program: PathBuf,

    /// Comma-separated CratonVM modes (the divergence must reproduce in one).
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "jit-on,nojit",
        value_parser = parse_one_mode
    )]
    modes: Vec<Mode>,

    /// Where to write the minimized repro (default:
    /// `difftest/regression/<Class>.java`).
    #[arg(long)]
    out: Option<PathBuf>,

    /// Hard per-run timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
    timeout_secs: u64,

    /// Explicit JDK home (else `DIFFTEST_JAVA_HOME`, else PATH).
    #[arg(long)]
    jdk: Option<PathBuf>,
}

#[derive(Args)]
struct GenOpcodesArgs {
    /// Where to write the generated `.java` programs (default:
    /// `difftest/corpus-opcodes`). The directory is **not** committed: the
    /// corpus is a build product, regenerated byte-identically from this
    /// binary, so it can never drift from the generator that describes it.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Trip count of the loop wrapped around every program's focus code.
    #[arg(long, default_value_t = OpcodeCorpusConfig::default().loop_iterations)]
    loop_iterations: u32,

    /// Skip the two programs that are large by construction (`wide` needs more
    /// than 255 locals, `goto_w` a method body over 32 KiB). They are reported
    /// as skipped-with-a-reason, never as covered.
    #[arg(long)]
    no_large: bool,

    /// Print the plan (one line per opcode) instead of writing files.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct MatrixArgs {
    /// Directory of programs to scan (default: `difftest/corpus`).
    #[arg(long)]
    corpus: Option<PathBuf>,

    /// The mode list the matrix's axis columns are computed for. Defaults to
    /// the full execution-path contract, so a gap in the output is a gap in the
    /// *corpus* rather than in the run configuration.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "nojit,interp-decoded,direct-emit,ir-jit,osr-eager,no-osr,forced-deopt",
        value_parser = parse_one_mode
    )]
    modes: Vec<Mode>,

    /// Where to write the machine-readable matrix (default:
    /// `difftest/coverage-matrix.json`).
    #[arg(long)]
    out: Option<PathBuf>,

    /// List every uncovered `(opcode, axis)` cell, with the fix it needs, and
    /// every opcode that is unreachable by construction, with the reason.
    #[arg(long)]
    show_gaps: bool,

    /// Ignore what the opcode generator declares. Without this the report says
    /// "a generator exists — regenerate the corpus" for an opcode the corpus
    /// lacks, which is a different job from "write a recipe"; with it every
    /// absent opcode reports as `no-generator`.
    #[arg(long)]
    no_generators: bool,

    /// Explicit JDK home (else `DIFFTEST_JAVA_HOME`, else PATH) — used only to
    /// find `javac` for `.java` seeds.
    #[arg(long)]
    jdk: Option<PathBuf>,

    /// Hard per-compile timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT.as_secs())]
    timeout_secs: u64,
}

/// clap per-value parser for `--modes`.
fn parse_one_mode(s: &str) -> Result<Mode, String> {
    Mode::from_label(s.trim()).ok_or_else(|| format!("unknown mode {s:?}"))
}

/// Default corpus directory: this crate's `corpus/`.
fn default_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// A corpus file's name, for a report line.
fn file_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
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
            ledger: self
                .ledger
                .clone()
                .unwrap_or_else(ledger::default_ledger_path),
            update_ledger: self.update_ledger,
            determinism_check: self.check_determinism,
            reconfirm: self.reconfirm,
            // Probed once per corpus by the harness.
            jdk_feature: None,
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
        Cmd::Mutate(args) => cmd_mutate(&args),
        Cmd::Min(args) => cmd_min(&args),
        Cmd::Gate(args) => cmd_gate(&args),
        Cmd::GenOpcodes(args) => cmd_gen_opcodes(&args),
        Cmd::Matrix(args) => cmd_matrix(&args),
    }
}

// ---------------------------------------------------------------------------
// gen-opcodes — C2 review P0 (the corpus the matrix needs to mean anything)
// ---------------------------------------------------------------------------

/// Default output directory for the generated opcode corpus.
///
/// A sibling of `corpus/`, not a child, because `harness::discover_programs` is
/// non-recursive and mixing ~200 generated programs into the live corpus would
/// change what every other subcommand runs by default.
fn default_opcode_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus-opcodes")
}

fn cmd_gen_opcodes(args: &GenOpcodesArgs) -> ExitCode {
    let config = OpcodeCorpusConfig {
        loop_iterations: args.loop_iterations,
        include_large: !args.no_large,
        ..OpcodeCorpusConfig::default()
    };
    let programs = opcorpus::generate_all(&config);

    if args.dry_run {
        for p in &programs {
            match p.support.reason() {
                None => println!(
                    "  {:#04x} {:<15} {} [{}]",
                    p.opcode,
                    p.mnemonic,
                    p.class_name,
                    p.forms.join(" ")
                ),
                Some(reason) => println!("  {:#04x} {:<15} SKIP — {reason}", p.opcode, p.mnemonic),
            }
        }
        return ExitCode::from(exit::OK);
    }

    let out = args.out.clone().unwrap_or_else(default_opcode_corpus);
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!(
            "cratonvm-difftest gen-opcodes: cannot create {}: {e}",
            out.display()
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    let (mut written, mut failed) = (0usize, 0usize);
    let mut not_generated: Vec<(&str, &str)> = Vec::new();
    for p in &programs {
        let Some(source) = &p.source else {
            not_generated.push((
                p.mnemonic,
                p.support.reason().unwrap_or("no reason recorded"),
            ));
            continue;
        };
        let path = out.join(format!("{}.java", p.class_name));
        match std::fs::write(&path, source) {
            Ok(()) => written += 1,
            Err(e) => {
                failed += 1;
                eprintln!(
                    "cratonvm-difftest gen-opcodes: cannot write {}: {e}",
                    path.display()
                );
            }
        }
    }

    println!(
        "cratonvm-difftest gen-opcodes — wrote {written} of 202 opcode program(s) to {}",
        out.display()
    );
    // Never silent: an opcode with no program is a documented fact, not an
    // omission, and the reason is what tells a reader whether to act on it.
    for (mnemonic, reason) in &not_generated {
        println!("  NOT GENERATED {mnemonic} — {reason}");
    }
    if failed > 0 {
        eprintln!("  ({failed} program(s) could not be written)");
        return ExitCode::from(exit::BOOTSTRAP);
    }
    println!(
        "  next: cratonvm-difftest matrix --corpus {} --show-gaps",
        out.display()
    );
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// run — Step 1
// ---------------------------------------------------------------------------

fn cmd_run(args: &RunArgs) -> ExitCode {
    let config = args.to_runner_config();
    let modes: Vec<&str> = config.modes.iter().map(|m| m.label()).collect();
    println!(
        "cratonvm-difftest run — corpus {} | cratonvm[{}] vs java | timeout {}s",
        config.corpus.display(),
        modes.join(","),
        config.timeout.as_secs()
    );
    print_normalization_banner(&config.modes);

    let summary = match harness::run_corpus(&config) {
        Ok(s) => s,
        Err(
            e @ (RunError::CratonvmBinaryMissing | RunError::JavaMissing | RunError::EmptyCorpus),
        ) => {
            eprintln!(
                "cratonvm-difftest run: {e} — bootstrap, exit {} (non-fatal).",
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
        Err(e) => {
            eprintln!("cratonvm-difftest run: {e}");
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };

    print!("{}", harness::render_summary(&summary));

    if config.update_ledger {
        let jdk =
            runner::jdk_version(config.jdk_home.as_deref()).unwrap_or_else(|| "unknown".into());
        let existing = match Ledger::load(&config.ledger) {
            Ok(existing) => existing,
            Err(e) => {
                eprintln!(
                    "cratonvm-difftest run: could not merge existing ledger {} ({e}); writing fresh ledger.",
                    config.ledger.display()
                );
                None
            }
        };
        let ledger = summary.to_merged_ledger(existing.as_ref(), host_tag(), captured_at(), jdk);
        match ledger.save(&config.ledger) {
            Ok(()) => println!("wrote ledger: {}", config.ledger.display()),
            Err(e) => eprintln!(
                "cratonvm-difftest run: could not write ledger {}: {e}",
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

/// Print, per distinct normalizer in play, exactly which named rules stood
/// between the two VMs.
///
/// A reader of a divergence report has to be able to answer "was anything
/// rewritten before this comparison?" without reading the source. Printing the
/// rule ids at the top of every run is the cheapest way to make an
/// over-normalization visible: a rule that should not be on is on the first
/// line of the log.
fn print_normalization_banner(modes: &[Mode]) {
    let mut seen: Vec<String> = Vec::new();
    for m in modes {
        let described = Normalizer::for_mode(*m).describe();
        if !seen.contains(&described) {
            seen.push(described);
        }
    }
    for line in seen {
        println!("  {line}");
    }
}

// ---------------------------------------------------------------------------
// matrix — C2 review P0 (opcode / execution-path coverage)
// ---------------------------------------------------------------------------

/// Generate the coverage matrix from the corpus's own class files.
///
/// `.java` seeds are compiled here with the same `javac` the differential run
/// uses, so the matrix describes the exact bytes that run executes. A corpus of
/// bare `.class` files needs no JDK at all.
fn cmd_matrix(args: &MatrixArgs) -> ExitCode {
    let corpus = args.corpus.clone().unwrap_or_else(default_corpus);
    let out = args.out.clone().unwrap_or_else(|| {
        runner::workspace_root()
            .join("difftest")
            .join("coverage-matrix.json")
    });
    let programs = harness::discover_programs(&corpus);
    if programs.is_empty() {
        eprintln!(
            "cratonvm-difftest matrix: corpus {} is empty — bootstrap, exit {} (non-fatal).",
            corpus.display(),
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    let needs_javac = programs
        .iter()
        .any(|p| p.extension().and_then(|s| s.to_str()) == Some("java"));
    if needs_javac && !runner::java_available(args.jdk.as_deref()) {
        eprintln!(
            "cratonvm-difftest matrix: javac not found and the corpus has .java seeds — \
             bootstrap, exit {} (non-fatal).",
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    // One scratch dir of compiled classes, pid-namespaced like the runner's.
    let workdir = std::env::temp_dir().join(format!("difftest_matrix_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&workdir);
    let timeout = Duration::from_secs(args.timeout_secs);
    let mut unscannable: Vec<(String, String)> = Vec::new();

    let (sources, prebuilt): (Vec<PathBuf>, Vec<PathBuf>) = programs
        .iter()
        .cloned()
        .partition(|p| p.extension().and_then(|s| s.to_str()) == Some("java"));

    // One `javac` per chunk, not one per program: a ~200-program generated
    // corpus would otherwise pay 200 JVM start-ups. If the batch fails, retry
    // per file so the failure names the program that caused it — `javac`
    // reports a whole batch's errors at once, which would otherwise mark every
    // program in the chunk unscannable.
    if runner::compile_java_batch(&sources, &workdir, args.jdk.as_deref(), timeout).is_err() {
        for source in &sources {
            if let Err(e) = runner::compile_java(source, &workdir, args.jdk.as_deref(), timeout) {
                unscannable.push((file_label(source), e.to_string()));
            }
        }
    }
    for source in &prebuilt {
        let dest = workdir.join(source.file_name().unwrap_or_default());
        if let Err(e) = std::fs::copy(source, &dest) {
            unscannable.push((file_label(source), e.to_string()));
        }
    }

    let (scans, mut failed) = matrix::scan_class_dir(&workdir);
    unscannable.append(&mut failed);
    // What the generator can produce is a property of *this binary*, not of the
    // corpus being scanned — so the report can say "a generator exists, run
    // gen-opcodes" for a corpus that predates it. `--no-generators` opts out.
    let generators: GeneratorIndex = if args.no_generators {
        GeneratorIndex::new()
    } else {
        opcorpus::generator_index(&OpcodeCorpusConfig::default())
    };
    let m = CoverageMatrix::build(scans, &args.modes, unscannable, &generators);

    print!("{}", m.render());
    if args.show_gaps {
        print!("{}", m.render_gaps());
    }

    match m.to_json() {
        Ok(json) => {
            if let Some(parent) = out.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&out, json) {
                Ok(()) => println!("wrote matrix: {}", out.display()),
                Err(e) => eprintln!(
                    "cratonvm-difftest matrix: could not write {}: {e}",
                    out.display()
                ),
            }
        }
        Err(e) => eprintln!("cratonvm-difftest matrix: could not serialize the matrix: {e}"),
    }

    let _ = std::fs::remove_dir_all(&workdir);
    // The matrix is a report, not a gate: a coverage gap is a backlog item, not
    // a build failure, and failing here would make adding an axis a red build.
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// gen — Step 4
// ---------------------------------------------------------------------------

fn cmd_gen(args: &GenArgs) -> ExitCode {
    let out = args.out.clone().unwrap_or_else(default_corpus);

    // Resolve the family selection (`all` ⇒ every family).
    let families: Vec<TargetFamily> = if args.family == "all" {
        TargetFamily::all().to_vec()
    } else {
        match TargetFamily::from_label(&args.family) {
            Some(f) => vec![f],
            None => {
                eprintln!(
                    "cratonvm-difftest gen: unknown --family {:?} (expected arith|concat|exceptions|all)",
                    args.family
                );
                return ExitCode::from(exit::BOOTSTRAP);
            }
        }
    };

    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!(
            "cratonvm-difftest gen: cannot create {}: {e}",
            out.display()
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    let mut written = 0usize;
    for family in families {
        for prog in generate::generate(family, args.count, args.seed) {
            let path = out.join(format!("{}.java", prog.name));
            match std::fs::write(&path, &prog.source) {
                Ok(()) => written += 1,
                Err(e) => eprintln!(
                    "cratonvm-difftest gen: cannot write {}: {e}",
                    path.display()
                ),
            }
        }
    }

    println!(
        "cratonvm-difftest gen — wrote {written} program(s) (family={}, seed={}) to {}",
        args.family,
        args.seed,
        out.display()
    );
    println!("  next: cratonvm-difftest run --corpus {}", out.display());
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// mutate — Step 5 (bytecode mutation tier)
// ---------------------------------------------------------------------------

fn cmd_mutate(args: &MutateArgs) -> ExitCode {
    let bin = match runner::cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "cratonvm-difftest mutate: cratonvm binary not found (build it / set CRATONVM_BIN) — \
                 exit {}.",
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };
    if !runner::java_available(args.jdk.as_deref()) {
        eprintln!(
            "cratonvm-difftest mutate: java not found — exit {}.",
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    let workdir = std::env::temp_dir().join(format!("difftest_mut_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&workdir);
    let timeout = std::time::Duration::from_secs(args.timeout_secs);

    // Resolve the seed to a compiled `.class` and its main class name.
    let is_java = args.program.extension().and_then(|s| s.to_str()) == Some("java");
    let (class_name, class_path) = if is_java {
        match runner::compile_java(&args.program, &workdir, args.jdk.as_deref(), timeout) {
            Ok(name) => (name.clone(), workdir.join(format!("{name}.class"))),
            Err(e) => {
                eprintln!("cratonvm-difftest mutate: {e}");
                let _ = std::fs::remove_dir_all(&workdir);
                return ExitCode::from(exit::BOOTSTRAP);
            }
        }
    } else {
        let name = args
            .program
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        (name, args.program.clone())
    };

    let bytes = match std::fs::read(&class_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "cratonvm-difftest mutate: cannot read {}: {e}",
                class_path.display()
            );
            let _ = std::fs::remove_dir_all(&workdir);
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };
    let n_consts = mutate::numeric_constants(&bytes).len();
    if n_consts == 0 {
        println!(
            "cratonvm-difftest mutate: {} has no numeric constants to perturb — nothing to do.",
            class_name
        );
        let _ = std::fs::remove_dir_all(&workdir);
        return ExitCode::from(exit::OK);
    }

    println!(
        "cratonvm-difftest mutate — {} ({n_consts} numeric constant(s)) | {} mutants | modes {} | seed {}",
        class_name,
        args.count,
        args.modes
            .iter()
            .map(|m| m.label())
            .collect::<Vec<_>>()
            .join(","),
        args.seed
    );

    // Mutants reuse the determinism-off / reconfirm-on path (each is freshly
    // valid by construction; reconfirm filters transients).
    let config = RunnerConfig {
        timeout,
        jdk_home: args.jdk.clone(),
        modes: args.modes.clone(),
        reconfirm: true,
        ..RunnerConfig::for_corpus(workdir.clone())
    };
    let mut_dir = workdir.join("m");
    let _ = std::fs::create_dir_all(&mut_dir);
    let mut_path = mut_dir.join(format!("{class_name}.class"));

    let mut rng = Rng::new(args.seed);
    let (mut ran, mut diverged) = (0usize, 0usize);
    for i in 0..args.count {
        let Some(mutant) = mutate::mutate_constant(&bytes, &mut rng) else {
            break;
        };
        if std::fs::write(&mut_path, &mutant).is_err() {
            continue;
        }
        match harness::run_one(
            &bin,
            &mut_dir,
            &class_name,
            &config.modes,
            &config,
            args.program.clone(),
        ) {
            Ok(RunOne::Result(r)) => {
                ran += 1;
                if r.diverged() {
                    diverged += 1;
                    let label = r
                        .classification()
                        .map(|c| format!("{c:?}"))
                        .unwrap_or_else(|| "divergent".into());
                    println!("  DIFF  mutant#{i} [{label}]");
                }
            }
            Ok(RunOne::Nondeterministic(_)) => {}
            Err(e) => eprintln!("  (mutant#{i} run error: {e})"),
        }
    }

    println!("cratonvm-difftest mutate — {ran} mutant(s) ran, {diverged} diverged");
    let _ = std::fs::remove_dir_all(&workdir);
    ExitCode::from(exit::OK)
}

// ---------------------------------------------------------------------------
// min — Step 6
// ---------------------------------------------------------------------------

fn cmd_min(args: &MinArgs) -> ExitCode {
    let bin = match runner::cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "cratonvm-difftest min: cratonvm binary not found — exit {}.",
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };
    if !runner::java_available(args.jdk.as_deref()) {
        eprintln!(
            "cratonvm-difftest min: java not found — exit {}.",
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }
    if args.program.extension().and_then(|s| s.to_str()) != Some("java") {
        eprintln!(
            "cratonvm-difftest min: source minimization needs a .java seed (got {}).",
            args.program.display()
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }
    let source = match std::fs::read_to_string(&args.program) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "cratonvm-difftest min: cannot read {}: {e}",
                args.program.display()
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };
    let class_name = args
        .program
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let workdir = std::env::temp_dir().join(format!("difftest_min_{}", std::process::id()));
    let pred_dir = workdir.join("p");
    let _ = std::fs::create_dir_all(&pred_dir);
    let java_path = pred_dir.join(format!("{class_name}.java"));
    let timeout = std::time::Duration::from_secs(args.timeout_secs);
    let config = RunnerConfig {
        timeout,
        jdk_home: args.jdk.clone(),
        modes: args.modes.clone(),
        reconfirm: true,
        ..RunnerConfig::for_corpus(pred_dir.clone())
    };

    // Interestingness predicate: the candidate still compiles AND still diverges.
    let predicate = |candidate: &str| -> bool {
        if std::fs::write(&java_path, candidate).is_err() {
            return false;
        }
        if runner::compile_java(&java_path, &pred_dir, args.jdk.as_deref(), timeout).is_err() {
            return false; // didn't compile
        }
        matches!(
            harness::run_one(&bin, &pred_dir, &class_name, &config.modes, &config, java_path.clone()),
            Ok(RunOne::Result(r)) if r.diverged()
        )
    };

    let original_lines = source.lines().count();
    println!(
        "cratonvm-difftest min — confirming {class_name} ({original_lines} lines) diverges ..."
    );
    if !predicate(&source) {
        eprintln!(
            "cratonvm-difftest min: {class_name} does not compile-and-diverge under modes {} — \
             nothing to minimize.",
            config
                .modes
                .iter()
                .map(|m| m.label())
                .collect::<Vec<_>>()
                .join(",")
        );
        let _ = std::fs::remove_dir_all(&workdir);
        return ExitCode::from(exit::BOOTSTRAP);
    }

    let minimized = minimize::minimize_source(&source, predicate);

    // Write the minimized repro (it keeps the same public class ⇒ same file name).
    let out = args.out.clone().unwrap_or_else(|| {
        runner::workspace_root()
            .join("difftest")
            .join("regression")
            .join(format!("{class_name}.java"))
    });
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let write_ok = std::fs::write(&out, &minimized.source).is_ok();

    println!(
        "cratonvm-difftest min — {class_name}: {original_lines} → {} lines ({} reduction steps)",
        minimized.lines, minimized.steps
    );
    if write_ok {
        println!("  wrote minimized repro: {}", out.display());
    } else {
        eprintln!("  (could not write {})", out.display());
    }
    println!("--- minimized repro ---\n{}", minimized.source.trim_end());

    let _ = std::fs::remove_dir_all(&workdir);
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
            "cratonvm-difftest gate: corpus {} is empty — bootstrap, exit {} (non-fatal).",
            config.corpus.display(),
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }
    if runner::cratonvm_binary().is_none() || !runner::java_available(config.jdk_home.as_deref()) {
        eprintln!(
            "cratonvm-difftest gate: cratonvm binary or java not found (build cratonvm / set CRATONVM_BIN, \
             --jdk / DIFFTEST_JAVA_HOME / PATH) — bootstrap, exit {} (non-fatal).",
            exit::BOOTSTRAP
        );
        return ExitCode::from(exit::BOOTSTRAP);
    }

    // A ledger written by a *newer* build cannot be read as an empty baseline:
    // that would judge every one of its rows as a new divergence and report a
    // confident, wrong verdict. Refuse loudly instead.
    if let Some(v) = Ledger::schema_version_of(&config.ledger) {
        if v > ledger::LEDGER_SCHEMA_VERSION {
            eprintln!(
                "cratonvm-difftest gate: {} is schema_version {v}, newer than this build's {} — \
                 upgrade cratonvm-difftest. exit {} (non-fatal).",
                config.ledger.display(),
                ledger::LEDGER_SCHEMA_VERSION,
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
    }

    // The committed known-divergence ledger. A missing or malformed file is
    // treated as an empty baseline (every divergence is then "new"); a
    // schema-1 file loads and migrates in place.
    let ledger = match Ledger::load(&config.ledger) {
        Ok(Some(l)) => l,
        Ok(None) => Ledger::new(host_tag(), captured_at(), "unknown".into()),
        Err(e) => {
            eprintln!(
                "cratonvm-difftest gate: note — {} is not in the promoted ledger schema ({e}); \
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
                "cratonvm-difftest gate: {e} — bootstrap, exit {} (non-fatal).",
                exit::BOOTSTRAP
            );
            return ExitCode::from(exit::BOOTSTRAP);
        }
    };

    print_normalization_banner(&config.modes);
    let report = harness::gate(&summary, &ledger);
    print!("{}", harness::render_gate(&report));
    let code = report.exit_code();
    let verdict = match code {
        exit::OK => "clean — no new or regressed divergence",
        exit::NEW_DIVERGENCE => "FAIL — a new or drifted divergence appeared",
        exit::FIXED_REGRESSED => "FAIL — a fixed divergence re-opened",
        _ => "bootstrap",
    };
    println!("cratonvm-difftest gate: {verdict}. exit {code}.");
    ExitCode::from(code)
}

#[cfg(test)]
mod gate_default_modes_tests {
    use super::*;
    use cratonvm_difftest::matrix::{axes_for, PathAxis};

    /// The `--modes` default that `RunArgs` (and therefore the CI `gate` step)
    /// falls back on. Parsed from the same string clap uses, so this test moves
    /// with the flag rather than restating it.
    const GATE_DEFAULT_MODES: &str = "jit-on,nojit,interp-decoded";

    fn default_modes() -> Vec<Mode> {
        GATE_DEFAULT_MODES
            .split(',')
            .map(|s| parse_one_mode(s).expect("default mode list must parse"))
            .collect()
    }

    /// The gate's default must cover BOTH interpreter implementations.
    ///
    /// ARCH-2026-08-04 A4b. The interpreter has two implementations of the same
    /// 122 opcodes, selected by `--noverify`. `Mode::InterpDecoded` is the only
    /// mode that passes that flag and the only one mapping to
    /// `PathAxis::InterpreterDecoded` — and it was absent from this default, so
    /// the CI gate compared the fast path against HotSpot and never ran the
    /// decoded one.
    ///
    /// `nojit` does NOT cover it: `nojit` is the fast path with the JIT out of
    /// the picture (`matrix.rs` maps it to `InterpreterFast`). Anyone reading
    /// the mode list quickly will assume otherwise, which is exactly how the
    /// axis went unexercised while being fully defined, labelled, and listed in
    /// both `Mode::all()` and `execution_paths()`.
    #[test]
    fn the_gate_default_covers_both_interpreter_implementations() {
        let modes = default_modes();
        let covered: Vec<PathAxis> = modes.iter().flat_map(|m| axes_for(*m).to_vec()).collect();

        assert!(
            covered.contains(&PathAxis::InterpreterFast),
            "the gate default must exercise the raw fast path; modes were {:?}",
            modes.iter().map(|m| m.label()).collect::<Vec<_>>()
        );
        assert!(
            covered.contains(&PathAxis::InterpreterDecoded),
            "the gate default must exercise the DECODED interpreter path — the \
             second of the interpreter's two implementations of the same 122 \
             opcodes. Only Mode::InterpDecoded provides it (it is the only mode \
             passing --noverify); `nojit` is the fast path, not this one. \
             modes were {:?}",
            modes.iter().map(|m| m.label()).collect::<Vec<_>>()
        );
    }

    /// Exactly one mode in the default supplies the decoded axis, and it is the
    /// one that passes `--noverify`.
    ///
    /// Guards the substitution that would silently re-open the gap: swapping
    /// `interp-decoded` for another interpreter-ish mode that does not actually
    /// disable the raw handlers.
    #[test]
    fn only_interp_decoded_supplies_the_decoded_axis() {
        let providers: Vec<&'static str> = default_modes()
            .into_iter()
            .filter(|m| axes_for(*m).contains(&PathAxis::InterpreterDecoded))
            .map(|m| m.label())
            .collect();
        assert_eq!(
            providers,
            vec!["interp-decoded"],
            "the decoded axis must come from interp-decoded and nothing else"
        );
    }
}
