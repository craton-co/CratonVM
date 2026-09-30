// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Large-file synthesis test — exercises the reader against a hand-crafted
//! class file pushed close to the JVMS §4.1 maxima:
//!
//!   * constant pool near `u16::MAX` entries,
//!   * a `Code` attribute with `code_length == 65535` (the JVMS §4.7.3 cap),
//!   * 65535 class-level attributes.
//!
//! The point is to confirm the reader (a) handles these without OOM,
//! pre-allocation panic, or stack overflow, and (b) returns the right
//! counts. The test deliberately avoids real opcode data — the bytecode
//! payload is `nop * 65534 + return` (0x00 padding + 0xB1) which is
//! syntactically valid and skips through any later validation.
//!
//! Reader gap §2.3-1 from `.claude/review-2026-05-24/reader.md`.

use cratonvm_reader::read_class;

const MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];
/// Java 8 (major 52). Anything in the supported range works; 52 is the
/// most-common bootstrap target.
const VERSION_MAJOR: u16 = 52;
const VERSION_MINOR: u16 = 0;

/// Constants used as constant-pool indices and as attribute name lookups.
const UTF8_OBJECT_IDX: u16 = 1;
const CLASS_THIS_IDX: u16 = 2;
const UTF8_CODE_IDX: u16 = 3;
const UTF8_DEPRECATED_IDX: u16 = 4;
const UTF8_M_IDX: u16 = 5;
const UTF8_SIG_V_IDX: u16 = 6;

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn push_utf8(out: &mut Vec<u8>, s: &[u8]) {
    out.push(1); // CONSTANT_Utf8
    push_u16(out, s.len() as u16);
    out.extend_from_slice(s);
}

/// Build a constant pool with `cp_count` declared entries. The first six
/// indices carry the named entries above; everything after is padding
/// (`Utf8` with a unique short content) so the declared count actually
/// matches the parsed entries.
fn build_constant_pool(cp_count: u16) -> Vec<u8> {
    assert!(
        cp_count >= 7,
        "test wants at least the 6 fixed entries + slot 0"
    );
    let mut out = Vec::with_capacity(cp_count as usize * 8);
    push_u16(&mut out, cp_count);

    // Entries are 1-indexed; the 0-th sentinel is implicit (the reader
    // pushes it as a tombstone), so we emit entries 1..cp_count.
    push_utf8(&mut out, b"java/lang/Object"); // 1
    out.push(7); // CONSTANT_Class @ 2
    push_u16(&mut out, UTF8_OBJECT_IDX);
    push_utf8(&mut out, b"Code"); // 3
    push_utf8(&mut out, b"Deprecated"); // 4 — a 0-length attribute we can repeat
    push_utf8(&mut out, b"m"); // 5
    push_utf8(&mut out, b"()V"); // 6

    // Padding entries — short unique Utf8 strings.
    //
    // Index 0 was implicit; entries 1..=6 above; emit 7..cp_count.
    // `cp_count` is declared, so we emit `cp_count - 7` more entries to
    // hit the right count.
    for i in 7..cp_count {
        // Encode i as 5-byte ASCII like "00007" to keep parsing simple.
        let s = format!("{:05}", i);
        push_utf8(&mut out, s.as_bytes());
    }
    out
}

/// Build a method whose `Code` attribute has the maximum legal code_length.
fn build_method_with_max_code() -> Vec<u8> {
    let mut out = Vec::with_capacity(65_600);

    // Method shell: access(u2)=PUBLIC, name_index, descriptor_index, attributes_count=1
    push_u16(&mut out, 0x0001); // ACC_PUBLIC
    push_u16(&mut out, UTF8_M_IDX);
    push_u16(&mut out, UTF8_SIG_V_IDX);
    push_u16(&mut out, 1); // 1 attribute: Code

    // Code attribute header
    push_u16(&mut out, UTF8_CODE_IDX);
    // attribute_length placeholder — we backpatch after building the body.
    let attr_len_pos = out.len();
    push_u32(&mut out, 0);

    let body_start = out.len();
    // max_stack, max_locals
    push_u16(&mut out, 1);
    push_u16(&mut out, 1);
    // code_length = 65535 — the JVMS §4.7.3 maximum.
    const CODE_LENGTH: u32 = 65_535;
    push_u32(&mut out, CODE_LENGTH);
    // code = (CODE_LENGTH - 1) * `nop` + 1 * `return`
    out.extend(std::iter::repeat(0x00u8).take(CODE_LENGTH as usize - 1));
    out.push(0xB1); // return
                    // exception_table_length, attributes_count both zero.
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);

    // Backpatch attribute_length.
    let body_len = (out.len() - body_start) as u32;
    out[attr_len_pos..attr_len_pos + 4].copy_from_slice(&body_len.to_be_bytes());

    out
}

