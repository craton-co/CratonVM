use super::*;

/// gen r5w1: a spawned thread's stack band ends BELOW its thread-local
/// storage. glibc puts the static TLS block at the top of the stack
/// mapping that `pthread_getattr_np` reports, and a band that covered it
/// let a moving collection rewrite TLS words that looked like objects.
#[cfg(target_os = "linux")]
#[test]
fn a_threads_stack_band_ends_below_its_thread_locals() {
    thread_local! {
        static MARK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let band = current_thread_stack().expect("Linux reports a stack");
            let tls = MARK.with(|m| m as *const std::cell::Cell<usize> as usize);
            let local = 0usize;
            let local_at = std::hint::black_box(&local) as *const usize as usize;
            assert!(
                band.lo <= local_at && local_at < band.hi,
                "a local is on the band"
            );
            assert!(band.hi <= tls, "the band stops below the TLS block");
        })
        .expect("spawn")
        .join()
        .expect("thread");
}

#[test]
fn empty_is_noop() {
    let s = ShadowStack::empty();
    assert!(s.is_unallocated());
    assert_eq!(s.depth(), 0);
    let mut seen = 0;
    s.for_each_value(|_| seen += 1);
    assert_eq!(seen, 0);
    assert_eq!(s.remap(&cratonvm_types::PointerMap::default()), 0);
}

#[test]
fn layout_offsets_match_jit_contract() {
    let s = ShadowStack::empty();
    let base = &s as *const ShadowStack as usize;
    assert_eq!(
        &s.top as *const usize as usize - base,
        ShadowStack::TOP_OFFSET
    );
    assert_eq!(
        &s.end as *const usize as usize - base,
        ShadowStack::END_OFFSET
    );
    assert_eq!(
        &s.base as *const usize as usize - base,
        ShadowStack::BASE_OFFSET,
        "the moving-young coverage verifier recovers the published shadow \
         window [base, top) from a live compiled frame using these offsets",
    );
}

/// The verifier compares a compiled frame's spill words against the values
/// in `[base, top)`. That range is what `for_each_value` walks, so the two
/// must agree exactly — a `top` above the pushed data would let the
/// verifier accept an unpublished oop because a stale slot happened to
/// hold it.
#[test]
fn published_window_matches_the_scanned_range() {
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(0x1000));
    assert!(s.push(0x2000));

    let mut scanned = Vec::new();
    s.for_each_value(|v| scanned.push(v));
    let window: Vec<usize> = (0..s.depth())
        .map(|i| unsafe { ((s.base + i * 8) as *const usize).read() })
        .collect();
    assert_eq!(scanned, window);
    assert_eq!(s.top, s.base + scanned.len() * 8);
}

/// `top` is the ONE field JIT-compiled code bumps inline, so it is the one
/// field the collector cannot assume is in range. `set_top` has clamped
/// since it was written, but the JIT does not go through `set_top`, so the
/// read side has to clamp too: before it did, a `top` above `end` made
/// `for_each_value` read past the backing `Box` and `remap` WRITE past it.
#[test]
fn a_top_above_end_cannot_widen_the_scan_or_the_remap() {
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(0x1000));

    // What an inline push with a defeated overflow guard leaves behind.
    s.top = s.end + 4096;

    assert_eq!(
        s.depth(),
        DEFAULT_SHADOW_SLOTS,
        "depth must saturate at the buffer, not at the corrupt top"
    );

    let mut seen = 0usize;
    s.for_each_value(|_| seen += 1);
    assert_eq!(
        seen, DEFAULT_SHADOW_SLOTS,
        "the scan must stop at `end`, not at `top`"
    );

    // The remap walks the same range and WRITES into it; the pushed value
    // is still rewritten, and nothing past `end` is touched.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000usize, 0x9000usize);
    assert_eq!(s.remap(&map), 1);
    let mut after = Vec::new();
    s.for_each_value(|v| {
        if v != 0 {
            after.push(v)
        }
    });
    assert_eq!(after, vec![0x9000]);
}

