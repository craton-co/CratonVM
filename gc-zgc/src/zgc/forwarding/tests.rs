use super::*;
use std::sync::Barrier;

/// Shorthand for "an already-encoded payload of this value".
///
/// Tests are the one place where writing a bare number as a payload is
/// fine: there is no heap and no encoder, so the identity encoding is the
/// honest one. Production code must never construct a payload this way —
/// see [`ZForwardingPayload`] and
/// `linux_shaped_absolute_destination_is_unencodable`.
fn p(encoded: u64) -> ZForwardingPayload {
    ZForwardingPayload::from_encoded(encoded)
}

// -- packing ------------------------------------------------------------

#[test]
fn entry_fields_tile_the_word() {
    assert_eq!(1 + ZFWD_TO_BITS + ZFWD_FROM_BITS, 64);
    // 22 bits of 8-byte granules = 32 MiB, the Medium page size.
    assert_eq!(ZFWD_MAX_FROM_OFFSET, 32 * 1024 * 1024 - 8);
    // 41 bits of 8-byte granules = 16 TiB. NOT "above ZGC's 42-bit
    // colored-pointer field" — that comparison is the false justification
    // ZFWD_TO_BITS documents. 16 TiB is a bound on the HEAP, because what
    // is stored is a heap-relative payload.
    assert_eq!(ZFWD_MAX_PAYLOAD, (1u64 << 44) - 8);
    // The 256 MiB default heap budget uses 25 of the 41 payload bits:
    // 16 TiB / 256 MiB = 65 536x headroom, exactly.
    let default_heap: u64 = 256 * 1024 * 1024;
    assert_eq!(ZFWD_MAX_PAYLOAD + 8, default_heap * 65_536);
}

/// FINDING 5 (2026-08-07). **INVARIANT ZFWD-1, demonstrated rather than
/// described.**
///
/// [`ZFWD_OCCUPIED_BIT`] and [`crate::zgc::vaddr::Z_COLORED_TAG`] are the
/// same bit. The doc comment on `ZFWD_OCCUPIED_BIT` says why that is safe;
/// this test shows what it would cost if the invariant it rests on were
/// broken, so nobody has to take the paragraph on faith.
#[test]
fn an_occupied_entry_is_bit_indistinguishable_from_a_vaddr_colored_word() {
    use crate::zgc::vaddr;

    assert_eq!(
        ZFWD_OCCUPIED_BIT,
        vaddr::Z_COLORED_TAG,
        "the two tags are the same bit BY DESIGN; if they have diverged, \
         re-read INVARIANT ZFWD-1 before assuming the divergence is a fix",
    );
    assert_eq!(ZFWD_OCCUPIED_BIT, 1u64 << 63);

    let from: u64 = 0x1_8000;
    let payload = p(0x2_4000);
    let entry = ZgcForwardingEntry::pack(from, payload).expect("a legal entry");
    assert!(ZgcForwardingEntry::is_occupied(entry));

    // (1) vaddr cannot tell a published forwarding entry from one of its
    //     own colored words. This is the hazard, not an accident.
    assert!(
        vaddr::is_colored_word(entry),
        "entry {entry:#x} does NOT look like a vaddr colored word any more; \
         if that is now structural, INVARIANT ZFWD-1 can be relaxed",
    );

    // (2) And what vaddr would decode out of it is neither the source nor
    //     the destination — it is the from field with the bottom of the
    //     payload field spliced on top. A barrier handed this word would
    //     dereference an address nobody ever computed.
    let misread = vaddr::offset_of(entry);
    assert_ne!(
        misread, from,
        "a mis-read entry that happened to equal the from-offset would make \
         this hazard invisible in exactly the test written to show it",
    );
    assert_ne!(misread, payload.encoded());

    // (3) The one entry value that is safe by accident: the empty slot is
    //     Z_NULL, which vaddr treats as null rather than as a colored word.
    //     That is why an all-zero table cannot be misread even if a slot
    //     word escaped.
    assert_eq!(ZFWD_EMPTY, vaddr::Z_NULL);
    assert!(!vaddr::is_colored_word(ZFWD_EMPTY));
}

/// FINDING 6 (2026-08-07). The from-offset field has **exactly zero**
/// granules of headroom over `page.rs`'s medium page.
///
/// The compile-time gate next to [`ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES`]
/// catches a page that grows. This test states the zero itself, and proves
/// the boundary at the encoder rather than only in arithmetic — because the
/// production failure is not an arithmetic one, it is a rare
/// `Unencodable` on objects in the top granule of a page.
#[test]
fn forwarding_from_offset_field_exactly_covers_the_default_medium_page() {
    use crate::zgc::page;

    let medium = page::ZPAGE_DEFAULT_MEDIUM as u64;
    let highest_object = medium - ZFWD_ALIGN_BYTES;

    let from_bits: u32 = ZFWD_FROM_BITS;
    let field_max: u64 = ZFWD_MAX_FROM_OFFSET;
    assert!(
        highest_object <= field_max,
        "page.rs's medium page is {medium} B, whose highest 8-aligned object \
         offset is {highest_object:#x}, but ZFWD_FROM_BITS={from_bits} only \
         reaches {field_max:#x}",
    );
    assert_eq!(
        ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES, 0,
        "the recorded fact is that the headroom is EXACTLY zero. A page that \
         GREW would have tripped the const assertion instead; a page that \
         SHRANK is safe, and the right response is to update \
         ZFWD_MEDIUM_PAGE_HEADROOM_GRANULES' doc comment (which argues from \
         the zero) rather than to delete this assertion.",
    );
    assert_eq!(ZFWD_MAX_FROM_OFFSET + ZFWD_ALIGN_BYTES, medium);

    // At the encoder: the last object slot of a medium page is encodable,
    // one granule past it is refused.
    assert!(
        ZgcForwardingEntry::pack(highest_object, p(0x1000)).is_some(),
        "the last object slot of a medium page must be forwardable",
    );
    assert!(
        ZgcForwardingEntry::pack(medium, p(0x1000)).is_none(),
        "an offset one granule past the page end must be refused, not wrapped",
    );

    // And at the table: refused as Unencodable, and — the part that makes
    // the field width load-bearing rather than cosmetic — it must NOT alias
    // onto offset 0 by truncation.
    let t = ZForwardingTable::for_page(1, 16);
    assert_eq!(
        t.try_insert(medium, p(0x1000)),
        Err(ZForwardingInsertError::Unencodable),
    );
    assert_eq!(
        t.find_payload(0),
        None,
        "an out-of-range key must be refused, never truncated onto offset 0",
    );
    assert_eq!(t.entry_count(), 0);

    // Small pages sit strictly inside the bound, so only Medium is tight.
    assert!(page::ZPAGE_DEFAULT_SMALL as u64 - ZFWD_ALIGN_BYTES < ZFWD_MAX_FROM_OFFSET);
}

