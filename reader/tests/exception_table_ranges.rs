// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression tests for JVMS §4.7.3 `exception_table` validation.
//!
//! Before the C2 parser-hardening pass the reader parsed every
//! `exception_table` entry as four raw `u16`s and stored them verbatim:
//! `start_pc`, `end_pc`, `handler_pc` and `catch_type` were never compared
//! against `code_length` or against the constant pool. `handler_pc` is a
//! **jump target** — the JIT reads it straight out of this table
//! (`jit/src/lib.rs`, `local_handler_reads_unsafe_local`) and the
//! interpreter uses it to reposition `pc` while unwinding — so an
//! attacker-chosen `handler_pc` of `0xFFFF` inside a four-byte method was
//! an out-of-range bytecode index handed to code entitled to assume the
//! parser had already rejected it.
//!
//! Every test here fails (parses successfully) without the
//! `validate_exception_range` / `validate_catch_type` checks in
//! `reader/src/attribute.rs`.
//!
//! Two paths are exercised deliberately:
//!
//! * the **PC range** checks run in `validate_attribute_shape`, on the
//!   eager `read_class` walk, so they surface as a `read_class` error;
//! * the **`catch_type`** check needs the constant pool, which the eager
//!   shape walk does not have, so it surfaces at `LazyAttribute::decode`.

use cratonvm_reader::read_class;

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Number of bytecode bytes in the method built below. Every PC in the
/// exception table is validated against this.
const CODE_LENGTH: u16 = 4;

/// Constant-pool indices in the class built by [`build_class`].
mod cp {
    /// `CONSTANT_Class` → "java/lang/Exception". A legal `catch_type`.
    pub const EXCEPTION_CLASS: u16 = 7;
    /// `CONSTANT_Utf8` → "notAClass". Present, but the wrong tag.
    pub const UTF8_NOT_A_CLASS: u16 = 8;
    /// `CONSTANT_Long`. Occupies slots 9 *and* 10.
    pub const LONG: u16 = 9;
    /// The unusable second slot of the `CONSTANT_Long` at index 9 (JVMS
    /// §4.4.5). Stored as a `Tombstone`.
    pub const LONG_SECOND_SLOT: u16 = 10;
    /// One past the last slot.
    pub const OUT_OF_BOUNDS: u16 = 11;
}

/// Build a class with one `public void m()` whose `Code` attribute holds a
/// single exception-table entry with the supplied PCs and `catch_type`.
///
/// The bytecode is `CODE_LENGTH` × `return`, so valid PCs are `0..4` and a
/// valid `end_pc` is `1..=4`.
fn build_class(start_pc: u16, end_pc: u16, handler_pc: u16, catch_type: u16) -> Vec<u8> {
    let mut d = Vec::<u8>::new();
    // magic + version 52.0
    d.extend_from_slice(&0xCAFE_BABE_u32.to_be_bytes());
    d.extend_from_slice(&0u16.to_be_bytes());
    d.extend_from_slice(&52u16.to_be_bytes());

    // constant_pool_count = 11 → real slots 1..=10.
    d.extend_from_slice(&11u16.to_be_bytes());

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
    utf8(&mut d, b"notAClass"); // 8
    d.push(5); // 9 = CONSTANT_Long, covering slots 9 and 10
    d.extend_from_slice(&1i64.to_be_bytes());

    // access ACC_PUBLIC|ACC_SUPER, this=2, super=0, no interfaces, no fields
    d.extend_from_slice(&0x0021u16.to_be_bytes());
    d.extend_from_slice(&2u16.to_be_bytes());
    d.extend_from_slice(&0u16.to_be_bytes());
    d.extend_from_slice(&0u16.to_be_bytes());
    d.extend_from_slice(&0u16.to_be_bytes());

    // methods_count = 1; public m()V with one attribute
    d.extend_from_slice(&1u16.to_be_bytes());
    d.extend_from_slice(&0x0001u16.to_be_bytes());
    d.extend_from_slice(&3u16.to_be_bytes());
    d.extend_from_slice(&4u16.to_be_bytes());
    d.extend_from_slice(&1u16.to_be_bytes());

    // Code body.
    let mut body = Vec::<u8>::new();
    body.extend_from_slice(&2u16.to_be_bytes()); // max_stack
    body.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    body.extend_from_slice(&(CODE_LENGTH as u32).to_be_bytes());
    body.extend(std::iter::repeat(0xB1).take(CODE_LENGTH as usize)); // `return` ×4
    body.extend_from_slice(&1u16.to_be_bytes()); // exception_table_length = 1
    body.extend_from_slice(&start_pc.to_be_bytes());
    body.extend_from_slice(&end_pc.to_be_bytes());
    body.extend_from_slice(&handler_pc.to_be_bytes());
    body.extend_from_slice(&catch_type.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes()); // nested attributes_count = 0

    d.extend_from_slice(&5u16.to_be_bytes()); // attribute_name_index = "Code"
    d.extend_from_slice(&(body.len() as u32).to_be_bytes());
    d.extend_from_slice(&body);

    // class attributes_count = 0
    d.extend_from_slice(&0u16.to_be_bytes());
    d
}

