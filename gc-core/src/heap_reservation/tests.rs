use super::*;

const MIB: usize = 1024 * 1024;
/// `HeapStore` declines to reserve below one granule, so every test that
/// wants the reserved arm must ask for at least this much.
const BIG: usize = 64 * MIB;

#[test]
fn a_large_heap_commits_only_what_was_asked_for() {
    let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
    assert_eq!(heap.reserved_len(), BIG);
    assert!(heap.base() != 0);
    if heap.is_reserved() {
        assert!(
            heap.committed_len() <= 4 * MIB,
            "-Xmx is address space, -Xms is memory (got {})",
            heap.committed_len()
        );
    } else {
        assert_eq!(
            heap.committed_len(),
            BIG,
            "the fallback is the pre-F-16 behaviour and says so"
        );
    }
}

/// The reserved arm must actually be TAKEN where the platform supports it.
/// Every other test here has a fallback arm, so a silent regression to the
/// pre-F-16 behaviour would leave them all green.
#[test]
#[cfg(any(windows, target_os = "linux"))]
fn this_platform_really_reserves() {
    let heap = ReservedHeap::new(BIG, MIB, "test");
    assert!(
        heap.is_reserved(),
        "windows and linux both reserve; falling back here means every \
         other test in this module quietly started covering the fallback"
    );
    assert!(heap.committed_len() < heap.reserved_len());
}

/// zt t29/g1 (`-XX:+AlwaysPreTouch` on G1): `set_pre_touch(true)` touches
/// the startup prefix without changing a byte, later growths keep their
/// contents too, and `false` touches nothing.
#[test]
fn zt29g1_pre_touch_covers_the_prefix_and_preserves_bytes() {
    let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
    let n = heap.committed_len();
    assert!(n > 0);
    // SAFETY: `[base, committed)` is committed by construction.
    unsafe {
        *(heap.base() as *mut u8) = 0x5A;
        *((heap.base() + n - 1) as *mut u8) = 0xA5;
    }
    assert_eq!(heap.set_pre_touch(false), 0);
    assert!(!heap.pre_touch_enabled());
    let touched = heap.set_pre_touch(true);
    assert!(heap.pre_touch_enabled());
    if heap.is_reserved() {
        assert_eq!(touched, n, "the whole committed prefix is touched");
    } else {
        assert_eq!(touched, 4 * MIB, "the fallback touches the startup commit");
    }
    // SAFETY: still committed.
    unsafe {
        assert_eq!(*(heap.base() as *const u8), 0x5A);
        assert_eq!(*((heap.base() + n - 1) as *const u8), 0xA5);
    }
    // A growth under the bit is committed, touched and still zero.
    assert!(heap.commit_to(16 * MIB));
    // SAFETY: committed just above.
    let grown =
        unsafe { std::slice::from_raw_parts((heap.base() + n) as *const u8, 16 * MIB - n) };
    assert!(grown.iter().all(|&b| b == 0));
}

#[test]
fn the_committed_prefix_is_writable_and_zeroed() {
    let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
    let n = heap.committed_len().min(4 * MIB);
    assert!(n > 0);
    // SAFETY: `[base, committed)` is committed by construction.
    let slice = unsafe { std::slice::from_raw_parts_mut(heap.base() as *mut u8, n) };
    assert!(
        slice.iter().all(|&b| b == 0),
        "committed pages arrive zeroed"
    );
    slice[0] = 0xAB;
    slice[n - 1] = 0xCD;
    assert_eq!((slice[0], slice[n - 1]), (0xAB, 0xCD));
}

#[test]
fn growing_the_prefix_makes_the_new_bytes_usable() {
    let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
    let before = heap.committed_len();
    assert!(heap.commit_to(16 * MIB));
    assert!(heap.committed_len() >= 16 * MIB);

    // SAFETY: just committed.
    let slice = unsafe {
        std::slice::from_raw_parts_mut((heap.base() + before) as *mut u8, 16 * MIB - before)
    };
    assert!(slice.iter().all(|&b| b == 0));
    slice[0] = 0x5A;
    assert_eq!(slice[0], 0x5A);
}

#[test]
fn growing_is_monotone_and_idempotent() {
    let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
    assert!(heap.commit_to(16 * MIB));
    let high = heap.committed_len();
    // A request BELOW the current prefix must not shrink it: the callers
    // are region claims arriving out of order, and a claim of a low index
    // must not un-map a higher one already in use.
    assert!(heap.commit_to(4 * MIB));
    assert_eq!(heap.committed_len(), high);
}

#[test]
fn a_request_past_the_reservation_is_refused_rather_than_clamped() {
    let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
    assert!(
        !heap.commit_to(BIG * 2),
        "the reservation is the hard limit -Xmx means; silently clamping \
         would report success for memory the caller does not have"
    );
}

