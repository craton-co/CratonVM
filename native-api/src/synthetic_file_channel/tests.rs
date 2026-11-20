use super::*;

/// The width asked of the allocator must always leave room for both private
/// slots, whatever the base. Guards the shape that cost a build in
/// `socket_channel.rs` (`F_REUSEADDR = 11` with `N_FIELDS = 11`): an index
/// outside the requested width is dropped on write and reads back as the
/// zero value, which is indistinguishable from "never set".
#[test]
fn the_requested_width_covers_every_private_slot() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    let slots = alloc_slots(&mut ctx);
    assert!(slots > PRIVATE_FD, "fd slot outside requested width");
    assert!(slots > PRIVATE_POS, "position slot outside requested width");
    assert_eq!(
        slots,
        base_for_class(&mut ctx, CLASS) + PRIVATE_WIDTH,
        "allocator width and accessor base must be derived from one function"
    );
}
