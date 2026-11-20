use super::*;

fn obj(slots: u32) -> ObjectHeader {
    ObjectHeader::new(ClassId::new(7), ObjectKind::Object, ArrayElementType::Reference, 0, slots)
}

/// A small from-space: objects of mixed sizes laid end to end, one free
/// block, some side-marked, one header-marked; the fused chunk's sweep
/// half equals `sweep_chunk` on the same range, and the promotion half is
/// unchanged by fusing.
#[test]
fn r23_w6_fused_chunk_equals_sweep_chunk() {
    let mut buf = vec![0u64; 4096];
    let base = buf.as_mut_ptr() as usize;
    let mut off = 0usize;
    let mut starts = Vec::new();
    for i in 0..40u32 {
        let h = obj(1 + i % 5);
        let size = gen_object_total_size(&h);
        // SAFETY: inside `buf` (40 objects of at most a few hundred bytes).
        unsafe { std::ptr::write((base + off) as *mut ObjectHeader, h) };
        starts.push(off);
        off += size;
        if i == 19 {
            off += 256; // a free block the walks must skip
        }
    }
    let used = off;
    let free_at = starts[19] + gen_object_total_size(&obj(1 + 19 % 5));
    let free = vec![(free_at, 256usize)];
    let bits = cratonvm_gc_core::young_mark::YoungMarkBits::new(base, used);
    for (i, &s) in starts.iter().enumerate() {
        if i % 3 == 0 {
            assert!(bits.try_mark(base + s));
        }
    }
    // One header-marked, not side-marked survivor.
    // SAFETY: a header written above.
    unsafe { &*((base + starts[4]) as *const ObjectHeader) }.set_gc_flags(GC_FLAG_MARKED);
    let side_sorted: Vec<usize> = starts
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 3 == 0)
        .map(|(_, &s)| base + s)
        .collect();
    let pinned = FxHashSet::default();
    let sweep = sweep_chunk(
        &SweepCtx {
            from_base: base,
            used,
            existing_free: &free,
            side_bits: &bits,
            side_sorted: &side_sorted,
            old_lo: 0,
            old_hi: 0,
            watched: None,
            size_memo: false,
        },
        0,
        used,
    )
    .expect("a clean grid");
    let ctx = |fuse| PromotionWalkCtx {
        from_base: base,
        used,
        free_blocks: &free,
        side_bits: &bits,
        side_sorted: &side_sorted,
        pinned: &pinned,
        selective_on: true,
        force_tenure: false,
        promotion_age: 15,
        size_memo: false,
        fuse,
    };
    let plain = promotion_walk_chunk(&ctx(None), 0, used).expect("clean");
    let fused = promotion_walk_chunk(
        &ctx(Some(FusedSweepCtx {
            old_lo: 0,
            old_hi: 0,
            watched: None,
        })),
        0,
        used,
    )
    .expect("clean");
    let s = fused.sweep.as_ref().expect("fused");
    assert!(plain.sweep.is_none());
    assert_eq!(s.dead, sweep.dead);
    assert_eq!(s.survivors, sweep.survivors);
    assert_eq!(s.live, sweep.live);
    assert_eq!(s.side_with_header_mark, sweep.side_with_header_mark);
    assert_eq!(s.forward_targets, sweep.forward_targets);
    assert_eq!(fused.candidates, plain.candidates);
    assert_eq!(fused.age_bumps, plain.age_bumps);
    assert_eq!(fused.side_marked, plain.side_marked);
    assert_eq!(fused.header_marked, plain.header_marked);
    assert!(!s.dead.is_empty() && !s.survivors.is_empty(), "the fixture exercises both arms");
    drop(buf);
}
