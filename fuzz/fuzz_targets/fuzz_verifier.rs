// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the bytecode verifier — the JVM's core type-safety boundary.
//!
//! `ClassManager::define_class` parses, links, and runs both structural
//! (JVMS Pass 2) and type-checking (Pass 3, StackMapTable-driven) verification
//! before a class is registered. Driving it on fuzzed bytes exercises the
//! verifier on adversarial input: a malformed or unverifiable class MUST be
//! rejected with an `Err`/`VerifyError`, NEVER a panic, OOB, or hang.
//!
//! Coverage note: the verifier reaches maximal depth only when superclasses
//! and referenced types resolve. Point it at a JDK boot classpath so
//! `java/lang/Object` et al. load:
//!
//!     CRATONVM_FUZZ_BOOTCP="/path/to/jdk/classes" cargo +nightly fuzz run fuzz_verifier
//!
//! Without a boot classpath, classes whose superclass is unresolvable bail at
//! link time — which still exercises class-file parsing, the structural
//! checks, and the verifier entry, just not the full type-merge lattice.
//!
//! Each input gets a fresh `ClassManager` so one crafted class cannot poison
//! the next. (The process-global string interner does accumulate across
//! inputs; that is an accepted, bounded cost for the parse/verify surface and
//! does not affect correctness.)
//!
//! Run with: cargo +nightly fuzz run fuzz_verifier
#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_classloading::{ClassLoaderId, ClassManager};

/// Keep verifier runs bounded. A verifier input is more expensive than a
/// reader-only input because successful parses allocate a fresh class manager,
/// load optional boot paths, link, and run verification.
const MAX_INPUT: usize = 2 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // `define_class` requires the supplied name to equal the class file's
    // `this_class` (the prohibited-package / name-binding check). Parse first
    // to recover the name; if the bytes are not even a structurally readable
    // class file, there is nothing for the verifier to chew on.
    let cf = match cratonvm_reader::read_class(data) {
        Ok(cf) => cf,
        Err(_) => return,
    };
    let name = cf.this_class.to_string();

    // Optional JDK boot classpath (PATH-separated) for superclass resolution.
    let boot: Vec<String> = match std::env::var_os("CRATONVM_FUZZ_BOOTCP") {
        Some(v) => std::env::split_paths(&v)
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        None => Vec::new(),
    };
    let empty: Vec<String> = Vec::new();

    let mut cm = ClassManager::new(&boot, &empty, &empty);

    // Parses → links → verifies. Any `Err` is the documented contract for
    // malformed/unverifiable input. The invariant under test is "no panic".
    let _ = cm.define_class(&name, data, ClassLoaderId::Bootstrap);
});
