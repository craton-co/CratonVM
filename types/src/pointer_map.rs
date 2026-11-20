// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! can: this map is [`SHARDS`] independent hash tables selected by a few
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
//! is then a binary search. ZGC's slide uses it unconditionally
//! (`zgc-zr4un-proposal-pointer-map-as-a-sorted-slice`) because its consumers
//! only look up. A mutating method other than `insert`, `get_mut` and a
//! vacant `entry` first moves the pairs into the shards (`PointerMap::thaw`),
//! so the API and its answers are the same in both forms. Equality compares
//! contents, not forms.
//!
//! # The overlay (gcj pcp)
//!
//! `insert` on the sorted form does not thaw: a key the sorted pairs hold has
//! its value replaced in place, and any other key goes to the shards, which
//! then hold the map's later inserts beside the sorted pairs. The invariant is
//! that no key is in both. Every lookup asks the sorted pairs first and the
//! shards second. The generational young pause builds its map sorted and then
//! inserts a few pairs from its serial phases (the reference and finalizer
//! drains); a thaw there re-hashed every pair of the pause.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

/// The shards' hasher: FxHash's multiply, with the product's high half
/// rotated into the low bits `hashbrown` takes its bucket index from.
///
/// # Why not plain `FxHasher`
///
/// Plain FxHash of one `usize` is `key * K`. A shard's keys all agree in bits
/// 0..7 (8-byte aligned, and bits 3..7 are the shard), so every product agrees
/// in its low seven bits too, and `hashbrown`, which starts its probe at
/// `hash & mask`, began every insert of a shard at one bucket in 128. The
/// probes then walked groups to find room: `HashMap::insert` and its
/// `Equivalent` compares were 15% of all GC CPU on `R23W6KeepLong -Xmx1g`
/// (G1, 3.5 M forwards in the first pause), on the driver and on every
/// evacuation worker alike. The rotated product's low bits come from the
/// product's high half, which every key bit reaches.
#[derive(Default, Clone, Copy)]
pub struct AddrHasher(u64);

const ADDR_HASH_K: u64 = 0x517c_c1b7_2722_0a95;

impl Hasher for AddrHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(ADDR_HASH_K);
        }
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(ADDR_HASH_K);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(ADDR_HASH_K);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0.rotate_left(32)
    }
}

/// One shard: `usize -> usize` under [`AddrHasher`].
type Shard = HashMap<usize, usize, BuildHasherDefault<AddrHasher>>;

/// Number of independent hash tables. A power of two; sixteen keeps a
/// parallel build balanced on up to sixteen workers and costs one extra
/// cache line of headers over the flat map.
pub const SHARDS: usize = 16;

/// g1ep3: below this many pairs [`PointerMap::from_sorted_unique_par`] builds
/// its index serially (the jobs' dispatch would cost more than the fill).
pub const PAR_INDEX_MIN: usize = 1 << 16;

