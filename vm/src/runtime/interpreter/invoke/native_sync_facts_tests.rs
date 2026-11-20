use super::{ClassId, NativeSyncAnswer, NativeSyncEpochs, NativeSyncFacts};

/// Each kind of word is checked against its own counter only (i11-L5,
/// wave 12): a found answer survives a move of the definition-epoch sum,
/// an unresolved one survives a move of the name-generation sum, and each
/// goes stale when its own counter moves. `0` is never a hit.
#[test]
fn each_answer_kind_is_tagged_by_its_own_counter() {
    let epochs = NativeSyncEpochs {
        found: 7,
        unresolved: 40,
    };
    let declaring = ClassId::new(123);
    let cases = [
        (NativeSyncAnswer::Found(None), None),
        (
            NativeSyncAnswer::Found(Some((false, declaring))),
            Some((false, declaring)),
        ),
        (
            NativeSyncAnswer::Found(Some((true, declaring))),
            Some((true, declaring)),
        ),
    ];
    for (answer, fact) in cases {
        let word = NativeSyncFacts::encode(answer, epochs);
        assert_ne!(word, 0);
        assert_eq!(NativeSyncFacts::decode(word, || 7, || 41), Some(fact));
        assert_eq!(NativeSyncFacts::decode(word, || 8, || 40), None);
        // The tag is 30 bits: the counter compares modulo 2^30.
        assert_eq!(
            NativeSyncFacts::decode(word, || 7 + (1 << 30), || 0),
            Some(fact)
        );
    }
    let unresolved = NativeSyncFacts::encode(NativeSyncAnswer::Unresolved, epochs);
    assert_ne!(unresolved, 0, "an unresolved word is not 'never filled'");
    assert_eq!(NativeSyncFacts::decode(unresolved, || 8, || 40), Some(None));
    assert_eq!(NativeSyncFacts::decode(unresolved, || 7, || 41), None);
    // An unresolved answer filled at counter 0 is still distinguishable
    // from the empty word.
    let at_zero = NativeSyncFacts::encode(
        NativeSyncAnswer::Unresolved,
        NativeSyncEpochs {
            found: 0,
            unresolved: 0,
        },
    );
    assert_eq!(NativeSyncFacts::decode(at_zero, || 0, || 0), Some(None));
    assert_eq!(NativeSyncFacts::decode(0, || 0, || 0), None);
}
