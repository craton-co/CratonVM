use super::*;

#[test]
fn the_word_round_trips_and_the_unlean_words_are_historical() {
    for n in [0usize, 1, 2, 7, 255] {
        // Cast: a test count.
        assert_eq!(service_args_word(Some(n), false), n as i64);
        assert_eq!(decode_service_args_word(n as i64), (n as i64, false));
        let w = service_args_word(Some(n), true);
        assert!(w > 0);
        assert_eq!(decode_service_args_word(w), (n as i64, true));
    }
    let owned = i64::from(SERVICE_ARGS_OWNED_BY_CALLEE);
    assert_eq!(service_args_word(None, false), owned);
    assert_eq!(decode_service_args_word(owned), (owned, false));
    let w = service_args_word(None, true);
    assert!(w > 0, "the lean owned word is a positive word");
    assert_eq!(decode_service_args_word(w), (owned, true));
    // THROWS3-3's IR word (count | bit 40) decodes as before.
    // Cast: a test count plus bit 40.
    let ir = (3u64 | SERVICE_ARGS_LEAN_PROPAGATE_BIT) as i64;
    assert_eq!(decode_service_args_word(ir), (3, true));
    assert!(service_lean_kind_admits(1));
    assert!(service_lean_kind_admits(3));
    assert!(service_lean_kind_admits(4));
    assert!(!service_lean_kind_admits(0));
    assert!(!service_lean_kind_admits(2));
}
