use super::*;

/// Every address of a mixed, ascending probe set (block bases, interior
/// words, block ends, free holes, duplicates, out of range below and above)
/// gets exactly `is_allocated_addr`'s verdict, with freed holes both
/// coalesced and not.
#[test]
fn gci_i2og_unallocated_among_ascending_matches_is_allocated_addr() {
    for coalesce in [false, true] {
        let mut og = OldGen::new(1 << 20);
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for i in 0..64usize {
            let size = 32 + (i % 5) * 16;
            let p = og.alloc(size, 8).expect("room") as usize;
            blocks.push((p, size));
        }
        // Free every third block, and two neighbours (a run to coalesce).
        for (i, &(p, size)) in blocks.iter().enumerate() {
            if i % 3 == 0 || i == 40 || i == 41 {
                // SAFETY: exactly a block `alloc(size, 8)` returned, freed once.
                unsafe { og.free(p as *mut u8, size) };
            }
        }
        if coalesce {
            let _ = og.coalesce_free_blocks();
        }
        let base = og.base_ptr() as usize;
        let mut probes: Vec<usize> = vec![base.wrapping_sub(8), 0];
        for &(p, size) in &blocks {
            probes.extend([p, p, p + 8, p + size - 8, p + size]);
        }
        probes.push(base + (1 << 20));
        probes.push(usize::MAX - 7);
        probes.sort_unstable();
        let expected: Vec<usize> = probes
            .iter()
            .copied()
            .filter(|&a| !og.is_allocated_addr(a as *const u8))
            .collect();
        assert_eq!(og.unallocated_among_ascending(&probes), expected, "coalesce={coalesce}");
        assert!(
            expected.iter().any(|&a| a >= base && a < base + (1 << 20)),
            "the probe set must contain freed in-range addresses (coalesce={coalesce})"
        );
        assert!(og.unallocated_among_ascending(&[]).is_empty());
    }
}

/// gci i3/og: the keyed variant answers by index, for pairs keyed by
/// their first element (the batched verdict's `(destination, source)`),
/// exactly as `is_allocated_addr` per key; a repeated key is reported at
/// each of its indices.
#[test]
fn gci_i3og_unallocated_indices_ascending_by_matches_is_allocated_addr() {
    let mut og = OldGen::new(1 << 20);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for i in 0..32usize {
        let size = 48 + (i % 3) * 16;
        let p = og.alloc(size, 8).expect("room") as usize;
        blocks.push((p, size));
    }
    for (i, &(p, size)) in blocks.iter().enumerate() {
        if i % 4 == 1 {
            // SAFETY: exactly a block `alloc(size, 8)` returned, freed once.
            unsafe { og.free(p as *mut u8, size) };
        }
    }
    let base = og.base_ptr() as usize;
    let mut pairs: Vec<(usize, usize)> = vec![(0, 7)];
    for (i, &(p, _)) in blocks.iter().enumerate() {
        pairs.push((p, 1000 + i));
        if i == 5 {
            pairs.push((p, 2000 + i)); // a repeated key
        }
    }
    pairs.push((base + (1 << 20), 9));
    pairs.sort_unstable();
    let expected: Vec<usize> = pairs
        .iter()
        .enumerate()
        .filter(|&(_, &(a, _))| !og.is_allocated_addr(a as *const u8))
        .map(|(i, _)| i)
        .collect();
    let got = og.unallocated_indices_ascending_by(&pairs, |&(a, _)| a);
    assert_eq!(got, expected);
    assert!(got.len() > 2, "freed blocks and both out-of-range keys are reported: {got:?}");
    let none: [(usize, usize); 0] = [];
    assert!(og.unallocated_indices_ascending_by(&none, |&(a, _)| a).is_empty());
}
