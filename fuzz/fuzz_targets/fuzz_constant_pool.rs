// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the **constant pool alone** (JVMS §4.4).
//!
//! `fuzz_classfile` fuzzes whole class files, so the fuzzer spends most of
//! its budget re-discovering the header and the field/method tables. This
//! target pins everything *except* the constant pool: the harness wraps the
//! fuzzer's bytes in a fixed `CAFEBABE` + Java-21 version header and a fixed
//! all-zero tail (`interfaces_count` / `fields_count` / `methods_count` /
//! `attributes_count` = 0), so every mutation lands inside the pool.
//!
//! Surface under test:
//!   * `cratonvm_reader::read_class` → the private `read_constant_pool`
//!     walker (`reader/src/class_reader.rs:300`), which is what decides
//!     `constant_pool_count`, per-tag entry widths, the modified-UTF8
//!     (CESU-8) decode plus its lone-surrogate `wide_utf8` recovery, and
//!     the category-2 (`Long`/`Double`) two-slot rule.
//!   * `cratonvm_reader::ConstantPool::{get, get_utf8, get_utf8_arc,
//!     get_utf8_wide, get_class_name, get_class_name_arc,
//!     get_name_and_type, validate}` — every cross-referencing accessor,
//!     driven over the entire index space including 0 and `len()`.
//!
//! Per-input layout (the fuzzer only has to learn 4 fixed bytes):
//!   * bytes 0..2 — `this_class` index (big-endian u16).
//!   * bytes 2..4 — `super_class` index (big-endian u16).
//!   * bytes 4..  — the raw constant pool: `constant_pool_count` u16
//!     followed by `count - 1` entries.
//!
//! Beyond panic-freedom this target asserts four *resource-exhaustion* and
//! *structural* invariants that a "count field trusted blindly" bug would
//! break:
//!
//!   1. `cp.len() <= MAX_CP_ENTRIES` — a `u16` count can never grow the
//!      pool past 65 535 slots.
//!   2. `cp.len() <= 1 + cp_bytes.len()` — every slot costs at least one
//!      wire byte (the tag), plus the reserved index-0 tombstone. This is
//!      the actual anti-OOM property: a declared count of 65 535 backed by
//!      three bytes of input must NOT produce a 65 535-entry `Vec`.
//!   3. JVMS §4.4.5 — a `Long`/`Double` occupies two slots, so the slot
//!      immediately after one must be a `Tombstone`, and neither may sit in
//!      the last slot.
//!   4. `validate()` emits at most two diagnostics per slot, so a hostile
//!      pool cannot turn a 65 535-slot pool into an unbounded `Vec<String>`.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_constant_pool

#![no_main]

use libfuzzer_sys::fuzz_target;

use cratonvm_reader::constant_pool::ConstantPoolEntry;

/// A `CONSTANT_Utf8` body is `u16`-length-prefixed and the pool holds at
/// most 65 535 entries, so a pathological-but-legal pool tops out around
/// 4 GiB. That is far past anything worth spending fuzz iterations on;
/// 256 KiB keeps the per-input cost bounded while still admitting pools
/// with tens of thousands of entries.
const MAX_INPUT: usize = 256 * 1024;

/// JVMS §4.1: `constant_pool_count` is a `u16`, so the pool can never hold
/// more than 65 535 slots (index 0 is the reserved tombstone and is
/// counted). Mirrors `MAX_CP_SIZE` in `reader/src/class_reader.rs`, which
/// is private to that module.
const MAX_CP_ENTRIES: usize = 65_535;

/// `validate()` pushes at most two diagnostics for any single entry (the
/// `Fieldref`/`Methodref`/`InterfaceMethodref` and `NameAndType` arms each
/// check two indices; every other arm checks one).
const MAX_VALIDATION_ERRORS_PER_ENTRY: usize = 2;

/// `CAFEBABE` + `minor = 0` + `major = 65` (Java 21). Fixed so the fuzzer
/// never wastes a mutation on the header.
const CLASS_HEADER: [u8; 8] = [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41];

