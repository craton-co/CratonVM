// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-4 lane A — the block-offset jump on the SERIAL arm, with the cursor on
//! too.
//!
//! The twin of `g1_w4a_block_offsets_parallel`; read that file for what the
//! workload is and why it is the one that can detect a lost edge. This binary
//! adds the two things its twin deliberately leaves out:
//!
//! * `CRATONVM_G1_PARALLEL_EVAC=0`, so the pause runs
//!   `G1Collector::scan_source_region_for_cset_refs`. Since wave 2 the two arms
//!   are wrappers over one body, and the jump was written into that body
//!   exactly once — but "written once" is a claim about the source and this is
//!   the claim about the behaviour. `CRATONVM_G1_PARALLEL_EVAC` is supposed to
//!   change how FAST a pause runs, not what it FINDS, and every previous
//!   divergence between these two walks was found the same way.
//! * `CRATONVM_G1_CARD_CURSOR=2`, the adaptive cursor. The jump and the
//!   per-object screen share one `next_dirty` cursor variable, and a region
//!   walking with both armed maintains it from two places. If the sharing were
//!   wrong the symptom would be an object screened against a stale card
//!   address, which on this workload is a lost young child.
//!
//! Card cleaning is on for the same reason as in the twin: it is what produces
//! the clean card runs there is anything to jump over.

mod g1_w2a_walk_common;




