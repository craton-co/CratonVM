use super::*;

/// Serialises every test that **touches** the global forwarding overflow
/// side table — interning a high address counts, not just asserting the
/// table's length.
///
/// # The premise this used to carry was wrong
///
/// It read: "Per-header invariants (exact address round-trip, tag bit) are
/// deterministic regardless and don't need this." A round-trip through the
/// *inline* encoding is indeed per-header. A round-trip through the
/// **overflow** encoding is not: `set_forwarding_ptr` stores only a token in
/// the header and parks the real address in this process-wide table, so
/// `forwarding_ptr()` is a lookup in shared mutable state. Any test that
/// interns is therefore exposed to `forwarding_overflow_table_clears`, whose
/// `clear_forwarding_overflow_table()` wipes the whole table — every other
/// test's tokens with it. `resolve` answers `0` for a missing token, so the
/// victim fails with `0 != <its address>`.
///
/// That is not theoretical. Before this guard was widened,
/// `cargo test -p cratonvm-gc --lib -- forwarding` failed **15 runs out of
/// 40**, spread across four different victims
/// (`forwarding_ptr_distinct_high_targets`, `forwarding_ptr_above_8gb_exact`,
/// `forwarding_ptr_mixed_sweep`,
/// `forwarding_ptr_overflow_survives_raw_roundtrip`) — whichever happened to
/// be between its intern and its read when the clear landed. In the full
/// suite it diluted to roughly one run in four, which is exactly the rate at
/// which a flake gets re-run rather than diagnosed.
///
/// The clearing test analysed the race in one direction only and concluded
/// its own assertions were safe. They are — concurrent interning only *adds*
/// entries, and its token is never reissued. What it did not consider is the
/// other direction: what its clear does to everyone else.
///
/// # How to apply
///
/// Hold this for the whole test if the test interns **any** address above
/// [`CompactHeader::FORWARD_INLINE_MAX_ADDR`], including a sweep where only
/// some entries are high. Tests that stay entirely inline do not need it.
/// `--test-threads=1` is not the fix: it would hide this class of bug across
/// the other 980 tests in the crate.
fn overflow_table_guard() -> std::sync::MutexGuard<'static, ()> {
    static OVERFLOW_TABLE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    OVERFLOW_TABLE_GUARD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// -- CompactHeader construction -----------------------------------------

#[test]
fn new_object_basic() {
    let h = CompactHeader::new_object(42, 0);
    assert_eq!(h.narrow_klass(), 42);
    assert_eq!(h.gc_age(), 0);
    assert!(!h.is_array());
    assert_eq!(h.lock_state(), LockState::Unlocked);
    assert!(!h.has_hash_code());
}

#[test]
fn new_object_with_age() {
    let h = CompactHeader::new_object(100, 15);
    assert_eq!(h.narrow_klass(), 100);
    assert_eq!(h.gc_age(), 15);
}

#[test]
fn new_object_max_klass() {
    let h = CompactHeader::new_object(u32::MAX, 0);
    assert_eq!(h.narrow_klass(), u32::MAX);
}

#[test]
fn new_array_with_element_type() {
    let h = CompactHeader::new_array(7, 5, 3);
    assert!(h.is_array());
    assert_eq!(h.narrow_klass(), 7);
    assert_eq!(h.element_type(), 5);
    assert_eq!(h.gc_age(), 3);
}

// -- NarrowKlass roundtrip ----------------------------------------------

#[test]
fn narrow_klass_roundtrip() {
    for klass in [0, 1, 255, 65535, 0xDEAD_BEEF, u32::MAX] {
        let h = CompactHeader::new_object(klass, 0);
        assert_eq!(h.narrow_klass(), klass, "klass={klass:#x}");
    }
}

#[test]
fn set_narrow_klass_preserves_lower_bits() {
    let mut h = CompactHeader::new_array(1, 9, 7);
    h.set_narrow_klass(0xABCD_1234);
    assert_eq!(h.narrow_klass(), 0xABCD_1234);
    // Lower bits unchanged.
    assert_eq!(h.gc_age(), 7);
    assert!(h.is_array());
    assert_eq!(h.element_type(), 9);
}

// -- GC age -------------------------------------------------------------

#[test]
fn gc_age_zero() {
    let h = CompactHeader::new_object(1, 0);
    assert_eq!(h.gc_age(), 0);
}

#[test]
fn gc_age_fifteen() {
    let h = CompactHeader::new_object(1, 15);
    assert_eq!(h.gc_age(), 15);
}

#[test]
fn gc_age_max_127() {
    let h = CompactHeader::new_object(1, 127);
    assert_eq!(h.gc_age(), 127);
}

#[test]
fn gc_age_clamped_above_max() {
    let h = CompactHeader::new_object(1, 200);
    assert_eq!(h.gc_age(), CompactHeader::MAX_GC_AGE);
}

#[test]
fn increment_age_caps_at_max() {
    let mut h = CompactHeader::new_object(1, 126);
    assert_eq!(h.increment_age(), 127);
    assert_eq!(h.increment_age(), 127); // stays capped
    assert_eq!(h.gc_age(), 127);
}

// -- Lock state ---------------------------------------------------------

#[test]
fn lock_state_transitions() {
    let mut h = CompactHeader::new_object(1, 0);
    assert_eq!(h.lock_state(), LockState::Unlocked);

    h.set_lock_state(LockState::ThinLocked);
    assert_eq!(h.lock_state(), LockState::ThinLocked);

    h.set_lock_state(LockState::Inflated);
    assert_eq!(h.lock_state(), LockState::Inflated);

    h.set_lock_state(LockState::Forwarded);
    assert_eq!(h.lock_state(), LockState::Forwarded);

    h.set_lock_state(LockState::Unlocked);
    assert_eq!(h.lock_state(), LockState::Unlocked);
}

// -- Hash code flag -----------------------------------------------------

#[test]
fn has_hash_code_flag() {
    let mut h = CompactHeader::new_object(1, 0);
    assert!(!h.has_hash_code());
    h.set_has_hash_code();
    assert!(h.has_hash_code());
    // Other fields untouched.
    assert_eq!(h.narrow_klass(), 1);
    assert_eq!(h.gc_age(), 0);
}

