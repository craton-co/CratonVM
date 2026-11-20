use super::*;

fn words(bm: &HeapBitmap) -> Vec<u64> {
    (0..bm.word_count()).map(|w| bm.word_at(w)).collect()
}

/// Every edge shape (inside one word, word-aligned ends, ragged ends,
/// clamped past the span, empty) over a bitmap already holding bits.
#[test]
fn r23_w8_set_range_single_writer_equals_set_range() {
    let base = 0x10_0000usize;
    let span = 1usize << 16;
    let ranges = [
        (base + 8, base + 64),
        (base + 512, base + 1024),
        (base + 520, base + 4096 + 24),
        (base + 1000, base + 1001),
        (base + span - 40, base + span + 4096),
        (base, base + span),
        (base + 64, base + 64),
        (base - 64, base + 16),
    ];
    for &(lo, hi) in &ranges {
        let a = HeapBitmap::labelled(base, span, "test");
        let b = HeapBitmap::labelled(base, span, "test");
        for bm in [&a, &b] {
            bm.insert(base + 0x800);
            bm.insert(base + 8);
        }
        a.set_range(lo, hi);
        b.set_range_single_writer(lo, hi);
        assert_eq!(words(&a), words(&b), "range {:#x}..{:#x}", lo - base, hi.wrapping_sub(base));
    }
}

/// The plain store replaces the word and ignores an out-of-range index.
#[test]
fn r23_w8_store_word_single_writer_replaces_the_word() {
    let bm = HeapBitmap::labelled(0x10_0000, 1 << 16, "test");
    bm.store_word_single_writer(3, 0xF0F0);
    assert_eq!(bm.word_at(3), 0xF0F0);
    bm.store_word_single_writer(3, 0xF000);
    assert_eq!(bm.word_at(3), 0xF000);
    bm.store_word_single_writer(bm.word_count(), 1);
    assert_eq!(words(&bm).iter().filter(|&&w| w != 0).count(), 1);
}

/// zgc pc3: the plain-store prune leaves the words the locked one does,
/// and the summarised live walk names the bases the plain walk does,
/// before and after the empty blocks are forgotten (a stale set bit only
/// costs a word scan; a clear bit is skipped).
#[test]
fn zgc_pc3_plain_retain_and_summarised_bases_agree_with_the_locked_forms() {
    const BASE: usize = 0x4000_0000;
    let span = 8 * 1024 * 1024; // 16 384 words, 256 blocks
    let build = || {
        let starts = HeapBitmap::labelled(BASE, span, "test");
        let marks = HeapBitmap::labelled(BASE, span, "test");
        // A dense dead run, survivors scattered through it, one survivor
        // on each side of a block edge, and an empty stretch.
        for i in 0..20_000usize {
            let a = BASE + i * 144;
            starts.insert(a);
            if i % 997 == 0 || i == 227 || i == 228 {
                marks.insert(a);
            }
        }
        starts.insert(BASE + span - 8);
        marks.insert(BASE + span - 8);
        (starts, marks)
    };
    let ranges = [(BASE, BASE + span)];
    let (locked, marks) = build();
    let (plain, _) = build();
    locked.retain_marked_within(&marks, &ranges);
    plain.retain_marked_within_plain(&marks, &ranges);
    assert_eq!(words(&locked), words(&plain));
    let expect = locked.bases_within(&ranges);
    assert!(expect.len() > 20, "{}", expect.len());
    assert_eq!(
        plain.bases_within_summarised(&ranges),
        expect,
        "stale summary bits"
    );
    assert!(plain.forget_empty_blocks() > 0);
    assert_eq!(
        plain.bases_within_summarised(&ranges),
        expect,
        "forgotten blocks"
    );
    assert_eq!(plain.bases_within(&ranges), expect);
}
