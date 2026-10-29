// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j1/og (2026-10-19) — the sliding compactors' forwarding lookup without
//! a hash map (`CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX`, default ON; `=0`
//! keeps the hash map).
//!
//! Both old-generation compactors (`OldGen::compact_walked`, the full-GC
//! mark-compact, and `OldGen::compact_around_pins_watching`, the pinned
//! compaction) resolve every reference slot of every survivor through a
//! "survivor base -> destination" table in their Phase 2. That table used to
//! be an `FxHashMap` filled by one insert per survivor in Phase 1: on a major
//! with a million survivors a table of tens of MiB, rehashed several times as
//! it grew, and probed at a random bucket for every old-to-old edge — one cache
//! miss per edge in the pass HotSpot Serial's `adjust` phase does with a
//! header read.
//!
//! The survivors are already in ascending SOURCE order (Phase 1 iterates the
//! walked grid, which ascends), so the same table is a sorted array. A
//! [`ForwardIndex`] keeps it as one `Vec<(source, destination)>` plus a block
//! directory: `first[b]` is the index of the first entry whose source lies at
//! or above `base + b * BLOCK`. A lookup is one directory read and a binary
//! search over the few entries of one 512-byte block (at most 32 objects at
//! the 16-byte minimum object size), which are contiguous and usually share a
//! cache line with the holder's own entry when the referent is nearby — the
//! common case after an earlier compaction laid objects out in allocation
//! order.
//!
//! The answers are EXACTLY the hash map's: `get(a)` is `Some(dest)` iff `a`
//! is the source of an entry, and an interior or unrelated address answers
//! `None` (the compactors' dangling-ref guard relies on that). Pinned by
//! `tests::gcj_j1og_the_index_answers_exactly_as_the_hash_map`.
//!
//! The directory costs 4 bytes per 512 bytes of the generation up to its last
//! survivor (8 MiB per GiB), bounded by the high-water mark rather than the
//! reservation, so a lightly used large reservation pays nothing for its
//! untouched top. gcj j2/x1: a SPARSE table (more than
//! [`SPARSE_BLOCKS_PER_ENTRY`] blocks per survivor, past a 2 MiB allowance)
//! falls back to the hash map, whose cost follows the survivor count: one
//! survivor near the top of a 4 GiB old generation would otherwise build a
//! 32 MiB directory inside the pause.

use rustc_hash::FxHashMap;

/// The kill switch (default ON; `=0` keeps the hash map).
pub(super) const FORWARD_INDEX_FLAG: &str = "CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX";

/// log2 of the directory's block size (512 bytes).
const BLOCK_SHIFT: u32 = 9;

/// gcj j2/x1: the directory is refused when it would need more than this many
/// blocks (4 bytes each) per entry, past [`SPARSE_BLOCK_ALLOWANCE`]. A hash
/// map entry costs about 32 bytes with its load factor, so 16 blocks (64
/// bytes) is twice the table it replaces.
const SPARSE_BLOCKS_PER_ENTRY: usize = 16;

/// gcj j2/x1: blocks always admitted (4096 blocks = 2 MiB of generation,
/// a 16 KiB directory), so a small table never falls back.
const SPARSE_BLOCK_ALLOWANCE: usize = 1 << 12;

/// A sorted `source -> destination` table with a block directory (see the
/// module doc).
pub(super) struct ForwardIndex {
    /// The generation's first byte; every source is at or above it.
    base: usize,
    /// `first[b]` .. `first[b + 1]`: the entries whose source lies in block
    /// `b`. One more element than there are blocks (a sentinel).
    first: Vec<u32>,
    /// `(source, destination)`, strictly ascending by source.
    entries: Vec<(usize, usize)>,
}