/// Merge two key-sorted lists of unique keys into `out`, key-sorted; a key in
/// both keeps `first`'s pair ([`PointerMap::compose_after`]: a pause-start key
/// keeps its chased value).
fn merge_preferring_first(
    first: &[(usize, usize)],
    second: &[(usize, usize)],
    out: &mut Vec<(usize, usize)>,
) {
        let (mut i, mut j) = (0usize, 0usize);
        while i < first.len() && j < second.len() {
            let (a, b) = (first[i], second[j]);
            match a.0.cmp(&b.0) {
                std::cmp::Ordering::Less => {
                    out.push(a);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(b);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.push(a);
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&first[i..]);
        out.extend_from_slice(&second[j..]);
}

#[inline(always)]
fn shard_of(key: usize) -> usize {
    (key >> 3) & (SHARDS - 1)
}

/// zt t32 (ZGC concurrent relocation): called with the KEY of every lookup
/// that hits (see [`PointerMap::set_hit_hook`]).
pub type PointerMapHitHook = std::sync::Arc<dyn Fn(usize) + Send + Sync>;

/// Old address -> new address for one collection. See the module doc.
#[derive(Clone)]
pub struct PointerMap {
    shards: Vec<Shard>,
    /// zt t1/rl: the sorted form, ascending by key, keys unique. Non-empty
    /// only for a map built by [`Self::from_pairs_sorted`] and not thawed
    /// since. gcj pcp: the shards may then hold the overlay (see the module
    /// doc), never a key the sorted pairs hold.
    sorted: Vec<(usize, usize)>,
    /// g1sz: the sorted form's bucket index (see [`SortedIndex`]); empty
    /// whenever `sorted` is.
    index: SortedIndex,
    /// zt t32: see [`Self::set_hit_hook`]. `None` on every map but a ZGC
    /// concurrent-relocation pause's.
    hit_hook: Option<PointerMapHitHook>,
    /// g1ep4: where the sorted form's buffers go when the map is dropped or
    /// thawed (see [`SortedBufferPool`]). Cloned with the map.
    pool: Option<std::sync::Arc<SortedBufferPool>>,
}

/// g1ep4: the sorted form's two buffers -- the pairs and the bucket index --
/// kept between collections, so the next map is built into memory that is
/// already mapped and faulted in.
///
/// The launcher caps mimalloc's arena objects at 64 KiB (the boot-RSS lever),
/// so every larger buffer is a fresh `mmap` the pause faults in page by page
/// (the kernel zeroing each one) and a `munmap` when the map is dropped. A G1
/// young pause's map of 1.3 M pairs is 21 MB of pairs and 5 MB of index; on
/// `G1epKeepRounds 10 -Xmx1g` those faults were about a third of the run's
/// 2.8-3.1 s of system CPU. A map built with a pool hands its buffers back
/// here when it is dropped (the VM drops a pause's map when the next pause
/// publishes its own, so two buffers alternate), and the next build takes
/// them. The pool holds at most [`Self::MAX_EACH`] buffers of each kind and
/// `cap_bytes` of them in all (per kind); a buffer that does not fit is freed.
pub struct SortedBufferPool {
    pairs: std::sync::Mutex<Vec<Vec<(usize, usize)>>>,
    starts: std::sync::Mutex<Vec<Vec<u32>>>,
    cap_bytes: usize,
}

impl SortedBufferPool {
    /// Buffers kept per kind: the published map's, the one being built, and
    /// a same-pause composition's two inputs.
    pub const MAX_EACH: usize = 4;

    /// A pool that keeps at most `cap_bytes` of pairs and as much again of
    /// index. `0` keeps nothing.
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            pairs: std::sync::Mutex::new(Vec::new()),
            starts: std::sync::Mutex::new(Vec::new()),
            cap_bytes,
        }
    }

    /// An empty pair buffer with room for at least `min` pairs.
    pub fn take_pairs(&self, min: usize) -> Vec<(usize, usize)> {
        take_buffer(&self.pairs, min)
    }

    /// An empty index buffer with room for at least `min` entries.
    pub fn take_starts(&self, min: usize) -> Vec<u32> {
        take_buffer(&self.starts, min)
    }

    /// Keep `buf` for a later [`Self::take_pairs`].
    pub fn give_pairs(&self, buf: Vec<(usize, usize)>) {
        give_buffer(&self.pairs, buf, self.cap_bytes);
    }

    /// Keep `buf` for a later [`Self::take_starts`].
    pub fn give_starts(&self, buf: Vec<u32>) {
        give_buffer(&self.starts, buf, self.cap_bytes);
    }

    /// The byte bound the pool was made with (per kind).
    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    /// Bytes the pool holds, both kinds.
    pub fn held_bytes(&self) -> usize {
        let lock = |m: &std::sync::Mutex<Vec<Vec<(usize, usize)>>>| -> usize {
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            g.iter().map(|b| b.capacity() * std::mem::size_of::<(usize, usize)>()).sum()
        };
        let s = self.starts.lock().unwrap_or_else(|e| e.into_inner());
        lock(&self.pairs) + s.iter().map(|b| b.capacity() * 4).sum::<usize>()
    }
}

