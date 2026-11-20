use super::preceding_aload_nonnull_local;
use crate::null_check_elim::analyze_with_receiver;

/// The two halves the getfield emitter ANDs together, tested together.
///
/// `emit_trusted_oop_receiver_check_at` elides only when
/// `preceding_aload_nonnull_local` names a local AND the dataflow proves
/// it. Testing either alone would miss the pairing -- which is where the
/// bug would be, since the two are indexed by the same bci and derived
/// from the same array.
#[test]
fn the_getfield_receiver_resolves_to_this_and_the_dataflow_proves_it() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2A,               // 0: aload_0
        0xB4, 0x00, 0x01,   // 1: getfield #1
        0xAC,               // 4: ireturn
    ];
    assert_eq!(
        preceding_aload_nonnull_local(&code, 1),
        Some(0),
        "the receiver of a getfield is the value the preceding aload pushed"
    );

    let seeded = analyze_with_receiver(&code, code.len(), true);
    assert!(seeded.is_nonnull(1, 0), "and the seed proves it at bci 1");

    let unseeded = analyze_with_receiver(&code, code.len(), false);
    assert!(
        !unseeded.is_nonnull(1, 0),
        "without the seed the very first `this.field` in a method still \
         pays the check -- this is the arm the seed removes"
    );
}

/// A `getfield` whose receiver did not come from an `aload` is refused
/// outright, so the elision can never be handed a local it did not derive.
/// The chained form `this.a.b` is the common instance: the inner
/// `getfield` pushes the receiver of the outer one.
#[test]
fn a_chained_getfield_receiver_is_not_attributed_to_a_local() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2A,               // 0: aload_0
        0xB4, 0x00, 0x01,   // 1: getfield #1   (this.a)
        0xB4, 0x00, 0x02,   // 4: getfield #2   (.b)
        0xAC,               // 7: ireturn
    ];
    assert_eq!(
        preceding_aload_nonnull_local(&code, 4),
        None,
        "`this.a` is not a local, and its nullness is not local 0's"
    );
}
