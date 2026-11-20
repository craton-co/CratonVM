use super::*;

/// [`getfield_index_of`] inverts [`getfield_index_arg`] for every flag
/// combination AND every trap-site key, and the flag mask covers exactly
/// the three passengers.
///
/// The pair exists because a DECODE site that misses a flag does not fail a
/// bounds check — it overflows `idx * SLOT_SIZE`. Two test doubles have
/// already done exactly that; see [`getfield_index_of`].
#[test]
fn the_index_argument_round_trips_through_every_flag_combination() {
    for slot in [0u32, 1, 7, u16::MAX as u32] {
        for is_ref in [false, true] {
            for proven in [false, true] {
                for key in [0u32, 1, 0x00ff_ffff] {
                    let arg = getfield_index_arg(slot, is_ref, proven, key);
                    assert_eq!(
                        getfield_index_of(arg as i64),
                        i64::from(slot),
                        "slot {slot} (is_ref={is_ref}, proven={proven}, key={key})"
                    );
                    assert_eq!(
                        getfield_npe_site_of(arg as i64),
                        key,
                        "the trap-site key must survive beside the flags"
                    );
                    assert_eq!(
                        arg & GETFIELD_EXPECT_REFERENCE != 0,
                        is_ref,
                        "the reference flag must ride only on the reference path"
                    );
                }
            }
        }
    }
}

/// A key wider than its field is TRUNCATED, never allowed to spill into the
/// slot index. `record_npe_trap_site` refuses to issue one, so this pins the
/// second line of defence rather than a reachable case.
#[test]
fn an_oversized_trap_key_cannot_reach_the_slot_index() {
    let arg = getfield_index_arg(7, false, false, u32::MAX);
    assert_eq!(getfield_index_of(arg as i64), 7);
    assert_eq!(getfield_npe_site_of(arg as i64), 0x00ff_ffff);
}

/// A real slot cannot reach either flag bit, which is the property that
/// lets them share the argument at all.
#[test]
fn a_class_file_slot_index_cannot_reach_the_flag_bits() {
    // A class-file field table is `u16`-sized.
    let widest = getfield_index_arg(u16::MAX as u32, false, false, 0);
    assert_eq!(widest & GETFIELD_FLAG_BITS, 0);
    assert_eq!(
        GETFIELD_FLAG_BITS,
        GETFIELD_RECEIVER_PROVEN_OOP
            | GETFIELD_EXPECT_REFERENCE
            | GETFIELD_NPE_SITE_MASK
            | GETFIELD_IMPLICIT_NULL_SITE
            | GETFIELD_NO_KEEPALIVE,
        "a sixth passenger must join the mask, or every decoder silently keeps it"
    );
}

/// t1 wave 10 (lane REFS3): the compare-only referent mark is a stripped
/// flag of its own, disjoint from every other passenger, and the encoder
/// never sets it.
#[test]
fn t1w10_refs3_the_no_keepalive_mark_is_a_stripped_flag_of_its_own() {
    for other in [
        GETFIELD_RECEIVER_PROVEN_OOP,
        GETFIELD_EXPECT_REFERENCE,
        GETFIELD_NPE_SITE_MASK,
        GETFIELD_IMPLICIT_NULL_SITE,
    ] {
        assert_eq!(GETFIELD_NO_KEEPALIVE & other, 0);
    }
    let arg = getfield_index_arg(u16::MAX as u32, true, true, 0x00ff_ffff);
    assert!(
        !getfield_is_no_keepalive(arg as i64),
        "the encoder never sets it"
    );
    let marked = arg | GETFIELD_NO_KEEPALIVE;
    assert!(getfield_is_no_keepalive(marked as i64));
    assert_eq!(getfield_index_of(marked as i64), i64::from(u16::MAX));
    assert_eq!(getfield_npe_site_of(marked as i64), 0x00ff_ffff);
}

/// The implicit-null-site mark (L1W19-2) is stripped with the other flags
/// and overlaps none of them.
#[test]
fn the_implicit_null_site_mark_is_a_stripped_flag_of_its_own() {
    assert_eq!(
        GETFIELD_IMPLICIT_NULL_SITE & GETFIELD_RECEIVER_PROVEN_OOP,
        0
    );
    assert_eq!(GETFIELD_IMPLICIT_NULL_SITE & GETFIELD_EXPECT_REFERENCE, 0);
    assert_eq!(GETFIELD_IMPLICIT_NULL_SITE & GETFIELD_NPE_SITE_MASK, 0);
    let arg = getfield_index_arg(u16::MAX as u32, true, false, 0x00ff_ffff)
        | GETFIELD_IMPLICIT_NULL_SITE;
    assert!(getfield_is_implicit_null_site(arg as i64));
    assert_eq!(getfield_index_of(arg as i64), i64::from(u16::MAX));
    assert_eq!(getfield_npe_site_of(arg as i64), 0x00ff_ffff);
    assert!(!getfield_is_implicit_null_site(
        getfield_index_arg(7, true, true, 9) as i64
    ));
}
