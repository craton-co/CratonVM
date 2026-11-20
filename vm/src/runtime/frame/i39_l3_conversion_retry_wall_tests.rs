use super::FrameStack;

/// Interpreter round i1 wave 39, lane L3: past the yields, a deferred
/// conversion's retries stop at the wall bound whatever the count; the
/// yields are bounded by the count only; a cleared conversion starts its
/// clock again.
#[test]
fn a_deferred_conversions_retries_are_bounded_in_wall_time() {
    let mut stack = FrameStack::new();
    assert!(stack.count_conversion_retry(), "the first retry");
    assert_eq!(stack.conversion_retries(), 1);
    stack.cold_mut().conversion_retries = FrameStack::CONVERSION_RETRY_YIELDS;
    assert!(
        stack.count_conversion_retry(),
        "just past the yields, in time"
    );
    let Some(long_ago) =
        std::time::Instant::now().checked_sub(FrameStack::MAX_CONVERSION_RETRY_WALL * 2)
    else {
        return;
    };
    stack.cold_mut().conversion_retry_began = Some(long_ago);
    assert!(!stack.count_conversion_retry(), "over the wall bound");
    assert!(stack.conversion_retry_elapsed() > FrameStack::MAX_CONVERSION_RETRY_WALL);
    stack.clear_conversion_pending();
    assert_eq!(stack.conversion_retry_elapsed(), std::time::Duration::ZERO);
    stack.cold_mut().conversion_retry_began = Some(long_ago);
    assert!(
        stack.count_conversion_retry(),
        "a yielding retry is bounded by the count only"
    );
}
