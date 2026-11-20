use super::*;

#[test]
fn an_event_array_is_reused_across_waits_and_never_aliased() {
    // A fresh thread, so the cell starts empty whatever ran before.
    std::thread::spawn(|| {
        let first = epoll_events_take(1024);
        assert!(first.buf.len() >= 1024);
        let ptr = first.buf.as_ptr();
        // A nested wait while the first holds its array gets its own.
        let nested = epoll_events_take(16);
        assert_ne!(nested.buf.as_ptr(), ptr);
        drop(nested);
        drop(first);
        // The larger array came back and serves the next, smaller, wait.
        let again = epoll_events_take(8);
        assert_eq!(again.buf.as_ptr(), ptr);
        assert!(again.buf.len() >= 8);
        drop(again);
        // A zero-length ask is still one entry (`epoll_wait` needs >= 1).
        assert!(!epoll_events_take(0).buf.is_empty());
        // An oversized array is served and not kept: the cell still
        // holds the 1024-entry one.
        let big = epoll_events_take(EPOLL_EVENTS_KEEP_MAX + 1);
        assert!(big.buf.len() > EPOLL_EVENTS_KEEP_MAX);
        drop(big);
        let after = epoll_events_take(8);
        assert!(after.buf.capacity() <= EPOLL_EVENTS_KEEP_MAX);
        assert_eq!(after.buf.as_ptr(), ptr);
    })
    .join()
    .expect("lease thread");
}
