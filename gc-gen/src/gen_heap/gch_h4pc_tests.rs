// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gch h4/pc (2026-10-09) — unit tests of the true-root young rounds' cost cut
//! (`gch-h3mj-proposal-true-root-rounds-rescan-every-marked-old-object-DONE-20261015`):
//!
//! * `CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS` (default ON since gci i1/og, `=0`
//!   kills): the old mark's BFS hands each popped object's young targets to
//!   `TrueRootYoung` in the scan it already makes, so `round` no longer
//!   rescans every marked old object;
//! * `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE` (default ON since gci i1/og,
//!   `=0` kills): the post-finalizer closure is skipped when the finalizer
//!   round marked nothing.
//!
//! Both must leave every mark exactly as the `=0` arm does: each heap-level
//! test runs the same fixture on both arms and compares.
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gch_h4pc_` (and `gci_i1og_`).

use super::*;
