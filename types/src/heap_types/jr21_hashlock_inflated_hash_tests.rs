//! JIT round 21, lane hashlock: an INFLATED compact word may carry the
//! identity-hash bits its NEUTRAL word had, in the same places.
use super::*;
use std::sync::atomic::Ordering;

fn compact(class_id: u32) -> ObjectHeader {
    let mut h = ObjectHeader::new(
        ClassId::new(class_id),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        3,
    );
    h.set_compact_shape(3, 24);
    assert!(h.is_short());
    h
}

#[test]
fn keeping_inflation_carries_the_bits_and_the_value() {
    for mint in [1i32, 0x3FFF, 0x4000, 0xF_FFFF, 0x10_0000, -5] {
        let c = compact(11);
        let want = c.identity_hash(|| mint).unwrap();
        assert_eq!(ObjectHeader::short_hash_from_mint(11, mint), want);
        let m = c.mark();
        let inflated = ObjectHeader::make_inflated_keeping_hash(m);
        assert_eq!(ObjectHeader::mark_state(inflated), MARK_INFLATED);
        assert_eq!(
            ObjectHeader::quartet_of(inflated),
            ObjectHeader::quartet_of(m)
        );
        assert_eq!(
            ObjectHeader::inflated_hash_bits(inflated),
            ObjectHeader::short_hash_bits(m)
        );
        // A NEUTRAL-only decoder still sees no hash in an INFLATED word.
        assert_eq!(ObjectHeader::short_hash_bits(inflated), 0);
        assert!(!ObjectHeader::thin_lockable(inflated));
        c.mark_word.store(inflated, Ordering::Relaxed);
        assert_eq!(c.identity_hash(|| 77), Ok(want));
    }
}

#[test]
fn a_bare_inflated_word_still_declines_and_other_shapes_are_unchanged() {
    let c = compact(5);
    let m = c.mark();
    c.mark_word
        .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
    assert_eq!(c.identity_hash(|| 3), Err(()));
    // Unhashed NEUTRAL, thin-locked and legacy words: exactly `make_inflated`.
    assert_eq!(
        ObjectHeader::make_inflated_keeping_hash(m),
        ObjectHeader::make_inflated(m)
    );
    let thin = ObjectHeader::make_thin_locked(m, 9, 2);
    assert_eq!(
        ObjectHeader::make_inflated_keeping_hash(thin),
        ObjectHeader::make_inflated(thin)
    );
    assert_eq!(ObjectHeader::inflated_hash_bits(thin), 0);
    let legacy = ObjectHeader::new(
        ClassId::new(5),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        1,
    );
    let lm = legacy.mark();
    assert_eq!(
        ObjectHeader::make_inflated_keeping_hash(lm),
        ObjectHeader::make_inflated(lm)
    );
    // A forwarded word never answers a hash from its payload.
    assert_eq!(
        ObjectHeader::inflated_hash_bits(ObjectHeader::quartet_of(m) | MARK_FORWARDED),
        0
    );
}

/// HASHLOCK21-2 (tag `hl21`): `installed_identity_hash` reports what
/// `identity_hash` answers `Ok` with, in every compact lock state: the
/// NEUTRAL bits, the bits an INFLATED word kept, and 0 for a thin-locked
/// word, a bare INFLATED word and an unhashed one.
#[test]
fn hl21_installed_identity_hash_reports_the_inflated_bits() {
    for (cid, mint) in [
        (11u32, 1i32),
        (23, 0x3FFF),
        (900_777, 0x4000),
        (7, 0xF_FFFF),
    ] {
        let c = compact(cid);
        assert_eq!(c.installed_identity_hash(), 0, "unhashed");
        let want = c.identity_hash(|| mint).unwrap();
        assert_eq!(c.installed_identity_hash(), want, "NEUTRAL");
        let m = c.mark();
        c.mark_word.store(
            ObjectHeader::make_inflated_keeping_hash(m),
            Ordering::Relaxed,
        );
        assert_eq!(c.installed_identity_hash(), want, "INFLATED keeping bits");
        assert_eq!(c.identity_hash(|| 77), Ok(want), "same as identity_hash");
        c.mark_word
            .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
        assert_eq!(
            c.installed_identity_hash(),
            0,
            "bare INFLATED: in the monitor"
        );
        let unhashed = compact(cid);
        let um = unhashed.mark();
        unhashed
            .mark_word
            .store(ObjectHeader::make_thin_locked(um, 4, 0), Ordering::Relaxed);
        assert_eq!(unhashed.installed_identity_hash(), 0, "thin-locked");
        // A word published after the fact (a hash minted while locked).
        unhashed
            .mark_word
            .store(ObjectHeader::make_inflated(um), Ordering::Relaxed);
        let minted = ObjectHeader::short_hash_from_mint(cid, mint);
        assert!(unhashed.publish_inflated_hash(minted));
        assert_eq!(unhashed.installed_identity_hash(), minted, "published");
    }
    // A long header keeps answering its aux word in any lock state.
    let legacy = ObjectHeader::new(
        ClassId::new(5),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        1,
    );
    let want = legacy.identity_hash(|| 0x1234).unwrap();
    let lm = legacy.mark();
    legacy
        .mark_word
        .store(ObjectHeader::make_inflated(lm), Ordering::Relaxed);
    assert_eq!(legacy.installed_identity_hash(), want);
}

#[test]
fn publish_records_only_a_short_form_hash_once() {
    let c = compact(23);
    let m = c.mark();
    // Not INFLATED: declines.
    let h = ObjectHeader::short_hash_from_mint(23, 0x0004_2421);
    assert!(!c.publish_inflated_hash(h));
    c.mark_word
        .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
    // Not of the short form for this class: declines, word untouched.
    let foreign = h ^ (1 << 30);
    assert!(!c.publish_inflated_hash(foreign));
    assert_eq!(c.mark(), ObjectHeader::make_inflated(m));
    assert!(c.publish_inflated_hash(h));
    assert!(c.publish_inflated_hash(h), "idempotent");
    assert_eq!(c.identity_hash(|| 1), Ok(h));
    assert_eq!(
        ObjectHeader::quartet_of(c.mark()),
        ObjectHeader::quartet_of(m),
        "the quartet survives"
    );
    // A different short-form value never overwrites the recorded one.
    let other = ObjectHeader::short_hash_from_mint(23, 0x0004_2422);
    assert!(!c.publish_inflated_hash(other));
    assert_eq!(c.identity_hash(|| 1), Ok(h));
    // Zero low bits are forced to 1, as `make_neutral_hashed` does.
    assert_eq!(
        ObjectHeader::short_hash_from_mint(23, 0x0010_0000),
        ObjectHeader::short_hash_value(23, 1)
    );
}