/// `indirect_slots` names exactly the words `remap` rewrites through —
/// sorted, once each, value entries and popped entries excluded — because
/// it is the list a frame walk skips so no word is rewritten twice.
///
/// The "frame" is a STACK array of this test, as a compiled frame's slots
/// are: indirect entries are trusted only inside the owner's stack.
#[test]
fn indirect_slots_names_the_words_remap_rewrites_through() {
    let mut frame = [0x2000usize, 0x3000, 0x4000];
    let fp = std::hint::black_box(frame.as_mut_ptr());
    let a = fp as usize;
    let b = fp as usize + 8;
    let c = fp as usize + 16;
    let mut s = ShadowStack::empty();
    assert!(
        s.indirect_slots().is_empty(),
        "unallocated: nothing to name"
    );
    s.ensure_allocated();
    assert!(s.push(b | ShadowStack::INDIRECT_TAG));
    assert!(s.push(0x5000));
    assert!(s.push(a | ShadowStack::INDIRECT_TAG));
    assert!(s.push(b | ShadowStack::INDIRECT_TAG));
    let popped_at = s.top;
    assert!(s.push(c | ShadowStack::INDIRECT_TAG));
    s.set_top(popped_at);
    assert_eq!(s.indirect_slots(), vec![a.min(b), a.max(b)]);
    let _ = std::hint::black_box(&mut frame);
}

/// An indirect entry publishes the word at its slot, and a remap rewrites
/// that word — never the entry — so the frame reads the moved address back
/// with no copy-back of its own. A value entry beside it behaves as before.
/// The frame is on this thread's stack, as a real compiled frame is.
#[test]
fn indirect_entries_resolve_and_rewrite_through_their_slot() {
    let mut frame = [0x2000usize, 0];
    let fp = std::hint::black_box(frame.as_mut_ptr());
    let slot0 = fp as usize;
    let slot1 = fp as usize + 8;
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(slot0 | ShadowStack::INDIRECT_TAG));
    assert!(s.push(0x3000));
    // A null frame slot (a reference home not yet written) is published
    // as null, which every consumer skips.
    assert!(s.push(slot1 | ShadowStack::INDIRECT_TAG));

    let mut seen = Vec::new();
    s.for_each_value(|v| seen.push(v));
    assert_eq!(seen, vec![0x2000, 0x3000, 0]);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000usize, 0x9000usize);
    map.insert(0x3000usize, 0xA000usize);
    assert_eq!(s.remap(&map), 2);
    // SAFETY: `fp` points at this test's live two-word array.
    let (w0, w1) = unsafe { (fp.read_volatile(), fp.add(1).read_volatile()) };
    assert_eq!(w0, 0x9000, "the frame slot itself is rewritten");
    assert_eq!(w1, 0, "a null slot is never a map key");

    let mut raw = Vec::new();
    for i in 0..s.depth() {
        raw.push(unsafe { ((s.base + i * 8) as *const usize).read() });
    }
    assert_eq!(
        raw,
        vec![
            slot0 | ShadowStack::INDIRECT_TAG,
            0xA000,
            slot1 | ShadowStack::INDIRECT_TAG
        ],
        "an indirect entry is never rewritten, only the slot it names"
    );
    let _ = std::hint::black_box(&mut frame);
}

/// THE CRASH (`ShadowOddLongProbe`, gc-common w5-g): a producer published
/// an odd `long` in a reference home, and every reader took it for
/// `slot | INDIRECT_TAG`. An odd value that is not a word of the owner's
/// stack is neither dereferenced nor written by `for_each_value`, `remap`,
/// `indirect_slots` or `resolve_entry`.
///
/// Two shapes: the probe's own value (`n` = 5,924,137, unmapped: before the
/// fix the read faulted), and a READABLE heap word, so a regression shows
/// up as a changed word or a published `0x2000` instead of a crash.
#[test]
fn an_odd_primitive_outside_the_stack_is_neither_read_nor_written() {
    let heap: Box<[usize; 1]> = Box::new([0x2000]);
    let heap_ptr = std::hint::black_box(heap.as_ptr());
    let heap_odd = heap_ptr as usize | ShadowStack::INDIRECT_TAG;
    let probe_odd = 5_924_137usize;
    assert_eq!(
        ShadowStack::indirect_slot(heap_odd),
        Some(heap_ptr as usize),
        "shape alone accepts it: the band is what refuses it"
    );
    assert_eq!(ShadowStack::indirect_slot(probe_odd), Some(probe_odd - 1));

    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(heap_odd));
    assert!(s.push(probe_odd));
    assert!(s.push(0x3000));

    let refused_before = rejected_indirect_entries();
    let mut seen = Vec::new();
    s.for_each_value(|v| seen.push(v));
    assert_eq!(seen, vec![0, 0, 0x3000], "published as null, not read");
    assert!(s.indirect_slots().is_empty(), "no frame word is claimed");

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000usize, 0x9000usize);
    map.insert(probe_odd - 1, 0xA000usize);
    map.insert(0x3000usize, 0xB000usize);
    assert_eq!(s.remap(&map), 1, "only the value entry is rewritten");
    // SAFETY: `heap_ptr` is the live box above.
    assert_eq!(
        unsafe { heap_ptr.read_volatile() },
        0x2000,
        "remap must never write through an entry outside the owner's stack"
    );
    let band = SlotBand::current_thread();
    // SAFETY: both are refused by the band before any read.
    assert_eq!(unsafe { ShadowStack::resolve_entry(heap_odd, band) }, 0);
    assert_eq!(unsafe { ShadowStack::resolve_entry(probe_odd, band) }, 0);
    assert!(rejected_indirect_entries() > refused_before);
    drop(heap);
}

