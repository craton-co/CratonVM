use super::*;

#[test]
fn r23_w4_lazy_bounds_follow_the_parallel_walks_anchor_rules() {
    // Anchors: one inside a skip block (dropped), one past `used`
    // (dropped), duplicates; skip ends join; stride 0x1000 subsamples.
    let skips = [(0x1800usize, 0x400usize), (0x5000, 0x800)];
    let anchors = [0x0800usize, 0x1000, 0x1000, 0x1900, 0x3000, 0x3100, 0x9000];
    let b = lazy_young_start_bounds(&anchors, &skips, 0x6000, 0x1000);
    assert_eq!(b, vec![0, 0x1000, 0x3000, 0x5800, 0x6000]);
    // The shape `new_lazy` accepts.
    assert!(cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
        0x10_0000,
        0x6000,
        b,
        skips.to_vec(),
        lazy_objstart_chunk_walker,
    )
    .is_some());
    // Nothing recorded: one chunk over the whole span.
    assert_eq!(lazy_young_start_bounds(&[], &[], 0x2000, 0x1000), vec![0, 0x2000]);
}

/// A synthetic chunk walker for the poisoning test: a start every 64
/// bytes; the chunk holding offset 2048 fails.
fn every_64_failing_at_2048(
    base: usize,
    lo: usize,
    hi: usize,
    _skips: &[(usize, usize)],
    bits: &cratonvm_gc_core::young_mark::ObjectStartBits,
) -> bool {
    if lo <= 2048 && 2048 < hi {
        return false;
    }
    let mut o = lo;
    while o < hi {
        bits.insert(base + o);
        o += 64;
    }
    true
}

/// A dead span in a proven chunk gets its headers poisoned; a dead span
/// touching a failed chunk is zeroed whole (fail closed).
#[test]
fn r23_w4_poison_through_a_lazy_grid_memsets_a_failed_chunks_span() {
    let mut words = vec![0xABAB_ABAB_ABAB_ABABu64; 512];
    let base = words.as_mut_ptr() as usize;
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
        base,
        4096,
        vec![0, 2048, 4096],
        Vec::new(),
        every_64_failing_at_2048,
    )
    .expect("a usable grid");
    let dead = [(64usize, 128usize), (2048 + 64, 128)];
    // SAFETY: the spans are inside `words`, which nothing else touches.
    let n = unsafe { poison_dead_object_headers(base, &dead, &starts) };
    assert_eq!(n, 2, "64 and 128 in the proven chunk");
    // SAFETY: `words` is 4096 initialized bytes, read only from here on.
    let buf = unsafe { std::slice::from_raw_parts(base as *const u8, 4096) };
    let zero = |o: usize, l: usize| buf[o..o + l].iter().all(|&b| b == 0);
    let stale = |o: usize, l: usize| buf[o..o + l].iter().all(|&b| b == 0xAB);
    assert!(zero(64, HEADER_SIZE) && zero(128, HEADER_SIZE));
    assert!(stale(64 + HEADER_SIZE, 64 - HEADER_SIZE), "a proven span keeps its payload");
    assert!(zero(2048 + 64, 128), "the failed chunk's span is zeroed whole");
    assert!(stale(2048 + 192, 8), "only the span");
}


/// gcj pcp: with precise edges skipping the bitmap (the default), a chunk
/// first walked after a forward is proven: the forwarded header sizes as
/// its object, and every start in the chunk -- the forwarded one included
/// -- is recorded.
#[test]
fn r23_w4b_lazy_walker_proves_a_chunk_with_a_forwarded_header_by_default() {
    let obj = || {
        ObjectHeader::new(
            ClassId::new(7),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            1,
        )
    };
    let size = gen_object_total_size(&obj());
    let n = 8usize;
    let used = 2 * n * size;
    let mut buf = vec![0u64; used / 8];
    let base = buf.as_mut_ptr() as usize;
    for i in 0..2 * n {
        // SAFETY: `i * size + HEADER_SIZE <= used`; 8-aligned.
        unsafe { std::ptr::write((base + i * size) as *mut ObjectHeader, obj()) };
    }
    // SAFETY: a header written above.
    let victim = unsafe { &*((base + (n + 2) * size) as *const ObjectHeader) };
    assert!(victim.try_self_forward(victim.mark()).is_ok());
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
        base,
        used,
        vec![0, n * size, used],
        Vec::new(),
        lazy_objstart_chunk_walker,
    )
    .expect("a usable grid");
    for i in 0..2 * n {
        assert!(starts.contains(base + i * size), "start {i}");
    }
    assert!(!starts.contains(base + (n + 2) * size + 8), "an interior word is not a start");
    let (_, walked, failed, _) = starts.lazy_census().unwrap();
    assert_eq!((walked, failed), (2, 0));
    drop(buf);
}

