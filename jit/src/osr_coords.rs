// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The two pc spaces the OSR entry metadata lives in, kept apart by the type
//! system.
//!
//! The `osr-01` lane brief (retired to
//! `osr-01-entry-metadata-contract-RETIRED-20260804.md`) asks for "a newtype
//! (or at minimum a debug assertion at each boundary) separating
//! interpreter-bci space from output-pc space", because every one of the four
//! near-misses that lane found was the same thing: **a plausible integer in the
//! wrong coordinate space**. [`crate::osr_contract`] catches the *consequence*
//! — vectors that end up disagreeing — and this module catches the *cause*.
//!
//! # The spaces
//!
//! | Space | Indexed by | Vectors |
//! |---|---|---|
//! | [`OutPcIndexed`] | a pc in the emitter's (possibly rewritten) bytecode | `Compiler::osr_entry_native`, the freshly built dead mask |
//! | [`BciIndexed`] | a pc in the method's ORIGINAL bytecode | `CompiledMethod::osr_pc_to_native`, `CompiledMethod::osr_dead_mask` |
//!
//! (There is a third space — a JVM local index — for `osr_local_assignments` /
//! `osr_xmm_assignments`. It is not a *pc* space, nothing ever converts between
//! it and these two, and the length relation it does have is
//! [`crate::osr_contract`]'s. It is deliberately absent here.)
//!
//! # Why the identity conversion is the one that needed a type
//!
//! There are exactly two ways out of output-pc space, and the dangerous one is
//! the boring one:
//!
//! * [`BciIndexed::from_translated`] — the bytecode loop rewriter is armed, so
//!   `LoopXform::rebuild_pc_to_native` (or the pointwise `osr_entry_pc` remap)
//!   produced a genuinely new vector. Announced by its own call, hard to do by
//!   accident.
//! * [`OutPcIndexed::into_bci_by_identity`] — the rewriter is *not* armed, so
//!   the emitter's pcs and the interpreter's bcis are the same numbers and the
//!   vector is simply reinterpreted. This was a bare `None => osr_entry_native`
//!   match arm: an assumption stated nowhere, checked nowhere, and true only
//!   because `code_len == orig_code_len` on that path. A future producer that
//!   sizes one of these vectors from the OUTPUT code length while the other is
//!   rebuilt to the ORIGINAL — the exact edit `osr_contract`'s "short dead
//!   mask" test describes — walks straight through a match arm and is caught,
//!   if at all, only by the length check downstream.
//!
//! So the identity conversion takes `orig_code_len` and verifies the length it
//! implies. Both conversions do; the checked identity is the one that did not
//! exist before.
//!
//! # What a mismatch costs
//!
//! The same as a contract violation, and for the same reason: the publication
//! site publishes **no** OSR metadata, `can_osr_enter` answers `false`
//! everywhere, and the method runs to completion in the interpreter, which is
//! always valid. Over-refusal costs an optimisation; under-refusal re-runs loop
//! iterations or resumes with the wrong locals.

use std::sync::atomic::{AtomicU64, Ordering};

/// A vector indexed by a pc in the emitter's output bytecode.
///
/// Deliberately has no indexing operator: nothing should *read* one of these by
/// pc. Its whole purpose is to be converted, once, at the publication site.
pub struct OutPcIndexed<T> {
    v: Vec<T>,
    /// What this vector is, for the diagnostic. `&'static str` rather than an
    /// enum because the set is open — a future OSR vector should be able to use
    /// this without editing an enum here.
    what: &'static str,
}

/// A vector indexed by an interpreter bci — the space the runtime reads.
///
/// `Debug`/`PartialEq` so a conversion's `Result` can be asserted on directly;
/// the payload types are all plain integers.
#[derive(Debug, PartialEq, Eq)]
pub struct BciIndexed<T> {
    v: Vec<T>,
}

/// A vector whose length does not match the space it claims to be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoordinateMismatch {
    /// Which vector.
    pub what: &'static str,
    /// How the conversion was reached, for the diagnostic: `"identity"` (the
    /// rewriter was not armed) or `"translated"` (it was).
    pub via: &'static str,
    /// The length the vector has.
    pub got: usize,
    /// The length interpreter-bci space requires (`orig_code_len + 1`).
    pub expected: usize,
}

impl std::fmt::Display for CoordinateMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} has {} entries after the {} conversion into interpreter-bci space, \
             which holds {} (orig_code_len + 1)",
            self.what, self.got, self.via, self.expected
        )
    }
}

/// How many artifacts have had their OSR metadata dropped by a coordinate
/// mismatch.
///
/// **Expected to stay zero**, like [`crate::osr_contract::osr_contract_violations`].
/// Kept as its own counter rather than folded into that one because the two
/// answer different questions: this says an integer was in the wrong space,
/// that one says two vectors disagree. A single number could not tell a reader
/// which.
static MISMATCHES: AtomicU64 = AtomicU64::new(0);

/// Artifacts whose OSR metadata was dropped for a coordinate mismatch.
pub fn osr_coordinate_mismatches() -> u64 {
    MISMATCHES.load(Ordering::Relaxed)
}

/// Test support: zero the counter.
#[cfg(test)]
pub(crate) fn reset_osr_coordinate_mismatches() {
    MISMATCHES.store(0, Ordering::Relaxed);
}

/// Count and report a mismatch. The publication site calls this and then
/// publishes no OSR metadata.
///
/// Unconditional rather than behind a debug flag, for the same reason
/// `osr_contract`'s is: this is a compiler bug that silently costs the method
/// its OSR service, and a diagnostic nobody enabled is how it would stay
/// unnoticed.
pub fn note_mismatch(m: &CoordinateMismatch, method_label: &str) {
    MISMATCHES.fetch_add(1, Ordering::Relaxed);
    eprintln!("[osr-coords] {method_label}: {m} — publishing no OSR metadata");
}

