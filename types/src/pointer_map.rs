// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The relocation map every moving collection hands the VM: old object
//! address -> new object address, one entry per survivor.
//!
//! # Why a sharded map and not the `FxHashMap` alias it replaced
//!
//! Until 2026-09-02 this was `pub type PointerMap = FxHashMap<usize, usize>`.
//! The moving young collection builds it with one insert per surviving object
//! — 154k in steady state and 1.4M on a tenure cycle of `OldGenRsetProbe` —
//! and, once the copy itself had been spread over eight workers (`gen_evac`),
//! that build was the largest SEQUENTIAL term left in the pause: `map_merge`
//! measured 15–25 ms of a ~105 ms pause on the r1 A/B, i.e. the workers'
//! append-only pair lists were being folded into one hash table on one core
//! at ~60 ns per entry, mostly cache misses on a table that does not fit L2.
//!
//! A single hash table cannot be written by several threads. Sixteen of them
//! can: this map is [`SHARDS`] independent `FxHashMap`s selected by a few
//! address bits, and [`PointerMap::par_extend_pairs`] lets `k` threads each own
//! a disjoint slice of the shards and fold the SAME pair lists into them, so
//! the build costs `N / k` inserts of wall time instead of `N`. A lookup costs
//! the alias's hash probe plus one shift and mask.
//!
//! # Shard selection
//!
//! Keys are object addresses, which are 8-byte aligned, so bits 3.. are the
//! ones that vary between neighbouring objects. `(key >> 3) & (SHARDS - 1)`
//! therefore spreads a run of adjacent survivors over every shard rather than
//! piling a page's worth into one — which matters for the parallel build,
//! whose balance is only as good as the shard spread.
//!
//! # API
//!
//! The surface is the subset of `HashMap`'s that the 76 files naming this type
//! use: `get`, `contains_key`, `insert`, `entry`, `remove`, `len`, `is_empty`,
//! `clear`, `iter`, `keys`, `values`, `values_mut`, `extend`, `retain`,
//! `FromIterator`, `IntoIterator`, `Index`, `Clone`, `PartialEq`, `Debug`,
//! plus `with_capacity_and_hasher` so the pre-sizing call sites did not have
//! to change. Iteration order is unspecified, as it was.

use std::collections::hash_map::Entry;
use std::hash::BuildHasherDefault;

use rustc_hash::{FxHashMap, FxHasher};

/// Number of independent hash tables. A power of two; sixteen keeps a
/// parallel build balanced on up to sixteen workers and costs one extra
/// cache line of headers over the flat map.
pub const SHARDS: usize = 16;

#[inline(always)]
fn shard_of(key: usize) -> usize {
    (key >> 3) & (SHARDS - 1)
}

/// Old address -> new address for one collection. See the module doc.
#[derive(Clone, PartialEq, Eq)]
pub struct PointerMap {
    shards: Vec<FxHashMap<usize, usize>>,
}

