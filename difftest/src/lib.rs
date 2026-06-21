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
//! ## Status: Step 2 — mode matrix + auto-classification
//!
//! The runner spawns the `cratonvm` binary and a real `java` as subprocesses
//! (timeout-guarded capture of stdout/stderr/exit-code), compiles a `.java`
//! seed once with `javac`, and runs the same `.class` on both VMs; the oracle
//! parses the uncaught-exception banner and diffs the four channels (exit code,
//! exception identity, stdout, stderr) under strict-by-default normalization.
//! The [`harness`] now fans CratonVM out across **every configured mode**
//! ([`runner::Mode`]) against one HotSpot run and feeds the per-mode
//! agree/diverge map into [`oracle::classify`], auto-labeling each divergence
//! (`JitOnly`/`GcMode`/`Universal`/`Hang`/`Crash`) — the automated `--nojit`
//! bisection. Generation, bytecode mutation, minimization, the determinism
//! pre-flight, and the committed-ledger gate verdict remain Step-3+ work.
//!
//! The cooperating pieces, each its own module:
//!
//! | Module          | Role (design §)             | State          |
//! |-----------------|-----------------------------|----------------|
//! | [`ledger`]      | divergence record + JSON    | wired          |
//! | [`runner`]      | two-VM A/B executor (§3.2)  | wired (matrix) |
//! | [`oracle`]      | diff + classify (§3.3)      | wired          |
//! | [`harness`]     | compile + run + diff corpus | wired (matrix) |
//! | [`generate`]    | corpus generator (§3.1)     | stub → Step 4  |
//! | [`minimize`]    | shrink a repro (§3.4)       | stub → Step 6  |

pub mod generate;
pub mod harness;
pub mod ledger;
pub mod minimize;
pub mod oracle;
pub mod runner;