// -- Array flag ---------------------------------------------------------

#[test]
fn is_array_flag() {
    let obj = CompactHeader::new_object(1, 0);
    assert!(!obj.is_array());
    let arr = CompactHeader::new_array(1, 0, 0);
    assert!(arr.is_array());
}

// -- Element type -------------------------------------------------------

#[test]
fn element_type_values() {
    for et in 0u8..=15 {
        let h = CompactHeader::new_array(1, et, 0);
        assert_eq!(h.element_type(), et, "element_type={et}");
    }
}

// -- Forwarding pointer -------------------------------------------------

#[test]
fn forwarding_ptr_roundtrip() {
    let addrs: Vec<usize> = vec![0, 8, 64, 0x1000, 0x0FFF_FFF8];
    for &addr in &addrs {
        let mut h = CompactHeader::new_object(42, 5);
        h.set_forwarding_ptr(addr);
        assert!(h.is_forwarded(), "addr={addr:#x}");
        assert_eq!(h.forwarding_ptr(), addr, "addr={addr:#x}");
        // Klass preserved.
        assert_eq!(h.narrow_klass(), 42);
    }
}

#[test]
fn forwarding_clears_other_lower_fields() {
    let mut h = CompactHeader::new_array(10, 7, 12);
    h.set_has_hash_code();
    // After installing forwarding pointer the lower 32 bits are overwritten.
    h.set_forwarding_ptr(0x8000);
    assert!(h.is_forwarded());
    assert_eq!(h.lock_state(), LockState::Forwarded);
    // Klass survives.
    assert_eq!(h.narrow_klass(), 10);
}

#[test]
fn is_forwarded_check() {
    let mut h = CompactHeader::new_object(1, 0);
    assert!(!h.is_forwarded());
    h.set_lock_state(LockState::Forwarded);
    assert!(h.is_forwarded());
}

// -- Forwarding pointer: high addresses (>4 GB / >8 GB) -----------------

/// Regression: a forwarding target above 8 GB must round-trip exactly.
///
/// The old encoder masked `addr >> 3` to 30 bits and silently dropped every
/// bit above ~8 GB, returning a *truncated* (wrong) address — a moving GC
/// would then read/write the wrong memory and corrupt the heap. The fix
/// routes such targets through the overflow side table.
#[test]
fn forwarding_ptr_above_8gb_exact() {
    // Interns into the shared overflow table — see `overflow_table_guard`.
    let _guard = overflow_table_guard();
    // 16 GB, 8-byte aligned. addr >> 3 needs 31 bits, so the 30-bit inline
    // field cannot hold it.
    let addr: usize = 16usize * 1024 * 1024 * 1024;
    assert!(addr & 0x7 == 0);
    assert!(addr > CompactHeader::FORWARD_INLINE_MAX_ADDR);

    let mut h = CompactHeader::new_object(0xABCD, 7);
    h.set_forwarding_ptr(addr);
    assert!(h.is_forwarded());
    assert_eq!(h.forwarding_ptr(), addr, "must not truncate >8 GB target");
    // Klass survives the overflow encoding.
    assert_eq!(h.narrow_klass(), 0xABCD);

    // Demonstrate that the OLD inline-only encoding *would* have truncated:
    let truncated = ((addr >> 3) as u64 & 0x3FFF_FFFF) << 3;
    assert_ne!(truncated as usize, addr, "old encoding loses high bits");
}

/// The overflow path survives a `raw()` / `from_raw()` round-trip, because
/// the token lives inside the 64-bit header value itself.
#[test]
fn forwarding_ptr_overflow_survives_raw_roundtrip() {
    // Interns into the shared overflow table — see `overflow_table_guard`.
    let _guard = overflow_table_guard();
    let addr: usize = 0x7_0000_0008; // 28 GB + 8, aligned
    let mut h = CompactHeader::new_object(3, 0);
    h.set_forwarding_ptr(addr);
    let copy = CompactHeader::from_raw(h.raw());
    assert!(copy.is_forwarded());
    assert_eq!(copy.forwarding_ptr(), addr);
}

/// Two distinct high targets must not alias to the same address.
#[test]
fn forwarding_ptr_distinct_high_targets() {
    // Interns twice into the shared overflow table — see
    // `overflow_table_guard`. This was the most frequent victim of the clear
    // race, holding two live tokens across four reads.
    let _guard = overflow_table_guard();
    let a: usize = 0x10_0000_0000; // 64 GB
    let b: usize = 0x10_0000_0008; // 64 GB + 8
    let mut ha = CompactHeader::new_object(1, 0);
    let mut hb = CompactHeader::new_object(2, 0);
    ha.set_forwarding_ptr(a);
    hb.set_forwarding_ptr(b);
    assert_eq!(ha.forwarding_ptr(), a);
    assert_eq!(hb.forwarding_ptr(), b);
    assert_ne!(ha.forwarding_ptr(), hb.forwarding_ptr());
}

/// Boundary: the largest inline-representable address stays on the fast
/// path (no overflow tag, no side-table entry) and round-trips exactly;
/// the next aligned address up tips into the overflow path.
#[test]
fn forwarding_ptr_inline_boundary() {
    // The `just_over` half interns into the shared overflow table, so this
    // test needs the guard even though its first half is pure inline —
    // "mostly inline" is not "inline". See `overflow_table_guard`.
    let _guard = overflow_table_guard();
    let max_inline = CompactHeader::FORWARD_INLINE_MAX_ADDR;
    assert_eq!(max_inline & 0x7, 0, "inline max must be 8-byte aligned");

    let mut h = CompactHeader::new_object(9, 1);
    h.set_forwarding_ptr(max_inline);
    assert_eq!(h.forwarding_ptr(), max_inline);
    // Fast path: the overflow tag is clear -> encoded inline, not in the table.
    assert_eq!(
        h.read_forward_field() & CompactHeader::FORWARD_OVERFLOW_TAG,
        0
    );

    // One object slot (8 bytes) higher cannot be encoded inline.
    let just_over = max_inline + 8;
    let mut h2 = CompactHeader::new_object(9, 1);
    h2.set_forwarding_ptr(just_over);
    assert_eq!(h2.forwarding_ptr(), just_over);
    assert_ne!(
        h2.read_forward_field() & CompactHeader::FORWARD_OVERFLOW_TAG,
        0
    );
}

