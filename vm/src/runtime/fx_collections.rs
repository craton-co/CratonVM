// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! FxHashMap / FxHashSet wrappers for hot paths in the JVM.
//!
//! Provides the Fx hash function (from rustc) as an inline hasher, plus
//! type aliases, constructor helpers, and a `ResolutionCache` optimised for
//! method / field / call-site resolution.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// FxHasher
// ---------------------------------------------------------------------------

const SEED: usize = 0;
const K: u64 = 0x517c_c1b7_2722_0a95;

/// FxHasher -- the Fx hash function from rustc.
/// This is a simple, fast, non-cryptographic hash suitable for hash maps
/// where DoS resistance is not required (e.g., internal caches).
pub struct FxHasher {
    hash: usize,
}

impl Default for FxHasher {
    #[inline]
    fn default() -> Self {
        FxHasher { hash: SEED }
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
        // Process full usize-width words first, then remaining bytes.
        let mut hash = self.hash;
        let mut buf = bytes;

        // Process usize-width chunks.
        while buf.len() >= std::mem::size_of::<usize>() {
            let mut word_bytes = [0u8; std::mem::size_of::<usize>()];
            word_bytes.copy_from_slice(&buf[..std::mem::size_of::<usize>()]);
            let word = usize::from_ne_bytes(word_bytes);
            hash = multiply_mix(hash, word);
            buf = &buf[std::mem::size_of::<usize>()..];
        }

        // Process remaining bytes one at a time (if any).
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

/// A `HashSet` using the Fx hash function.
pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;

// ---------------------------------------------------------------------------
// Constructor helpers
// ---------------------------------------------------------------------------

/// Create an empty `FxHashMap`.
#[inline]
pub fn fx_hashmap<K, V>() -> FxHashMap<K, V> {
    HashMap::with_hasher(FxBuildHasher::default())
}

/// Create an empty `FxHashSet`.
#[inline]
pub fn fx_hashset<T>() -> FxHashSet<T> {
    HashSet::with_hasher(FxBuildHasher::default())
}

/// Create an `FxHashMap` with the given pre-allocated capacity.
#[inline]
pub fn fx_hashmap_with_capacity<K, V>(cap: usize) -> FxHashMap<K, V> {
    HashMap::with_capacity_and_hasher(cap, FxBuildHasher::default())
}

// ---------------------------------------------------------------------------
// ResolutionCache
// ---------------------------------------------------------------------------

/// A resolved JVM method reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMethod {
    pub class_id: u64,
    pub method_index: u32,
    pub is_static: bool,
    pub is_native: bool,
}

/// A resolved JVM field reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedField {
    pub class_id: u64,
    pub field_index: u32,
    pub is_static: bool,
}

/// A resolved invokedynamic call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCallSite {
    pub bootstrap_index: u32,
    pub target_class_id: u64,
    pub target_method_index: u32,
}

/// Fast resolution cache for method, field, and call-site lookups.
///
/// Keys are `(class_hash, name_hash, desc_hash)` triples produced by
/// `FxHasher` so that the lookup itself is allocation-free.
pub struct ResolutionCache {
    methods: FxHashMap<(u64, u64, u64), ResolvedMethod>,
    fields: FxHashMap<(u64, u64, u64), ResolvedField>,
    call_sites: FxHashMap<u64, ResolvedCallSite>,
}

/// Hash a string with `FxHasher` and return the `u64` digest.
#[inline]
pub(crate) fn fx_hash_str(s: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.finish()
}

impl ResolutionCache {
    /// Create an empty resolution cache.
    pub fn new() -> Self {
        Self {
            methods: fx_hashmap(),
            fields: fx_hashmap(),
            call_sites: fx_hashmap(),
        }
    }

    // -- methods ----------------------------------------------------------

    pub fn put_method(&mut self, class: &str, name: &str, desc: &str, resolved: ResolvedMethod) {
        let key = (fx_hash_str(class), fx_hash_str(name), fx_hash_str(desc));
        self.methods.insert(key, resolved);
    }

    pub fn get_method(&self, class: &str, name: &str, desc: &str) -> Option<&ResolvedMethod> {
        let key = (fx_hash_str(class), fx_hash_str(name), fx_hash_str(desc));
        self.methods.get(&key)
    }

    // -- fields -----------------------------------------------------------

    pub fn put_field(&mut self, class: &str, name: &str, desc: &str, resolved: ResolvedField) {
        let key = (fx_hash_str(class), fx_hash_str(name), fx_hash_str(desc));
        self.fields.insert(key, resolved);
    }

    pub fn get_field(&self, class: &str, name: &str, desc: &str) -> Option<&ResolvedField> {
        let key = (fx_hash_str(class), fx_hash_str(name), fx_hash_str(desc));
        self.fields.get(&key)
    }

    // -- call sites -------------------------------------------------------

    pub fn put_call_site(&mut self, id: u64, site: ResolvedCallSite) {
        self.call_sites.insert(id, site);
    }

    pub fn get_call_site(&self, id: u64) -> Option<&ResolvedCallSite> {
        self.call_sites.get(&id)
    }

    // -- housekeeping -----------------------------------------------------

    /// Remove all entries.
    pub fn clear(&mut self) {
        self.methods.clear();
        self.fields.clear();
        self.call_sites.clear();
    }

    /// Total number of cached entries (methods + fields + call sites).
    pub fn len(&self) -> usize {
        self.methods.len() + self.fields.len() + self.call_sites.len()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ResolutionCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
