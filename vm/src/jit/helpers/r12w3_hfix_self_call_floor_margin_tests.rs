use super::*;

#[test]
fn a_small_stack_keeps_more_than_one_sample_stride_below_the_floor() {
    const K: usize = 1024;
    // The hunter's case: 256 KiB was one stride; now two (half the stack).
    assert_eq!(self_call_floor_margin(256 * K, false), 64 * K);
    assert_eq!(self_call_floor_margin(256 * K, true), 128 * K);
    // Three strides once the stack can spare them.
    assert_eq!(self_call_floor_margin(512 * K, true), 192 * K);
    assert_eq!(self_call_floor_margin(1024 * K, true), 256 * K);
    // Large stacks keep the full 1 MiB headroom either way.
    for size in [4096 * K, 8192 * K, 128 * 1024 * K] {
        assert_eq!(self_call_floor_margin(size, true), 1024 * K);
        assert_eq!(
            self_call_floor_margin(size, true),
            self_call_floor_margin(size, false)
        );
    }
    // Never more than half of any stack.
    for size in [128 * K, 192 * K, 256 * K, 384 * K] {
        assert!(self_call_floor_margin(size, true) <= (size / 2).max(64 * K));
    }
}