/// Sweep of representative addresses straddling the inline/overflow split,
/// including 0, small, near-boundary, and several multi-GB targets.
#[test]
fn forwarding_ptr_mixed_sweep() {
    // Four of the eight addresses below are above the inline ceiling and
    // intern into the shared overflow table — see `overflow_table_guard`.
    let _guard = overflow_table_guard();
    let addrs: Vec<usize> = vec![
        0,
        8,
        0x1000,
        0x0FFF_FFF8,
        CompactHeader::FORWARD_INLINE_MAX_ADDR, // last inline
        CompactHeader::FORWARD_INLINE_MAX_ADDR + 8, // first overflow
        12usize * 1024 * 1024 * 1024,           // 12 GB
        0xFF_FFFF_FFF8,                         // ~1 TB, aligned
    ];
    for &addr in &addrs {
        let mut h = CompactHeader::new_object(0x5151, 4);
        h.set_forwarding_ptr(addr);
        assert!(h.is_forwarded(), "addr={addr:#x}");
        assert_eq!(h.forwarding_ptr(), addr, "addr={addr:#x}");
        assert_eq!(
            h.narrow_klass(),
            0x5151,
            "klass clobbered at addr={addr:#x}"
        );
    }
}

/// The overflow side table can be cleared between GC cycles.
///
/// This test's *own* assertions are race-safe and its doc used to say so:
/// tokens are monotonic and never reissued, so a token dropped by `clear()`
/// resolves to 0 afterwards regardless of concurrent interning, and the
/// `>= 1` length check only ever gains entries. That reasoning was correct
/// and it concluded "correctness does not depend on the guard" — of the one
/// direction it looked at.
///
/// The direction it missed is what this test does **to** its neighbours.
/// `clear_forwarding_overflow_table()` is not scoped to this test's token;
/// it empties the process-wide table, so every concurrently-running test
/// holding an overflow forwarding loses its address. The guard is now
/// load-bearing here — it is what keeps this clear from landing between some
/// other test's intern and its read. See `overflow_table_guard`.
#[test]
fn forwarding_overflow_table_clears() {
    let _guard = overflow_table_guard();

    let addr = 20usize * 1024 * 1024 * 1024; // 20 GB -> overflow path
    let mut h = CompactHeader::new_object(1, 0);
    h.set_forwarding_ptr(addr);
    assert!(forwarding_overflow_table_len() >= 1);
    // The freshly interned high target resolves before clearing.
    assert_eq!(h.forwarding_ptr(), addr);

    clear_forwarding_overflow_table();
    // After clearing, this header's token is gone and never reissued, so it
    // resolves to 0 (the "unknown token" sentinel) — independent of any
    // concurrent interning.
    assert_eq!(h.forwarding_ptr(), 0);
}

// -- Raw roundtrip ------------------------------------------------------

#[test]
fn raw_roundtrip() {
    let h = CompactHeader::new_array(0xCAFE, 11, 99);
    let raw = h.raw();
    let h2 = CompactHeader::from_raw(raw);
    assert_eq!(h2.narrow_klass(), 0xCAFE);
    assert_eq!(h2.element_type(), 11);
    assert_eq!(h2.gc_age(), 99);
    assert!(h2.is_array());
}

// -- Size constant ------------------------------------------------------

#[test]
fn size_is_8_bytes() {
    assert_eq!(CompactHeader::SIZE, 8);
    assert_eq!(std::mem::size_of::<CompactHeader>(), 8);
}

#[test]
fn memory_savings_vs_old_header() {
    // The legacy ObjectHeader is `HEADER_SIZE` (16) bytes, compact is 8.
    let old_size: usize = crate::heap::HEADER_SIZE;
    let new_size = CompactHeader::SIZE;
    assert_eq!(old_size - new_size, 8);
}

// -- HashCodeTable ------------------------------------------------------

#[test]
fn hash_table_get_or_assign_unique() {
    let ht = HashCodeTable::new();
    let h1 = ht.get_or_assign(0x1000);
    let h2 = ht.get_or_assign(0x2000);
    assert_ne!(h1, h2);
}

#[test]
fn hash_table_get_or_assign_stable() {
    let ht = HashCodeTable::new();
    let h1 = ht.get_or_assign(0x1000);
    let h2 = ht.get_or_assign(0x1000);
    assert_eq!(h1, h2);
}

#[test]
fn hash_table_has_hash() {
    let ht = HashCodeTable::new();
    assert!(!ht.has_hash(0x1000));
    ht.get_or_assign(0x1000);
    assert!(ht.has_hash(0x1000));
    assert!(!ht.has_hash(0x2000));
}

#[test]
fn hash_table_get() {
    let ht = HashCodeTable::new();
    assert_eq!(ht.get(0x1000), None);
    let assigned = ht.get_or_assign(0x1000);
    assert_eq!(ht.get(0x1000), Some(assigned));
}

#[test]
fn hash_table_update_after_gc() {
    let ht = HashCodeTable::new();
    let hash = ht.get_or_assign(0x1000);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000usize, 0x5000usize);
    ht.update_after_gc(&map, &|_| false);
    // Old address gone, new address has the same hash.
    assert!(!ht.has_hash(0x1000));
    assert_eq!(ht.get(0x5000), Some(hash));
}

