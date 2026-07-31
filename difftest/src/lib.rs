// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! # Semantic differential fuzzer vs HotSpot
//!
//! Industrializes the manual "run it on HotSpot and eyeball the diff" loop
//! that has produced nearly every bug in `docs/internal/*` and `MEMORY.md`.
//! Generate / mutate Java programs, run them on **both** CratonVM and a real
//! JDK, and **diff observable behavior** — stdout, stderr, thrown exception
//! type + message, and process exit code — automatically.
//!
//! Design: `docs/feature-designs/differential-fuzzer.md`.
//!
//! ## Status: complete (Steps 0–7)
//!
//! The runner spawns the `cratonvm` binary and a real `java` as subprocesses,
//! compiles a `.java` program once with `javac`, and runs the same `.class` on
//! both VMs; the oracle diffs the four channels under strict-by-default
//! normalization; the [`harness`] fans CratonVM across **every configured
//! mode** ([`runner::Mode`]) against one HotSpot run, classifies each
//! divergence, runs the determinism pre-flight + re-confirmation, and
//! [`gate`](harness::gate)s a run against the committed [`ledger::Ledger`].
//! [`generate`] emits a seeded type-directed corpus; [`mutate`] is the
//! format-aware bytecode mutator (also a libFuzzer target +
//! OSS-Fuzz-onboarded); [`minimize`] ddmin-shrinks a confirmed divergence to a
//! minimal repro under `difftest/regression/`. The macro tier reuses
//! `cratonvm-difftest run` over any directory of real programs (`--check-determinism`
//! to reject flaky ones); see `README.md`.
//!
//! The cooperating pieces, each its own module:
//!
//! | Module          | Role (design §)             | State          |
//! |-----------------|-----------------------------|----------------|
//! | [`ledger`]      | divergence record + JSON    | wired          |
//! | [`runner`]      | two-VM A/B executor (§3.2)  | wired (matrix) |
//! | [`oracle`]      | diff + classify (§3.3)      | wired          |
//! | [`harness`]     | compile + run + diff + gate | wired (matrix) |
//! | [`generate`]    | corpus generator (§3.1)     | wired (3 families) |
//! | [`mutate`]      | bytecode mutator (§3.1 t3)  | wired          |
//! | [`minimize`]    | ddmin shrink (§3.4)         | wired          |
//! | [`census`]      | JDK-only dump reader (§9)   | wired          |
//!
//! ## JDK-only mode (`docs/feature-designs/jdk-only-mode.md`)
//!
//! Four additional [`runner::Mode`]s select a *compatibility policy* on the
//! launcher command line (`--jdk-only` / `--real-jdk`) and collect the three
//! census dumps [`census`] reads back. Because the policy is part of what was
//! measured, a ledger row is keyed by **`(class, jdk_profile)`**: the same
//! program diverging under the strict policy and under the compatible one is
//! two findings, and a `known` compatible row can never excuse a strict
//! divergence. The five historical modes are untouched — same labels, same
//! argv, same env, same rows — so the committed `difftest/seeds` gate baseline
//! keeps measuring exactly what it always did.

pub mod census;
pub mod generate;
pub mod harness;
pub mod ledger;
pub mod minimize;
pub mod mutate;
pub mod oracle;
pub mod runner;
