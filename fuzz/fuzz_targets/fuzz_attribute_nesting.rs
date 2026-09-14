// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for **recursive class-file attribute nesting** — the
//! stack-overflow / unbounded-recursion surface of the attribute decoders.
//!
//! Three mutually recursive grammars live in `reader/src/attribute.rs`:
//!
//!   * `decode_attribute_body` ⇄ `decode_attributes_vec` ⇄
//!     `decode_code_body` — a `Code` attribute's body carries its own
//!     attribute table, so `Code`-in-`Code` (and `Record`-in-`Record`)
//!     nests without limit on the wire. Bounded by the private
//!     `MAX_ATTRIBUTE_DEPTH` (`reader/src/attribute.rs:1106`, whose
//!     canonical value lives in `reader/src/limits.rs`).
//!   * `decode_annotation_depth` ⇄ `decode_element_value_depth` — an
//!     `element_value` with tag `@` holds a whole nested annotation, and
//!     tag `[` holds an array of element values. Bounded by the private
//!     `MAX_ANNOTATION_DEPTH` (`reader/src/attribute.rs:1892`).
//!   * `decode_type_annotation` → `decode_type_path` → `decode_annotation`
//!     — `RuntimeVisible/InvisibleTypeAnnotations` wrap the same annotation
//!     recursion behind a `u8`-counted `type_path`.
//!
//! Both caps are private to the `reader` crate, so this target does not
//! assert their exact values. It asserts the property that matters: a nest
//! far past any plausible cap must return `Err`, and — crucially — a
//! *shallow* nest built by the same generator must return `Ok`. Without
//! that positive control the "deep nest errors" assertion would pass
//! vacuously if the generator ever emitted malformed bodies.
//!
//! Surface under test:
//!   * `cratonvm_reader::decode_attribute` (`reader/src/attribute.rs:1015`)
//!   * `cratonvm_reader::attribute::validate_attribute_shape`
//!     (`reader/src/attribute.rs:874`)
//!
//! Per-input layout:
//!   * byte 0 — selects which attribute name the raw-bytes phase feeds to
//!     `decode_attribute` / `validate_attribute_shape`.
//!   * bytes 1.. — the raw attribute body (panic-only oracle).
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_attribute_nesting

#![no_main]

use std::sync::{Arc, Once};

use libfuzzer_sys::fuzz_target;

use cratonvm_reader::attribute::validate_attribute_shape;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// `attribute_length` is a `u32`, but the recursion this target probes is
/// depth-driven, not size-driven: a 64 KiB body already admits thousands of
/// nesting levels. Keeping the cap low keeps each iteration fast so the
/// fuzzer spends its budget on shapes rather than on memcpy.
const MAX_INPUT: usize = 64 * 1024;

/// Nesting level used for the "must be rejected" direction. Far past both
/// `MAX_ATTRIBUTE_DEPTH` (16) and any depth a real compiler emits, while
/// still producing a body of roughly 1 KiB.
const ATTRIBUTE_NEST_DEEP: usize = 64;

/// Nesting level used for the positive control. Comfortably inside
/// `MAX_ATTRIBUTE_DEPTH`, so a well-formed body at this depth must decode.
const ATTRIBUTE_NEST_SHALLOW: usize = 2;

/// Annotation nesting level for the "must be rejected" direction. Each
/// wire level costs two depth steps (annotation → element_value →
/// annotation), so this is roughly 800 against a cap of 256.
const ANNOTATION_NEST_DEEP: usize = 400;

/// Annotation nesting level for the positive control (16 depth steps).
const ANNOTATION_NEST_SHALLOW: usize = 8;

/// 1-based constant-pool index of the `Utf8` entry holding `"Code"`.
/// `decode_attributes_vec` resolves every *nested* attribute's name
/// through the pool, so a nested `Code` needs this index on the wire.
const CP_CODE: u16 = 1;

/// A `Utf8` index used wherever the grammar only needs *some* u16 that the
/// decoder does not dereference (annotation `type_index`,
/// `element_name_index`).
const CP_ANY: u16 = 1;

/// Attribute names the raw-bytes phase cycles through. Every one of these
/// either recurses itself or embeds a counted sub-table, so a miscounted
/// length shows up as an over-read rather than as silent truncation.
const RECURSIVE_ATTRIBUTE_NAMES: [&str; 8] = [
    "Code",
    "Record",
    "RuntimeVisibleAnnotations",
    "RuntimeInvisibleAnnotations",
    "RuntimeVisibleTypeAnnotations",
    "RuntimeInvisibleTypeAnnotations",
    "RuntimeVisibleParameterAnnotations",
    "AnnotationDefault",
];

