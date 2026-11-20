// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the unified source walk on the PARALLEL arm with every
//! wave-2 lever armed.
//!
//! This is the arm that matters in production: parallel evacuation is the
//! default path, and it is the arm on which the five divergences the audit
//! found would actually have fired. Armed here:
//!
//! * `CRATONVM_G1_CARD_CURSOR=2` — the density-adaptive cursor, so BOTH of its
//!   per-region decisions are exercised (the holders' regions go from sparse to
//!   dense as the repeated pauses dirty them);
//! * `CRATONVM_G1_CARD_CLEAN=1` — card cleaning, which is what makes a wrong
//!   per-object screen lose an edge on the SECOND pause rather than never;
//! * `CRATONVM_G1_RSET_PRUNE_ON_SCAN=1` — scan-time pruning, so an entry
//!   dropped in error shows up as a source region the next pause never walks.
//!
//! Every assertion is the same one `g1_w2a_source_walk_defaults` makes.

mod g1_w2a_walk_common;