/// FINDING 7 (2026-08-07). The "unreachable clamp" argument, evaluated from
/// [`cratonvm_types::HEADER_SIZE`] instead of recited from prose.
///
/// The clamp still holds at `HEADER_SIZE = 16`, but with **one doubling** of
/// margin: 4 Mi wanted against an 8 Mi clamp. The const assertion beside
/// [`ZFWD_WORST_CASE_CAPACITY`] is what fails the build if that margin is
/// spent; this test names the numbers so the failure is legible.
#[test]
fn the_capacity_clamp_is_unreachable_at_the_real_header_size() {
    use crate::zgc::page;

    // Read the constant, never a comment about it: heap_types.rs's own doc
    // on HEADER_SIZE still describes a "32 -> 24 shrink" above a 16.
    assert_eq!(cratonvm_types::HEADER_SIZE, 16, "the derivation's premise");
    assert_eq!(
        page::ZPAGE_MIN_ALLOC,
        cratonvm_types::HEADER_SIZE,
        "page.rs's minimum allocation is what bounds objects-per-page; if it \
         stops being HEADER_SIZE, ZFWD_SMALLEST_OBJECT_BYTES takes the \
         smaller of the two and this assertion is the notice to re-read it",
    );
    assert_eq!(ZFWD_SMALLEST_OBJECT_BYTES, cratonvm_types::HEADER_SIZE);

    let max_objects = page::ZPAGE_DEFAULT_MEDIUM / ZFWD_SMALLEST_OBJECT_BYTES;
    assert_eq!(max_objects, 2 * 1024 * 1024, "32 MiB / 16 B");
    assert_eq!(ZFWD_WORST_CASE_CAPACITY, 2 * max_objects);

    // The pre-rounding shortcut must agree with the sizing function that
    // actually runs — that equivalence is the whole reason
    // ZFWD_WORST_CASE_CAPACITY may skip next_power_of_two in a const.
    let capacity = zfwd_capacity_for(max_objects);
    assert_eq!(
        capacity,
        ZFWD_WORST_CASE_CAPACITY.next_power_of_two(),
        "ZFWD_WORST_CASE_CAPACITY's shortcut disagrees with zfwd_capacity_for",
    );
    assert!(capacity <= ZFWD_MAX_CAPACITY);

    // The margin, as a number, so spending it is visible.
    assert_eq!(capacity, 4 * 1024 * 1024);
    assert_eq!(ZFWD_MAX_CAPACITY, 8 * 1024 * 1024);
    assert_eq!(
        ZFWD_MAX_CAPACITY / capacity,
        2,
        "exactly one doubling of margin: HEADER_SIZE may go 16 -> 8 and no \
         further before the clamp starts truncating a LEGAL page's table",
    );

    // The pre-shrink figure the original argument was written against, kept
    // so the reasoning trail survives: at 24 B the worst case was ~1.4 Mi
    // objects, which rounds to the same 4 Mi capacity. The clamp held then
    // too — for a different reason than the one written down.
    assert_eq!(
        zfwd_capacity_for(page::ZPAGE_DEFAULT_MEDIUM / 24),
        4 * 1024 * 1024
    );
}

/// # 2026-08-07: this test's `from` was address-shaped, not offset-shaped
///
/// It was written as `pack(0x1234_5670 & !7, ...)` — 291 MiB — and it had
/// **never been executed** (nothing in CI built `--features zgc` until
/// today), so the mistake shipped unseen. `from` is not an address: it is a
/// *page-relative* offset bounded by [`ZFWD_MAX_FROM_OFFSET`] (32 MiB − 8),
/// which is the entire reason 22 bits is enough and the table is per-page
/// (see [`ZFWD_FROM_BITS`]). 291 MiB is not a legal offset into any page
/// this collector relocates, so `pack` was right to answer `None` and the
/// `.unwrap()` was the bug. The literal below keeps the old one's digits,
/// shifted into the field's actual range, so the correction is legible.
///
/// The bound must **not** be loosened to accommodate it: widening `from`
/// costs bits from [`ZFWD_TO_BITS`], and `1 + 41 + 22 == 64` is already
/// exact.
#[test]
fn pack_round_trips() {
    // 0x0123_4560 = 18.2 MiB: inside a medium page, and wide enough that a
    // shift error in either field would move it.
    const FROM: u64 = 0x0123_4567 & !7;
    assert!(
        FROM <= ZFWD_MAX_FROM_OFFSET,
        "the key must be a legal offset"
    );

    let e = ZgcForwardingEntry::pack(FROM, p(0x7FFF_FFF8)).unwrap();
    assert!(ZgcForwardingEntry::is_occupied(e));
    assert_eq!(ZgcForwardingEntry::from(e), FROM);
    assert_eq!(ZgcForwardingEntry::payload(e), p(0x7FFF_FFF8));

    // Both fields at their maxima at once: the only pattern that shows the
    // three fields tile the word with no bleed. `from` reads back without
    // the payload's low bits, the payload reads back without the tag, and
    // the word is exactly all-ones.
    let full = ZgcForwardingEntry::pack(ZFWD_MAX_FROM_OFFSET, p(ZFWD_MAX_PAYLOAD)).unwrap();
    assert_eq!(full, u64::MAX);
    assert!(ZgcForwardingEntry::is_occupied(full));
    assert_eq!(ZgcForwardingEntry::from(full), ZFWD_MAX_FROM_OFFSET);
    assert_eq!(ZgcForwardingEntry::payload(full), p(ZFWD_MAX_PAYLOAD));

    // from == 0 is a legal key and must still read back as occupied — this
    // is why the occupancy tag is explicit rather than "non-zero".
    let z = ZgcForwardingEntry::pack(0, p(0x1000)).unwrap();
    assert!(ZgcForwardingEntry::is_occupied(z));
    assert_eq!(ZgcForwardingEntry::from(z), 0);
    assert_eq!(ZgcForwardingEntry::payload(z), p(0x1000));
    assert_ne!(z, 0);
}

