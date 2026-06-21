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
//! ## Status: Step 0 — scaffolding
//!
//! This crate currently contains **only the scaffold**: the shared ledger
//! types, the binary-resolution helpers, and documented stubs for the four
//! cooperating pieces. No generation, mutation, or real diffing is wired yet —
//! every stub returns an empty / `Unimplemented` result and **never panics**,
//! so the crate builds green and the rest of the feature can land
//! incrementally (one mergeable step per `§4` of the design doc).
//!
//! The four cooperating pieces, each its own module:
//!
//! | Module          | Role (design §)            | Step 0 state |
//! |-----------------|----------------------------|--------------|
//! | [`ledger`]      | divergence record + JSON   | types only   |
//! | [`runner`]      | two-VM A/B executor (§3.2) | helpers + stub |
//! | [`oracle`]      | diff + classify (§3.3)     | types + plumbing |
//! | [`generate`]    | corpus generator (§3.1)    | stub         |
//! | [`minimize`]    | shrink a repro (§3.4)      | stub         |

pub mod generate;
pub mod ledger;
pub mod minimize;
pub mod oracle;
pub mod runner;
