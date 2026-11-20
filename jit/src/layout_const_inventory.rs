// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Inventory tripwire for the object-layout constants this crate bakes into
//! emitted machine code.
//!
//! Split out of `lib.rs` on 2026-09-17. The boundary is real rather than
//! arithmetic: everything here is ONE tripwire and the prose that justifies
//! its numbers. It reads this crate's own source as text, and nothing outside
//! it calls into the tripwire -- a test that audits the emitters has no reason
//! to live inside the largest emitter.
//!
//! One exception, stated so the sentence above stays true: the
//! `jit_gate_pass` census (`note_jit_gate_pass_hit` / `_fill` /
//! `jit_gate_pass_census`) is runtime state and IS called from outside -- by
//! the interpreter's `execute()` gate and the tiered stats dump. It landed here
//! with the rest of the text that moved out of `lib.rs`, not because it belongs
//! to the tripwire.
//!
//! Glob-re-exported from the crate root, so the move changed no path.
//!
//! # The `use` preamble below
//!
//! Inside `lib.rs` this text sat in the crate root's scope and named `Arc`,
//! `CompiledMethod`, `JitCache` and friends bare. A file module has its own
//! scope, so those become imports. The ones reached through `crate::` rather
//! than a bare path are items that live in SIBLING modules as `pub(crate)`:
//! a `pub use <module>::*` glob re-exports only `pub` items, so a
//! `pub(crate)` sibling is NOT visible at the crate root and `super::NAME`
//! cannot find it. Naming its real module is the fix, not widening it to
//! `pub`.

use std::sync::Arc;

// A glob, deliberately. This text was written in the crate root's scope and
// names dozens of its items bare; enumerating them would be a list that says
// nothing and goes stale the first time a test is added. `crate::*` restores
// exactly the scope the code was written against, and because the crate root
// glob-re-exports the split-out modules, it reaches their `pub` items too.
use crate::*;

// Reached by module path rather than through the glob above: these are
// `pub(crate)` in SIBLING modules, and a `pub use <module>::*` re-export
// carries only `pub` items -- so they are not at the crate root and
// `crate::*` cannot see them. Naming the real module is the fix; widening
// them to `pub` to satisfy a test would not be.
use crate::ea_ir_bridge::narrow_array_value_fits;
// Test-only free functions since interpreter round i1 wave 18.
#[cfg(test)]
use crate::jfr_compile_decision::{note_deferred_new_bail, take_deferred_new_retry};

// ---------------------------------------------------------------------------
// Layout-constant emission inventory (arch-2026-07-26 `layout-constant-hazards`)
// ---------------------------------------------------------------------------

/// Inventory tripwire for every object-layout constant this crate bakes into
/// emitted machine code, covering the two emitters the original header-offset
/// audit did not reach: this file and `ir_lower.rs`.
///
/// # Why this exists alongside the tripwire in `x64.rs`
///
/// `x64.rs::header_offset_emission_site_inventory_matches_the_doc` counts the
/// substring `<CONST> as <ty>` — the constant *immediately* followed by a cast.
/// That needle is exact for `x64.rs`, and its recorded totals were re-verified
/// against the tree on 2026-07-26 (31 / 11 / 16 / 5, all matching), as were the
/// three `ir_lower.rs` totals (2 / 2 / 1). The mechanism does what its name
/// says — but it is *structurally* blind to any site that uses the constant
/// inside a larger expression which is then cast, and that is exactly the shape
/// both of this file's own sites take:
///
/// ```text
/// let abs = (cratonvm_types::HEADER_SIZE + body_off) as i32;
/// (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32
/// ```
///
/// A substring scan for the cast form reports **zero** hits here. That is the
/// mechanical reason `lib.rs` never appeared in the header-offset inventory
/// even after `x64.rs` and `ir_lower.rs` were audited: the tool could not see
/// it, so no amount of care from the auditor would have.
///
/// This tripwire therefore counts *identifier occurrences in code* instead:
/// every use of the constant, in any expression shape, outside comments and
/// string literals. The zero entries are as load-bearing as the non-zero ones —
/// a constant that starts being used in a file where it never appeared before
/// also trips the assertion and forces the site into the inventory.
#[cfg(test)]
mod layout_constant_inventory;