/// Parse, then force the method's `Code` attribute through the lazy
/// decoder. Returns the first error message from either stage.
fn parse_and_decode(bytes: &[u8]) -> Result<(), String> {
    let mut class = read_class(bytes).map_err(|e| e.to_string())?;
    let methods = &mut class.methods;
    let constant_pool = &class.constant_pool;
    methods[0].attributes[0]
        .decode(constant_pool)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Must-accept baseline. Without these a validator that rejected everything
// would pass the whole file.
// ---------------------------------------------------------------------------

#[test]
fn well_formed_exception_table_still_parses_and_decodes() {
    // start_pc=0, end_pc=code_length (explicitly legal per §4.7.3),
    // handler_pc=2, catch_type = a real CONSTANT_Class.
    parse_and_decode(&build_class(0, CODE_LENGTH, 2, cp::EXCEPTION_CLASS))
        .expect("a well-formed exception table must still be accepted");
}

#[test]
fn catch_type_zero_is_the_legal_finally_handler() {
    parse_and_decode(&build_class(0, CODE_LENGTH, 3, 0))
        .expect("catch_type == 0 is the `finally` / catch-any handler and is legal");
}

#[test]
fn boundary_values_that_are_still_legal_are_accepted() {
    // handler_pc at the last valid code index.
    parse_and_decode(&build_class(0, 1, CODE_LENGTH - 1, 0)).expect("handler_pc == code_length-1");
    // start_pc at the last valid code index, end_pc == code_length.
    parse_and_decode(&build_class(CODE_LENGTH - 1, CODE_LENGTH, 0, 0))
        .expect("start_pc == code_length-1 with end_pc == code_length");
    // The minimum-width protected range.
    parse_and_decode(&build_class(0, 1, 0, 0)).expect("a one-byte protected range is legal");
}

// ---------------------------------------------------------------------------
// handler_pc — the headline case. This is the value that becomes a jump
// target downstream.
// ---------------------------------------------------------------------------

#[test]
fn handler_pc_past_end_of_code_is_rejected() {
    let err = parse_and_decode(&build_class(0, CODE_LENGTH, CODE_LENGTH, 0))
        .expect_err("handler_pc == code_length is one past the last instruction");
    assert!(
        err.contains("handler_pc") && err.contains("4.7.3"),
        "the rejection must name handler_pc and the clause it violates; got: {err}"
    );
}

#[test]
fn handler_pc_of_u16_max_is_rejected() {
    let err = parse_and_decode(&build_class(0, CODE_LENGTH, u16::MAX, 0))
        .expect_err("handler_pc = 0xFFFF in a 4-byte method must be rejected");
    assert!(err.contains("handler_pc"), "got: {err}");
}

// ---------------------------------------------------------------------------
// start_pc / end_pc ordering and range.
// ---------------------------------------------------------------------------

#[test]
fn start_pc_equal_to_code_length_is_rejected() {
    // §4.7.3 allows end_pc == code_length but NOT start_pc == code_length:
    // start_pc must index an actual instruction.
    let err = parse_and_decode(&build_class(CODE_LENGTH, CODE_LENGTH, 0, 0))
        .expect_err("start_pc == code_length is not a valid index into the code array");
    assert!(err.contains("start_pc"), "got: {err}");
}

#[test]
fn start_pc_of_u16_max_is_rejected() {
    let err = parse_and_decode(&build_class(u16::MAX, u16::MAX, 0, 0))
        .expect_err("start_pc = 0xFFFF must be rejected");
    assert!(err.contains("start_pc"), "got: {err}");
}

#[test]
fn end_pc_past_code_length_is_rejected() {
    let err = parse_and_decode(&build_class(0, CODE_LENGTH + 1, 0, 0))
        .expect_err("end_pc > code_length must be rejected");
    assert!(err.contains("end_pc"), "got: {err}");
}

#[test]
fn empty_protected_range_is_rejected() {
    // start_pc == end_pc: §4.7.3 requires start_pc < end_pc strictly.
    let err = parse_and_decode(&build_class(2, 2, 0, 0))
        .expect_err("start_pc == end_pc is an empty protected range");
    assert!(
        err.contains("must be less than"),
        "the rejection must name the ordering constraint; got: {err}"
    );
}

#[test]
fn inverted_protected_range_is_rejected() {
    // start_pc > end_pc. An unchecked consumer computing `end_pc - start_pc`
    // would underflow here.
    let err = parse_and_decode(&build_class(3, 1, 0, 0))
        .expect_err("start_pc > end_pc is an inverted protected range");
    assert!(err.contains("must be less than"), "got: {err}");
}

// ---------------------------------------------------------------------------
// catch_type — the four distinct constant-pool index hazards.
// ---------------------------------------------------------------------------

#[test]
fn catch_type_out_of_bounds_is_rejected() {
    let err = parse_and_decode(&build_class(0, CODE_LENGTH, 0, cp::OUT_OF_BOUNDS))
        .expect_err("catch_type past the end of the pool must be rejected");
    assert!(
        err.contains("catch_type") && err.contains("CONSTANT_Class"),
        "got: {err}"
    );
}

#[test]
fn catch_type_pointing_at_the_wrong_tag_is_rejected() {
    // The entry exists and the index is in range — it is simply a Utf8
    // rather than a Class. This is tag confusion, distinct from an
    // out-of-bounds index.
    let err = parse_and_decode(&build_class(0, CODE_LENGTH, 0, cp::UTF8_NOT_A_CLASS))
        .expect_err("catch_type must point at a CONSTANT_Class, not a CONSTANT_Utf8");
    assert!(err.contains("catch_type"), "got: {err}");
}

#[test]
fn catch_type_pointing_at_a_long_is_rejected() {
    let err = parse_and_decode(&build_class(0, CODE_LENGTH, 0, cp::LONG))
        .expect_err("catch_type must not point at a CONSTANT_Long");
    assert!(err.contains("catch_type"), "got: {err}");
}

/// The distinct JVMS §4.4.5 hazard: index 10 is *inside* the pool and is
/// not the `Long` itself — it is the `Long`'s unusable second slot. A
/// parser that only range-checks `1 <= index < count` accepts this.
#[test]
fn catch_type_pointing_at_the_second_slot_of_a_long_is_rejected() {
    let err = parse_and_decode(&build_class(0, CODE_LENGTH, 0, cp::LONG_SECOND_SLOT))
        .expect_err("catch_type must not point at the second slot of a category-2 entry");
    assert!(err.contains("catch_type"), "got: {err}");
}

// ---------------------------------------------------------------------------
// The checks must survive both entry points into the decoder, since
// `decode_attribute` is `pub` and reachable without the eager shape walk.
// ---------------------------------------------------------------------------

#[test]
fn bad_handler_pc_is_rejected_by_the_standalone_attribute_decoder_too() {
    use cratonvm_reader::attribute::validate_attribute_shape;

    let mut body = Vec::<u8>::new();
    body.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    body.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    body.extend_from_slice(&1u32.to_be_bytes()); // code_length = 1
    body.push(0xB1); // return
    body.extend_from_slice(&1u16.to_be_bytes()); // exception_table_length
    body.extend_from_slice(&0u16.to_be_bytes()); // start_pc
    body.extend_from_slice(&1u16.to_be_bytes()); // end_pc == code_length
    body.extend_from_slice(&999u16.to_be_bytes()); // handler_pc — way out of range
    body.extend_from_slice(&0u16.to_be_bytes()); // catch_type
    body.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

    let err = validate_attribute_shape("Code", &body)
        .expect_err("the shape walk must reject an out-of-range handler_pc on its own");
    assert!(err.to_string().contains("handler_pc"), "got: {err}");
}
