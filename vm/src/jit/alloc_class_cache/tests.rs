use super::*;


#[test]
fn get_empty_returns_none() {
    let cache = JitAllocClassCache::new();
    assert!(cache.get(0).is_none());
    assert!(cache.get(12345).is_none());
    assert!(cache.get(u32::MAX).is_none());
}

#[test]
fn insert_then_get_roundtrip() {
    let cache = JitAllocClassCache::new();
    let info = ClassAllocInfo {
        has_finalizer: true,
        reference_kind: 0,
        prim_inits: vec![(3, PrimKind::Int), (7, PrimKind::Long)].into_boxed_slice(),
        ref_inits: Box::new([]),
    };
    let published = cache.insert(42, info).unwrap();
    assert!(published.has_finalizer);
    assert_eq!(
        published.prim_inits.as_ref(),
        &[(3, PrimKind::Int), (7, PrimKind::Long)]
    );
    let got = cache.get(42).unwrap();
    assert!(got.has_finalizer);
    assert_eq!(got.prim_inits.len(), 2);
    // Neighbors untouched.
    assert!(cache.get(41).is_none());
    assert!(cache.get(43).is_none());
}

#[test]
fn first_insert_wins() {
    let cache = JitAllocClassCache::new();
    let a = ClassAllocInfo {
        has_finalizer: false,
        reference_kind: 0,
        prim_inits: Box::new([]),
        ref_inits: Box::new([]),
    };
    let b = ClassAllocInfo {
        has_finalizer: true,
        reference_kind: 0,
        prim_inits: Box::new([(1, PrimKind::Float)]),
        ref_inits: Box::new([]),
    };
    cache.insert(7, a);
    let winner = cache.insert(7, b).unwrap();
    assert!(!winner.has_finalizer, "first published entry must win");
    assert!(cache.get(7).unwrap().prim_inits.is_empty());
}

#[test]
fn out_of_range_id_is_uncached() {
    let cache = JitAllocClassCache::new();
    let info = ClassAllocInfo {
        has_finalizer: false,
        reference_kind: 0,
        prim_inits: Box::new([]),
        ref_inits: Box::new([]),
    };
    assert!(cache.insert(u32::MAX, info).is_none());
    assert!(cache.get(u32::MAX).is_none());
}

#[test]
fn cross_chunk_ids() {
    let cache = JitAllocClassCache::new();
    for id in [0u32, 1023, 1024, 1025, 999_999] {
        let info = ClassAllocInfo {
            has_finalizer: id % 2 == 0,
            reference_kind: 0,
            prim_inits: Box::new([(id, PrimKind::Double)]),
            ref_inits: Box::new([]),
        };
        cache.insert(id, info);
    }
    for id in [0u32, 1023, 1024, 1025, 999_999] {
        let got = cache.get(id).unwrap();
        assert_eq!(got.has_finalizer, id % 2 == 0);
        assert_eq!(got.prim_inits.as_ref(), &[(id, PrimKind::Double)]);
    }
}

/// `invalidate` must unpublish WITHOUT freeing: a reader that already has
/// the `&` — a thread frozen between `get()` and its `prim_inits` walk — is
/// unreachable from here and must not have its recipe reclaimed underneath.
///
/// This asserts on the entry's CONTENT after the invalidate, so it fails
/// (loudly under Miri/ASan, and by mismatch once the allocator reuses the
/// block) if the entry is freed rather than retired. It also fixes the
/// direction of the other half: `get` must report the class as gone.
#[test]
fn invalidate_unpublishes_but_does_not_free_a_live_borrow() {
    let cache = JitAllocClassCache::new();
    cache.insert(
        9,
        ClassAllocInfo {
            has_finalizer: true,
            reference_kind: 0,
            prim_inits: vec![(11, PrimKind::Long), (12, PrimKind::Double)].into_boxed_slice(),
            ref_inits: Box::new([]),
        },
    );
    // Stand in for the frozen reader: take the borrow BEFORE invalidating.
    let borrowed = cache.get(9).expect("published");
    assert!(
        cache.invalidate(9),
        "entry was published, so it unpublishes"
    );
    assert!(cache.get(9).is_none(), "invalidate must unpublish");
    assert!(borrowed.has_finalizer);
    assert_eq!(
        borrowed.prim_inits.as_ref(),
        &[(11, PrimKind::Long), (12, PrimKind::Double)],
        "a borrow taken before invalidate must still read its own recipe",
    );
}