/// GCAUD-7 — `update_after_gc` must remap **and sweep**.
///
/// Before this it was remap-only (`unwrap_or(addr)`), so an address the
/// pointer map did not mention kept its entry forever. With no sweep
/// anywhere (`remove_dead` has never had a caller), the next object
/// allocated at that recycled address inherits a dead object's identity
/// hash — and the table grows without bound.
///
/// All three outcomes are asserted together, because a sweep that drops
/// stationary survivors is just as wrong as one that keeps dead keys: an
/// object's `identityHashCode` must not change while it is alive.
#[test]
fn hash_table_update_after_gc_sweeps_dead_keys_and_keeps_stationary_survivors() {
    let ht = HashCodeTable::new();
    let moved = ht.get_or_assign(0x1000);
    let stayed = ht.get_or_assign(0x2000);
    let died = ht.get_or_assign(0x3000);
    assert_ne!(stayed, died);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000usize, 0x5000usize);
    // Only 0x2000 is a stationary survivor; 0x3000 was reclaimed.
    ht.update_after_gc(&map, &|addr| addr == 0x2000);

    assert_eq!(ht.get(0x5000), Some(moved), "a moved object keeps its hash");
    assert!(!ht.has_hash(0x1000), "the pre-move key must not linger");
    assert_eq!(
        ht.get(0x2000),
        Some(stayed),
        "a live object that did not move must keep its identity hash — \
         changing it mid-life breaks every hash container holding it",
    );
    assert!(
        !ht.has_hash(0x3000),
        "a dead key must be swept: an object later allocated at 0x3000 \
         would otherwise inherit this hash",
    );
    assert_eq!(ht.len(), 2, "the table must not grow without bound");
}

#[test]
fn hash_table_remove_dead() {
    let ht = HashCodeTable::new();
    ht.get_or_assign(0x1000);
    ht.get_or_assign(0x2000);
    ht.get_or_assign(0x3000);
    assert_eq!(ht.len(), 3);

    ht.remove_dead(&|addr| addr == 0x2000); // only 0x2000 is live
    assert_eq!(ht.len(), 1);
    assert!(ht.has_hash(0x2000));
    assert!(!ht.has_hash(0x1000));
}

#[test]
fn hash_table_savings_bytes() {
    let ht = HashCodeTable::new();
    // 1000 objects, no hashes requested: saves 1000*4 = 4000 bytes.
    assert_eq!(ht.savings_bytes(1000), 4000);

    // Assign one hash -- savings decrease slightly.
    ht.get_or_assign(0x1000);
    let savings = ht.savings_bytes(1000);
    assert!(savings < 4000);
    assert!(savings > 0);
}

#[test]
fn hash_table_len_and_empty() {
    let ht = HashCodeTable::new();
    assert!(ht.is_empty());
    assert_eq!(ht.len(), 0);
    ht.get_or_assign(0x1000);
    assert!(!ht.is_empty());
    assert_eq!(ht.len(), 1);
}

#[test]
fn hash_table_clear() {
    let ht = HashCodeTable::new();
    ht.get_or_assign(0x1000);
    ht.get_or_assign(0x2000);
    assert_eq!(ht.len(), 2);
    ht.clear();
    assert!(ht.is_empty());
}

// -- Migration helpers --------------------------------------------------

#[test]
fn migrate_preserves_class_and_age() {
    let mut old = crate::heap::ObjectHeader::new(
        cratonvm_types::ClassId::new(77),
        crate::heap::ObjectKind::Object,
        crate::heap::ArrayElementType::Reference,
        0,
        2,
    );
    old.set_gc_age(9);
    let ht = HashCodeTable::new();
    let compact = migrate_to_compact(&old, 77, &ht, 0x4000);
    assert_eq!(compact.narrow_klass(), 77);
    assert_eq!(compact.gc_age(), 9);
    assert!(!compact.is_array());
    assert!(!compact.has_hash_code());
}

#[test]
fn migrate_array_preserves_element_type() {
    let old = crate::heap::ObjectHeader::new(
        cratonvm_types::ClassId::new(10),
        crate::heap::ObjectKind::Array,
        crate::heap::ArrayElementType::Int,
        5,
        5,
    );
    let ht = HashCodeTable::new();
    let compact = migrate_to_compact(&old, 10, &ht, 0x5000);
    assert!(compact.is_array());
    assert_eq!(
        compact.element_type(),
        crate::heap::ArrayElementType::Int as u8
    );
}

#[test]
fn migrate_hash_code_to_side_table() {
    let old = crate::heap::ObjectHeader::new(
        cratonvm_types::ClassId::new(1),
        crate::heap::ObjectKind::Object,
        crate::heap::ArrayElementType::Reference,
        0,
        0,
    );
    // A long (legacy) header keeps its identity hash in the aux word, so
    // the migration source is set up the way a real hashed object is.
    old.identity_hash(|| 42).unwrap();
    let ht = HashCodeTable::new();
    let compact = migrate_to_compact(&old, 1, &ht, 0x6000);
    assert!(compact.has_hash_code());
    assert_eq!(ht.get(0x6000), Some(42));
}

#[test]
fn to_legacy_fields_roundtrip() {
    let h = CompactHeader::new_array(55, 3, 10);
    let lf = to_legacy_fields(h);
    assert_eq!(lf.narrow_klass, 55);
    assert!(lf.is_array);
    assert_eq!(lf.element_type, 3);
    assert_eq!(lf.gc_age, 10);
    assert_eq!(lf.lock_state, LockState::Unlocked);
    assert!(!lf.has_hash);
    assert!(!lf.is_forwarded);
    assert_eq!(lf.forwarding_addr, 0);
}

#[test]
fn to_legacy_fields_forwarded() {
    let mut h = CompactHeader::new_object(99, 0);
    h.set_forwarding_ptr(0x8000);
    let lf = to_legacy_fields(h);
    assert!(lf.is_forwarded);
    assert_eq!(lf.forwarding_addr, 0x8000);
    assert_eq!(lf.narrow_klass, 99);
}

#[test]
fn legacy_fields_all_populated() {
    let mut h = CompactHeader::new_array(200, 7, 42);
    h.set_has_hash_code();
    h.set_lock_state(LockState::Inflated);
    let lf = to_legacy_fields(h);
    assert_eq!(lf.narrow_klass, 200);
    assert!(lf.is_array);
    assert_eq!(lf.element_type, 7);
    assert_eq!(lf.gc_age, 42);
    assert_eq!(lf.lock_state, LockState::Inflated);
    assert!(lf.has_hash);
    assert!(!lf.is_forwarded);
}

