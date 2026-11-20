use super::split_post_init_fields;
use cratonvm_types::JIT_POST_TLAB_INIT_REGISTERED as REGISTERED;

/// A bare count passes through unflagged; a flagged one loses only bit 30.
/// The count includes inherited slots, so the cases go past the u16 range,
/// all far below bit 30.
#[test]
fn the_register_first_bit_splits_off_and_leaves_the_count() {
    for n in [0i64, 1, 3, 255, 65_535, 65_536, (1 << 29) - 1] {
        assert_eq!(split_post_init_fields(n), (n, false), "bare {n}");
        assert_eq!(split_post_init_fields(n | REGISTERED), (n, true), "flagged {n}");
    }
    assert_eq!(REGISTERED, 1 << 30, "the bit the emitters set");
}
