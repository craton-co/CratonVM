// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for `StackMapTable` attribute decoding.
//!
//! Surface under test:
//!   * `cratonvm_reader::stack_map::StackMapTable::parse` — decodes the
//!     attribute body (everything *after* `attribute_name_index` and
//!     `attribute_length`). The grammar is a mix of single-byte frame
//!     types (SAME / SAME_LOCALS_1_STACK_ITEM) and length-prefixed
//!     variants (APPEND / FULL_FRAME) plus per-verification-type tags
//!     for `Object_variable_info` (cpool index) and
//!     `Uninitialized_variable_info` (bytecode offset).
//!
//! The verifier in HotSpot historically had several bugs here (e.g.
//! CVE-2017-3289). The parser must reject all malformed frames cleanly;
//! a panic on any input is a CVE-class defect.
//!
//! After a successful parse we also call `StackMapTable::absolute_offsets`
//! — it does an additive walk over `offset_delta`s and is a candidate
//! site for u16 overflow on adversarial input. It must return an `Err`
//! (`InvalidClassData`) rather than panic when the running absolute
//! offset overshoots `u16::MAX`.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_stack_map

#![no_main]

use libfuzzer_sys::fuzz_target;

/// `Code.code_length` is bounded by u32 but in practice `code` is below
/// 64 KiB (HotSpot rejects larger). The StackMapTable can never be much
/// larger than `code` itself. 256 KiB is a generous cap.
const MAX_INPUT: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    let Ok(table) = cratonvm_reader::stack_map::StackMapTable::parse(data) else {
        return;
    };

    // Touch every parsed frame to make sure no `Vec` was over-allocated
    // via an unchecked count. Cheap and detects miscounts via the
    // bounded-iter contract.
    let _ = table.entries.len();
    for frame in &table.entries {
        // Debug-format the frame; this dereferences every nested
        // verification-type tag (Object cpool index, Uninitialized
        // bytecode offset) and surfaces any out-of-band data.
        let _ = format!("{:?}", frame);
    }

    // Exercise the additive offset walk on every parsed table. This is
    // the u16-overflow candidate site flagged in the module docstring:
    // a malformed frame sequence whose running absolute offset exceeds
    // `u16::MAX` must yield `Err(InvalidClassData)`, never a panic.
    let _ = table.absolute_offsets();
});