#[test]
fn invalidate_is_idempotent_and_reinsertable() {
    let cache = JitAllocClassCache::new();
    cache.insert(
        5,
        ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: Box::new([(1, PrimKind::Int)]),
            ref_inits: Box::new([]),
        },
    );
    assert!(cache.invalidate(5));
    assert!(
        !cache.invalidate(5),
        "second invalidate has nothing to take"
    );
    // The id is free again: a reloaded class with the same id must be able
    // to publish a fresh recipe rather than inherit the retired one.
    let fresh = cache
        .insert(
            5,
            ClassAllocInfo {
                has_finalizer: true,
                reference_kind: 0,
                prim_inits: Box::new([(2, PrimKind::Float)]),
                ref_inits: Box::new([]),
            },
        )
        .expect("slot is empty again");
    assert!(fresh.has_finalizer);
    assert_eq!(
        cache.get(5).unwrap().prim_inits.as_ref(),
        &[(2, PrimKind::Float)]
    );
}

#[test]
fn concurrent_insert_get_smoke() {
    use std::sync::Arc;
    let cache = Arc::new(JitAllocClassCache::new());
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                for i in 0..2000u32 {
                    let id = i % 512;
                    if let Some(info) = cache.get(id) {
                        // Whoever won, the entry must be self-consistent.
                        assert_eq!(info.prim_inits.len(), 1);
                        assert_eq!(info.prim_inits[0].0, id);
                    } else {
                        let _ = cache.insert(
                            id,
                            ClassAllocInfo {
                                has_finalizer: t % 2 == 0,
                                reference_kind: 0,
                                prim_inits: Box::new([(id, PrimKind::Int)]),
                                ref_inits: Box::new([]),
                            },
                        );
                    }
                }
            })
        })
        .collect();
    for th in threads {
        th.join().unwrap();
    }
    for id in 0..512u32 {
        assert_eq!(cache.get(id).unwrap().prim_inits[0].0, id);
    }
}

/// gcg g1/p: the compiled-`new` path counters count only when armed, keep
/// the two helpers apart, and belong to ONE cache (a second VM's cache
/// sees none of the first's entries).
#[test]
fn gcg_g1p_new_path_counts_are_armed_per_helper_and_per_cache() {
    let a = JitAllocClassCache::new();
    let b = JitAllocClassCache::new();
    assert_eq!(a.new_paths.snapshot(), (0, 0));
    // Unarmed: nothing moves.
    a.new_paths.note_new_object_if(false);
    a.new_paths.note_post_tlab_init_if(false);
    assert_eq!(a.new_paths.snapshot(), (0, 0));
    // Armed: each helper its own counter.
    for _ in 0..3 {
        a.new_paths.note_new_object_if(true);
    }
    for _ in 0..5 {
        a.new_paths.note_post_tlab_init_if(true);
    }
    assert_eq!(a.new_paths.snapshot(), (3, 5));
    assert_eq!(b.new_paths.snapshot(), (0, 0), "per VM, not per process");
}

/// gcg g1/p: the exit line carries the two keys the probe rows grep.
#[test]
fn gcg_g1p_the_new_paths_line_names_both_keys() {
    let line = format_compiled_new_paths_line((7, 11));
    assert!(line.contains("jit_new_object_calls=7 "), "{line}");
    assert!(line.contains("jit_post_tlab_init_calls=11 "), "{line}");
    assert!(
        line.starts_with("[cratonvm] compiled-new helper entries:"),
        "{line}"
    );
}

/// zr z3/ja: the `anewarray` pair is per VM, counts only when armed, and
/// its keys are the ones the probe rows grep.
#[test]
fn zr3ja_anewarray_counts_are_per_vm_and_named() {
    let a = JitAllocClassCache::new();
    let b = JitAllocClassCache::new();
    a.new_paths.note_anewarray_if(false, true);
    assert_eq!(a.new_paths.anewarray_snapshot(), (0, 0));
    a.new_paths.note_anewarray_if(true, true);
    a.new_paths.note_anewarray_if(true, false);
    assert_eq!(a.new_paths.anewarray_snapshot(), (2, 1));
    assert_eq!(b.new_paths.anewarray_snapshot(), (0, 0));
    let keys = format_compiled_anewarray_keys((5, 3));
    assert_eq!(keys, " jit_anewarray_calls=5 jit_anewarray_small_calls=3");
}

/// zt t10/ca2: the register-first count is per VM, counts only when
/// armed, and its key is the one the probe rows grep.
#[test]
fn zt10ca2_register_first_count_is_per_vm_and_named() {
    let a = JitAllocClassCache::new();
    let b = JitAllocClassCache::new();
    a.new_paths.note_post_tlab_init_registered_if(false);
    assert_eq!(a.new_paths.post_tlab_init_registered_snapshot(), 0);
    a.new_paths.note_post_tlab_init_registered_if(true);
    a.new_paths.note_post_tlab_init_registered_if(true);
    assert_eq!(a.new_paths.post_tlab_init_registered_snapshot(), 2);
    assert_eq!(b.new_paths.post_tlab_init_registered_snapshot(), 0);
    assert_eq!(format_register_first_key(4), " jit_post_tlab_init_registered_first_calls=4");
}