impl<T> OutPcIndexed<T> {
    /// Wrap a vector the emitter indexed by output pc.
    pub fn new(v: Vec<T>, what: &'static str) -> Self {
        Self { v, what }
    }

    /// Reinterpret as interpreter-bci space, which is sound **only** when the
    /// bytecode was not rewritten — checked against the length the original
    /// bytecode implies.
    pub fn into_bci_by_identity(
        self,
        orig_code_len: usize,
    ) -> Result<BciIndexed<T>, CoordinateMismatch> {
        let expected = orig_code_len + 1;
        if self.v.len() != expected {
            return Err(CoordinateMismatch {
                what: self.what,
                via: "identity",
                got: self.v.len(),
                expected,
            });
        }
        Ok(BciIndexed { v: self.v })
    }
}

impl<T> BciIndexed<T> {
    /// Accept a vector a transform produced in interpreter-bci space, checking
    /// the length rather than trusting it.
    ///
    /// Takes the already-translated vector instead of doing the translation:
    /// the two translations differ (`rebuild_pc_to_native` picks a steady-state
    /// image and preserves the `-1` refusal sentinel; the dead mask is a
    /// pointwise `osr_entry_pc` remap), and folding either into this module
    /// would put loop-rewriter knowledge somewhere it does not belong.
    pub fn from_translated(
        v: Vec<T>,
        orig_code_len: usize,
        what: &'static str,
    ) -> Result<Self, CoordinateMismatch> {
        let expected = orig_code_len + 1;
        if v.len() != expected {
            return Err(CoordinateMismatch {
                what,
                via: "translated",
                got: v.len(),
                expected,
            });
        }
        Ok(Self { v })
    }

    /// Entries — i.e. `orig_code_len + 1`.
    pub fn len(&self) -> usize {
        self.v.len()
    }

    /// Whether the vector is empty. (Never true in practice: a zero-length
    /// method does not reach the backend. Present because clippy asks for it
    /// next to `len`.)
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    /// Hand the vector to the artifact. This is the only way out, and it is
    /// one-way: nothing converts back.
    pub fn into_inner(self) -> Vec<T> {
        self.v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-byte method: both spaces hold `orig_code_len + 1 == 4` entries.
    const ORIG: usize = 3;

    #[test]
    fn the_identity_conversion_accepts_a_correctly_sized_vector() {
        let out = OutPcIndexed::new(vec![-1i32, 0x10, -1, -1], "osr_pc_to_native");
        let bci = out.into_bci_by_identity(ORIG).expect("lengths agree");
        assert_eq!(bci.len(), ORIG + 1);
        assert_eq!(bci.into_inner(), vec![-1, 0x10, -1, -1]);
    }

    /// The edit this module exists for: a vector sized from the OUTPUT code
    /// length reaching the identity arm, which used to be a bare `None =>`
    /// match arm that took anything.
    #[test]
    fn the_identity_conversion_refuses_an_output_sized_vector() {
        // A rewrite that duplicated the loop body would leave the emitter's
        // vector longer than the original method.
        let out = OutPcIndexed::new(vec![-1i32; ORIG + 4], "osr_pc_to_native");
        assert_eq!(
            out.into_bci_by_identity(ORIG),
            Err(CoordinateMismatch {
                what: "osr_pc_to_native",
                via: "identity",
                got: ORIG + 4,
                expected: ORIG + 1,
            })
        );
    }

    /// And the short direction, which is the silently *unsound* one downstream:
    /// `can_osr_enter_with` reads the dead mask through `unwrap_or(0)`, so a
    /// short mask reads as "no dead locals" for every bci in the tail.
    #[test]
    fn the_identity_conversion_refuses_a_short_vector() {
        let out = OutPcIndexed::new(vec![0u64; ORIG], "osr_dead_mask");
        let err = out.into_bci_by_identity(ORIG).expect_err("too short");
        assert_eq!(err.got, ORIG);
        assert_eq!(err.expected, ORIG + 1);
    }

    #[test]
    fn a_translated_vector_is_checked_against_the_original_length() {
        assert!(BciIndexed::from_translated(vec![0u64; ORIG + 1], ORIG, "osr_dead_mask").is_ok());
        let err = BciIndexed::from_translated(vec![0u64; ORIG + 2], ORIG, "osr_dead_mask")
            .expect_err("a translation that lands in the wrong space is refused");
        assert_eq!(err.via, "translated");
        assert_eq!(err.what, "osr_dead_mask");
    }

    /// The diagnostic names the vector and both lengths, because a coordinate
    /// bug's first question is always "by how much".
    #[test]
    fn the_message_names_the_vector_and_both_lengths() {
        let m = CoordinateMismatch {
            what: "osr_pc_to_native",
            via: "identity",
            got: 9,
            expected: 4,
        };
        let s = m.to_string();
        assert!(s.contains("osr_pc_to_native"), "{s}");
        assert!(s.contains('9') && s.contains('4'), "{s}");
    }

    #[test]
    fn a_noted_mismatch_is_counted() {
        reset_osr_coordinate_mismatches();
        assert_eq!(osr_coordinate_mismatches(), 0);
        note_mismatch(
            &CoordinateMismatch {
                what: "osr_dead_mask",
                via: "identity",
                got: 1,
                expected: 2,
            },
            "Test.m()V",
        );
        assert_eq!(osr_coordinate_mismatches(), 1);
        reset_osr_coordinate_mismatches();
    }
}