// =======================================================================
// Session 54 — Compact Object Headers (JEP 450)
// =======================================================================

// -- 54.1: NarrowKlassTable --

#[test]
fn s54_narrow_klass_table_new_empty() {
    let t = NarrowKlassTable::new();
    assert!(t.is_empty());
    assert_eq!(t.len(), 0);
}

#[test]
fn s54_narrow_klass_table_assign_sequential() {
    let t = NarrowKlassTable::new();
    let nk1 = t.get_or_assign(cratonvm_types::ClassId::new(100));
    let nk2 = t.get_or_assign(cratonvm_types::ClassId::new(200));
    assert_ne!(nk1, nk2);
    assert!(nk1 >= 1);
    assert!(nk2 >= 1);
    assert_eq!(t.len(), 2);
}

#[test]
fn s54_narrow_klass_table_idempotent() {
    let t = NarrowKlassTable::new();
    let nk1 = t.get_or_assign(cratonvm_types::ClassId::new(42));
    let nk2 = t.get_or_assign(cratonvm_types::ClassId::new(42));
    assert_eq!(nk1, nk2);
    assert_eq!(t.len(), 1);
}

#[test]
fn s54_narrow_klass_table_resolve() {
    let t = NarrowKlassTable::new();
    let nk = t.get_or_assign(cratonvm_types::ClassId::new(77));
    let resolved = t.resolve(nk);
    assert_eq!(resolved, Some(cratonvm_types::ClassId::new(77)));
}

#[test]
fn s54_narrow_klass_table_resolve_unknown() {
    let t = NarrowKlassTable::new();
    assert_eq!(t.resolve(999), None);
}

#[test]
fn s54_narrow_klass_table_contains() {
    let t = NarrowKlassTable::new();
    let cid = cratonvm_types::ClassId::new(55);
    assert!(!t.contains(cid));
    t.get_or_assign(cid);
    assert!(t.contains(cid));
}

#[test]
fn s54_narrow_klass_table_many_classes() {
    let t = NarrowKlassTable::new();
    for i in 0..500 {
        let cid = cratonvm_types::ClassId::new(i);
        let nk = t.get_or_assign(cid);
        assert!(nk > 0);
        assert_eq!(t.resolve(nk), Some(cid));
    }
    assert_eq!(t.len(), 500);
}

