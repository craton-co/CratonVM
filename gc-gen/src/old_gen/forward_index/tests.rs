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
