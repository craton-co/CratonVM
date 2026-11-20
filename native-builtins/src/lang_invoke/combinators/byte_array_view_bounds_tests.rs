use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeHeapAccess;

/// `(width, valid start positions)` for a 16-byte array.
const CASES: &[(u8, i32)] = &[
    (b'S', 15),
    (b'C', 15),
    (b'I', 13),
    (b'F', 13),
    (b'J', 9),
    (b'D', 9),
];

#[test]
fn a_negative_index_is_refused_with_the_jdk_message() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    for (elem, limit) in CASES {
        let err = byte_view_check_index(&ctx, arr, -1, *elem)
            .expect_err("a negative index must be refused");
        let text = format!("{err:?}");
        assert!(
            text.contains(&format!("Index -1 out of bounds for length {limit}")),
            "elem {}: {text}",
            *elem as char
        );
    }
}

#[test]
fn the_last_valid_start_is_accepted_and_the_next_one_is_not() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    for (elem, limit) in CASES {
        assert!(
            byte_view_check_index(&ctx, arr, limit - 1, *elem).is_ok(),
            "elem {}: index {} must fit",
            *elem as char,
            limit - 1
        );
        assert!(
            byte_view_check_index(&ctx, arr, *limit, *elem).is_err(),
            "elem {}: index {} must not fit",
            *elem as char,
            limit
        );
    }
}

#[test]
fn an_array_too_short_for_one_element_admits_no_index() {
    let mut ctx = mock_ctx();
    // Four bytes cannot hold a long; HotSpot reports the signed -3 bound.
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let err = byte_view_check_index(&ctx, arr, 0, b'J').expect_err("no index can fit");
    assert!(
        format!("{err:?}").contains("Index 0 out of bounds for length -3"),
        "{err:?}"
    );
}
