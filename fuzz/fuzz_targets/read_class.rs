// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the zero-copy `reader::read_class_arc` entry point.
//!
//! `read_class` (covered by `fuzz_classfile`) wraps `read_class_arc` after
//! copying the input into an `Arc<[u8]>`. This target drives
//! `read_class_arc` directly — the path the VM classloader takes when it
//! already holds the class bytes as a refcounted slice — so the
//! `Arc`-source bookkeeping (e.g. slices borrowed against the backing
//! `Arc`) is exercised on fuzzed input rather than only the owned-copy
//! path. The parser MUST NEVER panic on arbitrary input; any `Err` is
//! the documented contract for malformed class data.
//!
//! Run with: cargo +nightly fuzz run fuzz_read_class
#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `Arc::from(&[u8])` produces an `Arc<[u8]>` by copying the slice —
    // matching the conversion `read_class` performs internally, but here
    // we hand the `Arc` straight to `read_class_arc` so the zero-copy
    // entry point is the surface under test.
    let source: Arc<[u8]> = Arc::from(data);
    let _ = cratonvm_reader::read_class_arc(source);
});
