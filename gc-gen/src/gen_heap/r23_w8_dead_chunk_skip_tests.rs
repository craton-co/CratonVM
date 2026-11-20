use super::*;

fn obj(slots: u32) -> ObjectHeader {
    ObjectHeader::new(ClassId::new(7), ObjectKind::Object, ArrayElementType::Reference, 0, slots)
}

/// 120 objects end to end with a 256-byte free block after object 59.
/// Returns (backing, base, starts, used, free).
fn fixture() -> (Vec<u64>, usize, Vec<usize>, usize, Vec<(usize, usize)>) {
    let mut buf = vec![0u64; 16384];
    let base = buf.as_mut_ptr() as usize;
    let mut off = 0usize;
    let mut starts = Vec::new();
    let mut free = Vec::new();
    for i in 0..120u32 {
        let h = obj(1 + i % 5);
        let size = gen_object_total_size(&h);
        // SAFETY: inside `buf` (120 objects of at most a few hundred bytes).
        unsafe { std::ptr::write((base + off) as *mut ObjectHeader, h) };
        starts.push(off);
        off += size;
        if i == 59 {
            free.push((off, 256usize));
            off += 256;
        }
    }
    (buf, base, starts, off, free)
}

fn walk_ctx<'a>(
    base: usize,
    used: usize,
    free: &'a [(usize, usize)],
    bits: &'a cratonvm_gc_core::young_mark::YoungMarkBits,
    side_sorted: &'a [usize],
    pinned: &'a FxHashSet<usize>,
    fused: bool,
) -> PromotionWalkCtx<'a> {
    PromotionWalkCtx {
        from_base: base,
        used,
        free_blocks: free,
        side_bits: bits,
        side_sorted,
        pinned,
        selective_on: true,
        force_tenure: false,
        promotion_age: 15,
        size_memo: false,
        fuse: fused.then_some(FusedSweepCtx {
            old_lo: 0,
            old_hi: 0,
            watched: None,
        }),
    }
}

/// Byte ranges of a chunk's dead spans, coalesced.
fn coalesced(dead: &[(usize, usize, u32, u8, usize)]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &(off, sz, ..) in dead {
        match out.last_mut() {
            Some(last) if last.0 + last.1 == off => last.1 += sz,
            _ => out.push((off, sz)),
        }
    }
    out
}

/// Marks in chunks 0 and 2 only: the screened walk declares chunk 1 dead
/// whole, frees exactly the bytes the walk frees there, and decides every
/// promotion candidate, age bump and walked chunk exactly as the walk.
#[test]
fn r23_w8_a_chunk_without_live_data_is_dead_whole_and_frees_what_the_walk_frees() {
    let (buf, base, starts, used, free) = fixture();
    let bits = cratonvm_gc_core::young_mark::YoungMarkBits::new(base, used);
    let mut side_sorted = Vec::new();
    for (i, &s) in starts.iter().enumerate() {
        if (i < 40 && i % 3 == 0) || (i >= 80 && i % 4 == 0) {
            assert!(bits.try_mark(base + s));
            side_sorted.push(base + s);
        }
    }
    let pinned = FxHashSet::default();
    let ctx = walk_ctx(base, used, &free, &bits, &side_sorted, &pinned, true);
    let anchors = [0, starts[40], starts[80], used];
    let plain = parallel_promotion_walk(&ctx, &anchors, 1, None).expect("clean grid");
    let screen = DeadChunkScreen { keep: &[] };
    let screened = parallel_promotion_walk_screened(&ctx, &anchors, 1, None, Some(&screen))
        .expect("clean grid");
    assert_eq!(screened.len(), 3);
    assert_eq!(screened[0].dead_chunk_bytes, 0, "chunk 0 holds marks: walked");
    assert_eq!(screened[1].dead_chunk_bytes, starts[80] - starts[40]);
    assert_eq!(screened[2].dead_chunk_bytes, 0, "chunk 2 holds marks: walked");
    for (i, (sc, pl)) in screened.iter().zip(plain.iter()).enumerate() {
        assert_eq!(sc.candidates, pl.candidates, "chunk {i}");
        assert_eq!(sc.age_bumps, pl.age_bumps, "chunk {i}");
        let (s, p) = (
            sc.sweep.as_ref().expect("fused"),
            pl.sweep.as_ref().expect("fused"),
        );
        assert_eq!(s.survivors, p.survivors, "chunk {i}");
        assert_eq!(s.live, p.live, "chunk {i}");
        if i == 1 {
            assert_eq!(coalesced(&s.dead), coalesced(&p.dead), "the same bytes freed");
            assert_eq!(
                coalesced(&s.dead),
                vec![
                    (starts[40], free[0].0 - starts[40]),
                    (free[0].0 + free[0].1, starts[80] - free[0].0 - free[0].1),
                ],
                "everything outside the free block"
            );
            assert_eq!(p.live, 0);
        } else {
            assert_eq!(s.dead, p.dead, "chunk {i}");
        }
    }
    assert!(!plain[0].age_bumps.is_empty(), "the fixture ages survivors");
    drop(buf);
}

