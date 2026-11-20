// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The OSR entry-metadata contract, as executable checks.
//!
//! The `osr-01` lane brief, retired to
//! `osr-01-entry-metadata-contract-RETIRED-20260804.md`. OSR entry works;
//! what had no owner is the *contract between* the seven pieces of metadata it
//! rides on. Every near-miss this campaign found in that area was the same bug
//! class — **a plausible integer in the wrong coordinate space**.
//!
//! # There are THREE spaces, not two
//!
//! The brief says two (interpreter bci and output pc) and asks for an assertion
//! that `osr_pc_to_native`, `osr_dead_mask` and `osr_local_assignments` "agree
//! on length". Two of those three are not in the same space at all, so that
//! assertion cannot be written as stated — which is worth saying plainly,
//! because a length check across two different spaces would pass or fail for
//! reasons unrelated to anything:
//!
//! | Space | Indexed by | Vectors |
//! |---|---|---|
//! | **Interpreter bci** | a pc in the method's ORIGINAL bytecode | `osr_pc_to_native`, `osr_dead_mask` |
//! | **Output pc** | a pc in the emitter's (possibly rewritten) bytecode | `Compiler::osr_entry_native` before publication, `LoopXform::osr_entry_pc`'s result |
//! | **Local index** | a JVM local slot | `osr_local_assignments`, `osr_xmm_assignments` |
//!
//! The runtime (`CompiledMethod::can_osr_enter`, `osr_enter`) indexes the first
//! two by interpreter bci. The emitter writes them at whatever pc it is
//! emitting, which under a bytecode transform is an *output* pc. The conversion
//! is `LoopXform::rebuild_pc_to_native` plus the pointwise
//! `LoopXform::osr_entry_pc` remap of the dead mask, both at the publication
//! site in `x64.rs`.
//!
//! # What this module checks, and why each one
//!
//! Only invariants that hold **by construction today**. An invariant that is
//! merely *usually* true would refuse OSR on real methods, and over-refusal is
//! cheap only when it is rare.
//!
//! 1. **The two bci-indexed vectors have equal length.** This is the one that
//!    is silently unsound rather than merely wrong.
//!    `can_osr_enter_with` reads the dead mask with
//!    `.get(entry_pc).copied().unwrap_or(0)` — so a dead mask SHORTER than the
//!    entry table reads as "no dead locals" for every bci in the tail, and
//!    those entries are then admitted as safe. The trampoline then seeds a dead
//!    local over a live one sharing its register. Nothing downstream can
//!    notice.
//! 2. **Every local the trampoline will ask about has an assignment slot.**
//!    `osr_trampoline` loops `for i in 0..num_locals` and reads
//!    `assignments.get(i)`, so a short vector degrades to "no register home"
//!    silently — not memory-unsafe, but it means the producer and the consumer
//!    disagree about how many locals the frame has, and that disagreement is
//!    the shape of every other bug in this area.
//! 3. **No seeded local past 63 has a register home.** The dead mask is a
//!    `u64`, so it cannot protect such a local's shared register; the
//!    allocator's 64-local cap is what makes that moot today, and this is the
//!    check that says so where the mask's width depends on it.
//!
//! Deliberately NOT checked: "a bci with a non-zero dead mask must have a live
//! entry". It looks like an invariant and is not one. The dead mask is filled
//! from `Compiler::osr_block_live_in` (basic-block starts) while the entry
//! table is filled where the emitter decided an OSR entry is valid, and the two
//! sets are not equal on the untransformed path. Asserting it would produce a
//! check that fails for a reason unrelated to any defect — which is precisely
//! how the earlier version of the loop-rewriter's OSR test "passed for the
//! wrong reason and then failed for the wrong reason"
//! (`docs/jit/loop-rewriter-wiring.md`).
//!
//! # What a violation costs
//!
//! Refusing OSR for the method. The `osr-01` brief's "What to
//! refuse" is explicit: over-refusal costs an optimisation, under-refusal
//! re-runs loop iterations or resumes with the wrong locals. So the publication
//! site drops **all** OSR metadata on a violation rather than publishing a set
//! whose pieces disagree — `can_osr_enter` then answers `false` everywhere and
//! the method runs to completion in the interpreter, which is always valid.

use std::sync::atomic::{AtomicU64, Ordering};

/// A way the published OSR metadata contradicts itself.
///
/// Carries the numbers, not just a discriminant: a violation is a compiler bug
/// and the first question is always "by how much".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsrContractViolation {
    /// The two interpreter-bci-indexed vectors disagree on length.
    ///
    /// Unsound rather than untidy — see the module note, check 1.
    BciVectorLengthMismatch {
        pc_to_native: usize,
        dead_mask: usize,
    },
    /// `osr_local_assignments` is shorter than the local count the trampoline
    /// will iterate.
    LocalAssignmentsTooShort { len: usize, num_locals: usize },
    /// `osr_xmm_assignments` is shorter than the local count.
    XmmAssignmentsTooShort { len: usize, num_locals: usize },
    /// A local at index 64 or above has a register home.
    ///
    /// `osr_dead_mask` is a `u64` per bci, so it cannot name a local past 63:
    /// such a local is ALWAYS seeded, and if its home were shared with a live
    /// local (graph-colouring coalescing) the seed would clobber the live
    /// owner with nothing able to mask it. That is sound today only because
    /// `regalloc::color_graph` never homes a local past 63 — which is an
    /// invariant of a different file, so it is checked here, where the mask's
    /// width is the thing that depends on it.
    RegisterHomeBeyondDeadMask { local: usize, xmm: bool },
}