/// Build a class file with the requested CP size and class-level attribute
/// count. The class has one method whose Code attribute hits the JVMS
/// length cap, and `class_attr_count` class-level Deprecated attributes
/// (zero-length attribute payloads — cheap to synthesize but exercise
/// the attributes loop).
fn build_large_class(cp_count: u16, class_attr_count: u16) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&MAGIC);
    push_u16(&mut data, VERSION_MINOR);
    push_u16(&mut data, VERSION_MAJOR);

    data.extend(build_constant_pool(cp_count));

    push_u16(&mut data, 0x0021); // ACC_PUBLIC | ACC_SUPER
    push_u16(&mut data, CLASS_THIS_IDX); // this_class
    push_u16(&mut data, 0); // super_class = 0 (java/lang/Object semantics)
    push_u16(&mut data, 0); // interfaces_count
    push_u16(&mut data, 0); // fields_count
    push_u16(&mut data, 1); // methods_count
    data.extend(build_method_with_max_code());

    // Class-level attributes: `class_attr_count` Deprecated attributes
    // (each is 6 bytes: name_index(2) + length(4)=0; no body).
    push_u16(&mut data, class_attr_count);
    for _ in 0..class_attr_count {
        push_u16(&mut data, UTF8_DEPRECATED_IDX);
        push_u32(&mut data, 0);
    }

    data
}

#[test]
fn reader_handles_constant_pool_near_u16_max() {
    // u16::MAX is 65535 — pick a value comfortably large but a touch
    // under the absolute cap so the test stays robust against any
    // off-by-one in the upper-bound check. The point is "no OOM /
    // pre-alloc panic on a near-max CP", not exact-equals-u16::MAX.
    let cp_count: u16 = 60_000;
    let bytes = build_large_class(cp_count, 0);
    let class_file = read_class(&bytes).expect("near-max constant pool must parse successfully");
    // entries.len() == cp_count (0-th sentinel + cp_count-1 real entries).
    assert_eq!(
        class_file.constant_pool.len(),
        cp_count as usize,
        "constant pool entry vector length must equal declared count"
    );
}

#[test]
fn reader_accepts_code_length_at_jvms_cap() {
    // code_length == 65535 is the JVMS §4.7.3 upper bound; the reader
    // must accept it and the produced bytecode view must report the
    // right length.
    let bytes = build_large_class(7, 0);
    let mut class_file = read_class(&bytes).expect("class with max-legal code_length must parse");
    assert_eq!(class_file.methods.len(), 1);
    // Split borrow: `methods` and `constant_pool` are independent fields, so
    // `force_decode_all` over the method's attributes can take `&mut` while
    // the constant pool is borrowed immutably.
    cratonvm_reader::force_decode_all(
        &mut class_file.methods[0].attributes,
        &class_file.constant_pool,
    )
    .expect("Code attribute must decode successfully");
    let code = class_file.methods[0]
        .code()
        .expect("decoded method must have Code attribute");
    assert_eq!(
        code.code.len(),
        65_535,
        "Code.code view must report code_length == 65535"
    );
}

#[test]
fn reader_handles_max_class_level_attributes() {
    // 65535 class-level attributes — the u16 cap. Each is a zero-length
    // Deprecated attribute, so the parse cost is bounded.
    let class_attr_count: u16 = 65_535;
    let bytes = build_large_class(7, class_attr_count);
    let class_file = read_class(&bytes).expect("class with 65535 attributes must parse");
    assert_eq!(
        class_file.attributes.len(),
        class_attr_count as usize,
        "all 65535 attributes must be present in the attribute table"
    );
}