/// Constant pool just rich enough for nested-attribute name resolution.
/// `decode_attribute_body` dispatches on the interned name via
/// `Arc::ptr_eq` with a `&**name` string fallback, so a plain `Arc::from`
/// still routes to the right body decoder.
fn nesting_constant_pool() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(Arc::from("Code")),
        ConstantPoolEntry::Utf8(Arc::from("Record")),
        ConstantPoolEntry::Utf8(Arc::from("LineNumberTable")),
        ConstantPoolEntry::Utf8(Arc::from("StackMapTable")),
        ConstantPoolEntry::Utf8(Arc::from("RuntimeVisibleAnnotations")),
    ])
}

/// One `Code_attribute` body (JVMS §4.7.3), optionally carrying exactly one
/// nested attribute whose name index points at `"Code"`.
///
/// Layout: `u2 max_stack; u2 max_locals; u4 code_length; u1 code[];
/// u2 exception_table_length; u2 attributes_count; attribute_info[]`.
fn code_body(inner: Option<&[u8]>) -> Vec<u8> {
    let inner_len = inner.map_or(0, |i| i.len());
    let mut body = Vec::with_capacity(13 + inner_len + 6);
    body.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    body.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    body.extend_from_slice(&1u32.to_be_bytes()); // code_length (must be >= 1)
    body.push(0xB1); // `return`
    body.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    match inner {
        None => body.extend_from_slice(&0u16.to_be_bytes()),
        Some(i) => {
            body.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
            body.extend_from_slice(&CP_CODE.to_be_bytes()); // attribute_name_index
            body.extend_from_slice(&(i.len() as u32).to_be_bytes()); // attribute_length
            body.extend_from_slice(i);
        }
    }
    body
}

/// A `Code` body nested `levels` deep. `levels == 0` is a leaf `Code` with
/// an empty nested attribute table.
fn nested_code_body(levels: usize) -> Vec<u8> {
    let mut body = code_body(None);
    for _ in 0..levels {
        body = code_body(Some(&body));
    }
    body
}

/// A `RuntimeVisibleAnnotations` body holding one annotation nested
/// `levels` deep through `element_value` tag `@`.
///
/// Built as a flat prefix rather than by recursion so the *harness* never
/// recurses: every nesting level contributes the same seven bytes
/// (`type_index`, `num_element_value_pairs = 1`, `element_name_index`,
/// tag `@`) and the innermost annotation terminates with
/// `num_element_value_pairs = 0`.
fn nested_annotations_body(levels: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + levels * 7 + 4);
    body.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
    for _ in 0..levels {
        body.extend_from_slice(&CP_ANY.to_be_bytes()); // annotation.type_index
        body.extend_from_slice(&1u16.to_be_bytes()); // num_element_value_pairs
        body.extend_from_slice(&CP_ANY.to_be_bytes()); // element_name_index
        body.push(b'@'); // element_value tag: nested annotation
    }
    body.extend_from_slice(&CP_ANY.to_be_bytes()); // innermost type_index
    body.extend_from_slice(&0u16.to_be_bytes()); // innermost pair count
    body
}

/// A `RuntimeVisibleTypeAnnotations` body: one `type_annotation` with
/// `target_type = 0x13` (empty `target_info`), a `type_path` of
/// `path_length` entries, and an annotation nested `levels` deep.
fn nested_type_annotations_body(path_length: u8, levels: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + 2 * path_length as usize + levels * 7 + 4);
    body.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
    body.push(0x13); // target_type with a zero-length target_info
    body.push(path_length); // type_path.path_length
    for i in 0..path_length {
        body.push(i % 4); // type_path_kind
        body.push(0); // type_argument_index
    }
    // The annotation itself, minus the `num_annotations` prefix that
    // `nested_annotations_body` adds.
    body.extend_from_slice(&nested_annotations_body(levels)[2..]);
    body
}

