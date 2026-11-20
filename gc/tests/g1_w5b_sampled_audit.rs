// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-5 lane B — the audit as a SAMPLE, which is the form it can ship in.
//!
//! # Why a sample is not a weaker audit
//!
//! `CRATONVM_G1_BLOCK_OFFSET_AUDIT=1` walks every span a jump skipped, which
//! costs exactly the work the jump removed. That is the right price for a soak
//! or for this test suite and the wrong one for production, so the lever is a
//! PERIOD: `N` audits one jump in every `N`.
//!
//! The fault it looks for is a card PRODUCER, and a producer is systematic. A
//! producer that names the wrong object's start does it on every edge it
//! records, not once — so sampling does not lower the probability of catching
//! it, it delays the catch by `N` jumps. Against a workload that takes
//! hundreds of thousands of jumps per run, `N = 1024` surfaces a systematic
//! violation inside a single pause, for 1/1024 of the saving given back.
//!
//! And a violation disarms the jump process-wide, exactly as an unvouched
//! producer address does. That is what makes the sampled arm a safety
//! mechanism rather than a diagnostic: it is the only thing in a SHIPPING
//! binary that can see a producer naming a real object start belonging to the
//! wrong object, which is the one hole the `GC_FLAG_HEADER` vouch cannot
//! reach.
//!
//! # What this binary asserts, and why the ratio is the point
//!
//! A sampling period is precisely the kind of lever that can silently resolve
//! to "never" — the failure `orchestrator-wave-1-measurements.md` §7.1
//! retracted a whole measurement for. So the assertion is not "the audit found
//! nothing"; that would also be true of an audit that never ran. It is that
//! `jumps / audit_spans` comes out near the declared period, which can only be
//! true if the sampler is both firing and sampling.

mod g1_w2a_walk_common;

/// Period 4 rather than 1024: this fixture takes tens of jumps, not hundreds
/// of thousands, so a production-sized period would sample zero of them and
/// the ratio assertion below would have nothing to check. The MECHANISM is
/// period-independent; what a bigger period changes is only how long a
/// violation waits, and that is arithmetic rather than behaviour.
const PERIOD: u64 = 4;




