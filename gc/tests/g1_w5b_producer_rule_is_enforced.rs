// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-5 lane B — the block-offset table's precondition, ENFORCED rather than
//! audited.
//!
//! # What W4-A left open
//!
//! `docs/feature-designs/concurrent-gc-maturation.md` §8.1 kept the table
//! default-OFF for a correctness reason, not a performance one. The jump is
//! sound iff **an object holding a cross-region reference always has a dirty
//! START card**, and that rule was held up by an audit of four producers plus
//! one tripwire on one of them. A fifth producer that named a SLOT's address
//! would pass every test in the tree, and would not reliably show up as an
//! entry refusal either: a slot address that happens to sit at a real object
//! boundary further along the card decodes as a perfectly plausible header, so
//! the walk jumps to it, steps over the object that needed scanning, and
//! reports `entries_refused=0`.
//!
//! # What this binary is evidence of
//!
//! The three claims W5-B makes, in the order they matter.
//!
//! 1. **On a real G1 heap the rule holds, and the enforcement costs no jumps.**
//!    The workload below is the one every arm of the unified source walk is
//!    checked against — every live object but the root is reachable only
//!    through a remembered-set edge, so an object the jump skips is LOST and
//!    the graph assertion fails. With the lever armed it must report
//!    `jumps > 0`, `entries_refused == 0`, and now also `unvouched == 0` and
//!    `disarmed == false`. That last pair is the new statement: every entry
//!    the table handed the walk was, at the moment it was installed, an
//!    address whose sixteen bytes carried `GC_FLAG_HEADER` — the bit every
//!    allocator sets and nothing clears.
//!
//! 2. **The AUDIT arm engages and finds nothing.** `CRATONVM_G1_BLOCK_OFFSET_AUDIT`
//!    walks the bytes each jump skipped and asks of every object starting in
//!    them whether it holds a reference into another region. A hit is the one
//!    shape the vouch cannot see — a producer naming a real object start that
//!    belongs to the WRONG object. `audited > 0` beside `violations == 0` is
//!    the pair that means something; `violations == 0` alone would also be
//!    true of an audit that never ran.
//!
//! 3. It is a sibling binary, `g1_w5b_disarm_falls_back`, that says what
//!    happens when the enforcement fires.
//!
//! Note the flag discipline the round requires: `with_process_overrides`,
//! never `std::env::set_var`.

mod g1_w2a_walk_common;