impl ForwardIndex {
    /// Build over `entries` (`(source, destination)`, strictly ascending by
    /// source, every source at or above `base`). Hands `entries` back when
    /// the index cannot hold them (more than `u32::MAX` entries, or a source
    /// below `base`), so the caller can fall back to the hash map.
    pub(super) fn build(
        base: usize,
        entries: Vec<(usize, usize)>,
    ) -> Result<Self, Vec<(usize, usize)>> {
        if entries.len() >= u32::MAX as usize {
            return Err(entries);
        }
        if entries.first().is_some_and(|&(src, _)| src < base) {
            return Err(entries);
        }
        // Checked in every build, not only in debug: a lookup over an
        // out-of-order table would MISS a survivor and leave its referrer's
        // slot unrewritten, while the hash map is order-blind. One linear
        // pass, against Phase 2's pass over every edge.
        if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
            debug_assert!(
                false,
                "ForwardIndex::build: entries must be strictly ascending by source"
            );
            return Err(entries);
        }
        let blocks = match entries.last() {
            Some(&(src, _)) => ((src - base) >> BLOCK_SHIFT) + 1,
            None => 0,
        };
        // gcj j2/x1: a sparse table's directory would dwarf the hash map.
        let admitted = entries
            .len()
            .saturating_mul(SPARSE_BLOCKS_PER_ENTRY)
            .saturating_add(SPARSE_BLOCK_ALLOWANCE);
        if blocks > admitted {
            return Err(entries);
        }
        let mut first: Vec<u32> = Vec::with_capacity(blocks + 1);
        for (i, &(src, _)) in entries.iter().enumerate() {
            let b = (src - base) >> BLOCK_SHIFT;
            // Every block up to this entry's own that has no entry yet starts
            // here: its first entry at or above it is this one.
            while first.len() <= b {
                first.push(i as u32);
            }
        }
        // The sentinel (and nothing else: the last entry's block is the last
        // block, so `first.len() == blocks` here).
        while first.len() <= blocks {
            first.push(entries.len() as u32);
        }
        Ok(Self {
            base,
            first,
            entries,
        })
    }

    /// The destination of the entry whose source is exactly `addr`.
    #[inline]
    pub(super) fn get(&self, addr: usize) -> Option<usize> {
        let off = addr.checked_sub(self.base)?;
        let b = off >> BLOCK_SHIFT;
        if b + 1 >= self.first.len() {
            return None;
        }
        let lo = self.first[b] as usize;
        let hi = self.first[b + 1] as usize;
        let run = &self.entries[lo..hi];
        match run.binary_search_by_key(&addr, |&(src, _)| src) {
            Ok(i) => Some(run[i].1),
            Err(_) => None,
        }
    }

    /// How many entries the index holds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What a compactor's Phase 2 resolves a referent through: the index (the
/// default) or the hash map it replaced (`=0`, or an index that could not be
/// built).
pub(super) enum CompactDestinations {
    Hash(FxHashMap<usize, usize>),
    Index(ForwardIndex),
}

impl CompactDestinations {
    /// From Phase 1's survivors, `(source, destination)` in ascending source
    /// order. `use_index`: the switch, read once per compaction by the caller.
    pub(super) fn from_ascending(
        base: usize,
        entries: Vec<(usize, usize)>,
        use_index: bool,
    ) -> Self {
        let entries = if use_index {
            match ForwardIndex::build(base, entries) {
                Ok(ix) => return Self::Index(ix),
                Err(back) => back,
            }
        } else {
            entries
        };
        Self::Hash(entries.into_iter().collect())
    }

    /// The destination of the survivor whose base is exactly `addr`.
    #[inline]
    pub(super) fn get(&self, addr: usize) -> Option<usize> {
        match self {
            Self::Hash(m) => m.get(&addr).copied(),
            Self::Index(ix) => ix.get(addr),
        }
    }

