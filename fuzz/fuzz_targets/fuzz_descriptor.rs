// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the descriptor / field-type / generic-signature parsers.
//!
//! Surface under test (all consume a `&str` from the JVM constant pool —
//! attacker-controlled UTF-8 in a `.class` file):
//!   * `cratonvm_reader::method_descriptor::MethodDescriptor::parse`
//!     — `(ILjava/lang/String;)V`-style method descriptors (JVMS 4.3.3).
//!   * `cratonvm_reader::field_type::FieldType::parse`
//!     — a single complete field descriptor (JVMS 4.3.2).
//!   * `cratonvm_reader::field_type::FieldType::parse_partial`
//!     — the prefix-consuming variant returning `(FieldType, &str rest)`.
//!   * `cratonvm_reader::signature::parse_class_signature`
//!   * `cratonvm_reader::signature::parse_method_signature`
//!   * `cratonvm_reader::signature::parse_field_signature`
//!     — the generic-`Signature`-attribute grammar (JVMS 4.7.9.1), which
//!     is recursive (nested type arguments / bounds) and a classic
//!     stack-overflow / unbounded-recursion candidate.
//!
//! Oracle is panic-only: a malformed descriptor must return `Err`/`None`,
//! never panic or abort.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_descriptor

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_reader::field_type::FieldType;
use cratonvm_reader::method_descriptor::MethodDescriptor;
use cratonvm_reader::signature;

/// Constant-pool `Utf8` entries are length-prefixed by a `u16`, so a
/// single descriptor / signature string is at most 64 KiB. Cap a little
/// above that to keep recursive-grammar inputs bounded.
const MAX_INPUT: usize = 128 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // `Utf8` constant-pool entries are (modified) UTF-8; lossy decoding
    // gives us a valid `&str` to feed the parsers without rejecting
    // interesting byte patterns outright.
    let s = String::from_utf8_lossy(data);
    let s: &str = &s;

    // Method descriptor: `(...)Ret`.
    let _ = MethodDescriptor::parse(s);

    // Field type: whole-string and prefix-consuming forms.
    let _ = FieldType::parse(s);
    let _ = FieldType::parse_partial(s);

    // Generic signatures — recursive grammar; the prime stack-overflow
    // candidate among these parsers.
    let _ = signature::parse_class_signature(s);
    let _ = signature::parse_method_signature(s);
    let _ = signature::parse_field_signature(s);
});