#[test]
fn pack_rejects_unencodable() {
    // Misaligned.
    assert!(ZgcForwardingEntry::pack(4, p(0x1000)).is_none());
    assert!(ZgcForwardingEntry::pack(8, p(0x1004)).is_none());
    // Null payload.
    assert!(ZgcForwardingEntry::pack(8, p(0)).is_none());
    assert!(p(0).is_null());
    // from beyond the 32 MiB page bound.
    assert!(ZgcForwardingEntry::pack(ZFWD_MAX_FROM_OFFSET + 8, p(0x1000)).is_none());
    assert!(ZgcForwardingEntry::pack(ZFWD_MAX_FROM_OFFSET, p(0x1000)).is_some());
    // payload beyond 16 TiB.
    assert!(ZgcForwardingEntry::pack(8, p(ZFWD_MAX_PAYLOAD + 8)).is_none());
    assert!(ZgcForwardingEntry::pack(8, p(ZFWD_MAX_PAYLOAD)).is_some());
}

/// **The defect this module shipped with, pinned on both platforms.**
///
/// Every other test in this file uses small, Windows-shaped destinations
/// (`0x9000`, `0x10_0000`, `0x100_0000`) — all of which encode fine
/// anywhere. That is precisely why a 16 TiB ceiling on what the docs called
/// a "full heap address" survived review: the suite could not see the
/// failure, because on Linux the real heap base is ~2^47 and never appeared
/// in a test.
///
/// This asserts the *documented contract* in both directions:
///
/// 1. a raw Linux-shaped absolute address is refused with `Unencodable` —
///    specified behaviour, not a bug, and the reason nothing may pack one;
/// 2. the same destination expressed heap-relative (exactly what
///    `ZRelocate::encode_to` produces) packs, stores and round-trips.
///
/// If someone "simplifies" the encoding away and stores absolute addresses,
/// (2) fails on every platform and (1) documents why. If someone widens the
/// field to make absolute addresses fit, `entry_fields_tile_the_word` fails
/// — 44 + 22 + 1 does not fit a `u64`.
#[test]
fn linux_shaped_absolute_destination_is_unencodable() {
    // A plausible placement for a 256 MiB `Vec<u8>` on Linux: glibc routes
    // an allocation that size to `mmap`, and the kernel maps it in the gap
    // below the stack. Granule-aligned, so alignment is not what fails.
    const LINUX_HEAP_BASE: u64 = 0x7f00_0000_0000;
    // ~2^47 against the field's 2^44: eight times too big.
    assert!(LINUX_HEAP_BASE > ZFWD_MAX_PAYLOAD);
    assert!(LINUX_HEAP_BASE / ZFWD_MAX_PAYLOAD >= 7);

    let absolute: u64 = LINUX_HEAP_BASE + 0x2_0000;
    let t = ZForwardingTable::for_page(1, 16);

    // (1) Absolute: refused, loudly, on every platform.
    assert_eq!(ZgcForwardingEntry::pack(0x40, p(absolute)), None);
    assert_eq!(
        t.try_insert(0x40, p(absolute)),
        Err(ZForwardingInsertError::Unencodable),
        "a raw Linux heap address must be refused — see ZFWD_TO_BITS"
    );
    assert_eq!(t.find_payload(0x40), None);
    assert_eq!(t.entry_count(), 0);

    // (2) Heap-relative, biased off zero exactly as `ZRelocate::encode_to`
    //     does. Kept as a literal rather than importing
    //     `relocate::ZRELOCATE_ENCODING_BIAS`, so that a change to the bias
    //     over there has to come back through this file.
    const BIAS: u64 = 8;
    let payload = p(absolute - LINUX_HEAP_BASE + BIAS);
    assert_eq!(t.try_insert(0x40, payload), Ok(payload));
    assert_eq!(t.find_payload(0x40), Some(payload));
    assert_eq!(t.entry_count(), 1);

    // Reconstructing the address is the CALLER's decode. The table neither
    // performs nor validates it — that is the whole content of the
    // `ZForwardingPayload` newtype.
    assert_eq!(payload.encoded() + LINUX_HEAP_BASE - BIAS, absolute);

    // The bias is load-bearing, not cosmetic: the first byte of the heap is
    // heap-relative zero, and a zero payload is rejected.
    assert_eq!(ZgcForwardingEntry::pack(0x48, p(0)), None);
    assert!(ZgcForwardingEntry::pack(0x48, p(BIAS)).is_some());

    // And the whole 256 MiB default heap encodes, at any base.
    let top_of_default_heap = p(256 * 1024 * 1024 - 8 + BIAS);
    assert!(ZgcForwardingEntry::pack(0x50, top_of_default_heap).is_some());
}

// -- single-threaded table ---------------------------------------------

