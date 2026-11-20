use super::*;

/// **The class gate answers an ordinary class with no lock, and never gates
/// away a class that DOES own overlay state.**
///
/// # Why the counter is the assertion and the result cannot be
///
/// `gc_overlay_roots_for_collection` returns an empty `Vec` for a
/// non-owning object either way — through the gate or through the mutex — so
/// no assertion on the return value can distinguish them. That is exactly the
/// shape of a change that is on and inert, which the *previous* attempt at
/// this optimisation turned out to be. The engagement counters are the only
/// instrument.
///
/// # The half that matters is the second one
///
/// Too permissive costs a lock. Too strict drops an overlay edge, which frees
/// the contents of a live collection. So the test registers an owner under a
/// class and then asserts that queries for that class do **not** take the
/// fast path.
#[test]
fn the_class_gate_skips_the_mutex_for_a_class_that_owns_nothing() {
    // Relative to a baseline: this bitmap is process-global and cargo runs
    // tests in parallel threads, so an absolute count would be a flake.
    let never_owns: u32 = 51_001;
    assert!(
        !overlay_class_gate_disabled(),
        "no test in this binary may register an owner with an unknown class -- \
         that trips the process-global fail-safe and makes this assertion \
         depend on execution order. See TEST_OWNER_CLASS"
    );
    assert!(
        !overlay_owner_class_possible(never_owns),
        "a class that has never owned an overlay must be gated out"
    );
    let (h0, m0) = overlay_class_gate_stats();
    assert!(gc_overlay_roots_for_collection(0x7000_0000, Some(never_owns)).is_empty());
    let (h1, m1) = overlay_class_gate_stats();
    assert_eq!(h1, h0 + 1, "the gate must have answered it");
    assert_eq!(m1, m0, "and it must not have fallen through to the mutex");

    // Now the direction that would corrupt the heap if it were wrong.
    let owns: u32 = 51_002;
    let key = register_overlay_owner_key(0x7000_1000, 0xFEED, Some(owns));
    assert!(
        overlay_owner_class_possible(owns),
        "registering an owner must open the gate for its class -- the bit is \
         set BEFORE the map insert, so a reader can never miss an owner that \
         is already published"
    );
    let (h2, m2) = overlay_class_gate_stats();
    let _ = gc_overlay_roots_for_collection(0x7000_1000, Some(owns));
    let (h3, m3) = overlay_class_gate_stats();
    assert_eq!(
        h3, h2,
        "a class that DOES own overlay state must never be gated out -- that \
         would free the contents of a live collection"
    );
    assert_eq!(m3, m2 + 1, "it must fall through to the mutex");

    // And `None` cannot be gated: there is no class to test.
    let (h4, _) = overlay_class_gate_stats();
    let _ = gc_overlay_roots_for_collection(0x7000_2000, None);
    assert_eq!(
        overlay_class_gate_stats().0,
        h4,
        "a lookup with no class id must not take the fast path"
    );

    remove_overlay_owner_key(0x7000_1000, key);
}

/// A class id these tests register owners under.
///
/// Deliberately a REAL id and not `None`: registering with no class trips
/// `OVERLAY_OWNER_CLASS_UNKNOWN`, which is process-global and would disable
/// the class gate for every other test in this binary -- making the gate's own
/// engagement test flaky depending on execution order. Fail-safe is the right
/// production behaviour and the wrong test fixture.
const TEST_OWNER_CLASS: u32 = 4242;

