//! Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): an arena handle
//! passed to C is the block's real storage, and a pointer C hands back
//! into that block is the handle again, for exactly the call's span.
use super::*;

#[test]
fn a_pointer_into_a_passed_block_maps_back_to_its_handle_inside_the_call_only() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let handle = crate::unsafe_natives_ext::unsafe_arena_try_allocate(64).expect("block");
    let real = {
        let _call = ArenaCallScope::open();
        let real = arena_call_address(handle + 8, 16).expect("live block");
        assert!(!is_arena_handle(real), "C gets a machine address");
        assert_eq!(arena_call_pointer_to_handle(real), handle + 8);
        assert_eq!(arena_call_pointer_to_handle(real + 4), handle + 12);
        // One past the end of the block still names it (`strchr` of "").
        assert_eq!(arena_call_pointer_to_handle(real + 56), handle + 64);
        // Not in any passed block: unchanged.
        assert_eq!(arena_call_pointer_to_handle(0x1000), 0x1000);
        {
            let _nested = ArenaCallScope::open();
            assert_eq!(
                arena_call_pointer_to_handle(real),
                handle + 8,
                "outer blocks survive"
            );
        }
        assert_eq!(arena_call_pointer_to_handle(real), handle + 8);
        real
    };
    // After the call the block is still live, so its real address still
    // names it (a pointer read back out of memory); once freed, it does not.
    assert_eq!(arena_call_pointer_to_handle(real), handle + 8, "still live");
    // A block that cannot cover the length, and a freed handle, refuse.
    assert!(arena_call_address(handle + 60, 16).is_none());
    assert!(
        arena_call_address(handle + 64, 0).is_some(),
        "zero length at the end"
    );
    crate::unsafe_natives_ext::unsafe_arena_free(handle);
    assert!(arena_call_address(handle, 1).is_none());
    // A freed block's handle never comes back (its memory may already be
    // another live block's, which then names it: handles are never reused).
    // The translations above made outside any call recorded nothing.
    assert_ne!(arena_call_pointer_to_handle(real), handle + 8, "a freed block");
}

#[test]
fn an_in_vm_segment_borrow_does_not_leak_a_native_translation() {
    let handle = crate::unsafe_natives_ext::unsafe_arena_try_allocate(16).expect("block");
    assert!(crate::unsafe_natives_ext::unsafe_arena_copy_in(
        handle,
        &[0x5a; 16]
    ));
    let before = crate::unsafe_natives_ext::unsafe_arena_translation_stats().translations;
    let real = borrowed_host_base_address(handle, 16);
    assert_ne!(real, 0, "live block resolves to its storage");
    // SAFETY: `handle` stays live through this synchronous access, and
    // `borrowed_host_base_address` checked the complete 16-byte range.
    assert_eq!(
        unsafe { std::slice::from_raw_parts(real as *const u8, 16) },
        &[0x5a; 16]
    );
    assert_eq!(
        crate::unsafe_natives_ext::unsafe_arena_translation_stats().translations,
        before,
        "an in-VM borrow is not a native pointer publication"
    );
    crate::unsafe_natives_ext::unsafe_arena_free(handle);
}