    /// Did this compaction resolve through the index?
    pub(super) fn is_index(&self) -> bool {
        matches!(self, Self::Index(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift, so a failing seed can be replayed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    /// Random ascending 8-aligned survivor layouts (dense runs, sparse runs,
    /// gaps of many blocks), slid as the compactor slides them; every address
    /// of the covered range plus a margin on both sides is asked of both
    /// tables.
    #[test]
    fn gcj_j1og_the_index_answers_exactly_as_the_hash_map() {
        let base = 0x10_0000usize;
        for seed in 1..=48u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut entries: Vec<(usize, usize)> = Vec::new();
            let mut src = base + (rng.next() % 4) as usize * 8;
            let mut cursor = base;
            let n = (rng.next() % 600) as usize;
            for _ in 0..n {
                let size = 16 + (rng.next() % 12) as usize * 8;
                entries.push((src, cursor));
                cursor += size;
                // Dead gap after this survivor: none, a few words, or blocks.
                let gap = match rng.next() % 4 {
                    0 => 0,
                    1 => (rng.next() % 8) as usize * 8,
                    2 => (rng.next() % 200) as usize * 8,
                    _ => (rng.next() % 3000) as usize * 8,
                };
                src += size + gap;
            }
            let map: FxHashMap<usize, usize> = entries.iter().copied().collect();
            let ix = ForwardIndex::build(base, entries.clone()).unwrap_or_else(|_| {
                panic!("seed {seed}: build refused a valid layout");
            });
            assert_eq!(ix.len(), entries.len(), "seed {seed}");
            let end = src + 4096;
            let mut a = base.saturating_sub(64);
            while a < end {
                assert_eq!(ix.get(a), map.get(&a).copied(), "seed {seed} addr {a:#x}");
                a += 4;
            }
            // The wrapper answers the same through both arms.
            let by_ix = CompactDestinations::from_ascending(base, entries.clone(), true);
            let by_map = CompactDestinations::from_ascending(base, entries.clone(), false);
            assert!(by_ix.is_index());
            assert!(!by_map.is_index());
            for &(s, d) in &entries {
                assert_eq!(by_ix.get(s), Some(d), "seed {seed}");
                assert_eq!(by_map.get(s), Some(d), "seed {seed}");
                assert_eq!(by_ix.get(s + 8), by_map.get(s + 8), "seed {seed}");
            }
        }
    }

    #[test]
    fn gcj_j1og_an_empty_index_answers_none_everywhere() {
        let ix = ForwardIndex::build(0x1000, Vec::new()).unwrap_or_else(|_| panic!("refused"));
        assert_eq!(ix.len(), 0);
        for a in [0usize, 0x0fff, 0x1000, 0x1008, 0x2000, usize::MAX] {
            assert_eq!(ix.get(a), None);
        }
    }

    #[test]
    fn gcj_j1og_a_source_below_the_base_falls_back_to_the_hash_map() {
        let entries = vec![(0x800usize, 0x800usize), (0x1000, 0x810)];
        assert!(ForwardIndex::build(0x1000, entries.clone()).is_err());
        let d = CompactDestinations::from_ascending(0x1000, entries, true);
        assert!(!d.is_index());
        assert_eq!(d.get(0x800), Some(0x800));
        assert_eq!(d.get(0x1000), Some(0x810));
    }

    /// gcj j2/x1: one survivor near the top of a 1 GiB span and one at its
    /// base would need a 2M-block (8 MiB) directory for two entries: the
    /// build refuses and the wrapper answers through the hash map, the same
    /// answers. A table at the allowance's edge is still indexed.
    #[test]
    fn gcj_j2x1_a_sparse_table_falls_back_to_the_hash_map() {
        let base = 0x1000usize;
        let far = base + (1usize << 30);
        let entries = vec![(base, base), (far, base + 16)];
        assert!(ForwardIndex::build(base, entries.clone()).is_err());
        let d = CompactDestinations::from_ascending(base, entries, true);
        assert!(!d.is_index(), "sparse: the hash map");
        assert_eq!(d.get(base), Some(base));
        assert_eq!(d.get(far), Some(base + 16));
        assert_eq!(d.get(far + 8), None);
        // Two entries admit 2 * 16 + 4096 blocks: the last one in block
        // 4127 (4128 blocks) is indexed, one block further is not.
        let block = 1usize << BLOCK_SHIFT;
        let limit = 2 * SPARSE_BLOCKS_PER_ENTRY + SPARSE_BLOCK_ALLOWANCE;
        let edge = vec![(base, base), (base + (limit - 1) * block, base + 16)];
        assert!(ForwardIndex::build(base, edge).is_ok(), "at the allowance");
        let past = vec![(base, base), (base + limit * block, base + 16)];
        assert!(ForwardIndex::build(base, past).is_err(), "one block past it");
    }

    /// Entries exactly on block boundaries and in the last byte of a block
    /// land in the right block.
    #[test]
    fn gcj_j1og_block_edges_resolve() {
        let base = 0x4_0000usize;
        let block = 1usize << BLOCK_SHIFT;
        let entries = vec![
            (base, base),
            (base + block - 8, base + 16),
            (base + block, base + 32),
            (base + 5 * block - 16, base + 48),
            (base + 5 * block, base + 64),
        ];
        let ix = ForwardIndex::build(base, entries.clone()).unwrap_or_else(|_| panic!("refused"));
        for &(s, d) in &entries {
            assert_eq!(ix.get(s), Some(d));
        }
        assert_eq!(ix.get(base + 8), None);
        assert_eq!(ix.get(base + 3 * block), None);
        assert_eq!(ix.get(base + 5 * block + 8), None);
        assert_eq!(ix.get(base + 6 * block), None);
    }
}