/// A cross-thread reader passes the PEER's band: an entry inside it
/// resolves and remaps; the same entry against a band that excludes it
/// (the empty band, or the reader's own stack) publishes null.
#[test]
fn an_explicit_band_bounds_a_cross_thread_read() {
    // Stands in for a peer's stack: readable memory that is not the
    // calling thread's stack.
    let peer: Box<[usize; 4]> = Box::new([0, 0x2000, 0, 0]);
    let peer_ptr = std::hint::black_box(peer.as_ptr());
    let peer_band = SlotBand::new(peer_ptr as usize, peer_ptr as usize + 32);
    let entry = (peer_ptr as usize + 8) | ShadowStack::INDIRECT_TAG;
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(entry));

    let mut seen = Vec::new();
    s.for_each_value_in(peer_band, |v| seen.push(v));
    assert_eq!(seen, vec![0x2000]);
    assert_eq!(s.indirect_slots_in(peer_band), vec![peer_ptr as usize + 8]);

    let mut seen = Vec::new();
    s.for_each_value_in(SlotBand::NONE, |v| seen.push(v));
    assert_eq!(seen, vec![0]);
    let mut seen = Vec::new();
    s.for_each_value(|v| seen.push(v));
    assert_eq!(seen, vec![0], "the own-thread band is not the peer's");

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000usize, 0x9000usize);
    assert_eq!(s.remap(&map), 0, "not this thread's stack: skipped");
    assert_eq!(s.remap_in(&map, peer_band), 1);
    // SAFETY: `peer_ptr` is the live box above.
    assert_eq!(unsafe { peer_ptr.add(1).read_volatile() }, 0x9000);
    drop(peer);
}

/// `contains_slot` admits a whole word only, and the own-thread band holds
/// this test's locals (every compiled frame of a reader's owner is, like
/// them, a caller of the reader).
#[test]
fn slot_band_edges_and_the_own_thread_band() {
    let b = SlotBand::new(0x1_0000, 0x1_0040);
    assert!(b.contains_slot(0x1_0000));
    assert!(b.contains_slot(0x1_0038));
    assert!(!b.contains_slot(0x1_003C), "straddles hi");
    assert!(!b.contains_slot(0x1_0040));
    assert!(!b.contains_slot(0xFFF8));
    assert!(!SlotBand::NONE.contains_slot(0));
    assert!(!SlotBand::new(0, usize::MAX).contains_slot(usize::MAX - 3));

    let local = [0usize; 2];
    let at = std::hint::black_box(local.as_ptr()) as usize;
    let own = SlotBand::current_thread();
    assert!(
        own.contains_slot(at),
        "{own:?} must hold a caller's local {at:#x}"
    );
    if let Some(stack) = current_thread_stack() {
        assert!(stack.lo <= own.lo && own.hi == stack.hi);
    }
}

/// The per-OS-thread band registry cross-thread readers look peers up in.
#[test]
fn thread_stack_band_registry_round_trips() {
    const TID: u32 = 0xFFFF_FD01;
    assert_eq!(thread_stack_band(TID), None);
    publish_thread_stack_band(TID);
    assert_eq!(thread_stack_band(TID), current_thread_stack());
    unpublish_thread_stack_band(TID);
    assert_eq!(thread_stack_band(TID), None);
}

/// A tagged word that cannot be a slot address is ignored rather than
/// dereferenced: published as null, never rewritten through.
#[test]
fn a_malformed_indirect_entry_is_neither_read_nor_written() {
    assert_eq!(ShadowStack::indirect_slot(0x10), None, "untagged");
    assert_eq!(ShadowStack::indirect_slot(0x1), None, "null page");
    assert_eq!(ShadowStack::indirect_slot(0x2_0005), None, "misaligned");
    assert_eq!(ShadowStack::indirect_slot(0x2_0009), Some(0x2_0008));

    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(0x5 | ShadowStack::INDIRECT_TAG));
    let mut seen = Vec::new();
    s.for_each_value(|v| seen.push(v));
    assert_eq!(seen, vec![0]);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x5usize, 0x9000usize);
    assert_eq!(s.remap(&map), 0);
}