impl Default for PointerMap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PointerMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl PointerMap {
    /// An empty map.
    pub fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| FxHashMap::default()).collect(),
        }
    }

    /// An empty map with room for `capacity` entries spread over the shards.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            shards: (0..SHARDS)
                .map(|_| {
                    FxHashMap::with_capacity_and_hasher(
                        capacity.div_ceil(SHARDS),
                        BuildHasherDefault::<FxHasher>::default(),
                    )
                })
                .collect(),
        }
    }

    /// `HashMap`-shaped constructor kept so the pre-sizing call sites read as
    /// they did against the alias; the hasher is fixed.
    pub fn with_capacity_and_hasher(
        capacity: usize,
        _hasher: BuildHasherDefault<FxHasher>,
    ) -> Self {
        Self::with_capacity(capacity)
    }

    #[inline]
    pub fn get(&self, key: &usize) -> Option<&usize> {
        self.shards[shard_of(*key)].get(key)
    }

    #[inline]
    pub fn get_mut(&mut self, key: &usize) -> Option<&mut usize> {
        self.shards[shard_of(*key)].get_mut(key)
    }

    #[inline]
    pub fn contains_key(&self, key: &usize) -> bool {
        self.shards[shard_of(*key)].contains_key(key)
    }

    #[inline]
    pub fn insert(&mut self, key: usize, value: usize) -> Option<usize> {
        self.shards[shard_of(key)].insert(key, value)
    }

    #[inline]
    pub fn entry(&mut self, key: usize) -> Entry<'_, usize, usize> {
        self.shards[shard_of(key)].entry(key)
    }

    #[inline]
    pub fn remove(&mut self, key: &usize) -> Option<usize> {
        self.shards[shard_of(*key)].remove(key)
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(FxHashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(FxHashMap::is_empty)
    }

    pub fn capacity(&self) -> usize {
        self.shards.iter().map(FxHashMap::capacity).sum()
    }

    pub fn clear(&mut self) {
        self.shards.iter_mut().for_each(FxHashMap::clear);
    }

    /// Reserve room for `additional` more entries, spread over the shards.
    pub fn reserve(&mut self, additional: usize) {
        let per = additional.div_ceil(SHARDS);
        self.shards.iter_mut().for_each(|s| s.reserve(per));
    }

    pub fn retain(&mut self, mut f: impl FnMut(&usize, &mut usize) -> bool) {
        self.shards.iter_mut().for_each(|s| s.retain(&mut f));
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter {
            shards: self.shards.iter(),
            cur: None,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &usize> + '_ {
        self.shards.iter().flat_map(FxHashMap::keys)
    }

    pub fn values(&self) -> impl Iterator<Item = &usize> + '_ {
        self.shards.iter().flat_map(FxHashMap::values)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut usize> + '_ {
        self.shards.iter_mut().flat_map(FxHashMap::values_mut)
    }

    /// Fold `sources` — lists of `(old, new)` pairs, typically one per
    /// evacuation worker — into the map on `threads` threads.
    ///
    /// Thread `t` owns shards `[t * per, (t + 1) * per)` and scans EVERY
    /// source, inserting only the pairs whose shard it owns. Each thread
    /// therefore reads all `N` pairs (a sequential stream, cheap) and inserts
    /// `N / threads` of them (the random-access part, which is what the
    /// single-threaded fold was paying for). Insert semantics are those of
    /// `insert`: a later pair for the same key wins, exactly as the sequential
    /// `extend` this replaces.
    ///
    /// The caller's thread is one of the `threads`; with `threads <= 1` this
    /// is a plain sequential extend.
    pub fn par_extend_pairs(&mut self, sources: &[Vec<(usize, usize)>], threads: usize) {
        let total: usize = sources.iter().map(Vec::len).sum();
        if total == 0 {
            return;
        }
        self.reserve(total);
        let threads = threads.clamp(1, SHARDS);
        if threads <= 1 {
            for src in sources {
                for &(k, v) in src {
                    self.shards[shard_of(k)].insert(k, v);
                }
            }
            return;
        }
        let per = SHARDS.div_ceil(threads);
        let fold = |first: usize, slice: &mut [FxHashMap<usize, usize>]| {
            let last = first + slice.len();
            for src in sources {
                for &(k, v) in src {
                    let s = shard_of(k);
                    if s >= first && s < last {
                        slice[s - first].insert(k, v);
                    }
                }
            }
        };
        std::thread::scope(|scope| {
            let mut chunks = self.shards.chunks_mut(per).enumerate();
            let first = chunks.next();
            for (i, chunk) in chunks {
                let fold = &fold;
                scope.spawn(move || fold(i * per, chunk));
            }
            if let Some((i, chunk)) = first {
                fold(i * per, chunk);
            }
        });
    }
}

/// Borrowing iterator over every `(old, new)` pair.
pub struct Iter<'a> {
    shards: std::slice::Iter<'a, FxHashMap<usize, usize>>,
    cur: Option<std::collections::hash_map::Iter<'a, usize, usize>>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a usize, &'a usize);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(cur) = self.cur.as_mut() {
                if let Some(item) = cur.next() {
                    return Some(item);
                }
            }
            self.cur = Some(self.shards.next()?.iter());
        }
    }
}

/// Owning iterator over every `(old, new)` pair.
pub struct IntoIter {
    shards: std::vec::IntoIter<FxHashMap<usize, usize>>,
    cur: Option<std::collections::hash_map::IntoIter<usize, usize>>,
}

impl Iterator for IntoIter {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(cur) = self.cur.as_mut() {
                if let Some(item) = cur.next() {
                    return Some(item);
                }
            }
            self.cur = Some(self.shards.next()?.into_iter());
        }
    }
}