#[test]
fn s54_narrow_klass_table_thread_safe() {
    use std::sync::Arc;
    let t = Arc::new(NarrowKlassTable::new());
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let t = t.clone();
            std::thread::spawn(move || {
                for j in 0..50 {
                    let cid = cratonvm_types::ClassId::new(i * 50 + j);
                    let nk = t.get_or_assign(cid);
                    assert_eq!(t.resolve(nk), Some(cid));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(t.len(), 500);
}

// -- NarrowKlassTable removal path (refs-metaspace-unloading.md §R3) --

#[test]
fn narrow_klass_table_remove_classes_drops_both_directions() {
    let t = NarrowKlassTable::new();
    let keep = cratonvm_types::ClassId::new(7);
    let drop_a = cratonvm_types::ClassId::new(8);
    let drop_b = cratonvm_types::ClassId::new(9);

    let nk_keep = t.get_or_assign(keep);
    let nk_a = t.get_or_assign(drop_a);
    let nk_b = t.get_or_assign(drop_b);
    assert_eq!(t.len(), 3);

    assert_eq!(t.remove_classes(&[drop_a, drop_b]), 2);

    // Forward direction gone...
    assert!(!t.contains(drop_a));
    assert!(!t.contains(drop_b));
    // ...and the reverse direction too. A leak here is the one that
    // matters: `to_class_id` is what keeps the raw class id reachable.
    assert_eq!(t.resolve(nk_a), None);
    assert_eq!(t.resolve(nk_b), None);

    // The survivor is untouched in both directions.
    assert!(t.contains(keep));
    assert_eq!(t.resolve(nk_keep), Some(keep));
    assert_eq!(t.len(), 1);
}

#[test]
fn narrow_klass_table_remove_classes_is_idempotent_and_ignores_unknowns() {
    let t = NarrowKlassTable::new();
    let cid = cratonvm_types::ClassId::new(3);
    t.get_or_assign(cid);

    assert_eq!(t.remove_classes(&[cid]), 1);
    // Second removal of the same id, and of an id never assigned, are both
    // no-ops rather than panics or phantom counts — an unloader that runs
    // twice over the same batch must not corrupt the count it reports.
    assert_eq!(t.remove_classes(&[cid]), 0);
    assert_eq!(t.remove_classes(&[cratonvm_types::ClassId::new(999)]), 0);
    assert_eq!(t.remove_classes(&[]), 0);
    assert!(t.is_empty());
}

/// A retired narrow klass must NEVER be handed to a different class: a
/// header the sweeper has not reached yet still carries the old value, and
/// recycling would make it resolve to a live, wrong class. `resolve`
/// returning `None` is the required failure mode.
#[test]
fn narrow_klass_table_does_not_recycle_retired_ids() {
    let t = NarrowKlassTable::new();
    let old = cratonvm_types::ClassId::new(11);
    let nk_old = t.get_or_assign(old);
    assert_eq!(t.remove_classes(&[old]), 1);

    let fresh = cratonvm_types::ClassId::new(12);
    let nk_fresh = t.get_or_assign(fresh);
    assert_ne!(
        nk_fresh, nk_old,
        "a retired narrow klass was reissued: a stale header for class {old:?} \
         would now resolve to {fresh:?}"
    );
    assert_eq!(t.resolve(nk_old), None);
    assert_eq!(t.resolve(nk_fresh), Some(fresh));
}

/// The point of the whole change: repeated define/unload cycles must reach
/// a steady state instead of growing one entry per cycle. This is the shape
/// of any CGLIB/ByteBuddy/redeploy workload.
#[test]
fn narrow_klass_table_repeated_unload_cycles_leave_no_residue() {
    let t = NarrowKlassTable::new();
    for cycle in 0..64u32 {
        let batch: Vec<_> = (0..16)
            .map(|i| cratonvm_types::ClassId::new(cycle * 16 + i))
            .collect();
        for cid in &batch {
            t.get_or_assign(*cid);
        }
        assert_eq!(t.len(), 16);
        assert_eq!(t.remove_classes(&batch), 16);
        assert!(
            t.is_empty(),
            "residue after cycle {cycle}: {} entries",
            t.len()
        );
    }
}

#[test]
fn narrow_klass_table_remove_classes_is_thread_safe() {
    use std::sync::Arc;
    let t = Arc::new(NarrowKlassTable::new());
    let all: Vec<_> = (0..500u32).map(cratonvm_types::ClassId::new).collect();
    for cid in &all {
        t.get_or_assign(*cid);
    }
    // Ten threads each retire a disjoint slice concurrently; the totals
    // must add up to exactly one removal per class.
    let handles: Vec<_> = (0..10usize)
        .map(|i| {
            let t = Arc::clone(&t);
            let slice: Vec<_> = all[i * 50..(i + 1) * 50].to_vec();
            std::thread::spawn(move || t.remove_classes(&slice))
        })
        .collect();
    let removed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(removed, 500);
    assert!(t.is_empty());
}

/// gci i44/ch: a thread handed a narrow klass by `get_or_assign` must be
/// able to `resolve` it at once, even when a PEER assigned it and this
/// thread only took the fast path. The reverse entry used to be inserted
/// after `to_narrow` was released, so a fast-path reader could see the id
/// before its reverse entry existed and resolve its own class to `None`.
/// Every thread races on the SAME fresh ids (the old test gave each thread
/// disjoint ids, which never takes the fast path on a peer's entry).
#[test]
fn gci_i44ch_a_peer_assigned_narrow_klass_resolves_at_once() {
    use std::sync::{Arc, Barrier};
    for round in 0..40u32 {
        let t = Arc::new(NarrowKlassTable::new());
        let start = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let t = Arc::clone(&t);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    for j in 0..64u32 {
                        let cid = cratonvm_types::ClassId::new(round * 64 + j);
                        let nk = t.get_or_assign(cid);
                        assert_eq!(
                            t.resolve(nk),
                            Some(cid),
                            "round {round}: narrow klass {nk} was handed out before \
                             its reverse entry existed"
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(t.len(), 64);
    }
}

/// gci i44/ch: `remove_classes` racing `get_or_assign` for the SAME class
/// must never leave a reverse entry whose forward entry is gone. Such an
/// orphan makes a retired narrow klass resolve to its unloaded class, which
/// `narrow_klass_table_does_not_recycle_retired_ids` states must not
/// happen. Checked on the two maps directly once every thread has joined:
/// each reverse entry `nk -> class` must be matched by `class -> nk`.
#[test]
fn gci_i44ch_racing_assign_and_remove_leave_no_orphan_reverse_entry() {
    use std::sync::{Arc, Barrier};
    let t = Arc::new(NarrowKlassTable::new());
    let ids: Vec<_> = (0..32u32).map(cratonvm_types::ClassId::new).collect();
    let start = Arc::new(Barrier::new(6));
    let handles: Vec<_> = (0..6usize)
        .map(|w| {
            let t = Arc::clone(&t);
            let ids = ids.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                for _ in 0..400 {
                    if w % 2 == 0 {
                        for cid in &ids {
                            t.get_or_assign(*cid);
                        }
                    } else {
                        t.remove_classes(&ids);
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let forward = t.to_narrow.read();
    let reverse = t.to_class_id.read();
    for (&nk, &raw) in reverse.iter() {
        assert_eq!(
            forward.get(&raw).copied(),
            Some(nk),
            "orphan reverse entry {nk} -> class {raw}: a retired narrow klass \
             still resolves to its class"
        );
    }
    assert_eq!(forward.len(), reverse.len());
}

// -- 54.2: HeaderView --

#[test]
fn s54_header_view_from_legacy() {
    let mut old = crate::heap::ObjectHeader::new(
        cratonvm_types::ClassId::new(10),
        crate::heap::ObjectKind::Object,
        crate::heap::ArrayElementType::Reference,
        0,
        3,
    );
    old.set_gc_age(5);
    let view = HeaderView::from_legacy(&old);
    assert!(!view.is_array());
    assert_eq!(view.gc_age(), 5);
    assert!(!view.is_forwarded());
    assert_eq!(view.header_size(), crate::heap::HEADER_SIZE);
}

#[test]
fn s54_header_view_from_compact() {
    let h = CompactHeader::new_array(99, 10, 7);
    let view = HeaderView::from_compact(h);
    assert!(view.is_array());
    assert_eq!(view.gc_age(), 7);
    assert_eq!(view.element_type(), 10);
    assert!(!view.is_forwarded());
    assert_eq!(view.header_size(), 8);
}

#[test]
fn s54_header_view_legacy_forwarded() {
    let mut old = crate::heap::ObjectHeader::new(
        cratonvm_types::ClassId::new(1),
        crate::heap::ObjectKind::Object,
        crate::heap::ArrayElementType::Reference,
        0,
        0,
    );
    // Raw bits, not `set_forwarding_address`: the constructor asserts the
    // target is a plausible, 8-byte-aligned heap pointer, and 0x1234 is
    // deliberately neither. Writing the word directly is how a test says
    // "pretend the collector already forwarded this" without pretending the
    // address is real.
    old.mark_word.store(
        cratonvm_types::MARK_FORWARDED,
        std::sync::atomic::Ordering::Relaxed,
    );
    let view = HeaderView::from_legacy(&old);
    assert!(view.is_forwarded());
}

#[test]
fn s54_header_view_compact_forwarded() {
    let mut h = CompactHeader::new_object(50, 3);
    h.set_forwarding_ptr(0x8000);
    let view = HeaderView::from_compact(h);
    assert!(view.is_forwarded());
}

// -- 54.3: CompactAllocator --

#[test]
fn s54_compact_allocator_new() {
    let alloc = CompactAllocator::new(1024);
    assert_eq!(alloc.capacity(), 1024);
    assert_eq!(alloc.used_bytes(), 0);
    assert_eq!(alloc.object_count(), 0);
}

#[test]
fn s54_compact_allocator_alloc_object() {
    let alloc = CompactAllocator::new(4096);
    let cid = cratonvm_types::ClassId::new(42);
    let ptr = alloc.alloc_object(cid, 3);
    assert!(ptr.is_some());
    let ptr = ptr.unwrap();

    // Verify the header
    let header = alloc.read_header(ptr);
    assert!(!header.is_array());
    assert_eq!(header.gc_age(), 0);

    // Verify class ID resolution
    let resolved = alloc.class_id_at(ptr);
    assert_eq!(resolved, Some(cid));

    assert_eq!(alloc.object_count(), 1);
    // Derived, not restated: this delta moved 24 -> 16 when the legacy
    // header shrank 32 -> 24 on 2026-08-06, and it is the number the
    // savings report is FOR — a literal here reports the wrong saving
    // as confidently as the right one.
    assert_eq!(alloc.bytes_saved(), LEGACY_MINUS_COMPACT_HEADER);
}

#[test]
fn s54_compact_allocator_alloc_array() {
    let alloc = CompactAllocator::new(4096);
    let cid = cratonvm_types::ClassId::new(10);
    let ptr = alloc.alloc_array(cid, crate::heap::ArrayElementType::Int, 5);
    assert!(ptr.is_some());
    let ptr = ptr.unwrap();

    let header = alloc.read_header(ptr);
    assert!(header.is_array());
    assert_eq!(
        header.element_type(),
        crate::heap::ArrayElementType::Int as u8
    );

    let length = alloc.read_array_length(ptr);
    assert_eq!(length, 5);

    assert_eq!(alloc.object_count(), 1);
}

#[test]
fn s54_compact_allocator_oom() {
    let alloc = CompactAllocator::new(16); // Too small for a 3-field object
    let cid = cratonvm_types::ClassId::new(1);
    // 8 header + 3*16 fields = 56 bytes, won't fit in 16
    let ptr = alloc.alloc_object(cid, 3);
    assert!(ptr.is_none());
}

#[test]
fn s54_compact_allocator_multiple_objects() {
    let alloc = CompactAllocator::new(1024 * 1024);
    for i in 0..100 {
        let cid = cratonvm_types::ClassId::new(i);
        let ptr = alloc.alloc_object(cid, 2);
        assert!(ptr.is_some(), "Failed to alloc object {}", i);
    }
    assert_eq!(alloc.object_count(), 100);
    assert_eq!(alloc.bytes_saved(), 100 * LEGACY_MINUS_COMPACT_HEADER);
}

#[test]
fn s54_compact_allocator_identity_hash() {
    let alloc = CompactAllocator::new(4096);
    let cid = cratonvm_types::ClassId::new(1);
    let ptr = alloc.alloc_object(cid, 1).unwrap();

    let h1 = alloc.identity_hash_code(ptr as usize);
    let h2 = alloc.identity_hash_code(ptr as usize);
    assert_eq!(h1, h2, "Identity hash should be stable");

    let ptr2 = alloc.alloc_object(cid, 1).unwrap();
    let h3 = alloc.identity_hash_code(ptr2 as usize);
    assert_ne!(h1, h3, "Different objects should have different hashes");
}

#[test]
fn s54_compact_allocator_header_view() {
    let alloc = CompactAllocator::new(4096);
    let cid = cratonvm_types::ClassId::new(5);
    let ptr = alloc.alloc_object(cid, 2).unwrap();

    let view = alloc.header_view(ptr);
    assert!(!view.is_array());
    assert_eq!(view.gc_age(), 0);
    assert_eq!(view.header_size(), 8);
}

// -- 54.4: Savings report --

#[test]
fn s54_savings_report_empty() {
    let alloc = CompactAllocator::new(4096);
    let report = alloc.savings_report();
    assert_eq!(report.object_count, 0);
    assert_eq!(report.header_bytes_saved, 0);
    assert_eq!(report.total_savings, 0);
}

#[test]
fn s54_savings_report_with_objects() {
    let alloc = CompactAllocator::new(1024 * 1024);
    for i in 0..50 {
        alloc
            .alloc_object(cratonvm_types::ClassId::new(i), 1)
            .unwrap();
    }
    let report = alloc.savings_report();
    assert_eq!(report.object_count, 50);
    assert_eq!(report.header_bytes_saved, 50 * LEGACY_MINUS_COMPACT_HEADER);
    assert_eq!(report.klass_table_entries, 50);
    assert_eq!(report.hash_table_entries, 0); // no hash codes requested
}

#[test]
fn s54_savings_report_format() {
    let alloc = CompactAllocator::new(4096);
    alloc
        .alloc_object(cratonvm_types::ClassId::new(1), 2)
        .unwrap();
    let report = alloc.savings_report();
    let formatted = report.format();
    assert!(formatted.contains("Compact Object Headers Report"));
    assert!(formatted.contains("Objects:"));
    assert!(formatted.contains("Header savings:"));
    assert!(formatted.contains("Total savings:"));
}

#[test]
fn s54_savings_report_with_hash_codes() {
    let alloc = CompactAllocator::new(4096);
    let ptr = alloc
        .alloc_object(cratonvm_types::ClassId::new(1), 1)
        .unwrap();
    alloc.identity_hash_code(ptr as usize);
    let report = alloc.savings_report();
    assert_eq!(report.hash_table_entries, 1);
}

// -- 54.5: element_byte_size helper --

#[test]
fn s54_element_byte_size_values() {
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Boolean), 1);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Byte), 1);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Char), 2);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Short), 2);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Int), 4);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Float), 4);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Long), 8);
    assert_eq!(element_byte_size(crate::heap::ArrayElementType::Double), 8);
    assert_eq!(
        element_byte_size(crate::heap::ArrayElementType::Reference),
        8
    );
}

