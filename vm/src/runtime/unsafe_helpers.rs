// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.2 — VM-side facade for `sun.misc.Unsafe` support.
//!
//! The authoritative arena / cleaner state used by
//! `Unsafe.{allocateMemory,freeMemory,get/putByte,invokeCleaner,…}`
//! lives in the `native-builtins` crate (see
//! `native-builtins/src/lib.rs` — the `unsafe_arena` submodule), because
//! the crate graph is `vm -> native-builtins` and a reverse edge would
//! create a cycle.
//!
//! This module provides a *VM-side* helper surface that needs to do
//! Unsafe-ish work without going through the native bridge — the
//! classic use cases are shutdown-time arena cleanup (GC / JIT
//! teardown) and raw byte-buffer operations that the interpreter
//! wants to perform inline. For now that surface is small: the
//! helpers below just give the interpreter convenience methods to
//! reason about Unsafe operations without reaching into the private
//! store.
//!
//! If the interpreter needs to share arena state with the native-level
//! store in the future (e.g. for defragmentation or stats reporting),
//! we'll add a `LocalUnsafeArena` trait in `native-api` that both
//! sides implement against the singleton in `native-builtins`.

// ---------------------------------------------------------------------------
// Slot-based Unsafe offset conversions
// ---------------------------------------------------------------------------
//
// Unsafe uses byte offsets that the real JDK computes via
// `(i << ASHIFT) + ABASE`. Our VM uses slot indices. These helpers
// convert between the two so interpreter / JIT fast paths don't
// duplicate the arithmetic that the native bridge already has.

/// `arrayBaseOffset` per the VM's slot layout. Must match the value
/// `native-builtins::native_unsafe_array_base_offset` returns.
pub const ABASE: usize = 16;

/// Convert a byte offset into an element index for the given `scale`.
/// `scale` is the per-element byte size (1 for byte/bool, 2 for
/// short/char, 4 for int/float, 8 for long/double/ref).
///
/// Matches `unsafe_array_index_from_offset` in native-builtins.
pub fn byte_offset_to_index(byte_offset: usize, scale: usize) -> usize {
    let scale = scale.max(1);
    byte_offset.saturating_sub(ABASE) / scale
}

/// Convert an element index to a byte offset.
pub fn index_to_byte_offset(idx: usize, scale: usize) -> usize {
    ABASE.saturating_add(idx.saturating_mul(scale.max(1)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_offset_roundtrip() {
        // int[] at index 3 -> byte offset 16 + 3*4 = 28
        assert_eq!(index_to_byte_offset(3, 4), 28);
        assert_eq!(byte_offset_to_index(28, 4), 3);
    }

    #[test]
    fn long_array_roundtrip() {
        // long[] at index 2 -> 16 + 2*8 = 32
        assert_eq!(index_to_byte_offset(2, 8), 32);
        assert_eq!(byte_offset_to_index(32, 8), 2);
    }

    #[test]
    fn byte_array_roundtrip() {
        // byte[] at index 42 -> 16 + 42*1 = 58
        assert_eq!(index_to_byte_offset(42, 1), 58);
        assert_eq!(byte_offset_to_index(58, 1), 42);
    }

    #[test]
    fn ref_array_scale_eight() {
        // Object[] uses scale=8 on 64-bit
        assert_eq!(index_to_byte_offset(7, 8), 16 + 7 * 8);
    }

    #[test]
    fn byte_offset_below_base_yields_zero() {
        // Defensive: offset < ABASE should not underflow
        assert_eq!(byte_offset_to_index(0, 4), 0);
        assert_eq!(byte_offset_to_index(8, 4), 0);
    }

    #[test]
    fn scale_zero_treats_as_one() {
        assert_eq!(index_to_byte_offset(5, 0), 16 + 5);
    }
}
