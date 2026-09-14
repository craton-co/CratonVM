// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deterministic mutation harness for the `.class` file parser.
//!
//! # What this is, and what it is not
//!
//! This is **not** a coverage-guided fuzzer. `fuzz/fuzz_targets/` holds
//! `libfuzzer` targets for that (`fuzz_classfile`, `fuzz_constant_pool`,
//! `fuzz_attribute_nesting`, …), but they require a nightly toolchain and
//! `cargo fuzz`, so they do not run in CI and — as of this commit — no
//! coverage-guided campaign has ever been run against them. See
//! `docs/feature-designs/class-file-parser-hardening.md`.
//!
//! What this file provides instead is an *exhaustive, deterministic*
//! mutation sweep that runs as an ordinary `cargo test`: no `rand`, no
//! seed, no wall-clock bound, identical results on every host and every
//! run. It takes one valid class file and applies every mutation in four
//! systematic families, asserting two invariants:
//!
//! 1. **The parser never panics.** In this codebase a panic in the reader
//!    is reachable from untrusted input and is a denial of service at
//!    minimum, so a panic is a test failure regardless of what the input
//!    was. Each mutant is run under [`std::panic::catch_unwind`] so the
//!    failure names the exact byte offset and substituted value.
//! 2. **Every strict prefix of a valid class file is rejected.** A
//!    truncated class is unconditionally invalid — the reader consumes the
//!    input exactly and rejects trailing bytes — so `Err` is the only
//!    correct answer for all `N` prefixes.
//!
//! For the substitution families the harness deliberately does *not*
//! require `Err`: flipping a byte inside a `max_stack` field or a `Utf8`
//! payload yields a class that is still perfectly valid, and asserting
//! `Err` there would be asserting a falsehood. The invariant that holds
//! for every mutation is "terminates, without panicking, with a
//! `Result`" — which is exactly the property a memory-safety bug breaks.
//!
//! # Depth of the parse each mutant is driven through
//!
//! `read_class` alone would only exercise the eager path; ~70 % of
//! attribute bodies are wrapped as `LazyAttribute::Raw` and never touched.
//! [`drive`] therefore forces the full decode of every class, field and
//! method attribute, then runs the bytecode of every `Code` attribute
//! through `verified_code`. That pulls the annotation/element-value
//! recursion, `StackMapTable::parse`, `LineNumberTable`, the exception
//! table and the instruction decoder into the blast radius.
//!
//! # Coverage
//!
//! See `mutation_coverage_totals_are_exact`, which pins the arithmetic.
//! For the ~200-byte fixture the four families total a few thousand
//! mutants and the whole file runs in well under a second.

use cratonvm_reader::attribute::Attribute;
use cratonvm_reader::{force_decode_all, read_class, verified_code, ClassReaderError};

// ---------------------------------------------------------------------------
// The seed: one valid class file, built byte by byte.
// ---------------------------------------------------------------------------

