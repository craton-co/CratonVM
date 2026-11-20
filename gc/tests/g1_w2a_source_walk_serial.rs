// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the unified source walk on the SERIAL arm, same levers.
//!
//! `CRATONVM_G1_PARALLEL_EVAC=0` routes the pause through
//! `G1Collector::scan_source_region_for_cset_refs` instead of
//! `SharedEvac::seed_source_region`. Since wave 2 those are two forty-line
//! wrappers over one body, and this binary is the half of the evidence that
//! says so: it runs the workload `g1_w2a_source_walk_parallel` runs, with the
//! same levers, and asserts the same expectation.
//!
//! An arm that finds a different graph is the defect class this whole
//! unification exists to close — `CRATONVM_G1_PARALLEL_EVAC` is supposed to
//! change how FAST a pause runs, not what it FINDS.

mod g1_w2a_walk_common;




