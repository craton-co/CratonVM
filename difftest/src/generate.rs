// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Corpus generator (design §3.1).
//!
//! Three input tiers, smallest-blast-radius first: (1) curated seeds,
//! (2) grammar-based typed-expression source generation weighted toward the
//! bug history, (3) bytecode mutation (the libFuzzer tier). A generated
//! program is a self-contained `main` that prints every intermediate, so the
//! **same `.class` runs on both VMs** with no wrapper.
//!
//! ## Status: Step 0
//!
//! Stub only — [`generate`] returns an empty `Vec`. The typed source generator
//! lands in Step 4 behind `difftest gen --grammar`; the bytecode mutator in
//! Step 5.

/// A generated program ready to compile + run on both VMs.
#[derive(Debug, Clone)]
pub struct GeneratedProgram {
    /// Suggested class / file name (no extension).
    pub name: String,
    /// Java source whose `main` prints a deterministic transcript.
    pub source: String,
}

/// Which target family to bias generation toward (design §4 "First targets",
/// ordered by historical bug density).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFamily {
    /// `invokedynamic` — lambdas, string-concat indy, records, switch patterns.
    InvokeDynamic,
    /// Reflection / generics — `getDeclaredMethod`, bridge methods, modifiers.
    Reflection,
    /// JIT correctness — escape analysis, catch-bypass, inline-cache dispatch.
    JitCorrectness,
    /// GC value-correctness — allocate / retain / checksum (bt-style).
    GcValue,
    /// Exception semantics — JEP-358 messages, stacktrace order, exit codes.
    Exceptions,
    /// Arithmetic edge cases — overflow, `MIN_VALUE / -1`, shift masking, FP.
    Arithmetic,
}

/// Generate up to `count` programs biased toward `family`.
///
/// **Step 0 stub:** returns an empty `Vec` (never panics). Step 4 implements
/// type-directed generation so every emitted program compiles.
pub fn generate(_family: TargetFamily, _count: usize) -> Vec<GeneratedProgram> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_empty_in_step0() {
        assert!(generate(TargetFamily::Arithmetic, 100).is_empty());
    }
}