/// The smallest kept buffer with room for `min`; failing that a new one with
/// an eighth of headroom (the next pause's map is usually a little larger),
/// the largest kept one freed rather than grown (a grow copies its capacity).
fn take_buffer<T>(list: &std::sync::Mutex<Vec<Vec<T>>>, min: usize) -> Vec<T> {
    let mut l = list.lock().unwrap_or_else(|e| e.into_inner());
    let fit = l
        .iter()
        .enumerate()
        .filter(|(_, b)| b.capacity() >= min)
        .min_by_key(|(_, b)| b.capacity())
        .map(|(i, _)| i);
    match fit {
        Some(i) => l.swap_remove(i),
        None => {
            if let Some(i) = (0..l.len()).max_by_key(|&i| l[i].capacity()) {
                drop(l.swap_remove(i));
            }
            drop(l);
            Vec::with_capacity(min + min / 8)
        }
    }
}

fn give_buffer<T>(list: &std::sync::Mutex<Vec<Vec<T>>>, mut buf: Vec<T>, cap_bytes: usize) {
    let size = std::mem::size_of::<T>();
    if buf.capacity() == 0 || buf.capacity() * size > cap_bytes {
        return;
    }
    buf.clear();
    let mut l = list.lock().unwrap_or_else(|e| e.into_inner());
    l.push(buf);
    // Over either bound: free the smallest (the largest is the one the next
    // pause most likely needs).
    while l.len() > SortedBufferPool::MAX_EACH
        || l.iter().map(|b| b.capacity() * size).sum::<usize>() > cap_bytes
    {
        let Some(i) = (0..l.len()).min_by_key(|&i| l[i].capacity()) else {
            break;
        };
        drop(l.swap_remove(i));
    }
}

/// g1sz: a bucket index over the sorted form, so a lookup is one index load
/// and a binary search over the few pairs of one bucket instead of a binary
/// search over the whole vector.
///
/// A G1 young pause's map holds every object the pause copied (540 k on
/// `BinTreesClassic 18`, 8.6 MB of pairs), and a plain binary search over that
/// is ~20 dependent probes, most of them cache misses. Bucket `b` covers the
/// keys whose `(key >> shift) - base == b`; `starts[b]..starts[b + 1]` is its
/// range in `sorted`. `shift` is the smallest that keeps the bucket count at
/// or below the pair count, so the index costs at most 4 bytes per 16-byte
/// pair and a bucket holds a handful of pairs on a dense collection set.
#[derive(Clone, Default)]
struct SortedIndex {
    starts: Vec<u32>,
    base: usize,
    shift: u32,
}

impl SortedIndex {
    /// The index of `sorted` (ascending, unique keys), or an empty one when
    /// the vector is too long for `u32` positions or empty.
    fn build(sorted: &[(usize, usize)], buf: Vec<u32>) -> Self {
        let (Some(&(lo, _)), Some(&(hi, _))) = (sorted.first(), sorted.last()) else {
            return Self::default();
        };
        if sorted.len() >= u32::MAX as usize {
            return Self::default();
        }
        // Object addresses are 8-aligned, so no bucket narrower than 8 bytes.
        let mut shift = 3u32;
        while shift < usize::BITS - 1 && ((hi >> shift) - (lo >> shift)) >= sorted.len() {
            shift += 1;
        }
        let base = lo >> shift;
        let buckets = (hi >> shift) - base + 1;
        let mut starts = buf;
        starts.reserve(buckets + 1);
        let mut pos = 0usize;
        for b in 0..buckets {
            while pos < sorted.len() && (sorted[pos].0 >> shift) - base < b {
                pos += 1;
            }
            starts.push(pos as u32);
        }
        starts.push(sorted.len() as u32);
        Self {
            starts,
            base,
            shift,
        }
    }