#[test]
fn decommitting_gives_pages_back_and_they_read_as_zero_again() {
    let heap = ReservedHeap::new(BIG, 32 * MIB, "test");
    if !heap.is_reserved() {
        assert!(heap.decommit_to(MIB).is_none());
        assert_eq!(heap.committed_len(), BIG);
        return;
    }
    let probe = heap.base() + 24 * MIB;
    // SAFETY: committed by construction.
    unsafe { std::ptr::write_bytes(probe as *mut u8, 0xFF, 4096) };
    assert!(heap.decommit_to(8 * MIB).is_some());
    assert_eq!(heap.committed_len(), 8 * MIB);
    assert_eq!(heap.reserved_len(), BIG, "the RESERVATION is untouched");

    // Re-commit and confirm the dirty bytes did not survive: a recycled
    // region must not hand an allocator someone else's data.
    assert!(heap.commit_to(32 * MIB));
    let slice = unsafe { std::slice::from_raw_parts(probe as *const u8, 4096) };
    assert!(
        slice.iter().all(|&b| b == 0),
        "recommitted pages must be zero, not the bytes that were there"
    );
}

#[test]
fn a_zero_sized_heap_is_inert_rather_than_a_panic() {
    let heap = ReservedHeap::new(0, 0, "test");
    assert_eq!(heap.reserved_len(), 0);
    assert_eq!(heap.committed_len(), 0);
    assert!(!heap.commit_to(1));
}

#[test]
fn the_initial_commit_is_clamped_into_the_reservation() {
    // -Xms above -Xmx: take the reservation, not a refusal — the two are
    // separately specified and a user who oversizes one should get a heap.
    let heap = ReservedHeap::new(BIG, 4 * BIG, "test");
    assert_eq!(heap.committed_len(), BIG);
}

/// gci i42/ta: the shrink's range end, both arms of
/// `CRATONVM_G1_UNCOMMIT_TAIL_GRANULE`.
#[test]
fn gci_i42ta_the_shrink_range_ends_at_the_prefix_granule_boundary() {
    let g = crate::reservation::GRANULE;
    // On: a mid-granule prefix rounds up; a granule-aligned one is itself.
    assert_eq!(decommit_end(7 * MIB, true), 8 * MIB);
    assert_eq!(decommit_end(8 * MIB, true), 8 * MIB);
    assert_eq!(decommit_end(g + 1, true), 2 * g);
    assert_eq!(decommit_end(0, true), 0);
    assert_eq!(decommit_end(usize::MAX, true), usize::MAX, "saturates, never wraps");
    // Off: the pre-i42 range, byte for byte.
    for c in [0, 7 * MIB, 8 * MIB, g + 1] {
        assert_eq!(decommit_end(c, false), c);
    }
}

/// gci i42/ta: a shrink of an odd-region prefix (1 MiB regions on 2 MiB
/// granules) gives back the prefix's mapped last granule too, and the
/// heap still grows and reads zero afterwards.
#[test]
fn gci_i42ta_a_shrink_releases_the_mapped_remainder_of_the_last_granule() {
    let heap = ReservedHeap::new(BIG, 7 * MIB, "test");
    if !heap.is_reserved() {
        return;
    }
    assert_eq!(heap.committed_len(), 7 * MIB);
    // Granules 0..=3 back the 7 MiB prefix (the last one half above it).
    assert_eq!(heap.store.lock().committed_bytes(), 8 * MIB);
    let probe = heap.base() + 6 * MIB + 64;
    // SAFETY: inside the committed prefix.
    unsafe { *(probe as *mut u8) = 0xEE };
    assert!(heap.decommit_to(3 * MIB).is_some());
    assert_eq!(heap.committed_len(), 3 * MIB);
    if heap.uncommit_tail_granule {
        assert_eq!(
            heap.store.lock().committed_bytes(),
            4 * MIB,
            "granules 2 and 3 are wholly at or above the new prefix's granule; both go back"
        );
    }
    assert!(heap.commit_to(7 * MIB));
    if heap.uncommit_tail_granule {
        // SAFETY: committed again just above.
        assert_eq!(unsafe { *(probe as *const u8) }, 0, "a given-back granule reads zero");
    }
}

/// A heap too small for the backing store to reserve must still WORK — it
/// simply gets the wholly-committed block, and every caller must be unable
/// to tell except through `committed_len`.
#[test]
fn a_sub_granule_heap_falls_back_and_is_fully_usable() {
    let heap = ReservedHeap::new(64 * 1024, 8 * 1024, "test");
    assert!(!heap.is_reserved());
    assert_eq!(heap.committed_len(), 64 * 1024);
    assert!(heap.commit_to(64 * 1024));
    // SAFETY: the fallback is committed in full.
    let slice = unsafe { std::slice::from_raw_parts_mut(heap.base() as *mut u8, 64 * 1024) };
    slice[0] = 1;
    slice[64 * 1024 - 1] = 2;
    assert_eq!((slice[0], slice[64 * 1024 - 1]), (1, 2));
}
