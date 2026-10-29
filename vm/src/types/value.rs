// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Re-export from cratonvm-types crate.
pub use cratonvm_types::{
    decode_value, encode_value, is_object_tag, jlong_bits_as_aligned_object_ptr, CompactTag,
    CompactValue, ObjectRef, Value, VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL,
    VTAG_OBJECT, VTAG_RETADDR, VTAG_UNINIT,
};