/// `access_flags = ACC_PUBLIC | ACC_SUPER`, then the four zero counts that
/// follow `super_class`: interfaces, fields, methods, attributes.
const CLASS_TAIL_COUNTS: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 || data.len() > MAX_INPUT {
        return;
    }

    let this_class = [data[0], data[1]];
    let super_class = [data[2], data[3]];
    let cp_bytes = &data[4..];

    // Assemble the synthetic class file. The capacity is exact, so the
    // harness itself performs a single bounded allocation no larger than
    // `MAX_INPUT + 18` regardless of what the pool declares.
    let mut class_file = Vec::with_capacity(CLASS_HEADER.len() + cp_bytes.len() + 14);
    class_file.extend_from_slice(&CLASS_HEADER);
    class_file.extend_from_slice(cp_bytes);
    class_file.extend_from_slice(&[0x00, 0x21]); // access_flags
    class_file.extend_from_slice(&this_class);
    class_file.extend_from_slice(&super_class);
    class_file.extend_from_slice(&CLASS_TAIL_COUNTS);

    // Malformed pools must return `Err`. Reaching `Ok` means the pool
    // parsed *and* `this_class` resolved to a `Class` entry whose
    // `name_index` is a `Utf8` — i.e. the fuzzer built a coherent pool.
    let Ok(cf) = cratonvm_reader::read_class(&class_file) else {
        return;
    };
    let cp = &cf.constant_pool;

    // ---- Invariant 1: the u16 count caps the pool. ----
    assert!(
        cp.len() <= MAX_CP_ENTRIES,
        "constant pool grew to {} slots, past the u16 cap of {MAX_CP_ENTRIES}",
        cp.len()
    );

    // ---- Invariant 2: no slot is free. ----
    // Every entry costs at least its one-byte tag; index 0 is the reserved
    // tombstone. A pool larger than that is a declared count that was
    // believed without being paid for — the OOM shape this target exists
    // to catch.
    assert!(
        cp.len() <= 1 + cp_bytes.len(),
        "constant pool has {} slots but only {} wire bytes were supplied",
        cp.len(),
        cp_bytes.len()
    );

    // Index 0 is the reserved sentinel and must never resolve to data.
    assert!(
        matches!(cp.get(0), Some(ConstantPoolEntry::Tombstone) | None),
        "constant pool index 0 must be the reserved tombstone"
    );
    assert!(
        cp.get_utf8(0).is_none(),
        "constant pool index 0 must not resolve as Utf8"
    );

    // ---- Invariant 3: category-2 slot rules (JVMS §4.4.5). ----
    // Also drives every accessor across the whole index space, including
    // index 0 and the first out-of-range index, so an off-by-one in any
    // lookup surfaces as an OOB rather than staying latent.
    let len = cp.len();
    for i in 0..len {
        let idx = i as u16;
        let entry = cp.get(idx);
        // Cross-referencing accessors: each must return `None` (not panic
        // and not a wrong-typed hit) for every index, valid or not.
        let _ = cp.get_utf8(idx);
        let _ = cp.get_utf8_arc(idx);
        let _ = cp.get_class_name(idx);
        let _ = cp.get_class_name_arc(idx);
        let _ = cp.get_name_and_type(idx);

        // The exact-UTF-16 side table only exists for surrogate-bearing
        // `Utf8` entries; it must never carry a key the pool itself does
        // not hold as `Utf8`.
        if cp.get_utf8_wide(idx).is_some() {
            assert!(
                cp.get_utf8(idx).is_some(),
                "cp#{idx}: wide UTF-16 side table entry without a matching Utf8 entry"
            );
        }

        if matches!(
            entry,
            Some(ConstantPoolEntry::Long(_)) | Some(ConstantPoolEntry::Double(_))
        ) {
            assert!(
                i + 1 < len,
                "cp#{idx}: category-2 constant occupies the last pool slot (JVMS 4.4.5)"
            );
            assert!(
                matches!(cp.get(idx + 1), Some(ConstantPoolEntry::Tombstone)),
                "cp#{idx}: category-2 constant is not followed by a tombstone (JVMS 4.4.5)"
            );
        }
    }

    // First index past the end must miss. Guarded on the u16 range so the
    // cast below cannot wrap (invariant 1 already rules that out, but the
    // guard keeps the assertion honest if that ever changes).
    if len <= u16::MAX as usize {
        assert!(
            cp.get(len as u16).is_none(),
            "constant pool index {len} is past the declared count but resolved"
        );
    }

    // ---- Invariant 4: validation output is bounded by the pool size. ----
    let errors = cp.validate();
    assert!(
        errors.len() <= len.saturating_mul(MAX_VALIDATION_ERRORS_PER_ENTRY),
        "validate() produced {} diagnostics for a {len}-slot pool",
        errors.len()
    );
});
