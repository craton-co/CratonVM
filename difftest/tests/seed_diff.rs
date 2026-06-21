// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration test for the Step 1 two-VM runner + oracle over the committed
//! seed corpus. This is the automated form of the manual "run on HotSpot and
//! eyeball the diff" loop (design §4 Step 1 / §7 bootstrapping proof).
//!
//! Gated like every other subprocess test in the workspace: it **skips** (does
//! not fail) when the `cratonvm` binary has not been built or `java`/`javac` is
//! not reachable, so a bare `cargo test` stays green. Run it explicitly with:
//!
//! ```text
//!   cargo build -p cratonvm-cli         # produce target/debug/cratonvm
//!   cargo test  -p cratonvm-difftest -- --ignored --nocapture
//! ```
//!
//! The assertions validate the **harness**, not VM correctness: it must run
//! every seed on both VMs, classify each as agree/diverge, and produce a
//! round-trippable ledger. (Divergences are expected and tracked — per
//! `MEMORY.md`, CratonVM is ~90% there — so the test does NOT assert parity.)

use std::path::PathBuf;

use cratonvm_difftest::harness;
use cratonvm_difftest::ledger::Ledger;
use cratonvm_difftest::runner::{self, Mode, RunnerConfig, DEFAULT_TIMEOUT};

fn seeds_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seeds")
}

/// Skip (don't fail) when prerequisites are missing, mirroring
/// `vm/tests/intrinsic_diff.rs`.
fn prerequisites_ok() -> bool {
    if runner::cratonvm_binary().is_none() {
        eprintln!(
            "[seed_diff] cratonvm binary not found — build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return false;
    }
    if !runner::java_available(None) {
        eprintln!("[seed_diff] java not on PATH / DIFFTEST_JAVA_HOME — skipping.");
        return false;
    }
    true
}

#[test]
#[ignore = "requires the cratonvm binary + java/javac on PATH"]
fn runs_seed_corpus_and_writes_ledger() {
    if !prerequisites_ok() {
        return;
    }

    let ledger_path = std::env::temp_dir().join("difftest_seed_diff_ledger.json");
    // Fan out across the interpreter/JIT axis so divergences auto-classify, and
    // exercise the determinism pre-flight + re-confirmation.
    let modes = vec![Mode::JitOn, Mode::NoJit];
    let config = RunnerConfig {
        modes: modes.clone(),
        ledger: ledger_path.clone(),
        update_ledger: true,
        determinism_check: true,
        reconfirm: true,
        ..RunnerConfig::for_corpus(seeds_dir())
    };

    let expected = harness::discover_programs(&config.corpus).len();
    assert!(expected >= 3, "seed corpus should have >= 3 programs");

    let summary = harness::run_corpus(&config).expect("run_corpus");

    // Every seed compiled and ran on both VMs (none skipped).
    assert_eq!(
        summary.skipped.len(),
        0,
        "no seed should fail to compile/run: {:?}",
        summary.skipped
    );
    assert_eq!(
        summary.total(),
        expected,
        "every discovered seed should produce a result"
    );

    // Each result ran HotSpot once and every configured CratonVM mode.
    for r in &summary.results {
        assert!(
            r.hotspot.exit_code.is_some() || r.hotspot.timed_out,
            "{} hotspot produced no exit status",
            r.program
        );
        assert_eq!(
            r.modes.len(),
            modes.len(),
            "{} should have one outcome per mode",
            r.program
        );
        for m in &r.modes {
            assert!(
                m.cratonvm.exit_code.is_some() || m.cratonvm.timed_out,
                "{} cratonvm[{}] produced no exit status",
                r.program,
                m.mode.label()
            );
        }
        // A divergent result must auto-classify to a precise label.
        if r.diverged() {
            assert!(
                r.classification().is_some(),
                "{} diverged but did not classify",
                r.program
            );
        }
    }

    // The ledger materialized from this run must round-trip. (Writing the
    // ledger is the CLI's job; `run_corpus` returns the summary, so the test
    // drives `to_ledger` + `save` itself.)
    let ledger = summary.to_ledger("test-host".into(), "epoch:0".into(), "test-jdk".into());
    ledger.save(&ledger_path).expect("save ledger");
    let loaded = Ledger::load(&ledger_path)
        .expect("ledger readable")
        .expect("ledger present");
    assert_eq!(loaded.entries.len(), summary.diverged());
    let _ = std::fs::remove_file(&ledger_path);

    eprintln!(
        "[seed_diff] ran {} seeds: {} agreed, {} diverged",
        summary.total(),
        summary.total() - summary.diverged(),
        summary.diverged()
    );
}

/// Determinism sanity (the oracle's core soundness assumption, design §3.3):
/// a known-deterministic seed must produce identical HotSpot output across two
/// runs. This validates the *oracle*, not the VM, so it never flakes on a
/// CratonVM bug.
#[test]
#[ignore = "requires java/javac on PATH"]
fn hotspot_runs_are_deterministic() {
    if !runner::java_available(None) {
        eprintln!("[seed_diff] java not available — skipping.");
        return;
    }
    let workdir = std::env::temp_dir().join("difftest_determinism");
    let _ = std::fs::create_dir_all(&workdir);
    let seed = seeds_dir().join("ArithEdge.java");
    let main_class =
        runner::compile_java(&seed, &workdir, None, DEFAULT_TIMEOUT).expect("ArithEdge compiles");

    let a = runner::run_hotspot(&workdir, &main_class, None, DEFAULT_TIMEOUT).expect("run a");
    let b = runner::run_hotspot(&workdir, &main_class, None, DEFAULT_TIMEOUT).expect("run b");
    assert_eq!(
        a.stdout, b.stdout,
        "ArithEdge HotSpot stdout must be stable"
    );
    assert_eq!(a.exit_code, b.exit_code);

    let _ = std::fs::remove_dir_all(&workdir);
}
