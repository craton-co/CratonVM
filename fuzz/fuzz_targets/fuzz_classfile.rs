// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the JVM `.class` file parser.
//!
//! Surface under test:
//!   * `cratonvm_reader::read_class` — the public entry point.
//!   * `cratonvm_reader::force_decode_all` — flushes every `LazyAttribute::Raw`
//!     through `decode_attribute`, so the per-attribute body decoders
//!     (`Code`, `StackMapTable`, `LineNumberTable`, `LocalVariableTable`,
//!     `RuntimeVisibleAnnotations`, etc.) are also exercised — not only
//!     the eagerly-parsed header / constant pool / method / field tables.
//!
//! The oracle is **panic-only**: any `Err` is accepted (malformed input
//! should fail gracefully); only an unrecoverable panic / abort indicates
//! a parser bug worth reporting.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_classfile
//!
//! Reproduce a crash with:
//!   cargo +nightly fuzz run fuzz_classfile fuzz/artifacts/fuzz_classfile/crash-<id>

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Hard cap on input size. The JVM spec allows class files up to ~4 GiB
/// in theory (u32 attribute_length), but practical files are well under
/// 1 MiB; capping here keeps the fuzzer's per-input cost bounded and
/// prevents `cargo fuzz` from grinding on giant OOM-shaped inputs that
/// would dwarf the time spent on interesting code paths.
const MAX_INPUT: usize = 2 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // The parser must never panic on arbitrary input. `Err` is fine —
    // that is the documented contract for malformed class data.
    let Ok(mut cf) = cratonvm_reader::read_class(data) else {
        return;
    };

    // Drive a few downstream observers so the post-parse lazy paths
    // also see fuzzed input:
    //
    //   * `force_decode_all` walks every `LazyAttribute::Raw` and
    //     dispatches to `decode_attribute`, exercising the Code /
    //     StackMapTable / Annotation* / BootstrapMethods decoders.
    //   * Iterating methods + fields with their access-flag bitsets
    //     forces the constant-pool name/descriptor resolution path.
    //   * `source_file` exercises the post-decode attribute walker.
    //
    // Each of these is allowed to return `Err`; only a panic is a bug.
    let _ = cratonvm_reader::force_decode_all(&mut cf.attributes, &cf.constant_pool);
    for m in &mut cf.methods {
        let _ = cratonvm_reader::force_decode_all(&mut m.attributes, &cf.constant_pool);
        let _ = m.name.as_bytes().len();
        let _ = m.descriptor.as_bytes().len();
    }
    for f in &mut cf.fields {
        let _ = cratonvm_reader::force_decode_all(&mut f.attributes, &cf.constant_pool);
    }
    let _ = cf.is_interface();
    let _ = cf.is_enum();
    let _ = cf.source_file();
});