/// A valid Java-8 class with enough structure that mutations reach a wide
/// spread of the parser:
///
/// * a constant pool exercising `Utf8`, `Class` and cross-references,
/// * one method with a `Code` attribute,
/// * a non-empty `exception_table` with a real `catch_type`,
/// * nested `LineNumberTable` and `StackMapTable` attributes inside `Code`,
/// * a class-level `SourceFile`.
fn valid_class() -> Vec<u8> {
    let mut d = Vec::<u8>::new();
    d.extend_from_slice(&0xCAFE_BABE_u32.to_be_bytes());
    d.extend_from_slice(&0u16.to_be_bytes()); // minor
    d.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8)

    // constant_pool_count = 14 → real slots 1..=13
    d.extend_from_slice(&14u16.to_be_bytes());
    let utf8 = |d: &mut Vec<u8>, s: &[u8]| {
        d.push(1);
        d.extend_from_slice(&(s.len() as u16).to_be_bytes());
        d.extend_from_slice(s);
    };
    let class = |d: &mut Vec<u8>, name_index: u16| {
        d.push(7);
        d.extend_from_slice(&name_index.to_be_bytes());
    };
    utf8(&mut d, b"java/lang/Object"); // 1
    class(&mut d, 1); // 2
    utf8(&mut d, b"m"); // 3
    utf8(&mut d, b"()V"); // 4
    utf8(&mut d, b"Code"); // 5
    utf8(&mut d, b"java/lang/Exception"); // 6
    class(&mut d, 6); // 7
    utf8(&mut d, b"LineNumberTable"); // 8
    utf8(&mut d, b"StackMapTable"); // 9
    utf8(&mut d, b"SourceFile"); // 10
    utf8(&mut d, b"T.java"); // 11
    utf8(&mut d, b"T"); // 12
    class(&mut d, 12); // 13

    d.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    d.extend_from_slice(&13u16.to_be_bytes()); // this_class -> "T"
    d.extend_from_slice(&2u16.to_be_bytes()); // super_class -> "java/lang/Object"
    d.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
    d.extend_from_slice(&0u16.to_be_bytes()); // fields_count

    // ---- one method: public void m() ----
    d.extend_from_slice(&1u16.to_be_bytes()); // methods_count
    d.extend_from_slice(&0x0001u16.to_be_bytes()); // ACC_PUBLIC
    d.extend_from_slice(&3u16.to_be_bytes()); // name -> "m"
    d.extend_from_slice(&4u16.to_be_bytes()); // descriptor -> "()V"
    d.extend_from_slice(&1u16.to_be_bytes()); // attributes_count

    // LineNumberTable body: one { start_pc = 0, line_number = 1 } entry.
    let mut lnt = Vec::<u8>::new();
    lnt.extend_from_slice(&1u16.to_be_bytes());
    lnt.extend_from_slice(&0u16.to_be_bytes());
    lnt.extend_from_slice(&1u16.to_be_bytes());

    // StackMapTable body: one `same_frame` (tag 0..=63, offset_delta = tag).
    let mut smt = Vec::<u8>::new();
    smt.extend_from_slice(&1u16.to_be_bytes()); // number_of_entries
    smt.push(0u8); // same_frame, offset_delta = 0

    // Code body.
    let mut code = Vec::<u8>::new();
    code.extend_from_slice(&2u16.to_be_bytes()); // max_stack
    code.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    code.extend_from_slice(&4u32.to_be_bytes()); // code_length
    code.extend_from_slice(&[0xB1, 0xB1, 0xB1, 0xB1]); // return ×4
    code.extend_from_slice(&1u16.to_be_bytes()); // exception_table_length
    code.extend_from_slice(&0u16.to_be_bytes()); //   start_pc
    code.extend_from_slice(&2u16.to_be_bytes()); //   end_pc
    code.extend_from_slice(&2u16.to_be_bytes()); //   handler_pc
    code.extend_from_slice(&7u16.to_be_bytes()); //   catch_type -> Class "java/lang/Exception"
    code.extend_from_slice(&2u16.to_be_bytes()); // attributes_count
    code.extend_from_slice(&8u16.to_be_bytes()); //   LineNumberTable
    code.extend_from_slice(&(lnt.len() as u32).to_be_bytes());
    code.extend_from_slice(&lnt);
    code.extend_from_slice(&9u16.to_be_bytes()); //   StackMapTable
    code.extend_from_slice(&(smt.len() as u32).to_be_bytes());
    code.extend_from_slice(&smt);

    d.extend_from_slice(&5u16.to_be_bytes()); // attribute_name_index -> "Code"
    d.extend_from_slice(&(code.len() as u32).to_be_bytes());
    d.extend_from_slice(&code);

    // ---- class attributes: SourceFile ----
    d.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
    d.extend_from_slice(&10u16.to_be_bytes()); // -> "SourceFile"
    d.extend_from_slice(&2u32.to_be_bytes());
    d.extend_from_slice(&11u16.to_be_bytes()); // -> "T.java"
    d
}

// ---------------------------------------------------------------------------
// Driving one input all the way through the reader.
// ---------------------------------------------------------------------------