    /// [`Self::build`] with the bucket fill split over `run`'s `jobs` (see
    /// [`PointerMap::from_sorted_unique_par`]). Job `j` takes a contiguous
    /// range of pairs and fills the buckets from just past the previous
    /// pair's bucket through its own last pair's, so the jobs write disjoint
    /// ranges of `starts`. The same index.
    ///
    /// g1ep3: the serial fill was the longest single step of a G1 young
    /// pause's map build (215-228 ms of `G1epKeepRounds 10 -Xmx1g`'s 84
    /// builds, 1.3 M pairs each, against 255-314 ms for the parallel per-region
    /// sorts).
    fn build_par(
        sorted: &[(usize, usize)],
        jobs: usize,
        run: &dyn Fn(usize, &(dyn Fn(usize) + Sync)),
        buf: Vec<u32>,
    ) -> Self {
        let (Some(&(lo, _)), Some(&(hi, _))) = (sorted.first(), sorted.last()) else {
            return Self::default();
        };
        if sorted.len() >= u32::MAX as usize {
            return Self::default();
        }
        let n = sorted.len();
        let mut shift = 3u32;
        while shift < usize::BITS - 1 && ((hi >> shift) - (lo >> shift)) >= n {
            shift += 1;
        }
        let base = lo >> shift;
        let buckets = (hi >> shift) - base + 1;
        let bucket = |i: usize| (sorted[i].0 >> shift) - base;
        let per = n.div_ceil(jobs.max(1));
        let jobs = n.div_ceil(per);
        // `first[j]`: the first bucket job `j` fills; the last job also fills
        // the closing `starts[buckets] = n`.
        let first: Vec<usize> = (0..=jobs)
            .map(|j| match j {
                0 => 0,
                j if j == jobs => buckets + 1,
                j => bucket(j * per - 1) + 1,
            })
            .collect();
        // g1ep4: `buf` (empty, from a pool or new) takes the index; every
        // entry is written below, the zeros only make it initialised.
        let mut starts = buf;
        starts.resize(buckets + 1, 0);
        let mut slices: Vec<std::sync::Mutex<&mut [u32]>> = Vec::with_capacity(jobs);
        let mut rest: &mut [u32] = &mut starts;
        for j in 0..jobs {
            let (own, tail) = std::mem::take(&mut rest).split_at_mut(first[j + 1] - first[j]);
            slices.push(std::sync::Mutex::new(own));
            rest = tail;
        }
        run(jobs, &|j| {
            let mut own = slices[j].lock().unwrap_or_else(|e| e.into_inner());
            let (p0, p1) = (j * per, ((j + 1) * per).min(n));
            let mut next = first[j];
            for i in p0..p1 {
                let bi = bucket(i);
                while next <= bi {
                    own[next - first[j]] = i as u32;
                    next += 1;
                }
            }
            if p1 == n {
                while next <= buckets {
                    own[next - first[j]] = n as u32;
                    next += 1;
                }
            }
        });
        drop(slices);
        Self {
            starts,
            base,
            shift,
        }
    }

    /// The range of `sorted` that can hold `key`: `None` when `key` is outside
    /// every bucket, the whole vector when there is no index.
    #[inline]
    fn range(&self, key: usize, len: usize) -> Option<(usize, usize)> {
        if self.starts.is_empty() {
            return Some((0, len));
        }
        let b = (key >> self.shift).checked_sub(self.base)?;
        if b + 1 >= self.starts.len() {
            return None;
        }
        Some((self.starts[b] as usize, self.starts[b + 1] as usize))
    }
}

impl Drop for PointerMap {
    /// g1ep4: a pooled map's sorted buffers go back to its pool.
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.give_pairs(std::mem::take(&mut self.sorted));
            pool.give_starts(std::mem::take(&mut self.index.starts));
        }
    }
}