#[test]
fn insert_find_round_trip() {
    let t = ZForwardingTable::with_capacity_for(64);
    assert!(t.capacity().is_power_of_two());
    assert!(t.capacity() >= 128);

    // 0x400 sits outside the `i * 8` range used below, so the loop cannot
    // collide with it and lose its own CAS.
    assert_eq!(t.find_payload(0x400), None);
    assert_eq!(t.insert(0x400, p(0x9000)), p(0x9000));
    assert_eq!(t.find_payload(0x400), Some(p(0x9000)));
    assert_eq!(t.entry_count(), 1);

    for i in 0u64..32 {
        let from = i * 8;
        let to = p(0x10_0000 + i * 16);
        assert_eq!(t.insert(from, to), to);
    }
    for i in 0u64..32 {
        assert_eq!(t.find_payload(i * 8), Some(p(0x10_0000 + i * 16)));
    }
    assert_eq!(t.find_payload(0x400), Some(p(0x9000)));
    assert_eq!(t.entry_count(), 33);
}

#[test]
fn repeat_insert_returns_the_first_winner() {
    let t = ZForwardingTable::with_capacity_for(16);
    assert_eq!(t.insert(0x80, p(0xAAA0)), p(0xAAA0));
    // A second thread's speculative copy loses; it must get the first
    // winner's payload back, not its own.
    assert_eq!(t.insert(0x80, p(0xBBB0)), p(0xAAA0));
    assert_eq!(t.insert(0x80, p(0xCCC0)), p(0xAAA0));
    assert_eq!(t.find_payload(0x80), Some(p(0xAAA0)));
    // Only one publication was counted.
    assert_eq!(t.entry_count(), 1);
}

#[test]
fn find_rejects_bad_keys_without_probing_forever() {
    let t = ZForwardingTable::with_capacity_for(4);
    assert_eq!(t.find_payload(3), None); // misaligned
    assert_eq!(t.find_payload(ZFWD_MAX_FROM_OFFSET + 8), None); // out of range
    assert_eq!(t.find_payload(0x18), None); // simply absent
}

#[test]
fn try_insert_reports_unencodable() {
    let t = ZForwardingTable::with_capacity_for(4);
    assert_eq!(
        t.try_insert(4, p(0x1000)),
        Err(ZForwardingInsertError::Unencodable)
    );
    assert_eq!(
        t.try_insert(8, p(0)),
        Err(ZForwardingInsertError::Unencodable)
    );
    assert_eq!(
        t.try_insert(ZFWD_MAX_FROM_OFFSET + 8, p(0x1000)),
        Err(ZForwardingInsertError::Unencodable)
    );
    // The out-of-field-range arm, which on Linux is what an unencoded
    // destination hits. See `linux_shaped_absolute_destination_is_unencodable`.
    assert_eq!(
        t.try_insert(8, p(ZFWD_MAX_PAYLOAD + 8)),
        Err(ZForwardingInsertError::Unencodable)
    );
}

/// A full table must terminate with `TableFull`, not spin. The probe
/// sequence visits every slot exactly once (power-of-two capacity, step 1),
/// so the loop is bounded by `capacity` and cannot livelock.
#[test]
fn table_full_terminates_and_still_finds_existing_keys() {
    // live_objects = 1 -> 2 -> clamped up to ZFWD_MIN_CAPACITY.
    let t = ZForwardingTable::with_capacity_for(1);
    assert_eq!(t.capacity(), ZFWD_MIN_CAPACITY);

    for i in 0u64..(ZFWD_MIN_CAPACITY as u64) {
        let from = i * 8;
        let to = p(0x2000 + i * 8);
        assert_eq!(t.try_insert(from, to), Ok(to), "slot {i} should be free");
    }
    assert_eq!(t.entry_count(), ZFWD_MIN_CAPACITY);

    // One more distinct key: every slot is occupied by a different key.
    let overflow_from = (ZFWD_MIN_CAPACITY as u64) * 8;
    assert_eq!(
        t.try_insert(overflow_from, p(0x9000)),
        Err(ZForwardingInsertError::TableFull)
    );
    // ...and it is absent, without hanging.
    assert_eq!(t.find_payload(overflow_from), None);

    // A key that IS present must still resolve on a full table (the probe
    // finds the match before exhausting the slots).
    for i in 0u64..(ZFWD_MIN_CAPACITY as u64) {
        assert_eq!(t.find_payload(i * 8), Some(p(0x2000 + i * 8)));
        assert_eq!(t.try_insert(i * 8, p(0xDEAD_0000)), Ok(p(0x2000 + i * 8)));
    }
}

#[test]
fn sizing_honours_load_factor_and_clamps() {
    // Sizing is asserted through `zfwd_capacity_for` so the clamp cases do
    // not have to allocate a 64 MiB slot array to be covered.
    assert_eq!(zfwd_capacity_for(0), ZFWD_MIN_CAPACITY);
    assert_eq!(zfwd_capacity_for(1), ZFWD_MIN_CAPACITY);
    assert_eq!(zfwd_capacity_for(3), ZFWD_MIN_CAPACITY);
    // 100 live objects -> 200 -> 256 slots -> load factor < 0.5.
    assert_eq!(zfwd_capacity_for(100), 256);
    assert_eq!(zfwd_capacity_for(128), 256);
    assert_eq!(zfwd_capacity_for(129), 512);
    // A corrupt count must clamp rather than request a terabyte, on both
    // the overflow path (`checked_next_power_of_two` -> None) and the
    // merely-too-large path.
    assert_eq!(zfwd_capacity_for(usize::MAX), ZFWD_MAX_CAPACITY);
    assert_eq!(zfwd_capacity_for(ZFWD_MAX_CAPACITY), ZFWD_MAX_CAPACITY);
    // The constructor honours the same rule.
    assert_eq!(ZForwardingTable::with_capacity_for(100).capacity(), 256);
    assert_eq!(
        ZForwardingTable::with_capacity_for(0).capacity(),
        ZFWD_MIN_CAPACITY
    );
}