/// Parse `bytes` and force every lazy decoder the reader owns.
///
/// Returns the first error encountered. Any panic propagates to the caller
/// (which catches it and reports the offending mutation).
fn drive(bytes: &[u8]) -> Result<(), ClassReaderError> {
    let mut class = read_class(bytes)?;

    // Field and method attributes.
    let cp = &class.constant_pool;
    for field in class.fields.iter_mut() {
        force_decode_all(&mut field.attributes, cp)?;
    }
    for method in class.methods.iter_mut() {
        force_decode_all(&mut method.attributes, cp)?;
    }
    force_decode_all(&mut class.attributes, cp)?;

    // Every decoded `Code` body through the instruction decoder. This is
    // what pulls `verified_code` (and the switch/wide/opcode-length logic
    // behind it) into the mutation blast radius.
    for method in class.methods.iter() {
        for attr in method.attributes.iter() {
            if let Some(Attribute::Code(code)) = attr.as_decoded() {
                let _ = verified_code(&code.code)?;
            }
        }
    }
    Ok(())
}

/// Run `drive` on one mutant, converting a panic into a test failure that
/// names the mutation. A panic here is the bug this harness exists to find.
fn drive_no_panic(bytes: &[u8], what: &str) -> Result<(), ClassReaderError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drive(bytes))) {
        Ok(result) => result,
        Err(_) => panic!(
            "reader PANICKED on a malformed class file ({what}). A panic in the \
             parser is reachable from untrusted input and must be an Err instead."
        ),
    }
}

// ---------------------------------------------------------------------------
// Family 0 — the baseline must be valid, or every other test is vacuous.
// ---------------------------------------------------------------------------

#[test]
fn baseline_class_parses_and_fully_decodes() {
    let bytes = valid_class();
    drive(&bytes).expect("the mutation seed must itself be a valid class file");
}

// ---------------------------------------------------------------------------
// Family 1 — truncation at every length.
// ---------------------------------------------------------------------------

/// Every strict prefix of the seed must be rejected. Returns the mutation
/// count.
fn sweep_truncations() -> usize {
    let bytes = valid_class();
    for len in 0..bytes.len() {
        let mutant = &bytes[..len];
        let what = format!("truncated to {len} of {} bytes", bytes.len());
        assert!(
            drive_no_panic(mutant, &what).is_err(),
            "a class file truncated to {len} bytes must be rejected, not parsed"
        );
    }
    bytes.len()
}

#[test]
fn every_truncation_is_rejected_without_panicking() {
    let n = sweep_truncations();
    assert!(n > 100, "seed too small to be a meaningful sweep: {n}");
}

// ---------------------------------------------------------------------------
// Family 2 — single-byte substitution at every offset.
// ---------------------------------------------------------------------------

/// Byte values chosen to hit tag boundaries, continuation-byte patterns and
/// sign boundaries: `0x00` (reserved cp index / NUL, illegal in modified
/// UTF-8), `0x01` (`CONSTANT_Utf8` tag), `0x7F` (largest 1-byte UTF-8),
/// `0x80` (bare continuation byte), `0xFF` (invalid in any UTF-8 form).
const SUBSTITUTES: [u8; 5] = [0x00, 0x01, 0x7F, 0x80, 0xFF];

fn sweep_byte_substitutions() -> usize {
    let bytes = valid_class();
    let mut count = 0usize;
    for offset in 0..bytes.len() {
        for &value in SUBSTITUTES.iter() {
            // Where `bytes[offset] == value` the mutation is the identity.
            // It is still run and still counted, which keeps the total a
            // pure function of the seed length rather than of its contents.
            let mut mutant = bytes.clone();
            mutant[offset] = value;
            let what = format!("byte[{offset}] = {value:#04X}");
            // Ok or Err are both acceptable — many single-byte edits leave
            // a valid class. The assertion is that we got a Result at all.
            let _ = drive_no_panic(&mutant, &what);
            count += 1;
        }
    }
    count
}

