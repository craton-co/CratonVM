// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Minimal FxHasher + type aliases for hot-path caches in classloading.
//!
//! Mirrors the implementation in `vm/src/runtime/fx_collections.rs` but lives
//! here to avoid a circular dependency (vm → classloading → vm would cycle).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

// ---------------------------------------------------------------------------
// FxHasher
// ---------------------------------------------------------------------------

const K: u64 = 0x517c_c1b7_2722_0a95;

/// FxHasher -- the Fx hash function from rustc.
/// Fast, non-cryptographic hasher suitable for internal caches.
pub struct FxHasher {
    hash: usize,
}

impl Default for FxHasher {
    #[inline]
    fn default() -> Self {
        FxHasher { hash: 0 }
    }
}

#[inline]
fn multiply_mix(hash: usize, word: usize) -> usize {
    (hash.rotate_left(5) ^ word).wrapping_mul(K as usize)
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash as u64
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.hash;
        let mut buf = bytes;

        while buf.len() >= std::mem::size_of::<usize>() {
            let mut word_bytes = [0u8; std::mem::size_of::<usize>()];
            word_bytes.copy_from_slice(&buf[..std::mem::size_of::<usize>()]);
            let word = usize::from_ne_bytes(word_bytes);
            hash = multiply_mix(hash, word);
            buf = &buf[std::mem::size_of::<usize>()..];
        }

        for &byte in buf {
            hash = multiply_mix(hash, byte as usize);
        }

        self.hash = hash;
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.hash = multiply_mix(self.hash, i as usize);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.hash = multiply_mix(self.hash, i as usize);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.hash = multiply_mix(self.hash, i as usize);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.hash = multiply_mix(self.hash, i as usize);
        #[cfg(not(target_pointer_width = "64"))]
        {
            self.hash = multiply_mix(self.hash, (i >> 32) as usize);
        }
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.hash = multiply_mix(self.hash, i);
    }
}

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// Build-hasher that constructs `FxHasher` instances.
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// A `HashMap` using the Fx hash function.
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

// ---------------------------------------------------------------------------
// Constructor helpers
// ---------------------------------------------------------------------------

/// Create an `FxHashMap` with the given pre-allocated capacity.
#[inline]
pub fn fx_hashmap_with_capacity<K, V>(cap: usize) -> FxHashMap<K, V> {
    HashMap::with_capacity_and_hasher(cap, FxBuildHasher::default())
}