#[test]
fn snapshot_returns_every_published_pair() {
    let t = ZForwardingTable::with_capacity_for(8);
    for i in 0u64..6 {
        t.insert(i * 8, p(0x5000 + i * 8));
    }
    let mut snap = t.snapshot_payloads();
    snap.sort();
    let expect: Vec<(u64, ZForwardingPayload)> =
        (0u64..6).map(|i| (i * 8, p(0x5000 + i * 8))).collect();
    assert_eq!(snap, expect);
}

// -- the concurrency test that matters ---------------------------------

/// **The load barrier's core race.** N threads all discover the same stale
/// pointer, all speculatively copy the object to their own destination, and
/// all call `insert`. Exactly one must win; every thread — winner and
/// losers alike — must come away with the *same* address, or the object's
/// identity has split into N copies.
///
/// The `Barrier` releases every thread into `insert` at once so the CAS
/// actually collides, and the outer loop repeats over many keys so the
/// window is hit many times per run rather than once.
#[test]
fn concurrent_insert_same_key_converges_on_one_winner() {
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 64;

    let table = Arc::new(ZForwardingTable::with_capacity_for(ROUNDS as usize * 2));
    let barrier = Arc::new(Barrier::new(THREADS as usize));

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let table = Arc::clone(&table);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            // (winner_payload, did_i_win) per round.
            let mut observed: Vec<(ZForwardingPayload, bool)> =
                Vec::with_capacity(ROUNDS as usize);
            for r in 0..ROUNDS {
                let from = r * 8;
                // Every thread proposes a DIFFERENT destination for the
                // SAME source, so the returned value identifies the winner.
                let my_to = p(0x10_0000 + r * 0x1000 + (t + 1) * 8);
                barrier.wait();
                let winner = table.insert(from, my_to);
                observed.push((winner, winner == my_to));
            }
            observed
        }));
    }

    let per_thread: Vec<Vec<(ZForwardingPayload, bool)>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();

    for r in 0..ROUNDS as usize {
        let first = per_thread[0][r].0;
        let mut winners = 0usize;
        for t in 0..THREADS as usize {
            let (payload, i_won) = per_thread[t][r];
            assert_eq!(
                payload, first,
                "round {r}: thread {t} observed {payload:?}, thread 0 observed {first:?} \
                 — the object's identity split"
            );
            if i_won {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "round {r}: expected exactly one CAS winner");
        // The published entry must agree with what every thread was told.
        assert_eq!(table.find_payload(r as u64 * 8), Some(first));
    }
    assert_eq!(table.entry_count(), ROUNDS as usize);
}

/// Distinct keys inserted concurrently must all be present and correct —
/// this is the collision-probing half (a lost CAS to a *different* key must
/// make the loser probe on, not adopt the winner's address).
#[test]
fn concurrent_insert_distinct_keys_all_resolve() {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 128;

    let total = (THREADS * PER_THREAD) as usize;
    let table = Arc::new(ZForwardingTable::with_capacity_for(total));
    let barrier = Arc::new(Barrier::new(THREADS as usize));

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let table = Arc::clone(&table);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for i in 0..PER_THREAD {
                let from = (t * PER_THREAD + i) * 8;
                let to = p(0x100_0000 + from);
                assert_eq!(table.insert(from, to), to);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            let from = (t * PER_THREAD + i) * 8;
            assert_eq!(table.find_payload(from), Some(p(0x100_0000 + from)));
        }
    }
    assert_eq!(table.entry_count(), total);
}

// -- relocation-set selection ------------------------------------------

fn small(page_id: u64, live_bytes: usize) -> PageCandidate {
    PageCandidate {
        page_id,
        live_bytes,
        capacity_bytes: 2 * 1024 * 1024,
        size_class_index: ZFWD_SIZE_CLASS_SMALL,
    }
}

/// A candidate with an explicit capacity, for the budget tests.
///
/// `small`'s fixed 2 MiB capacity cannot express a page whose *live* bytes
/// exceed the whole 64 MiB evacuation budget while still being sparse
/// enough to pass `max_live_occupancy` — and a candidate whose `live_bytes`
/// exceed its `capacity_bytes` reports zero garbage, which would make the
/// budget tests pass through the zero-garbage filter instead of through the
/// rule they are about.
fn sparse(page_id: u64, live_bytes: usize, capacity_bytes: usize) -> PageCandidate {
    PageCandidate {
        page_id,
        live_bytes,
        capacity_bytes,
        size_class_index: ZFWD_SIZE_CLASS_SMALL,
    }
}

/// A page the profitability ranking REFUSES is evacuated anyway when an
/// allocation failure named its window.
///
/// This is the whole feature. `select` sorts by garbage ratio and drops
/// anything at or above `max_live_occupancy`, which answers "reclaim the
/// most bytes per byte copied". A window can be 98.9 % free and still
/// refuse a request, and the pages walling it are dense by construction —
/// exactly what both filters throw away.
///
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
/// `TestMVStoreTool` OOMs on a 262 160-byte request with 2 888 live bytes in
/// 49 runs standing between it and 266 104 contiguous bytes, in a cycle that
/// relocated 1 817 474 objects by this ranking and left those 34 objects.
#[test]
fn a_targeted_page_is_evacuated_even_though_the_ranking_refuses_it() {
    let mb = 1024 * 1024;
    // Page 7 is 90 % live: above `max_live_occupancy`, and the LAST thing
    // the garbage ranking would pick.
    let dense_wall = small(7, (mb * 9) / 10);
    let pages = vec![small(1, mb / 8), dense_wall, small(3, mb / 4)];

    let untargeted = ZRelocationPolicy {
        max_evacuation_bytes: usize::MAX,
        ..ZRelocationPolicy::default()
    };
    let set = ZRelocationSet::select(&pages, &untargeted);
    assert!(
        !set.contains(7),
        "control: the ranking must refuse a 90%-live page, or this test \
         proves nothing"
    );

    let targeted = ZRelocationPolicy {
        max_evacuation_bytes: usize::MAX,
        target_pages: Some((7, 7)),
        ..ZRelocationPolicy::default()
    };
    let set = ZRelocationSet::select(&pages, &targeted);
    assert!(
        set.contains(7),
        "a page in the window an allocation failure named must be evacuated \
         even with nothing to reclaim — unblocking the request IS the benefit"
    );
    assert_eq!(
        set.pages().first().map(|p| p.page_id),
        Some(7),
        "and it must sort FIRST: the evacuation budget is a prefix rule, so \
         a target sorted after a full budget is silently dropped"
    );
}