#[test]
fn single_byte_substitutions_never_panic() {
    let n = sweep_byte_substitutions();
    assert_eq!(n, valid_class().len() * SUBSTITUTES.len());
}

// ---------------------------------------------------------------------------
// Family 3 — every u16 field driven to its extremes.
// ---------------------------------------------------------------------------

/// `u16::MAX` is the "flip each length field to its maximum" case from the
/// threat model: it is the largest count any `u2` table header can declare,
/// and the value that turns a 200-byte file into a 65 535-entry allocation
/// request if preallocation is not bounded by the input length. `0` is the
/// "zero each index" case: constant-pool index 0 is the reserved sentinel,
/// so every index field driven to 0 must be rejected rather than silently
/// resolving to a `Tombstone`.
const U16_EXTREMES: [u16; 3] = [0, 1, u16::MAX];

fn sweep_u16_extremes() -> usize {
    let bytes = valid_class();
    let mut count = 0usize;
    for offset in 0..bytes.len().saturating_sub(1) {
        for &value in U16_EXTREMES.iter() {
            let mut mutant = bytes.clone();
            mutant[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
            let what = format!("u16 at [{offset}] = {value}");
            let _ = drive_no_panic(&mutant, &what);
            count += 1;
        }
    }
    count
}

#[test]
fn u16_field_extremes_never_panic_and_never_exhaust_memory() {
    let n = sweep_u16_extremes();
    assert_eq!(n, (valid_class().len() - 1) * U16_EXTREMES.len());
}

// ---------------------------------------------------------------------------
// Family 4 — every u32 field driven to its extremes.
// ---------------------------------------------------------------------------

/// `u32::MAX` and `0x8000_0000` are the `attribute_length` / `code_length`
/// overflow cases: on a parser using unchecked `offset + length` these wrap
/// a bound into a small value that then passes a `<= end` test. `0x0001_0000`
/// is exactly one past `MAX_CODE_LENGTH`, the off-by-one twin of the
/// `code_length < 65536` rule.
const U32_EXTREMES: [u32; 4] = [0, 0x0001_0000, 0x8000_0000, u32::MAX];

fn sweep_u32_extremes() -> usize {
    let bytes = valid_class();
    let mut count = 0usize;
    for offset in 0..bytes.len().saturating_sub(3) {
        for &value in U32_EXTREMES.iter() {
            let mut mutant = bytes.clone();
            mutant[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            let what = format!("u32 at [{offset}] = {value:#010X}");
            let _ = drive_no_panic(&mutant, &what);
            count += 1;
        }
    }
    count
}

#[test]
fn u32_length_extremes_never_panic_and_never_exhaust_memory() {
    let n = sweep_u32_extremes();
    assert_eq!(n, (valid_class().len() - 3) * U32_EXTREMES.len());
}

// ---------------------------------------------------------------------------
// Coverage bookkeeping.
// ---------------------------------------------------------------------------

/// Pins the mutation arithmetic so the number quoted in
/// `docs/feature-designs/class-file-parser-hardening.md` cannot silently drift
/// when the seed class changes.
#[test]
fn mutation_coverage_totals_are_exact() {
    let len = valid_class().len();
    let truncations = len;
    let byte_subs = len * SUBSTITUTES.len();
    let u16_subs = (len - 1) * U16_EXTREMES.len();
    let u32_subs = (len - 3) * U32_EXTREMES.len();
    let total = truncations + byte_subs + u16_subs + u32_subs;

    // The seed is fixed, so these are constants; asserting them makes any
    // change to the seed a deliberate, visible edit rather than a silent
    // change in coverage.
    assert_eq!(len, 218, "seed class length changed — update the doc");
    assert_eq!(truncations, 218);
    assert_eq!(byte_subs, 1_090);
    assert_eq!(u16_subs, 651);
    assert_eq!(u32_subs, 860);
    assert_eq!(
        total, 2_819,
        "total mutation count changed — update docs/feature-designs/class-file-parser-hardening.md"
    );
}
