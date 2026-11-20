use super::*;

const MIB: usize = 1024 * 1024;

/// zr z10/mt: `CRATONVM_ZGC_SIDE_TABLE_NO_THP` (default ON) puts the
/// bitmaps of a heap whose bitmaps reach the threshold (64 MiB arena:
/// 1 MiB of words) on mappings of their own on Linux, and the spans line
/// counts them; a heap under the threshold, another platform, or the kill
/// switch leaves them allocator blocks. Either way the words are outside
/// the arena.
#[test]
fn zr10mt_big_bitmaps_sit_on_their_own_mappings_and_the_spans_line_counts_them() {
    let heap = ZgcRealHeap::with_capacity(64 * MIB);
    let on = zgc_side_table_no_thp_enabled() && cfg!(target_os = "linux");
    let spans = heap.footprint_spans();
    let present = |r: (usize, usize)| usize::from(r != (0, 0));
    let bitmaps =
        present(spans.start_bits) + present(spans.mark_bits) + present(spans.old_bits);
    assert!(bitmaps >= 1, "{spans:?}");
    assert_eq!(spans.side_maps, if on { bitmaps } else { 0 }, "{spans:?}");
    let (lo, hi) = spans.start_bits;
    assert!(hi <= spans.arena.0 || lo >= spans.arena.1, "{spans:?}");
    assert!(spans
        .render()
        .contains(&format!(" side_maps={} arena_no_thp=", spans.side_maps)));
    // The old-start bitmap, built on first use, follows the same switch.
    assert_eq!(heap.old_bits().on_own_mapping(), on);
    let after = heap.footprint_spans();
    let bitmaps =
        present(after.start_bits) + present(after.mark_bits) + present(after.old_bits);
    assert_eq!(after.side_maps, if on { bitmaps } else { 0 }, "{after:?}");

    let small = ZgcRealHeap::with_capacity(16 * MIB);
    assert_eq!(
        small.footprint_spans().side_maps,
        0,
        "256 KiB bitmaps stay put"
    );
}