/// The target does not drag in the rest of the arena.
///
/// A bias that admitted everything would "fix" the window by evacuating the
/// whole heap, which is not a trade anybody chose. Only pages inside the
/// named range get the exemption.
#[test]
fn targeting_one_window_does_not_admit_the_pages_around_it() {
    let mb = 1024 * 1024;
    let pages = vec![
        small(5, (mb * 9) / 10), // dense, OUTSIDE the window
        small(7, (mb * 9) / 10), // dense, inside
        small(9, (mb * 9) / 10), // dense, OUTSIDE
    ];
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: usize::MAX,
        target_pages: Some((7, 7)),
        ..ZRelocationPolicy::default()
    };
    let set = ZRelocationSet::select(&pages, &policy);
    assert!(set.contains(7), "the targeted page is in");
    assert!(
        !set.contains(5),
        "page 5 is outside the window and stays out"
    );
    assert!(
        !set.contains(9),
        "page 9 is outside the window and stays out"
    );
}

/// **The bug the exemption closes.** A targeted page whose own `live_bytes`
/// exceeds the whole budget used to end the selection on iteration one —
/// because targets sort first — so the cycle evacuated *nothing at all*.
///
/// Not the target, and not the profitable pages that would have been chosen
/// had no target been named: naming a target made the outcome strictly
/// worse than not naming one. That is the shape the assertions below pin,
/// and the control arm (`budget_exempts_targets: false`) reproduces the old
/// behaviour exactly, so the test states what changed rather than only what
/// is true now.
#[test]
fn an_over_budget_target_no_longer_ends_the_selection() {
    let mb = 1024 * 1024;
    // 96 MiB live in the targeted page, against a 64 MiB budget.
    let pages = vec![small(1, mb / 8), sparse(7, 96 * mb, 512 * mb)];
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: 64 * mb,
        target_pages: Some((7, 7)),
        ..ZRelocationPolicy::default()
    };

    let set = ZRelocationSet::select(&pages, &policy);
    assert!(
        set.contains(7),
        "an over-budget TARGET must still be evacuated: it is unblocking a \
         request that has already failed"
    );
    assert_eq!(
        set.targeted_overrun_bytes(),
        96 * mb - 64 * mb,
        "the overrun is reported, so the exemption's cost is measurable \
         rather than inferred"
    );

    // THE CONTROL: the pre-2026-09-20 behaviour, which is exactly what the
    // field exists to be able to restore — and what the bug looked like.
    let old = ZRelocationPolicy {
        budget_exempts_targets: false,
        ..policy
    };
    let set = ZRelocationSet::select(&pages, &old);
    assert!(
        set.is_empty(),
        "without the exemption the prefix rule ends the selection on the \
         FIRST page — which is the target, because targets sort first — so \
         the cycle evacuates nothing whatsoever, not even the profitable \
         page it would have chosen had no target been named: {:?}",
        set.page_ids()
    );
    assert_eq!(set.targeted_overrun_bytes(), 0);
}

/// A *range* of targeted pages that exhausts the budget between themselves
/// keeps all of them. This is the same failure in slow motion: with a
/// prefix rule the last targets in the window are dropped, and the window
/// is precisely the set that has to be emptied for the failed request to
/// succeed — emptying most of it unblocks nothing.
#[test]
fn a_range_of_targets_is_not_truncated_by_the_budget() {
    let mb = 1024 * 1024;
    let pages = vec![
        sparse(10, 30 * mb, 256 * mb),
        sparse(11, 30 * mb, 256 * mb),
        sparse(12, 30 * mb, 256 * mb),
        small(99, mb / 8), // profitable, outside the window
    ];
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: 64 * mb,
        target_pages: Some((10, 12)),
        ..ZRelocationPolicy::default()
    };
    let set = ZRelocationSet::select(&pages, &policy);
    for id in [10u64, 11, 12] {
        assert!(
            set.contains(id),
            "targeted page {id} must survive the budget"
        );
    }
    assert_eq!(set.targeted_overrun_bytes(), 90 * mb - 64 * mb);
    // The budget still bites on the page BEHIND the window: 90 MiB have
    // already been committed, so the prefix rule declines the rest. That is
    // the exemption staying an exemption rather than becoming a raised
    // budget.
    assert!(!set.contains(99));
    assert_eq!(set.len(), 3);

    // Without the exemption only the FIRST target fits and the other two
    // are dropped — "most of the window emptied" unblocks nothing, which is
    // the same failure as dropping the window entirely.
    let old = ZRelocationPolicy {
        budget_exempts_targets: false,
        ..policy
    };
    let set = ZRelocationSet::select(&pages, &old);
    assert_eq!(set.page_ids(), vec![10, 11]);
}

