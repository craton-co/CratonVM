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
//!
//! # The sorted form (zt t1/rl)
//!
//! [`PointerMap::from_pairs_sorted`] builds the map as ONE sorted
//! `Vec<(old, new)>` instead of hashing the pairs into the shards: 16 bytes a
//! move instead of ~19-38 (a hash entry plus control byte at a 7/8 load,
//! rounded up per shard), and no hash insert per move in the pause. A lookup
//! is then a binary search. ZGC's slide opts into it
//! (`CRATONVM_ZGC_POINTER_MAP_SORTED=1`,
//! `zgc-zr4un-proposal-pointer-map-as-a-sorted-slice`) because its consumers
//! only look up. Every MUTATING method first moves the pairs into the shards
//! (`PointerMap::thaw`), so the API and its answers are the same in both
//! forms; the invariant is that at most one of the two holds entries.
//! Equality compares contents, not forms.

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
#[derive(Clone)]
pub struct PointerMap {
    shards: Vec<FxHashMap<usize, usize>>,
    /// zt t1/rl: the sorted form, ascending by key, keys unique. Non-empty
    /// only for a map built by [`Self::from_pairs_sorted`] and not mutated
    /// since; the shards are then all empty.
    sorted: Vec<(usize, usize)>,
}

impl PartialEq for PointerMap {
    /// Content equality, whichever form either side is in (the derive this
    /// replaces compared the shards, which was content equality while the
    /// shards were the only form).
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl Eq for PointerMap {}

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
            sorted: Vec::new(),
        }
    }

    /// zt t1/rl: the map of `pairs` in the SORTED form (see the module doc):
    /// the vector itself, sorted by key, with the LAST pair for a repeated
    /// key kept (the semantics of `insert` / `extend` in order). Lookups are
    /// a binary search; the first mutation converts to the hashed form.
    pub fn from_pairs_sorted(mut pairs: Vec<(usize, usize)>) -> Self {
        // Stable, so pairs with the same key keep their input order and the
        // last of each run is the one `insert` in order would have kept.
        pairs.sort_by_key(|&(k, _)| k);
        // `dedup_by` keeps the FIRST of a run; carry each later value into
        // the kept slot so the last one survives.
        pairs.dedup_by(|later, kept| {
            if later.0 == kept.0 {
                kept.1 = later.1;
                true
            } else {
                false
            }
        });
        // A shrink reallocates (and briefly holds both buffers), so only
        // when the slack is worth it.
        if pairs.capacity() > pairs.len() + pairs.len() / 8 {
            pairs.shrink_to_fit();
        }
        let mut m = Self::new();
        m.sorted = pairs;
        m
    }

    /// Is this map in the sorted form (built by [`Self::from_pairs_sorted`]
    /// and not mutated since)? For footprint accounting.
    pub fn is_sorted_form(&self) -> bool {
        !self.sorted.is_empty()
    }

    /// Bytes this map holds for its entries, by form: the sorted vector's
    /// capacity times 16, or the shards' buckets (entry plus one control byte
    /// each, the `hashbrown` layout).
    pub fn heap_bytes(&self) -> usize {
        let pair = std::mem::size_of::<(usize, usize)>();
        let shards: usize = self.shards.iter().map(|s| s.capacity() * (pair + 1)).sum();
        self.sorted.capacity() * pair + shards
    }

    /// Move the sorted form's pairs into the shards (no-op in the hashed
    /// form). Every mutating method calls it first.
    fn thaw(&mut self) {
        if self.sorted.is_empty() {
            return;
        }
        let pairs = std::mem::take(&mut self.sorted);
        let per = pairs.len().div_ceil(SHARDS);
        for s in self.shards.iter_mut() {
            s.reserve(per);
        }
        for (k, v) in pairs {
            self.shards[shard_of(k)].insert(k, v);
        }
    }

    #[inline]
    fn sorted_get(&self, key: usize) -> Option<&usize> {
        self.sorted
            .binary_search_by_key(&key, |&(k, _)| k)
            .ok()
            .map(|i| &self.sorted[i].1)
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
            sorted: Vec::new(),
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
        if !self.sorted.is_empty() {
            return self.sorted_get(*key);
        }
        self.shards[shard_of(*key)].get(key)
    }

    #[inline]
    pub fn get_mut(&mut self, key: &usize) -> Option<&mut usize> {
        self.thaw();
        self.shards[shard_of(*key)].get_mut(key)
    }

    #[inline]
    pub fn contains_key(&self, key: &usize) -> bool {
        if !self.sorted.is_empty() {
            return self.sorted_get(*key).is_some();
        }
        self.shards[shard_of(*key)].contains_key(key)
    }

    #[inline]
    pub fn insert(&mut self, key: usize, value: usize) -> Option<usize> {
        self.thaw();
        self.shards[shard_of(key)].insert(key, value)
    }

    #[inline]
    pub fn entry(&mut self, key: usize) -> Entry<'_, usize, usize> {
        self.thaw();
        self.shards[shard_of(key)].entry(key)
    }

    #[inline]
    pub fn remove(&mut self, key: &usize) -> Option<usize> {
        self.thaw();
        self.shards[shard_of(*key)].remove(key)
    }

    pub fn len(&self) -> usize {
        self.sorted.len() + self.shards.iter().map(FxHashMap::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty() && self.shards.iter().all(FxHashMap::is_empty)
    }

    pub fn capacity(&self) -> usize {
        self.sorted.capacity() + self.shards.iter().map(FxHashMap::capacity).sum::<usize>()
    }

    pub fn clear(&mut self) {
        self.sorted.clear();
        self.shards.iter_mut().for_each(FxHashMap::clear);
    }

    /// Reserve room for `additional` more entries, spread over the shards.
    pub fn reserve(&mut self, additional: usize) {
        self.thaw();
        let per = additional.div_ceil(SHARDS);
        self.shards.iter_mut().for_each(|s| s.reserve(per));
    }

    pub fn retain(&mut self, mut f: impl FnMut(&usize, &mut usize) -> bool) {
        self.thaw();
        self.shards.iter_mut().for_each(|s| s.retain(&mut f));
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter {
            sorted: self.sorted.iter(),
            shards: self.shards.iter(),
            cur: None,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &usize> + '_ {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &usize> + '_ {
        self.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut usize> + '_ {
        self.thaw();
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

    /// [`Self::par_extend_pairs`] on the CALLER's workers: `run(n, job)` is
    /// asked to call `job(i)` for every `i` in `0..n` (on any threads,
    /// concurrently or not) and to return when all have finished. A job the
    /// runner skips is folded afterwards on the calling thread, and a job run
    /// twice is a no-op the second time, so a runner can cost time, never
    /// entries. Insert semantics are those of `par_extend_pairs`.
    ///
    /// gce e1/y: the generational heap drives this from its persistent
    /// evacuation pool instead of the `std::thread::scope` above, which
    /// spawned `threads - 1` fresh OS threads on every moving young cycle.
    pub fn par_extend_pairs_with(
        &mut self,
        sources: &[Vec<(usize, usize)>],
        threads: usize,
        run: impl FnOnce(usize, &(dyn Fn(usize) + Sync)),
    ) {
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
        // One uncontended lock per chunk: it hands each job its own `&mut`
        // slice through a shared `Fn`, and records that the chunk was folded.
        type Chunk<'m> = std::sync::Mutex<(usize, &'m mut [FxHashMap<usize, usize>], bool)>;
        let chunks: Vec<Chunk<'_>> = self
            .shards
            .chunks_mut(per)
            .enumerate()
            .map(|(i, c)| std::sync::Mutex::new((i * per, c, false)))
            .collect();
        let job = |i: usize| {
            if let Some(m) = chunks.get(i) {
                let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                let (first, slice, done) = &mut *g;
                if !*done {
                    fold(*first, &mut **slice);
                    *done = true;
                }
            }
        };
        run(chunks.len(), &job);
        // Whatever the runner skipped (`done` gates the rest).
        for i in 0..chunks.len() {
            job(i);
        }
    }
}

/// Borrowing iterator over every `(old, new)` pair.
pub struct Iter<'a> {
    /// zt t1/rl: the sorted form's pairs, walked first (empty otherwise).
    sorted: std::slice::Iter<'a, (usize, usize)>,
    shards: std::slice::Iter<'a, FxHashMap<usize, usize>>,
    cur: Option<std::collections::hash_map::Iter<'a, usize, usize>>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a usize, &'a usize);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((k, v)) = self.sorted.next() {
            return Some((k, v));
        }
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
    /// zt t1/rl: the sorted form's pairs, walked first (empty otherwise).
    sorted: std::vec::IntoIter<(usize, usize)>,
    shards: std::vec::IntoIter<FxHashMap<usize, usize>>,
    cur: Option<std::collections::hash_map::IntoIter<usize, usize>>,
}