/// g1ep4: an upper bound on the bucket index [`SortedIndex::build`] makes for
/// `pairs`: its shift keeps the buckets at or below the pair count, plus the
/// closing entry and one for the rounding of the span.
fn index_len_hint(pairs: &[(usize, usize)]) -> usize {
    pairs.len() + 2
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
            shards: (0..SHARDS).map(|_| Shard::default()).collect(),
            sorted: Vec::new(),
            index: SortedIndex::default(),
            hit_hook: None,
            pool: None,
        }
    }

    /// zt t32 (ZGC concurrent relocation, `CRATONVM_ZGC_CONCURRENT_RELOCATE`):
    /// run `hook` with the key of every [`Self::get`], [`Self::get_mut`] and
    /// [`Self::contains_key`] that hits. The collector's map then names
    /// destinations whose bytes are copied lazily, and the hook copies the
    /// object a consumer is about to learn the new address of -- every remap of
    /// a table or a frame learns it through a lookup. Iteration does not run
    /// the hook (its consumers build address sets and dereference nothing).
    /// Cloned with the map; `None` clears it.
    pub fn set_hit_hook(&mut self, hook: Option<PointerMapHitHook>) {
        self.hit_hook = hook;
    }

    #[inline]
    fn note_hit(&self, key: usize) {
        if let Some(h) = &self.hit_hook {
            h(key);
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
        Self::from_sorted_unique(pairs)
    }

    /// g1sz: the sorted form of `pairs` that the caller has ALREADY sorted by
    /// key and deduplicated (a parallel build: G1's per-region bucket sort).
    /// Debug builds check the order.
    pub fn from_sorted_unique(pairs: Vec<(usize, usize)>) -> Self {
        Self::from_sorted_unique_in(pairs, None)
    }

    /// g1ep4: [`Self::from_sorted_unique`], the index taken from `pool` and
    /// both buffers handed back to it when the map is dropped or thawed.
    pub fn from_sorted_unique_in(
        pairs: Vec<(usize, usize)>,
        pool: Option<&std::sync::Arc<SortedBufferPool>>,
    ) -> Self {
        debug_assert!(
            pairs.windows(2).all(|w| w[0].0 < w[1].0),
            "from_sorted_unique: keys not strictly ascending"
        );
        let buf = pool.map_or_else(Vec::new, |p| p.take_starts(index_len_hint(&pairs)));
        let mut m = Self::new();
        m.index = SortedIndex::build(&pairs, buf);
        m.sorted = pairs;
        m.pool = pool.cloned();
        m
    }

    /// [`Self::from_sorted_unique`] with the bucket index filled by `run`:
    /// `run(jobs, job)` must call `job(i)` once for every `i < jobs`, in any
    /// order and on any threads, and return after the last one. Below
    /// [`PAR_INDEX_MIN`] pairs, or with one job, the serial build. The same
    /// map either way.
    pub fn from_sorted_unique_par(
        pairs: Vec<(usize, usize)>,
        jobs: usize,
        run: &dyn Fn(usize, &(dyn Fn(usize) + Sync)),
    ) -> Self {
        Self::from_sorted_unique_par_in(pairs, jobs, run, None)
    }

    /// g1ep4: [`Self::from_sorted_unique_par`] with the buffers of `pool`
    /// (see [`Self::from_sorted_unique_in`]).
    pub fn from_sorted_unique_par_in(
        pairs: Vec<(usize, usize)>,
        jobs: usize,
        run: &dyn Fn(usize, &(dyn Fn(usize) + Sync)),
        pool: Option<&std::sync::Arc<SortedBufferPool>>,
    ) -> Self {
        if pairs.len() < PAR_INDEX_MIN || jobs <= 1 {
            return Self::from_sorted_unique_in(pairs, pool);
        }
        debug_assert!(
            pairs.windows(2).all(|w| w[0].0 < w[1].0),
            "from_sorted_unique_par: keys not strictly ascending"
        );
        let buf = pool.map_or_else(Vec::new, |p| p.take_starts(index_len_hint(&pairs)));
        let mut m = Self::new();
        m.index = SortedIndex::build_par(&pairs, jobs, run, buf);
        m.sorted = pairs;
        m.pool = pool.cloned();
        m
    }

    /// g1ep4: the pool this map's sorted buffers go back to, if any.
    pub fn buffer_pool(&self) -> Option<&std::sync::Arc<SortedBufferPool>> {
        self.pool.as_ref()
    }

    /// Is this map in the sorted form (built by [`Self::from_pairs_sorted`]
    /// and not mutated since) with no overlay, i.e. does [`Self::iter`] yield
    /// its pairs in key order? For footprint accounting and merges.
    pub fn is_sorted_form(&self) -> bool {
        !self.sorted.is_empty() && self.shards.iter().all(Shard::is_empty)
    }

    /// Bytes this map holds for its entries, by form: the sorted vector's
    /// capacity times 16, or the shards' buckets (entry plus one control byte
    /// each, the `hashbrown` layout).
    pub fn heap_bytes(&self) -> usize {
        let pair = std::mem::size_of::<(usize, usize)>();
        let shards: usize = self.shards.iter().map(|s| s.capacity() * (pair + 1)).sum();
        self.sorted.capacity() * pair
            + self.index.starts.capacity() * std::mem::size_of::<u32>()
            + shards
    }

    /// Move the sorted form's pairs into the shards (no-op in the hashed
    /// form). Every mutating method calls it first.
    fn thaw(&mut self) {
        if self.sorted.is_empty() {
            return;
        }
        let pairs = std::mem::take(&mut self.sorted);
        let index = std::mem::take(&mut self.index);
        let per = pairs.len().div_ceil(SHARDS);
        for s in self.shards.iter_mut() {
            s.reserve(per);
        }
        for &(k, v) in &pairs {
            self.shards[shard_of(k)].insert(k, v);
        }
        if let Some(pool) = &self.pool {
            pool.give_pairs(pairs);
            pool.give_starts(index.starts);
        }
    }

    #[inline]
    fn sorted_get(&self, key: usize) -> Option<&usize> {
        self.sorted_pos(key).map(|i| &self.sorted[i].1)
    }

    /// The position of `key` in the sorted pairs, if they hold it.
    #[inline]
    fn sorted_pos(&self, key: usize) -> Option<usize> {
        if self.sorted.is_empty() {
            return None;
        }
        let (lo, hi) = self.index.range(key, self.sorted.len())?;
        self.sorted[lo..hi]
            .binary_search_by_key(&key, |&(k, _)| k)
            .ok()
            .map(|i| lo + i)
    }

    /// An empty map with room for `capacity` entries spread over the shards.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            shards: (0..SHARDS)
                .map(|_| {
                    Shard::with_capacity_and_hasher(
                        capacity.div_ceil(SHARDS),
                        BuildHasherDefault::<AddrHasher>::default(),
                    )
                })
                .collect(),
            sorted: Vec::new(),
            index: SortedIndex::default(),
            hit_hook: None,
            pool: None,
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
        let found = match self.sorted_get(*key) {
            Some(v) => Some(v),
            None => self.shards[shard_of(*key)].get(key),
        };
        if found.is_some() {
            self.note_hit(*key);
        }
        found
    }

    #[inline]
    pub fn get_mut(&mut self, key: &usize) -> Option<&mut usize> {
        if let Some(i) = self.sorted_pos(*key) {
            self.note_hit(*key);
            return Some(&mut self.sorted[i].1);
        }
        if self.hit_hook.is_some() && self.shards[shard_of(*key)].contains_key(key) {
            self.note_hit(*key);
        }
        self.shards[shard_of(*key)].get_mut(key)
    }

    #[inline]
    pub fn contains_key(&self, key: &usize) -> bool {
        let found = self.sorted_get(*key).is_some() || self.shards[shard_of(*key)].contains_key(key);
        if found {
            self.note_hit(*key);
        }
        found
    }

    /// gcj pcp: on the sorted form, a key the sorted pairs hold is replaced in
    /// place and any other key joins the overlay (see the module doc); no thaw.
    #[inline]
    pub fn insert(&mut self, key: usize, value: usize) -> Option<usize> {
        if let Some(i) = self.sorted_pos(key) {
            return Some(std::mem::replace(&mut self.sorted[i].1, value));
        }
        self.shards[shard_of(key)].insert(key, value)
    }

    /// gcj pcp: a key the sorted pairs hold thaws the map first (an
    /// occupied entry must live in a shard); any other key is the overlay's.
    #[inline]
    pub fn entry(&mut self, key: usize) -> Entry<'_, usize, usize> {
        if self.sorted_pos(key).is_some() {
            self.thaw();
        }
        self.shards[shard_of(key)].entry(key)
    }

    #[inline]
    pub fn remove(&mut self, key: &usize) -> Option<usize> {
        self.thaw();
        self.shards[shard_of(*key)].remove(key)
    }

    pub fn len(&self) -> usize {
        self.sorted.len() + self.shards.iter().map(Shard::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty() && self.shards.iter().all(Shard::is_empty)
    }

    pub fn capacity(&self) -> usize {
        self.sorted.capacity() + self.shards.iter().map(Shard::capacity).sum::<usize>()
    }

    pub fn clear(&mut self) {
        self.sorted.clear();
        let index = std::mem::take(&mut self.index);
        if let Some(pool) = &self.pool {
            pool.give_starts(index.starts);
        }
        self.shards.iter_mut().for_each(Shard::clear);
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

    /// g1pin: the entries whose key lies in `[lo, hi)`. The sorted form finds
    /// them by two binary searches and visits nothing else; the hashed form
    /// filters a full iteration.
    pub fn range(&self, lo: usize, hi: usize) -> impl Iterator<Item = (&usize, &usize)> + '_ {
        let first = self.sorted.partition_point(|&(k, _)| k < lo);
        let end = self.sorted.partition_point(|&(k, _)| k < hi).max(first);
        Iter {
            sorted: self.sorted[first..end].iter(),
            shards: self.shards.iter(),
            cur: None,
        }
        .filter(move |&(&k, _)| k >= lo && k < hi)
    }

    pub fn keys(&self) -> impl Iterator<Item = &usize> + '_ {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &usize> + '_ {
        self.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut usize> + '_ {
        self.thaw();
        self.shards.iter_mut().flat_map(Shard::values_mut)
    }

    /// g1ep: compose two consecutive same-pause passes' maps -- `self` ran
    /// first, `next` second -- into `self`, in the SORTED form.
    ///
    /// The answer is that of the hashed loop it replaces (`G1Collector::
    /// compose_forward_maps`): every value of `self` chases one hop through
    /// `next`, and a key of `next` is added only when `self` lacks it (on a
    /// collision `self`'s entry, the pause-start address, wins). That loop
    /// thawed a sorted `self` into the shards (one hash insert per entry) and
    /// then probed or inserted every key of `next`: on a G1 starved full
    /// (`G1epKeepRounds 4 -Xmx1g`, a young pause of about a million forwards
    /// composed with a compaction of 2.4 million) it was 8% of the run's CPU,
    /// on one thread inside the pause. Here it is one lookup per `self` entry
    /// and a linear merge of two key-sorted lists; a hashed side is sorted
    /// first. The hit hook is kept.
    pub fn compose_after(&mut self, next: &PointerMap) {
        if next.is_empty() {
            return;
        }
        // g1ep4: the three vectors from either map's pool when it has one.
        let pool = self.pool.clone().or_else(|| next.pool.clone());
        let take = |n: usize| {
            pool.as_ref()
                .map_or_else(|| Vec::with_capacity(n), |p| p.take_pairs(n))
        };
        let mut first = take(self.len());
        first.extend(
            self.iter()
                .map(|(&k, &v)| (k, next.get(&v).copied().unwrap_or(v))),
        );
        if !self.is_sorted_form() {
            first.sort_unstable_by_key(|&(k, _)| k);
        }
        let mut second = take(next.len());
        second.extend(next.iter().map(|(&k, &v)| (k, v)));
        if !next.is_sorted_form() {
            second.sort_unstable_by_key(|&(k, _)| k);
        }
        let mut out = take(first.len() + second.len());
        merge_preferring_first(&first, &second, &mut out);
        if let Some(p) = &pool {
            p.give_pairs(first);
            p.give_pairs(second);
        }
        let hook = self.hit_hook.take();
        // The old `self` is dropped here, its buffers back to its pool.
        *self = Self::from_sorted_unique_in(out, pool.as_ref());
        self.hit_hook = hook;
    }

    /// g1ep7: [`Self::compose_after`] with the chase of every pair of `self`
    /// through `next` and the bucket index split into `jobs` jobs on `run`
    /// (the evacuation pool). The same map. On `G1epKeepRounds 10 -Xmx1g` the
    /// serial compose of a starved full's young map (millions of pairs, each a
    /// random probe into the in-place map) was about 7 % of the main thread.
    /// Falls back to the serial body for an unsorted map, a hooked one (the
    /// hook is the caller's per-hit callback) or a small one.
    pub fn compose_after_par(
        &mut self,
        next: &PointerMap,
        jobs: usize,
        run: &dyn Fn(usize, &(dyn Fn(usize) + Sync)),
    ) {
        if next.is_empty() {
            return;
        }
        if !self.par_compose_fits(next, jobs) {
            self.compose_after(next);
            return;
        }
        let pool = self.pool.clone().or_else(|| next.pool.clone());
        let take = |n: usize| {
            pool.as_ref()
                .map_or_else(|| Vec::with_capacity(n), |p| p.take_pairs(n))
        };
        let n = self.sorted.len();
        let mut first = take(n);
        first.resize(n, (0, 0));
        let per = n.div_ceil(jobs);
        let jobs = n.div_ceil(per);
        let slices: Vec<std::sync::Mutex<&mut [(usize, usize)]>> =
            first.chunks_mut(per).map(std::sync::Mutex::new).collect();
        let src = &self.sorted;
        run(jobs, &|j| {
            let mut own = slices[j].lock().unwrap_or_else(|e| e.into_inner());
            let lo = j * per;
            for (o, &(k, v)) in own.iter_mut().zip(&src[lo..]) {
                *o = (k, next.sorted_get(v).copied().unwrap_or(v));
            }
        });
        drop(slices);
        let second = &next.sorted;
        let mut out = take(first.len() + second.len());
        merge_preferring_first(&first, second, &mut out);
        if let Some(p) = &pool {
            p.give_pairs(first);
        }
        // The old `self` is dropped here, its buffers back to its pool.
        *self = Self::from_sorted_unique_par_in(out, jobs, run, pool.as_ref());
    }

    /// Can [`Self::compose_after_par`] split the compose: more than one job, a
    /// map large enough to pay for it, both maps in sorted form and neither
    /// hooked.
    fn par_compose_fits(&self, next: &PointerMap, jobs: usize) -> bool {
        jobs > 1
            && self.sorted.len() >= PAR_INDEX_MIN
            && self.is_sorted_form()
            && next.is_sorted_form()
            && self.hit_hook.is_none()
            && next.hit_hook.is_none()
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
        let threads = threads.clamp(1, SHARDS);
        if threads <= 1 {
            self.reserve(total);
            for src in sources {
                for &(k, v) in src {
                    self.shards[shard_of(k)].insert(k, v);
                }
            }
            return;
        }
        // gcj j1yg-pause: each job reserves its own shards, so the tables'
        // allocation and control-byte fill run on the workers too; `reserve`
        // on the caller was a serial pass over every new table (~700 K
        // entries on a JIT-warm javac young pause).
        self.thaw();
        let per_shard = total.div_ceil(SHARDS);
        let per = SHARDS.div_ceil(threads);
        let fold = |first: usize, slice: &mut [Shard]| {
            for shard in slice.iter_mut() {
                shard.reserve(per_shard);
            }
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
        let fold = |first: usize, slice: &mut [Shard]| {
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
        type Chunk<'m> = std::sync::Mutex<(usize, &'m mut [Shard], bool)>;
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
    shards: std::slice::Iter<'a, Shard>,
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
    shards: std::vec::IntoIter<Shard>,
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

#[cfg(test)]
mod zt32_hit_hook_tests;

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
    fn into_iter(mut self) -> IntoIter {
        IntoIter {
            sorted: std::mem::take(&mut self.sorted).into_iter(),
            shards: std::mem::take(&mut self.shards).into_iter(),
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
mod tests;