/// The exemption is **only** for targets: an untargeted page is still
/// budgeted, and a cycle with no target named behaves exactly as before.
///
/// Without this, "exempt" could have been implemented as "raise the budget
/// when a target exists", which would let the profitability ranking spend
/// unboundedly on the back of one small target.
#[test]
fn the_exemption_does_not_leak_to_untargeted_pages() {
    let mb = 1024 * 1024;
    // Page 7 is a small DENSE wall (the ranking refuses it, the target
    // admits it); page 1 is big and sparse, so it is eligible but
    // unaffordable; page 2 is small and profitable.
    let pages = vec![
        small(7, mb),
        sparse(1, 96 * mb, 512 * mb),
        sparse(2, mb, 16 * mb),
    ];
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: 64 * mb,
        target_pages: Some((7, 7)),
        ..ZRelocationPolicy::default()
    };
    let set = ZRelocationSet::select(&pages, &policy);
    assert!(set.contains(7), "the target is in");
    assert!(
        !set.contains(1),
        "a 96 MiB UNTARGETED page is still refused by a 64 MiB budget"
    );
    assert!(
        set.deferred_for_budget() > 0,
        "and the prefix rule still reports what it declined"
    );
    assert_eq!(
        set.targeted_overrun_bytes(),
        0,
        "the target fitted, so nothing overran"
    );

    // No target at all: the budget is exactly the rule it always was.
    let untargeted = ZRelocationPolicy {
        max_evacuation_bytes: 64 * mb,
        ..ZRelocationPolicy::default()
    };
    let set = ZRelocationSet::select(&pages, &untargeted);
    assert!(!set.contains(1));
    assert_eq!(set.targeted_overrun_bytes(), 0);
}

#[test]
fn select_orders_by_garbage_ratio_descending() {
    let mb = 1024 * 1024;
    let pages = vec![
        small(1, mb / 2), // 25% live
        small(2, mb / 8), // 6.25% live  <- most garbage
        small(3, mb / 4), // 12.5% live
    ];
    let policy = ZRelocationPolicy {
        max_live_occupancy: 0.5,
        ..Default::default()
    };
    let set = ZRelocationSet::select(&pages, &policy);
    assert_eq!(set.page_ids(), vec![2, 3, 1]);
    assert_eq!(set.len(), 3);
    assert_eq!(set.total_live_bytes(), mb / 2 + mb / 8 + mb / 4);
    assert!(set.contains(2));
    assert!(!set.contains(99));
}

#[test]
fn select_tie_breaks_on_page_id_for_determinism() {
    let mb = 1024 * 1024;
    let pages = vec![small(7, mb / 8), small(3, mb / 8), small(5, mb / 8)];
    let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    assert_eq!(set.page_ids(), vec![3, 5, 7]);
}

#[test]
fn select_stops_at_the_budget() {
    // Four pages, 128 KiB live each; budget fits exactly two.
    let pages = vec![
        small(1, 128 * 1024),
        small(2, 128 * 1024),
        small(3, 128 * 1024),
        small(4, 128 * 1024),
    ];
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: 256 * 1024,
        ..Default::default()
    };
    let set = ZRelocationSet::select(&pages, &policy);
    assert_eq!(set.len(), 2);
    assert_eq!(set.total_live_bytes(), 256 * 1024);
    assert_eq!(set.deferred_for_budget(), 2);
    // Tie on ratio -> ascending page id.
    assert_eq!(set.page_ids(), vec![1, 2]);
}

#[test]
fn select_zero_budget_selects_nothing() {
    let pages = vec![small(1, 1024)];
    let policy = ZRelocationPolicy {
        max_evacuation_bytes: 0,
        ..Default::default()
    };
    let set = ZRelocationSet::select(&pages, &policy);
    assert!(set.is_empty());
    assert_eq!(set.deferred_for_budget(), 1);
}

#[test]
fn select_skips_pages_at_or_above_the_occupancy_cutoff() {
    let mb = 1024 * 1024;
    let pages = vec![
        small(1, mb / 16), // 3.1% live -> selected
        small(2, mb),      // 50% live  -> above 0.25 cutoff
        small(3, 2 * mb),  // 100% live -> zero garbage AND above cutoff
    ];
    let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    assert_eq!(set.page_ids(), vec![1]);
}

#[test]
fn select_skips_zero_garbage_and_zero_capacity_pages() {
    let pages = vec![
        PageCandidate {
            page_id: 1,
            live_bytes: 4096,
            capacity_bytes: 4096, // no garbage
            size_class_index: ZFWD_SIZE_CLASS_SMALL,
        },
        PageCandidate {
            page_id: 2,
            live_bytes: 0,
            capacity_bytes: 0, // degenerate
            size_class_index: ZFWD_SIZE_CLASS_SMALL,
        },
    ];
    let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    assert!(set.is_empty());
}

/// Large pages hold one object: evacuating buys no compaction (the page is
/// either fully live or freed whole), so the default policy excludes them.
#[test]
fn select_excludes_large_pages_by_default() {
    let mb = 1024 * 1024;
    let large = PageCandidate {
        page_id: 42,
        live_bytes: mb / 16,
        capacity_bytes: 8 * mb,
        size_class_index: ZFWD_SIZE_CLASS_LARGE,
    };
    let pages = vec![large, small(1, mb / 8)];

    let default_set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    assert_eq!(default_set.page_ids(), vec![1]);
    assert!(!default_set.contains(42));

    // The knob exists, and flipping it admits the large page — which, by
    // garbage ratio, outranks the small one.
    let opt_in = ZRelocationPolicy {
        relocate_large_pages: true,
        ..Default::default()
    };
    let opt_in_set = ZRelocationSet::select(&pages, &opt_in);
    assert_eq!(opt_in_set.page_ids(), vec![42, 1]);
}

#[test]
fn empty_relocation_set_is_inert() {
    let set = ZRelocationSet::empty();
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert_eq!(set.total_live_bytes(), 0);
    assert_eq!(set.total_garbage_bytes(), 0);
    assert!(!set.contains(1));
    assert!(set.pages().is_empty());
}

// -- registry -----------------------------------------------------------

