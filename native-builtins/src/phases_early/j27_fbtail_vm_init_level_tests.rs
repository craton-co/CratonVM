use super::vm_init_level_answer;

#[test]
fn the_jdk_static_is_the_answer_whatever_the_boot_level() {
    // HotSpot answers the static: 0 and 1 during boot, 4 at `main`.
    for (jdk, live) in [(0, 4), (1, 2), (1, 3), (4, 4), (4, 1)] {
        assert_eq!(vm_init_level_answer(Some(jdk), live), jdk, "static {jdk}, live {live}");
    }
}

#[test]
fn without_a_static_the_floor_of_two_stays() {
    assert_eq!(vm_init_level_answer(None, 0), 2);
    assert_eq!(vm_init_level_answer(None, 1), 2);
    assert_eq!(vm_init_level_answer(None, 3), 3);
    assert_eq!(vm_init_level_answer(None, 4), 4);
}