impl<'a> IntoIterator for &'a PointerMap {
    type Item = (&'a usize, &'a usize);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl IntoIterator for PointerMap {
    type Item = (usize, usize);
    type IntoIter = IntoIter;
    fn into_iter(self) -> IntoIter {
        IntoIter {
            shards: self.shards.into_iter(),
            cur: None,
        }
    }
}

impl Extend<(usize, usize)> for PointerMap {
    fn extend<I: IntoIterator<Item = (usize, usize)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

impl<'a> Extend<(&'a usize, &'a usize)> for PointerMap {
    fn extend<I: IntoIterator<Item = (&'a usize, &'a usize)>>(&mut self, iter: I) {
        for (&k, &v) in iter {
            self.insert(k, v);
        }
    }
}

impl FromIterator<(usize, usize)> for PointerMap {
    fn from_iter<I: IntoIterator<Item = (usize, usize)>>(iter: I) -> Self {
        let mut m = Self::new();
        m.extend(iter);
        m
    }
}

impl From<FxHashMap<usize, usize>> for PointerMap {
    fn from(map: FxHashMap<usize, usize>) -> Self {
        map.into_iter().collect()
    }
}

impl From<std::collections::HashMap<usize, usize>> for PointerMap {
    fn from(map: std::collections::HashMap<usize, usize>) -> Self {
        map.into_iter().collect()
    }
}

impl std::ops::Index<&usize> for PointerMap {
    type Output = usize;
    fn index(&self, key: &usize) -> &usize {
        self.get(key).expect("no entry found for key")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_behaves_like_the_hash_map_it_replaced() {
        let mut m = PointerMap::with_capacity(100);
        assert!(m.is_empty());
        for i in 0..1000usize {
            assert_eq!(m.insert(i * 8, i * 8 + 4096), None);
        }
        assert_eq!(m.len(), 1000);
        assert_eq!(m.get(&(8 * 7)), Some(&(56 + 4096)));
        assert_eq!(m[&0], 4096);
        assert!(m.contains_key(&(8 * 999)));
        assert!(!m.contains_key(&(8 * 1000)));
        assert_eq!(m.insert(8, 1), Some(8 + 4096));
        *m.entry(16).or_insert(0) = 2;
        assert_eq!(m.get(&16), Some(&2));
        assert_eq!(m.remove(&16), Some(2));
        assert_eq!(m.iter().count(), 999);
        assert_eq!(m.keys().count(), 999);
        assert_eq!(m.values().filter(|&&v| v == 1).count(), 1);
        for v in m.values_mut() {
            *v += 1;
        }
        assert_eq!(m.get(&8), Some(&2));
        let owned: Vec<(usize, usize)> = m.clone().into_iter().collect();
        assert_eq!(owned.len(), 999);
        let back: PointerMap = owned.into_iter().collect();
        assert_eq!(back, m);
        m.retain(|&k, _| k < 80);
        assert_eq!(m.len(), 9);
        m.clear();
        assert!(m.is_empty());
    }

    #[test]
    fn every_shard_is_used_by_adjacent_objects() {
        let mut m = PointerMap::new();
        for i in 0..SHARDS {
            m.insert(i * 8, 1);
        }
        assert!(m.shards.iter().all(|s| s.len() == 1));
    }

    #[test]
    fn the_parallel_fold_equals_the_sequential_one_and_last_pair_wins() {
        let sources: Vec<Vec<(usize, usize)>> = (0..8)
            .map(|w| {
                (0..5000usize)
                    .map(|i| ((i * 8) ^ (w * 40), i + w))
                    .collect()
            })
            .collect();
        let mut seq = PointerMap::new();
        for s in &sources {
            seq.extend(s.iter().copied());
        }
        for threads in [1, 2, 3, 8, 16, 64] {
            let mut par = PointerMap::new();
            par.par_extend_pairs(&sources, threads);
            assert_eq!(par, seq, "threads={threads}");
        }
        let dup = vec![vec![(8usize, 1usize)], vec![(8, 2)]];
        let mut m = PointerMap::new();
        m.par_extend_pairs(&dup, 4);
        assert_eq!(m.get(&8), Some(&2));
    }
}