/// Bounded-recursion checks, asserted in both directions.
///
/// The `_shallow` calls are the positive controls: they prove the
/// generators emit well-formed bodies, which is what makes the matching
/// `_deep` rejections meaningful rather than vacuous.
///
/// None of this depends on the fuzzer's bytes, so it runs once per process
/// rather than per iteration — the generators are O(n²) in nesting depth
/// and would otherwise eat the whole execution budget on constant work. A
/// regressed cap still surfaces: libFuzzer reports the process-start
/// assertion failure (or stack overflow) exactly the same way.
fn depth_limits_self_test() {
    let cp = nesting_constant_pool();

    // `Code`-in-`Code`.
    assert!(
        cratonvm_reader::decode_attribute("Code", &nested_code_body(ATTRIBUTE_NEST_SHALLOW), &cp)
            .is_ok(),
        "generator emitted a malformed Code body at depth {ATTRIBUTE_NEST_SHALLOW}; \
         the deep-nesting assertions below would be vacuous"
    );
    let deep_code = nested_code_body(ATTRIBUTE_NEST_DEEP);
    assert!(
        cratonvm_reader::decode_attribute("Code", &deep_code, &cp).is_err(),
        "Code nested {ATTRIBUTE_NEST_DEEP} deep must be rejected by the \
         attribute-depth cap, not decoded"
    );
    // The same body must be *shape-checkable* without recursing:
    // `validate_attribute_shape` walks nested `Code` headers iteratively
    // and must terminate at any depth.
    let _ = validate_attribute_shape("Code", &deep_code);

    // Annotation / element_value mutual recursion.
    assert!(
        cratonvm_reader::decode_attribute(
            "RuntimeVisibleAnnotations",
            &nested_annotations_body(ANNOTATION_NEST_SHALLOW),
            &cp,
        )
        .is_ok(),
        "generator emitted a malformed annotation at depth {ANNOTATION_NEST_SHALLOW}"
    );
    assert!(
        cratonvm_reader::decode_attribute(
            "RuntimeVisibleAnnotations",
            &nested_annotations_body(ANNOTATION_NEST_DEEP),
            &cp,
        )
        .is_err(),
        "annotations nested {ANNOTATION_NEST_DEEP} deep must be rejected by the \
         annotation-depth cap, not decoded"
    );

    // TypeAnnotation: a full 255-entry `type_path` backed by 510 real
    // bytes is legal and must decode; the same attribute wrapping an
    // over-deep annotation must not.
    assert!(
        cratonvm_reader::decode_attribute(
            "RuntimeVisibleTypeAnnotations",
            &nested_type_annotations_body(u8::MAX, ANNOTATION_NEST_SHALLOW),
            &cp,
        )
        .is_ok(),
        "a 255-entry type_path with matching bytes is well-formed and must decode"
    );
    assert!(
        cratonvm_reader::decode_attribute(
            "RuntimeVisibleTypeAnnotations",
            &nested_type_annotations_body(u8::MAX, ANNOTATION_NEST_DEEP),
            &cp,
        )
        .is_err(),
        "type annotations nested {ANNOTATION_NEST_DEEP} deep must be rejected"
    );

    // A `type_path` that declares 255 entries with no bytes behind it must
    // fail rather than read past the attribute body.
    let mut truncated_path = Vec::new();
    truncated_path.extend_from_slice(&1u16.to_be_bytes()); // num_annotations
    truncated_path.push(0x13); // target_type, empty target_info
    truncated_path.push(u8::MAX); // path_length, unbacked
    assert!(
        cratonvm_reader::decode_attribute("RuntimeVisibleTypeAnnotations", &truncated_path, &cp)
            .is_err(),
        "an unbacked 255-entry type_path must be rejected, not over-read"
    );
}

static SELF_TEST: Once = Once::new();

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > MAX_INPUT {
        return;
    }

    SELF_TEST.call_once(depth_limits_self_test);

    let cp = nesting_constant_pool();

    // -------------------------------------------------------------------
    // Raw fuzzed bodies. Panic-only oracle: an arbitrary body is allowed
    // to be `Err`, but must never panic, over-read, or recurse off the
    // stack.
    // -------------------------------------------------------------------
    let name = RECURSIVE_ATTRIBUTE_NAMES[data[0] as usize % RECURSIVE_ATTRIBUTE_NAMES.len()];
    let body = &data[1..];

    // Shape pre-check (used by the class reader before lazy decode) and
    // the full decoder both see the same bytes.
    let _ = validate_attribute_shape(name, body);
    let _ = cratonvm_reader::decode_attribute(name, body, &cp);

    // Also feed the raw body as a nested attribute inside a well-formed
    // `Code`, which is the shape a real class file would carry it in and
    // routes the bytes through `decode_attributes_vec`'s length accounting
    // rather than through the top-level entry point.
    // `MAX_INPUT` already keeps `body.len()` far inside the u32 the
    // attribute header can express.
    let mut wrapper = Vec::with_capacity(13 + 6 + body.len());
    wrapper.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    wrapper.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    wrapper.extend_from_slice(&1u32.to_be_bytes()); // code_length
    wrapper.push(0xB1);
    wrapper.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    wrapper.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
    wrapper.extend_from_slice(&CP_CODE.to_be_bytes());
    wrapper.extend_from_slice(&(body.len() as u32).to_be_bytes());
    wrapper.extend_from_slice(body);
    let _ = cratonvm_reader::decode_attribute("Code", &wrapper, &cp);
});
