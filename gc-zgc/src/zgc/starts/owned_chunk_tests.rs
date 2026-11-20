use super::*;

/// `insert_in_owned_chunk` must register exactly what `insert` registers,
/// on both arms, and must not drop a neighbour's bit in a chunk's edge
/// word (the words that keep the `fetch_or`).
#[test]
fn owned_chunk_inserts_register_like_insert_and_keep_edge_neighbours() {
    const BASE: usize = 0x10_0000;
    const SPAN: usize = 64 * 1024;
    for bitmap in [true, false] {
        let starts = ZObjectStarts::with_bitmap(BASE, SPAN, bitmap);
        assert_eq!(starts.is_bitmap(), bitmap);
        // A chunk whose ends are NOT on 512-byte word boundaries.
        let lo = BASE + 512 + 40;
        let hi = BASE + 4 * 512 + 24;
        // A neighbour's base in the chunk's first and last words, set the
        // ordinary way before this thread's objects arrive.
        starts.insert(lo - 8);
        starts.insert(hi);
        let mine = [
            lo,             // edge word (shared): fetch_or
            BASE + 2 * 512, // wholly-owned word: plain store
            BASE + 2 * 512 + 504,
            BASE + 3 * 512 + 8,
            hi - 8, // edge word (shared): fetch_or
        ];
        for &a in &mine {
            starts.insert_in_owned_chunk(a, lo, hi);
        }
        for &a in &mine {
            assert!(
                starts.contains(a),
                "bitmap={bitmap}: {a:#x} must be registered"
            );
        }
        assert!(
            starts.contains(lo - 8),
            "bitmap={bitmap}: neighbour below kept"
        );
        assert!(starts.contains(hi), "bitmap={bitmap}: neighbour above kept");
        assert!(!starts.contains(lo + 8), "bitmap={bitmap}: no stray bit");
        let mut all = starts.bases();
        all.sort_unstable();
        let mut want: Vec<usize> = mine.iter().copied().chain([lo - 8, hi]).collect();
        want.sort_unstable();
        assert_eq!(all, want, "bitmap={bitmap}: exactly the inserted bases");
        // An off-grid address still spills rather than being dropped.
        if bitmap {
            starts.insert_in_owned_chunk(lo + 4, lo, hi);
            assert!(starts.contains(lo + 4));
            assert!(starts.has_spill());
        }
    }
}