impl std::fmt::Display for OsrContractViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OsrContractViolation::BciVectorLengthMismatch {
                pc_to_native,
                dead_mask,
            } => write!(
                f,
                "osr_pc_to_native has {pc_to_native} entries but osr_dead_mask has \
                 {dead_mask}; both are indexed by interpreter bci, and a short dead \
                 mask reads as `no dead locals` for every bci in the tail"
            ),
            OsrContractViolation::LocalAssignmentsTooShort { len, num_locals } => write!(
                f,
                "osr_local_assignments has {len} entries but the trampoline seeds \
                 {num_locals} locals"
            ),
            OsrContractViolation::XmmAssignmentsTooShort { len, num_locals } => write!(
                f,
                "osr_xmm_assignments has {len} entries but the trampoline seeds \
                 {num_locals} locals"
            ),
            OsrContractViolation::RegisterHomeBeyondDeadMask { local, xmm } => write!(
                f,
                "local {local} has a {} register home, but osr_dead_mask is 64 bits wide \
                 and cannot mask a local past 63",
                if *xmm { "XMM" } else { "GPR" }
            ),
        }
    }
}

/// How many artifacts have had their OSR metadata dropped by [`check`].
///
/// **Expected to stay zero.** Non-zero is a compiler bug, and the counter
/// exists so that "it never fires" can be a measurement over a real workload
/// rather than an assumption — the same discipline the `ir_lower` catch-all
/// refusal was landed under. Relaxed, like `bailout::bailout_counts`: a sample
/// is what a diagnostic counter needs and all it needs.
static VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Artifacts whose OSR metadata was dropped because it contradicted itself.
pub fn osr_contract_violations() -> u64 {
    VIOLATIONS.load(Ordering::Relaxed)
}

/// Test support: zero the counter.
#[cfg(test)]
pub fn reset_osr_contract_violations() {
    VIOLATIONS.store(0, Ordering::Relaxed);
}

/// Check one artifact's OSR metadata against itself.
///
/// Takes the vectors rather than a `CompiledMethod` on purpose: the doc asks
/// for the properties to be "asserted against the *mapping function* over a
/// synthetic vector, not against whatever the emitter happened to place there",
/// and a checker that can only be handed a real compile cannot be tested that
/// way.
///
/// `Ok(())` means the pieces agree. `Err` means they do not and the caller must
/// publish no OSR metadata at all.
pub fn check(
    pc_to_native: &[i32],
    dead_mask: &[u64],
    local_assignments: &[Option<u8>],
    xmm_assignments: &[Option<u8>],
    num_locals: usize,
) -> Result<(), OsrContractViolation> {
    if pc_to_native.len() != dead_mask.len() {
        return Err(OsrContractViolation::BciVectorLengthMismatch {
            pc_to_native: pc_to_native.len(),
            dead_mask: dead_mask.len(),
        });
    }
    if local_assignments.len() < num_locals {
        return Err(OsrContractViolation::LocalAssignmentsTooShort {
            len: local_assignments.len(),
            num_locals,
        });
    }
    if xmm_assignments.len() < num_locals {
        return Err(OsrContractViolation::XmmAssignmentsTooShort {
            len: xmm_assignments.len(),
            num_locals,
        });
    }
    // Only locals the trampoline actually seeds (`0..num_locals`); a longer
    // vector's tail is never read.
    for (local, a) in local_assignments
        .iter()
        .enumerate()
        .take(num_locals)
        .skip(64)
    {
        if a.is_some() {
            return Err(OsrContractViolation::RegisterHomeBeyondDeadMask { local, xmm: false });
        }
    }
    for (local, a) in xmm_assignments.iter().enumerate().take(num_locals).skip(64) {
        if a.is_some() {
            return Err(OsrContractViolation::RegisterHomeBeyondDeadMask { local, xmm: true });
        }
    }
    Ok(())
}

/// [`check`], plus the counter and the diagnostic, for the publication site.
///
/// Returns `false` when the caller must drop every OSR field.
pub fn check_at_publication(
    pc_to_native: &[i32],
    dead_mask: &[u64],
    local_assignments: &[Option<u8>],
    xmm_assignments: &[Option<u8>],
    num_locals: usize,
    method_label: &str,
) -> bool {
    match check(
        pc_to_native,
        dead_mask,
        local_assignments,
        xmm_assignments,
        num_locals,
    ) {
        Ok(()) => true,
        Err(v) => {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
            // Unconditional, not behind a debug flag. This is a compiler bug
            // that silently costs OSR for the method; a diagnostic nobody
            // enabled is how it would stay unnoticed.
            eprintln!("[osr-contract] {method_label}: {v} — publishing no OSR metadata");
            false
        }
    }
}

#[cfg(test)]
mod tests;