/// Every refusal: a marked base in the chunk, a keep address in it, a
/// `lo` inside a marked object, and an unfused walk.
#[test]
fn r23_w8_the_dead_chunk_screen_refuses_whatever_the_walk_must_see() {
    let (buf, base, starts, used, free) = fixture();
    let bits = cratonvm_gc_core::young_mark::YoungMarkBits::new(base, used);
    assert!(bits.try_mark(base + starts[10]));
    assert!(bits.try_mark(base + starts[40]));
    let side_sorted = vec![base + starts[10], base + starts[40]];
    let pinned = FxHashSet::default();
    let ctx = walk_ctx(base, used, &free, &bits, &side_sorted, &pinned, true);
    let none = DeadChunkScreen { keep: &[] };
    // A marked base inside.
    assert!(dead_chunk_sweep(&ctx, &none, starts[30], starts[50]).is_none());
    // Clear of marks: dead whole.
    assert!(dead_chunk_sweep(&ctx, &none, starts[50], starts[90]).is_some());
    // A keep word (a conservative candidate, interior or not) inside.
    let keep = [base + starts[70] + 8];
    let kept = DeadChunkScreen { keep: &keep };
    assert!(dead_chunk_sweep(&ctx, &kept, starts[50], starts[90]).is_none());
    assert!(dead_chunk_sweep(&ctx, &kept, starts[80], starts[90]).is_some());
    // `lo` inside the marked object at `starts[40]`: its extent reaches
    // past `lo`.
    assert!(dead_chunk_sweep(&ctx, &none, starts[40] + 8, starts[48]).is_none());
    // The marked object ends exactly at `lo`: allowed.
    assert!(dead_chunk_sweep(&ctx, &none, starts[41], starts[48]).is_some());
    // Unfused (no phantom rule in the walked chunks): never.
    let unfused = walk_ctx(base, used, &free, &bits, &side_sorted, &pinned, false);
    assert!(dead_chunk_sweep(&unfused, &none, starts[50], starts[90]).is_none());
    // Degenerate bounds.
    assert!(dead_chunk_sweep(&ctx, &none, starts[90], starts[90]).is_none());
    assert!(dead_chunk_sweep(&ctx, &none, starts[90], used + 8).is_none());
    drop(buf);
}

/// A span shorter than a header between two free blocks (a lone GAP
/// filler) is left alone, as the walk leaves it; the rest is one span
/// per gap between free blocks.
#[test]
fn r23_w8_a_sub_header_span_between_free_blocks_stays() {
    let mut buf = vec![0u64; 512];
    let base = buf.as_mut_ptr() as usize;
    let used = 4096usize;
    let free = vec![(64usize, 64usize), (136, 64)];
    let bits = cratonvm_gc_core::young_mark::YoungMarkBits::new(base, used);
    let side_sorted: Vec<usize> = Vec::new();
    let pinned = FxHashSet::default();
    let ctx = walk_ctx(base, used, &free, &bits, &side_sorted, &pinned, true);
    let chunk = dead_chunk_sweep(&ctx, &DeadChunkScreen { keep: &[] }, 0, 400)
        .expect("no live data");
    let spans: Vec<(usize, usize)> = chunk
        .sweep
        .as_ref()
        .expect("fused")
        .dead
        .iter()
        .map(|&(o, s, ..)| (o, s))
        .collect();
    assert_eq!(spans, vec![(0, 64), (200, 200)], "the 8 bytes at 128 stay");
    assert_eq!(chunk.dead_chunk_bytes, 400);
    assert!(chunk.candidates.is_empty() && chunk.age_bumps.is_empty());
    drop(buf);
}

/// Wave 8b: the fine grid keeps 0 and `used`, stays strictly ascending,
/// takes one anchor per 256 KiB bucket, and refuses a grid of the wrong
/// shape.
#[test]
fn r23_w8_fine_dead_chunk_anchors_subsample_the_grid() {
    let k = 1024usize;
    let used = 2048 * k;
    let grid = vec![0, 16 * k, 300 * k, 310 * k, 600 * k, 1100 * k, 1100 * k + 8, 2000 * k, used];
    let fine = fine_dead_chunk_anchors(&grid, used);
    assert_eq!(fine, vec![0, 300 * k, 600 * k, 1100 * k, 2000 * k, used]);
    assert!(fine.windows(2).all(|w| w[0] < w[1]));
    assert!(fine.iter().all(|a| grid.contains(a)), "only certified anchors");
    assert!(fine_dead_chunk_anchors(&grid[1..], used).is_empty(), "must start at 0");
    assert!(fine_dead_chunk_anchors(&grid, used + 8).is_empty(), "must end at used");
    assert!(fine_dead_chunk_anchors(&[], 0).is_empty());
    // `used` itself is kept even when it shares a bucket with the last anchor.
    let tight = vec![0, 100 * k, 100 * k + 64];
    assert_eq!(fine_dead_chunk_anchors(&tight, 100 * k + 64), vec![0, 100 * k + 64]);
}


