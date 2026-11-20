use super::*;
// =======================================================================
// Phase G (RG.1 .. RG.12) — JIT / interpreter correctness invariants
//
// These tests pin the shape of every Phase G item: either the JIT accepts
// and lowers the opcode itself, or it must cleanly bail so the interpreter
// handles it correctly. Each test is self-contained and does not require
// the full VM to run.
// =======================================================================

use crate::x64::{is_jit_compatible, jit_scan};
/// A synthetic array for the unit tests of other modules (round 16, lane
/// matrix): the length at `ARRAY_LENGTH_OFFSET`, `width`-byte elements from
/// `HEADER_SIZE` (each element's low `width` bytes, little endian), which is all
/// compiled array code reads. It lives here rather than beside its callers so
/// `layout_const_inventory` keeps counting machine-code emission sites in
/// `ir_lower.rs`, not test fixtures. The `#[cfg(test)]` is redundant for the
/// compiler (this whole file is a test module) but tells `no_test_only_public_api`,
/// which reads `src/**.rs` outside `#[cfg(test)]` regions as production code.
#[cfg(test)]
pub(crate) fn synthetic_array(width: usize, elems: &[i64]) -> Vec<u8> {
    use cratonvm_types::{ARRAY_LENGTH_OFFSET, HEADER_SIZE};
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * width];
    // Cast: a test array's length fits u32.
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * width;
        buf[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }
    buf
}

/// A synthetic `java/lang/String` field layout for the unit tests of other
/// modules (nio20 wave 3, lane L2): `value` / `hash` / `coder` in the first
/// three compact words and in the first three legacy cells, `coder` one byte
/// in the compact layout. It lives here for `synthetic_array`'s reason: so
/// `layout_const_inventory` keeps counting machine-code emission sites in
/// `ir_lower.rs`, not test fixtures.
#[cfg(test)]
pub(crate) fn synthetic_string_layout(string_class_id: u32) -> super::StringFieldLayout {
    use cratonvm_types::{
        FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET, HEADER_SIZE, SLOT_SIZE,
    };
    let cell = |i: usize| (HEADER_SIZE + i * SLOT_SIZE) as i32; // Cast: small test offsets
    let hs = HEADER_SIZE as i32; // Cast: small header constant
    super::StringFieldLayout {
        value_field_index: 0,
        value_compact_offset: hs,
        value_compact_is_narrow: false,
        value_legacy_offset: cell(0) + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: small header constant
        hash_field_index: 1,
        hash_compact_offset: hs + 8,
        hash_legacy_offset: cell(1) + FIELD_CELL_PAYLOAD32_OFFSET as i32, // Cast: small header constant
        has_coder: true,
        coder_field_index: 2,
        coder_compact_offset: hs + 12,
        coder_legacy_offset: cell(2) + FIELD_CELL_PAYLOAD32_OFFSET as i32, // Cast: small header constant
        coder_compact_is_byte: true,
        string_class_id,
        builder: None,
        array_list: None,
        hash_is_zero_compact_offset: None,
        locale_language: None,
    }
}

/// A synthetic `java/lang/StringBuilder` layout for `ir_lower`'s prefix tests
/// (nio20 wave 4, lane L5), for `synthetic_string_layout`'s reason: `value` /
/// `count` / `coder` in the first compact words and the first three legacy
/// cells, `coder` one byte in the compact layout.
#[cfg(test)]
pub(crate) fn synthetic_string_builder_layout(class_id: u32) -> super::StringBuilderFieldLayout {
    use cratonvm_types::{
        FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET, HEADER_SIZE, SLOT_SIZE,
    };
    let cell = |i: usize| (HEADER_SIZE + i * SLOT_SIZE) as i32; // Cast: small test offsets
    let hs = HEADER_SIZE as i32; // Cast: small header constant
    super::StringBuilderFieldLayout {
        count_field_index: 1,
        count_compact_offset: hs + 8,
        count_legacy_offset: cell(1) + FIELD_CELL_PAYLOAD32_OFFSET as i32, // Cast: small header constant
        value_field_index: 0,
        value_compact_offset: hs,
        value_legacy_offset: cell(0) + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: small header constant
        value_compact_is_narrow: false,
        coder_field_index: 2,
        coder_compact_offset: hs + 12,
        coder_legacy_offset: cell(2) + FIELD_CELL_PAYLOAD32_OFFSET as i32, // Cast: small header constant
        coder_compact_is_byte: true,
        class_id,
        maybe_latin1_compact_offset: None,
        maybe_latin1_false_is_exact: false,
    }
}


mod parity_probe_method;
#[allow(unused_imports)]
pub use parity_probe_method::*;
#[allow(unused_imports)]
use parity_probe_method::*;
mod ir_fp_wiring_routes_through_ir_only;
#[allow(unused_imports)]
pub use ir_fp_wiring_routes_through_ir_only::*;
#[allow(unused_imports)]
use ir_fp_wiring_routes_through_ir_only::*;
mod clear_all_releases_committed;
#[allow(unused_imports)]
pub use clear_all_releases_committed::*;
#[allow(unused_imports)]
use clear_all_releases_committed::*;