fn object(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// A collection that DIED leaves its owner-index entry at the address it
/// died at, so a fresh object allocated over that address may evict its
/// overlay state. This is the case `native_map_init`'s purge exists for.
#[test]
fn dead_tenant_is_still_recorded_at_its_address() {
    let addr = 0x5EAD_0100_usize;
    let key = 0x5EAD_0101_usize;
    register_overlay_owner_key(addr, key, Some(TEST_OWNER_CLASS));
    assert!(
        overlay_owner_still_at(addr, key),
        "a tenant that has not been pruned must still be recorded at its address"
    );
}

/// A collection that MOVED is re-keyed to its new address by the post-GC
/// remap, so its old address no longer names it. Evicting on a raw-pointer
/// match alone would empty this still-live collection; the guard must
/// answer `false` for the old address and `true` for the new one.
#[test]
fn relocated_tenant_is_no_longer_recorded_at_its_old_address() {
    let old_addr = 0x5EAD_0200_usize;
    let new_addr = 0x5EAD_0300_usize;
    let key = 0x5EAD_0201_usize;
    register_overlay_owner_key(old_addr, key, Some(TEST_OWNER_CLASS));
    assert!(overlay_owner_still_at(old_addr, key));

    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(old_addr, new_addr);
    gc_update_collection_overlay_refs(&pointer_map);

    assert!(
        !overlay_owner_still_at(old_addr, key),
        "a relocated collection must not still be recorded at its pre-move address, \
         or HashMap.<init> on a fresh object at that address would evict its live overlay"
    );
    assert!(
        overlay_owner_still_at(new_addr, key),
        "the relocated collection must be recorded at its post-move address"
    );
    // Leave the global index as we found it so sibling tests in this
    // process see no residue.
    clear_overlay_entries_for_key(key, new_addr);
}

/// End-to-end on the overlay itself: an entry registered for a relocated
/// owner survives the address recycle, and one registered for a dead owner
/// is still evictable.
#[test]
fn relocated_owners_overlay_entry_survives_an_address_recycle() {
    let old_addr = 0x5EAD_0400_usize;
    let new_addr = 0x5EAD_0500_usize;
    let key = 0x5EAD_0401_usize;
    register_overlay_owner_key(old_addr, key, Some(TEST_OWNER_CLASS));
    hm_int_fast_shard_for(key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_default()
        .entries
        .insert(1, (object(0x1000), Value::Int(7)));

    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(old_addr, new_addr);
    gc_update_collection_overlay_refs(&pointer_map);

    // What `native_map_init` now asks before evicting.
    assert!(!overlay_owner_still_at(old_addr, key));
    assert_eq!(
        hm_int_fast_shard_for(key)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|state| state.entries.len()),
        Some(1),
        "the live, relocated map's only storage must still hold its entry"
    );
    clear_overlay_entries_for_key(key, new_addr);
}

/// A `pointer_map` that contains both `A -> B` and `B -> C` must land A's
/// keys at B and B's at C — not sweep both to C.
///
/// Old-gen sliding compaction routinely produces exactly that shape: it
/// hands one surviving object the address another surviving object just
/// vacated, so a single map legitimately holds a value that is also a key.
/// (The moving young phase alone cannot — its keys are from-space and its
/// values are to-space or old gen — which is why this only ever bites on a
/// compacting cycle.)
///
/// The re-key used to relocate entries in place, one move at a time. Apply
/// `A -> B` first and A's keys sit at B; the later `B -> C` then carries
/// them onward with B's own, leaving the collection that really is at B
/// with no entry at its own address. `gc_overlay_roots_for_collection(B)`
/// answers "owns nothing", the non-moving young marker and `old_gen_gc`'s
/// mark BFS both skip its backing array, and it is freed while live.
/// Whether it fired depended on `HashMap` iteration order — applying the
/// moves in reverse happens to be harmless — so this uses a CHAIN of eight
/// hops rather than a single pair. In-place relocation survives that only
/// if every one of the eight lands in exactly reverse order, which the
/// randomly-seeded `HashMap` iteration will not do. A two-address version
/// of this test would pass half the time on the broken code.
#[test]
fn a_chained_pointer_map_does_not_sweep_one_owners_keys_onto_another() {
    // A private address range, so the process-global index cannot make
    // this pass or fail for a sibling test's reason.
    const HOPS: usize = 8;
    let addr = |i: usize| 0x5EAD_0A00_usize + i * 0x100;
    let key = |i: usize| 0x5EAD_0A01_usize + i * 0x100;

    // Owner i sits at addr(i) and moves to addr(i+1) — the address owner
    // i+1 is simultaneously vacating. Old-gen sliding compaction produces
    // exactly this: every live object slides down onto the space its
    // neighbour just left. (The moving young phase alone cannot — its keys
    // are from-space and its values are to-space or old gen — which is why
    // this only ever bites on a compacting cycle.)
    let mut pointer_map = cratonvm_types::PointerMap::default();
    for i in 0..HOPS {
        register_overlay_owner_key(addr(i), key(i), Some(TEST_OWNER_CLASS));
        pointer_map.insert(addr(i), addr(i + 1));
    }

    gc_update_collection_overlay_refs(&pointer_map);

    for i in 0..HOPS {
        assert!(
            overlay_owner_still_at(addr(i + 1), key(i)),
            "owner {i} moved to 0x{:x} but its key is not recorded there — \
             a later hop in the same pointer_map carried it onward, so \
             every per-owner root walk now reports this collection as \
             owning nothing and frees its backing array while it is live",
            addr(i + 1)
        );
        assert!(
            !overlay_owner_still_at(addr(i), key(i)),
            "owner {i}'s pre-move address must no longer name it"
        );
    }
    // The tail address gained nothing but the last owner: keys must not
    // have piled up at the end of the chain.
    for i in 0..HOPS.saturating_sub(1) {
        assert!(
            !overlay_owner_still_at(addr(HOPS), key(i)),
            "owner {i}'s key was swept to the end of the chain"
        );
    }

    for i in 0..HOPS {
        remove_overlay_owner_key(addr(i + 1), key(i));
    }
}