impl Iterator for IntoIter {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(pair) = self.sorted.next() {
            return Some(pair);
        }
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
            sorted: self.sorted.into_iter(),
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

    /// zt t1/rl: the sorted form answers exactly what the hashed form built
    /// from the same pairs in order answers (last pair for a key wins), and
    /// every mutation converts it without losing an entry.
    #[test]
    fn zt1_rl_the_sorted_form_equals_the_hashed_form() {
        let pairs: Vec<(usize, usize)> = (0..3000usize)
            .map(|i| (((i * 7919) % 2000) * 8, i))
            .collect();
        let hashed: PointerMap = pairs.iter().copied().collect();
        let sorted = PointerMap::from_pairs_sorted(pairs.clone());
        assert!(sorted.is_sorted_form());
        assert_eq!(sorted.len(), hashed.len());
        assert_eq!(sorted, hashed);
        assert_eq!(hashed, sorted);
        for k in (0..2100usize).map(|i| i * 8) {
            assert_eq!(sorted.get(&k), hashed.get(&k), "key {k}");
            assert_eq!(sorted.contains_key(&k), hashed.contains_key(&k));
        }
        assert!(!sorted.contains_key(&4));
        assert_eq!(sorted.iter().count(), hashed.len());
        assert_eq!(sorted.keys().count(), hashed.len());
        assert_eq!(
            sorted.values().sum::<usize>(),
            hashed.values().sum::<usize>()
        );
        let owned: Vec<(usize, usize)> = sorted.clone().into_iter().collect();
        assert_eq!(owned.len(), hashed.len());
        assert!(sorted.heap_bytes() >= sorted.len() * 16);
        // A mutation thaws into the shards and keeps every entry.
        let mut m = sorted.clone();
        assert_eq!(m.insert(8, 1), hashed.get(&8).copied());
        assert!(!m.is_sorted_form());
        assert_eq!(m.len(), hashed.len());
        assert_eq!(m.get(&8), Some(&1));
        let mut r = sorted.clone();
        r.retain(|&k, _| k < 80);
        assert_eq!(r.len(), 10);
        let mut c = sorted;
        c.clear();
        assert!(c.is_empty());
        assert!(PointerMap::from_pairs_sorted(Vec::new()).is_empty());
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

    /// gce e1/y: the caller-driven fold equals the sequential one, whether the
    /// runner runs every job on its own thread, skips half of them, or runs
    /// each twice; the last pair for a key still wins.
    #[test]
    fn the_runner_fold_equals_the_sequential_one_even_when_the_runner_skips_jobs() {
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
        for threads in [1, 2, 3, 8, 16] {
            let mut all = PointerMap::new();
            all.par_extend_pairs_with(&sources, threads, |n, job| {
                std::thread::scope(|s| {
                    for i in 0..n {
                        s.spawn(move || job(i));
                    }
                })
            });
            assert_eq!(all, seq, "threads={threads}");
            let mut skipping = PointerMap::new();
            skipping.par_extend_pairs_with(&sources, threads, |n, job| {
                (0..n).step_by(2).for_each(job);
            });
            assert_eq!(
                skipping, seq,
                "threads={threads}: skipped jobs are folded after"
            );
            let mut twice = PointerMap::new();
            twice.par_extend_pairs_with(&sources, threads, |n, job| {
                (0..n).chain(0..n).for_each(job);
            });
            assert_eq!(twice, seq, "threads={threads}: a repeated job is a no-op");
        }
        let dup = vec![vec![(8usize, 1usize)], vec![(8, 2)]];
        let mut m = PointerMap::new();
        m.par_extend_pairs_with(&dup, 4, |n, job| (0..n).rev().for_each(job));
        assert_eq!(m.get(&8), Some(&2));
    }
}
