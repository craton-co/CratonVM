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
//! ## Status: Step 1 — real two-VM runner + four-channel oracle
//!
//! The runner now spawns the `cratonvm` binary and a real `java` as
//! subprocesses (timeout-guarded capture of stdout/stderr/exit-code), compiles
//! a `.java` seed once with `javac`, and runs the same `.class` on both VMs;
//! the oracle parses the uncaught-exception banner and diffs the four channels
//! (exit code, exception identity, stdout, stderr) under strict-by-default
//! normalization. The [`harness`] ties them together over a corpus. Generation,
//! bytecode mutation, minimization, the per-mode `Classification` join, and the
//! committed-ledger gate verdict remain stubs/Step-2+ work.
//!
//! The cooperating pieces, each its own module:
//!
//! | Module          | Role (design §)            | State       |
//! |-----------------|----------------------------|-------------|
//! | [`ledger`]      | divergence record + JSON   | wired       |
//! | [`runner`]      | two-VM A/B executor (§3.2) | wired (1 mode) |
//! | [`oracle`]      | diff + classify (§3.3)     | wired       |
//! | [`harness`]     | compile + run + diff corpus | wired       |
//! | [`generate`]    | corpus generator (§3.1)    | stub → Step 4 |
//! | [`minimize`]    | shrink a repro (§3.4)      | stub → Step 6 |

pub mod generate;
pub mod harness;
pub mod ledger;
pub mod minimize;
pub mod oracle;
pub mod runner;