/// Lifetime and ownership of compiled code and its side tables.
///
/// See `docs/jit/code-cache-lifetime.md`. Every test here is written to be
/// robust against the other tests in this binary running concurrently: nothing
/// asserts an absolute value of a process-global counter, and the install
/// barrier these tests exercise is deliberately per-`JitCache` so that flushing
/// one cache cannot refuse another's publication.
#[cfg(test)]
mod code_cache_lifetime_tests;

/// The `inlined_class_names` early-out must be a shortcut, never a change of
/// answer.
///
/// `invalidate_for_class` runs on EVERY class definition and almost never
/// matches, so it now refuses before scanning when no published body names the
/// class. That is only sound while the set is a true over-approximation of what
/// Reading a devirtualisation target out of a compiled method's own inline
/// cache — the evidence that exists when the receiver PROFILE does not.
///
/// Every case here is a refusal except the last, because the value of this
/// accessor is entirely in what it declines to answer: it feeds a class-id
/// guard, and a guard built on a thrashing cache spends a compare and a branch
/// to reach the ordinary call anyway.
/// Concurrency properties of the inline-cache publication protocol.
///
/// Generated code reads a PIC way with no lock, in a fixed order: the class id,
/// then ONE load of the tagged entry word. These tests model that reader in
/// Rust and race it against every writer: concurrent installs, recompiled
/// targets, `clear_entries`, and invalidation with a retired body.
#[cfg(test)]
mod inline_cache_publication_tests;

#[cfg(test)]
mod mic_devirt_evidence;

/// the maps contain, and the failure mode if it is not — a real CHA
/// invalidation silently skipped, leaving a devirtualised call bound to a
/// method that now has a second implementor — is a miscompile, not a slowdown.
/// So each case below is the equivalence, not the speed.
#[cfg(test)]
mod local_handler_site_cache;

#[cfg(test)]
mod invalidate_early_out;

/// Census of `execute()`'s static JIT-eligibility gate: how often the positive
/// memo (`JitRealm::jit_gate_pass`) answered, versus how often the full gate —
/// including `jit_method_calls_native_shadowed`'s O(method-bytecode) decode —
/// had to run and fill it.
///
/// `fills` is bounded by the number of distinct eligible methods; `hits` is the
/// number of `execute()` entries that would previously have re-run the whole
/// gate. The ratio is the fix's whole justification, so it is measured rather
/// than asserted. Always on.
///
/// STRIPED, because "always on" is only affordable that way. The hit side runs
/// on every interpreted `execute()` entry of an already-eligible method, from
/// every mutator thread at once, and a `fetch_add` on ONE process-wide word
/// there is a cache line bounced between every core that runs Java — the same
/// shape `validate_code_ptr`'s census and `ACTIVE_JIT_EXECUTIONS` were each
/// rewritten to avoid. The shared `RwLock` read next to it does not make that
/// free: a read lock is its own contended line, and adding a second one
/// doubles it. A [`StripedCounter`] puts each thread on its own line and pays
/// the sum only when the stats dump reads it.
///
/// [`StripedCounter`]: cratonvm_types::striped_counter::StripedCounter
static JIT_GATE_PASS_HITS: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();
/// Bounded by the number of distinct eligible methods, so it is not hot; it is
/// striped too only so the pair is read the same way.
static JIT_GATE_PASS_FILLS: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();

#[inline]
pub fn note_jit_gate_pass_hit() {
    JIT_GATE_PASS_HITS.inc();
}

#[inline]
pub fn note_jit_gate_pass_fill() {
    JIT_GATE_PASS_FILLS.inc();
}

/// `(hits, fills)` for the stats dump. Each is a sum over the stripes, so a
/// reading taken while mutators run can be off by the increments in flight —
/// the same tolerance the relaxed counters it replaced had.
pub fn jit_gate_pass_census() -> (u64, u64) {
    (
        JIT_GATE_PASS_HITS.get() as u64,
        JIT_GATE_PASS_FILLS.get() as u64,
    )
}

/// See [`ir_lower::ic_frame_republish_sites`].
pub use crate::ir_lower::ic_frame_republish_sites;

#[cfg(test)]
mod devirt_intrinsic_yield_tests;

#[cfg(test)]
mod deferred_new_retry_gate_tests;

#[cfg(test)]
mod deopt_saved_register_range_tests;

/// Regressions from the 2026-09-12 JIT review: array scalar replacement may
/// forward a stored value to a narrow load only when the value already fits.
#[cfg(test)]
mod narrow_array_forwarding_tests;