#[test]
fn push_scan_remap_roundtrip() {
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(!s.is_unallocated());
    assert!(s.push(0x1000));
    assert!(s.push(0x2000));
    assert!(s.push(0x3000));
    assert_eq!(s.depth(), 3);

    let mut collected = Vec::new();
    s.for_each_value(|v| collected.push(v));
    assert_eq!(collected, vec![0x1000, 0x2000, 0x3000]);

    // Relocate 0x2000 -> 0x9000.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000usize, 0x9000usize);
    assert_eq!(s.remap(&map), 1);

    let mut after = Vec::new();
    s.for_each_value(|v| after.push(v));
    assert_eq!(after, vec![0x1000, 0x9000, 0x3000]);

    // Pop semantics via set_top.
    s.set_top(s.base + 8);
    assert_eq!(s.depth(), 1);
    s.reset();
    assert_eq!(s.depth(), 0);
}

/// gcj j1/cm: `set_top` on a stack whose `end` is below its `base` (field
/// assignment) refuses instead of panicking in `Ord::clamp`, and `push`
/// refuses a `top` below `base` or unaligned instead of writing through it.
#[test]
fn gcj1cm_set_top_and_push_refuse_out_of_range_fields() {
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    let (base, end) = (s.base, s.end);
    s.end = base - 8;
    s.set_top(base + 16);
    assert_eq!(s.top, base, "refused, unchanged");
    s.end = end;
    s.top = base - 8;
    assert!(!s.push(0x1000), "top below base");
    s.top = base + 1;
    assert!(!s.push(0x1000), "unaligned top");
    s.top = base;
    assert!(s.push(0x1000));
    s.set_top(end + 64);
    assert_eq!(s.top, end, "clamped to end");
    s.set_top(base - 64);
    assert_eq!(s.top, base, "clamped to base");
}

/// gci i47w lane oe48: dropping a shadow stack withdraws the cross-thread
/// window published for its buffer -- and only that window -- so a peer
/// reader can no longer find a struct address that names freed memory
/// (i48e `jp24_s_churn_def_r2`, an access violation in the helper-window
/// pass). Tids no real thread in the test binary holds.
#[test]
fn gci_i47w_oe48_dropping_a_shadow_stack_withdraws_its_published_window() {
    use crate::gc_quiescence::{
        publish_self_shadow_addr, shadow_window_of_tid, unregister_jit_depth_slot,
    };
    const GONE: u32 = 0xFFFF_4E01;
    const KEPT: u32 = 0xFFFF_4E02;
    let mut gone = Box::new(ShadowStack::empty());
    gone.ensure_allocated();
    let mut kept = Box::new(ShadowStack::empty());
    kept.ensure_allocated();
    let kept_triple = (&*kept as *const ShadowStack as usize, kept.base, kept.end);
    publish_self_shadow_addr(GONE, &*gone as *const ShadowStack as usize, gone.base, gone.end);
    publish_self_shadow_addr(KEPT, kept_triple.0, kept_triple.1, kept_triple.2);
    drop(gone);
    let gone_after = shadow_window_of_tid(GONE);
    let kept_after = shadow_window_of_tid(KEPT);
    unregister_jit_depth_slot(GONE);
    unregister_jit_depth_slot(KEPT);
    assert_eq!(gone_after, None, "the dropped stack's window is withdrawn");
    assert_eq!(kept_after, Some(kept_triple), "another stack's window is kept");
    // An unallocated stack publishes nothing and withdraws nothing.
    drop(ShadowStack::empty());
}

/// gci i47w lane oe48: a stack MOVED after it published leaves a stale
/// struct address behind; its final drop still withdraws it, because the
/// match is on the buffer, which moves with the struct.
#[test]
fn gci_i47w_oe48_a_moved_shadow_stack_withdraws_the_address_it_published_before_the_move() {
    use crate::gc_quiescence::{
        publish_self_shadow_addr, shadow_window_of_tid, unregister_jit_depth_slot,
    };
    const TID: u32 = 0xFFFF_4E03;
    let mut boxed = Box::new(ShadowStack::empty());
    boxed.ensure_allocated();
    publish_self_shadow_addr(
        TID,
        &*boxed as *const ShadowStack as usize,
        boxed.base,
        boxed.end,
    );
    // Moves the struct out and frees the old box without dropping it.
    let moved: ShadowStack = *boxed;
    let after_move = shadow_window_of_tid(TID);
    drop(moved);
    let after_drop = shadow_window_of_tid(TID);
    unregister_jit_depth_slot(TID);
    assert!(after_move.is_some(), "a move alone drops nothing");
    assert_eq!(after_drop, None, "the final drop withdraws the stale address");
}