fn r23_w4b_lazy_walker_refuses_a_chunk_with_a_forwarded_header_body() {
    let obj = || {
        ObjectHeader::new(
            ClassId::new(7),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            1,
        )
    };
    let size = gen_object_total_size(&obj());
    assert!(size >= HEADER_SIZE && size % 8 == 0);
    let n = 8usize;
    let used = 2 * n * size;
    let mut buf = vec![0u64; used / 8];
    let base = buf.as_mut_ptr() as usize;
    for i in 0..2 * n {
        // SAFETY: `i * size + HEADER_SIZE <= used`; 8-aligned.
        unsafe { std::ptr::write((base + i * size) as *mut ObjectHeader, obj()) };
    }
    // Self-forward the third object of the second chunk.
    // SAFETY: a header written above.
    let victim = unsafe { &*((base + (n + 2) * size) as *const ObjectHeader) };
    assert!(victim.try_self_forward(victim.mark()).is_ok());
    assert!(victim.is_forwarded());
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
        base,
        used,
        vec![0, n * size, used],
        Vec::new(),
        lazy_objstart_chunk_walker,
    )
    .expect("a usable grid");
    assert!(starts.contains(base + size), "the plain chunk proves");
    assert!(!starts.contains(base + (n + 1) * size), "the forwarded chunk fails closed");
    assert!(!starts.ensure_addr(base + (n + 2) * size));
    let (_, walked, failed, _) = starts.lazy_census().unwrap();
    assert_eq!((walked, failed), (1, 1));
    assert_eq!(
        starts.next_start_at_or_above(base + n * size),
        None,
        "no bit of the refused chain survives"
    );
    drop(buf);
}

/// Wave 4c: a chunk whose walk fails when `lo == 0x2000`.
fn every_64_failing_chunk_at_0x2000(
    base: usize,
    lo: usize,
    hi: usize,
    _skips: &[(usize, usize)],
    bits: &cratonvm_gc_core::young_mark::ObjectStartBits,
) -> bool {
    if lo == 0x2000 {
        return false;
    }
    let mut o = lo;
    while o < hi {
        bits.insert(base + o);
        o += 64;
    }
    true
}

/// Wave 4c: the pre-forward proof proves the PREFIX up to the highest
/// chunk a word's page needs, so a word above an unprovable chunk fails
/// the proof (the cycle is skipped) even though its own chunk would walk.
#[test]
fn r23_w4c_word_proofs_prove_the_prefix() {
    let base = 0x40_0000usize;
    let mut starts = cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
        base,
        0x4000,
        vec![0, 0x1000, 0x2000, 0x3000, 0x4000],
        Vec::new(),
        every_64_failing_chunk_at_0x2000,
    )
    .expect("a usable grid");
    starts.set_lazy_induction(true);
    assert!(lazy_young_starts_prove_words(&starts, &[base + 0x100], 1, None));
    assert!(lazy_young_starts_prove_words(&starts, &[base + 0x1100, 0x10], 1, None));
    assert!(starts.contains(base + 0x1040));
    assert!(!lazy_young_starts_prove_words(&starts, &[base + 0x3100], 1, None));
    assert!(!starts.contains(base + 0x3040), "chunk 3 walks, but its anchor is unproven");
    assert!(lazy_young_starts_prove_words(&starts, &[0x10, base + 0x4000], 1, None), "outside");
    assert!(!lazy_young_starts_prove_all(&starts, 1, None));
}

/// Wave 4b: `all_recorded_starts_in` reads the bits without walking.
#[test]
fn r23_w4b_recorded_starts_are_read_without_a_lazy_walk() {
    let starts = cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
        0x10_0000,
        0x2000,
        vec![0, 0x800, 0x2000],
        Vec::new(),
        every_64_failing_at_2048,
    )
    .expect("a usable grid");
    let mut seen = 0usize;
    assert!(starts.all_recorded_starts_in(0, 0x2000, |_| {
        seen += 1;
        true
    }));
    assert_eq!(seen, 0, "nothing walked, nothing recorded");
    assert_eq!(starts.lazy_census().map(|c| c.1), Some(0));
    assert!(starts.ensure_addr(0x10_0000));
    assert!(starts.all_recorded_starts_in(0, 0x2000, |_| {
        seen += 1;
        true
    }));
    assert_eq!(seen, 0x800 / 64, "chunk 0 only; chunk 1 (holding 2048) was never asked");
    assert!(!starts.all_recorded_starts_in(0, 0x2000, |a| a != 0x10_0040));
}
