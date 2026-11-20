use super::*;

const BASE: usize = 0x4000_0000;
// 8 MiB of young: 1 Mi bits, 16 Ki words, 32 summary blocks.
const SPAN: usize = 8 << 20;

#[test]
fn r23_w9_summary_collect_equals_the_full_scan() {
    let on = YoungMarkBits::new(BASE, SPAN);
    assert!(!on.summary.is_empty());
    let block_bytes = MARK_SUMMARY_BLOCK_WORDS * 64 * 8;
    let addrs = [
        BASE,
        BASE + 8,
        BASE + block_bytes - 8,
        BASE + block_bytes,
        BASE + 5 * block_bytes + 4096,
        BASE + 17 * block_bytes + 520,
        BASE + SPAN - 8,
    ];
    for b in [&on] {
        for &a in &addrs {
            assert!(b.try_mark(a));
            assert!(!b.try_mark(a), "claimed once");
        }
        assert!(b.unmark(BASE + 5 * block_bytes + 4096));
    }
    let got = on.collect_marked();
    let mut want: Vec<usize> = addrs
        .iter()
        .copied()
        .filter(|&a| a != BASE + 5 * block_bytes + 4096)
        .collect();
    want.sort_unstable();
    assert_eq!(got, want);
    // Only the touched blocks are read: blocks 0, 1, 5 (unmarked, kept),
    // 17 and 31, with 0 and 1 merged into one range.
    let ranges = on.marked_word_ranges();
    assert_eq!(ranges.len(), 4, "{ranges:?}");
    let read: usize = ranges.iter().map(|&(lo, hi)| hi - lo).sum();
    assert_eq!(read, 5 * MARK_SUMMARY_BLOCK_WORDS);    }

/// An unmarked bitmap reads nothing; a zero-span one is empty either way.
#[test]
fn r23_w9_summary_of_an_empty_bitmap_reads_nothing() {
    let on = YoungMarkBits::new(BASE, SPAN);
    assert!(on.marked_word_ranges().is_empty());
    assert!(on.collect_marked().is_empty());
    let zero = YoungMarkBits::new(BASE, 0);
    assert!(zero.marked_word_ranges().is_empty());
    assert!(zero.collect_marked().is_empty());
}

/// Concurrent markers: every claim is recorded in the summary.
#[test]
fn r23_w9_summary_sees_marks_from_every_thread() {
    let on = YoungMarkBits::new(BASE, SPAN);
    std::thread::scope(|s| {
        for t in 0..4usize {
            let b = &on;
            s.spawn(move || {
                for i in 0..256usize {
                    b.try_mark(BASE + ((i * 4 + t) * 32 * 1024) % SPAN);
                }
            });
        }
    });
    let off_list: Vec<usize> = {
        let mut v: Vec<usize> = (0..4usize)
            .flat_map(|t| (0..256usize).map(move |i| BASE + ((i * 4 + t) * 32 * 1024) % SPAN))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    assert_eq!(on.collect_marked(), off_list);
}
