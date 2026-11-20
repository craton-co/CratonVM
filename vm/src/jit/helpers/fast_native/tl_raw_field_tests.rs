/// nio21 wave 17 L2: the raw readers' word screen. Null is a null field;
/// an unaligned or out-of-range word declines (the bytecode decides)
/// rather than being dereferenced for the wrapper test.
#[test]
fn tl_checked_ref_screens_null_and_implausible_words() {
    // SAFETY: none of these words is dereferenced: 0 returns before the
    // header read and the others fail `plausible_heap_pointer` first.
    unsafe {
        assert!(matches!(super::tl_checked_ref(0), Some(None)));
        assert!(super::tl_checked_ref(0x7).is_none());
        assert!(super::tl_checked_ref(0x1001).is_none());
        assert!(super::tl_checked_ref(1u64 << 60).is_none());
    }
}
