// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the unified source walk on the DEFAULT configuration.
//!
//! Parallel evacuator (the shipping default since 2026-08-13), card cursor off,
//! card cleaning off, scan-time rset prune off. This binary sets NOTHING, which
//! is the point: it is the arm that says the walk unification changed no
//! shipping behaviour. Its two siblings
//! (`g1_w2a_source_walk_parallel`, `g1_w2a_source_walk_serial`) run the
//! identical workload with the levers armed and on the other evacuator arm, and
//! assert the identical expectation.
//!
//! See `g1_w2a_walk_common/mod.rs` for why one arm is one test binary.

mod g1_w2a_walk_common;

#[test]
fn the_default_configuration_finds_every_remembered_set_edge() {
    assert_eq!(g1_w2a_walk_common::run(), g1_w2a_walk_common::expected());
}