// -- 54.6: Integration with CompactHeader --

#[test]
fn s54_compact_header_8_vs_legacy_16() {
    assert_eq!(CompactHeader::SIZE, 8);
    // 32 -> 24 when `forwarding_ptr` folded into the mark word, 24 -> 16
    // when the identity hash followed and the kind/element_type/gc_age/
    // gc_flags quartet joined them in bits 48..62. The gap this type would
    // still close is now 8, not 24 -- most of the reason it existed has
    // been taken by the real header.
    assert_eq!(crate::heap::HEADER_SIZE, 16);
    assert_eq!(crate::heap::HEADER_SIZE - CompactHeader::SIZE, 8);
}

#[test]
fn s54_compact_allocator_klass_table_access() {
    let alloc = CompactAllocator::new(4096);
    let cid = cratonvm_types::ClassId::new(99);
    alloc.alloc_object(cid, 1).unwrap();

    let kt = alloc.klass_table();
    assert_eq!(kt.len(), 1);
    assert!(kt.contains(cid));
}

#[test]
fn s54_compact_allocator_hash_table_access() {
    let alloc = CompactAllocator::new(4096);
    let ht = alloc.hash_table();
    assert!(ht.is_empty());
}

// -- 54.7: Mixed array element types --

#[test]
fn s54_compact_array_all_element_types() {
    let alloc = CompactAllocator::new(1024 * 1024);
    let cid = cratonvm_types::ClassId::new(1);
    let types = [
        crate::heap::ArrayElementType::Boolean,
        crate::heap::ArrayElementType::Byte,
        crate::heap::ArrayElementType::Char,
        crate::heap::ArrayElementType::Short,
        crate::heap::ArrayElementType::Int,
        crate::heap::ArrayElementType::Float,
        crate::heap::ArrayElementType::Long,
        crate::heap::ArrayElementType::Double,
        crate::heap::ArrayElementType::Reference,
    ];
    for et in &types {
        let ptr = alloc.alloc_array(cid, *et, 10);
        assert!(ptr.is_some(), "Failed to alloc array of type {:?}", et);
        let ptr = ptr.unwrap();
        let header = alloc.read_header(ptr);
        assert!(header.is_array());
        assert_eq!(header.element_type(), *et as u8);
        assert_eq!(alloc.read_array_length(ptr), 10);
    }
}

