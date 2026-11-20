use super::*;

#[test]
fn membership_tracks_resize_and_cross_thread_free() {
    let _layout = arena_layout_test_lock();
    let store = unsafe_arena::store();
    let a = store.try_allocate(64).expect("block");
    let tail = a + 63;
    assert!(!unsafe_arena_names_block(0));
    assert_eq!(unsafe_arena_names_block(-8), !store.real_pointers());
    for _ in 0..3 {
        assert!(unsafe_arena_names_block(tail));
    }
    assert_eq!(store.try_reallocate(a, 32), Some(a));
    assert_eq!(unsafe_arena_names_block(tail), !store.real_pointers());
    assert_eq!(store.try_reallocate(a, 64), Some(a));
    assert!(
        unsafe_arena_names_block(tail),
        "a negative lookup must not stick"
    );
    std::thread::spawn(move || unsafe_arena::store().free(a))
        .join()
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            unsafe_arena_names_block(tail),
            !store.real_pointers(),
            "real addresses lose membership; handles retain their tag"
        );
    }
}
