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
//! ## Status: Step 4 — type-directed source generator
//!
//! The runner spawns the `cratonvm` binary and a real `java` as subprocesses,
//! compiles a `.java` program once with `javac`, and runs the same `.class` on
//! both VMs; the oracle diffs the four channels under strict-by-default
//! normalization; the [`harness`] fans CratonVM across **every configured
//! mode** ([`runner::Mode`]) against one HotSpot run, classifies each
//! divergence, runs the determinism pre-flight + re-confirmation, and
//! [`gate`](harness::gate)s a run against the committed [`ledger::Ledger`]
//! (§3.5 exit codes). Step 4 wires [`generate`] — a seeded, reproducible,
//! type-directed Java generator (arithmetic / string-concat / exception
//! families) whose every emitted program compiles and is self-deterministic.
//! The bytecode-mutation tier and the ddmin minimizer remain Step-5/6 work.
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
//! | [`minimize`]    | shrink a repro (§3.4)       | stub → Step 6  |

pub mod generate;
pub mod harness;
pub mod ledger;
pub mod minimize;
pub mod oracle;
pub mod runner;
