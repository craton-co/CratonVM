// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the `jimage` (Java modules image) container parser.
//!
//! Surface under test:
//!   * `cratonvm_reader::JImageReader::from_bytes` — header parse,
//!     redirect/offset/locations/strings section bounds checking.
//!   * `JImageReader::iter_entries` — walks every location record,
//!     dereferencing the strings table to reconstruct each resource path.
//!   * `JImageReader::find_resource` — synthetic name lookup against
//!     the perfect-hash redirect table.
//!
//! Why these post-construction methods matter: `from_bytes` only validates
//! section *boundaries*. The redirect-table indirection, location-record
//! variable-length decoding, and strings-table NUL-termination are only
//! reached once a consumer iterates entries, so the panic oracle must
//! drive that path under fuzz.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_jimage

#![no_main]

use libfuzzer_sys::fuzz_target;

/// A jimage of the bootstrap JDK is ~150 MiB. We do not need anywhere near
/// that to find parser bugs — capping at 4 MiB keeps the corpus
/// manageable and prevents the fuzzer from sinking time into giant
/// pathological inputs that mostly stress allocator throughput.
const MAX_INPUT: usize = 4 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // `from_bytes` takes ownership of a `Vec<u8>`; the fuzzer hands us a
    // borrowed slice, so copy once. The copy is bounded by `MAX_INPUT`.
    let owned = data.to_vec();
    let Ok(reader) = cratonvm_reader::JImageReader::from_bytes(owned) else {
        return;
    };

    // Cheap header observers — these must always succeed once
    // `from_bytes` returned `Ok`.
    let _ = reader.version();
    let _ = reader.resource_count();

    // Force the iterator. This walks the offsets + locations tables and
    // pulls every resource path out of the strings buffer, which is the
    // main place where adversarial offsets / unterminated strings would
    // surface as panics if the bounds were not checked.
    let _ = reader.iter_entries();

    // A handful of `find_resource` lookups under attacker-controlled
    // input drive the redirect-hash path on synthetic names that the
    // image (almost certainly) does not contain. The interesting
    // behaviour is the *negative* lookup: it still has to index into
    // the redirect / offset / locations tables exactly once.
    let _ = reader.find_resource("/java.base/java/lang/Object.class");
    let _ = reader.find_resource("");
});