#[test]
fn registry_install_get_and_clear() {
    let reg = ZForwardingRegistry::new();
    assert!(reg.get(1).is_none());
    assert_eq!(reg.table_count(), 0);

    let t = Arc::new(ZForwardingTable::for_page(1, 32));
    reg.install(1, t);
    assert!(reg.contains(1));
    assert_eq!(reg.table_count(), 1);

    let got = reg.get(1).expect("page 1 must be registered");
    assert_eq!(got.page_id(), 1);
    got.insert(0x18, p(0x7000));
    // The registry hands out a shared Arc, not a copy.
    assert_eq!(reg.get(1).unwrap().find_payload(0x18), Some(p(0x7000)));

    reg.clear();
    assert!(reg.get(1).is_none());
    // The Arc already handed out stays alive and usable.
    assert_eq!(got.find_payload(0x18), Some(p(0x7000)));
}

#[test]
fn registry_install_for_set_covers_every_selected_page() {
    let mb = 1024 * 1024;
    let pages = vec![small(1, mb / 8), small(2, mb / 16), small(3, mb / 4)];
    let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    assert_eq!(set.len(), 3);

    let reg = ZForwardingRegistry::new();
    reg.install_for_set(&set, ZFWD_ASSUMED_AVG_OBJECT_BYTES);
    assert_eq!(reg.table_count(), 3);
    for id in set.page_ids() {
        let t = reg.get(id).expect("selected page must have a table");
        assert_eq!(t.page_id(), id);
        assert!(t.capacity().is_power_of_two());
    }
    let mut ids = reg.page_ids();
    ids.sort();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn registry_install_for_set_tolerates_a_zero_average() {
    let pages = vec![small(1, 4096)];
    let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    let reg = ZForwardingRegistry::new();
    // Clamped up to ZFWD_SMALLEST_OBJECT_BYTES: must not divide by zero,
    // and must not be talked into a looser bound than the page's own
    // smallest possible object.
    reg.install_for_set(&set, 0);
    assert_eq!(reg.table_count(), 1);
}

/// Concurrent readers must never block each other out, and every one must
/// see the same table. Exercises the "clone the Arc, drop the guard"
/// discipline under contention with a writer.
#[test]
fn registry_concurrent_get_is_consistent() {
    const THREADS: usize = 8;
    let reg = Arc::new(ZForwardingRegistry::new());
    reg.install(9, Arc::new(ZForwardingTable::for_page(9, 256)));
    reg.get(9).unwrap().insert(0x20, p(0xABC0));

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let reg = Arc::clone(&reg);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for i in 0..256u64 {
                // Half the threads also write, so a fair-RwLock writer is
                // queued while readers are in flight.
                if t % 2 == 0 {
                    let page = 100 + (t as u64) * 256 + i;
                    reg.install(page, Arc::new(ZForwardingTable::for_page(page, 1)));
                }
                let table = reg.get(9).expect("page 9 stays registered");
                assert_eq!(table.find_payload(0x20), Some(p(0xABC0)));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(reg.get(9).unwrap().find_payload(0x20), Some(p(0xABC0)));
}

/// zs s2/sl2 (defect 3): a live table is never replaced, and one page's
/// table can be removed without touching the others (ZR-1: only the pages
/// `note_mark_complete` promoted may lose their tables).
#[test]
fn zs2sl2_registry_refuses_a_replacement_and_removes_one_page() {
    let reg = ZForwardingRegistry::new();
    let t1 = Arc::new(ZForwardingTable::for_page(1, 32));
    let t2 = Arc::new(ZForwardingTable::for_page(2, 32));
    reg.install(1, Arc::clone(&t1));
    reg.install(2, Arc::clone(&t2));
    t1.insert(0x18, p(0x7000));

    // A second table for page 1 is refused: the published entry survives.
    let other = Arc::new(ZForwardingTable::for_page(1, 32));
    let kept = reg.install(1, Arc::clone(&other));
    assert!(Arc::ptr_eq(&kept, &t1), "the live table stays registered");
    assert_eq!(reg.refused_replacements(), 1);
    assert_eq!(reg.get(1).unwrap().find_payload(0x18), Some(p(0x7000)));
    // Re-installing the SAME table is not a replacement.
    reg.install(1, Arc::clone(&t1));
    assert_eq!(reg.refused_replacements(), 1);

    // Per-page removal: page 1 goes, page 2 stays (`clear` would drop both).
    let removed = reg.remove(1).expect("page 1 was installed");
    assert!(Arc::ptr_eq(&removed, &t1));
    assert!(!reg.contains(1));
    assert!(reg.contains(2));
    assert!(reg.remove(1).is_none());
    // Once removed, a new cycle's table installs normally.
    reg.install(1, Arc::clone(&other));
    assert!(Arc::ptr_eq(&reg.get(1).unwrap(), &other));
    assert_eq!(reg.refused_replacements(), 1);
    assert_eq!(reg.remove_pages(&[1, 2, 3]), 2);
    assert_eq!(reg.table_count(), 0);
}

/// zs s2/sl2: `install_for_set` keeps a page's live table too.
#[test]
fn zs2sl2_install_for_set_keeps_a_live_table() {
    let mb = 1024 * 1024;
    let pages = vec![small(1, mb / 8), small(2, mb / 16)];
    let set = ZRelocationSet::select(&pages, &ZRelocationPolicy::default());
    let reg = ZForwardingRegistry::new();
    let live = Arc::new(ZForwardingTable::for_page(1, 32));
    live.insert(0x18, p(0x7000));
    reg.install(1, Arc::clone(&live));
    reg.install_for_set(&set, ZFWD_ASSUMED_AVG_OBJECT_BYTES);
    assert_eq!(reg.table_count(), 2);
    assert!(Arc::ptr_eq(&reg.get(1).unwrap(), &live));
    assert_eq!(reg.refused_replacements(), 1);
}
