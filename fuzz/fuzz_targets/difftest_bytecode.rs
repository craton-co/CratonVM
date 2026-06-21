// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Semantic differential fuzzer — bytecode-mutation tier (design §3.1 tier 3 /
//! §4 Step 5).
//!
//! This is the **fast, in-process, panic-only** half of the bytecode tier: it
//! drives the same constant-pool mutator the `difftest mutate` subcommand uses
//! over arbitrary libFuzzer-supplied bytes, asserting only that the mutator
//! **never panics** on malformed input and **never changes a class file's
//! length** (the in-place invariant that keeps mutants verification-clean).
//!
//! It does NOT fork `java` (libFuzzer's in-process model forbids that in the
//! hot loop). Divergent inputs are promoted to the slow differential tier by
//! running `difftest mutate <seed>` out-of-process — see the crate docs.
//!
//! Run (requires the nightly toolchain + `cargo install cargo-fuzz`):
//!
//! ```text
//!   cargo +nightly fuzz run difftest_bytecode
//! ```
//!
//! Seed the corpus from any compiled `.class` (e.g. the difftest seeds compiled
//! by `javac`), then `cargo +nightly fuzz tmin difftest_bytecode <artifact>` to
//! minimize a crash.

#![no_main]

use cratonvm_difftest::generate::Rng;
use cratonvm_difftest::mutate;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The constant-pool walk must be panic-free on arbitrary bytes.
    let _consts = mutate::numeric_constants(data);

    // The mutator must be panic-free and length-preserving on any input.
    let mut rng = Rng::new(0xD1FF_7E57_BC0D_E001);
    if let Some(mutant) = mutate::mutate_constant(data, &mut rng) {
        assert_eq!(
            mutant.len(),
            data.len(),
            "constant mutation must be length-preserving"
        );
    }
});