/// zt t12/ca4: the announce-helper count is per VM, counts only when
/// armed, and its key is the one the spill-sink rows grep.
#[test]
fn zt12ca4_zgc_announce_count_is_per_vm_and_named() {
    let a = JitAllocClassCache::new();
    let b = JitAllocClassCache::new();
    a.new_paths.note_zgc_announce_if(false);
    assert_eq!(a.new_paths.zgc_announce_snapshot(), 0);
    for _ in 0..3 {
        a.new_paths.note_zgc_announce_if(true);
    }
    assert_eq!(a.new_paths.zgc_announce_snapshot(), 3);
    assert_eq!(a.new_paths.snapshot(), (0, 0), "its own counter");
    assert_eq!(b.new_paths.zgc_announce_snapshot(), 0, "per VM");
    assert_eq!(format_zgc_announce_key(9), " jit_zgc_announce_calls=9");
}

/// JIT round 21 wave 1 (lane alloc): the `newarray` / ZGC-announce
/// counters are per VM, count only when armed, keep their four keys apart,
/// and are named as `R21AllocLean`'s run notes grep them.
#[test]
fn r21_alloc_newarray_counts_are_per_vm_and_named() {
    let a = JitAllocClassCache::new();
    let b = JitAllocClassCache::new();
    a.new_paths.note_newarray_if(false, true);
    a.new_paths.note_newarray_tlab_if(false);
    a.new_paths.note_zgc_note_if(false);
    assert_eq!(a.new_paths.newarray_snapshot(), (0, 0, 0, 0));
    a.new_paths.note_newarray_if(true, true);
    a.new_paths.note_newarray_if(true, true);
    a.new_paths.note_newarray_if(true, false);
    a.new_paths.note_newarray_tlab_if(true);
    for _ in 0..4 {
        a.new_paths.note_zgc_note_if(true);
    }
    assert_eq!(a.new_paths.newarray_snapshot(), (3, 2, 1, 4));
    assert_eq!(b.new_paths.newarray_snapshot(), (0, 0, 0, 0), "per VM");
    // The earlier pairs are untouched by the new counters.
    assert_eq!(a.new_paths.snapshot(), (0, 0));
    assert_eq!(a.new_paths.anewarray_snapshot(), (0, 0));
    let keys = format_compiled_newarray_keys((9, 7, 5, 3));
    assert_eq!(
        keys,
        " jit_newarray_calls=9 jit_newarray_small_calls=7 \
         jit_newarray_tlab_served=5 jit_zgc_note_calls=3"
    );
}

/// gcg g1/p: counted from many threads, nothing is lost (the counters are
/// read once at exit, so `Relaxed` adds are enough).
#[test]
fn gcg_g1p_new_path_counts_are_exact_under_threads() {
    let cache = std::sync::Arc::new(JitAllocClassCache::new());
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let cache = std::sync::Arc::clone(&cache);
            std::thread::spawn(move || {
                for _ in 0..10_000 {
                    cache.new_paths.note_new_object_if(true);
                    cache.new_paths.note_post_tlab_init_if(true);
                }
            })
        })
        .collect();
    for th in threads {
        th.join().unwrap();
    }
    assert_eq!(cache.new_paths.snapshot(), (40_000, 40_000));
}

/// JIT round 21, lane alloc2: the spill counters are per VM, count only
/// when armed, keep entries and spills apart, and carry the keys the
/// probe rows grep.
#[test]
fn jr21_alloc2_spill_counts_are_per_vm_and_named() {
    let a = JitAllocClassCache::new();
    let b = JitAllocClassCache::new();
    a.new_paths.note_new_object_spill_if(false);
    a.new_paths.note_newarray_if(false, false);
    a.new_paths.note_newarray_spill_if(false);
    assert_eq!(a.new_paths.spill_snapshot(), (0, 0, 0));
    a.new_paths.note_new_object_spill_if(true);
    for _ in 0..3 {
        a.new_paths.note_newarray_if(true, false);
    }
    a.new_paths.note_newarray_spill_if(true);
    assert_eq!(a.new_paths.spill_snapshot(), (1, 3, 1));
    assert_eq!(b.new_paths.spill_snapshot(), (0, 0, 0), "per VM");
    let keys = format_compiled_spill_keys((2, 9, 4));
    assert_eq!(
        keys,
        " jit_new_object_spills=2 jit_newarray_spills=4"
    );
}