// -- 54.8: Klass table + compact header roundtrip --

#[test]
fn s54_klass_roundtrip_through_header() {
    let alloc = CompactAllocator::new(4096);
    let cid = cratonvm_types::ClassId::new(12345);
    let ptr = alloc.alloc_object(cid, 0).unwrap();
    let header = alloc.read_header(ptr);
    let nk = header.narrow_klass();
    let resolved = alloc.klass_table().resolve(nk);
    assert_eq!(resolved, Some(cid));
}

// -- 54.9: Large allocation stress --

#[test]
fn s54_compact_allocator_stress() {
    let alloc = CompactAllocator::new(16 * 1024 * 1024); // 16 MB
    let mut ptrs = Vec::new();
    for i in 0..10_000 {
        let cid = cratonvm_types::ClassId::new(i % 100);
        if let Some(ptr) = alloc.alloc_object(cid, 2) {
            ptrs.push(ptr);
        } else {
            break;
        }
    }
    assert_eq!(ptrs.len(), 10_000);
    assert_eq!(alloc.object_count(), 10_000);
    assert_eq!(alloc.bytes_saved(), 10_000 * LEGACY_MINUS_COMPACT_HEADER);

    // Verify all objects have correct class IDs
    for (i, ptr) in ptrs.iter().enumerate() {
        let cid = alloc.class_id_at(*ptr);
        assert_eq!(cid, Some(cratonvm_types::ClassId::new(i as u32 % 100)));
    }
}

// -- 54.10: Memory layout verification --

#[test]
fn s54_compact_object_layout_size() {
    // Object with 2 fields: 8 (header) + 2*16 (slots) = 40, aligned to 8
    let alloc = CompactAllocator::new(4096);
    let p1 = alloc
        .alloc_object(cratonvm_types::ClassId::new(1), 2)
        .unwrap();
    let p2 = alloc
        .alloc_object(cratonvm_types::ClassId::new(1), 2)
        .unwrap();
    let diff = (p2 as usize) - (p1 as usize);
    // 8 + 2*16 = 40, 8-byte aligned
    assert_eq!(diff, 40);
}

#[test]
fn s54_compact_array_layout_size() {
    let alloc = CompactAllocator::new(4096);
    // int[5]: 8 (header) + 8 (length+pad) + 5*4 = 36, aligned to 8 → 40
    let p1 = alloc
        .alloc_array(
            cratonvm_types::ClassId::new(1),
            crate::heap::ArrayElementType::Int,
            5,
        )
        .unwrap();
    let p2 = alloc
        .alloc_array(
            cratonvm_types::ClassId::new(1),
            crate::heap::ArrayElementType::Int,
            5,
        )
        .unwrap();
    let diff = (p2 as usize) - (p1 as usize);
    // 8 + 8 + ceil(20/8)*8 = 8 + 8 + 24 = 40
    assert_eq!(diff, 40);
}

/// T10.9.B — smoke test: HashCodeTable and NarrowKlassTable use FxHashMap.
/// Verifies insert/lookup works after the HashMap → FxHashMap swap.
#[test]
fn t10_9_b_fx_hashmap_swap_smoke() {
    // HashCodeTable insertion
    let tbl = HashCodeTable::new();
    let h1 = tbl.get_or_assign(0x1000);
    let h2 = tbl.get_or_assign(0x2000);
    let h1_again = tbl.get_or_assign(0x1000);
    assert_eq!(h1, h1_again, "same address returns same hash");
    assert_ne!(h1, h2, "different addresses get different hashes");

    // NarrowKlassTable round-trip
    let nk = NarrowKlassTable::new();
    let id1 = nk.get_or_assign(cratonvm_types::ClassId::new(42));
    let id2 = nk.get_or_assign(cratonvm_types::ClassId::new(43));
    let id1_again = nk.get_or_assign(cratonvm_types::ClassId::new(42));
    assert_eq!(id1, id1_again, "same ClassId returns same narrow id");
    assert_ne!(id1, id2, "different ClassId gets distinct narrow id");
}
