use super::*;


/// Source pin: inside `record_gc_notification` the refresh comes after
/// the bean snapshot and before the first `queue.record`, behind the
/// "a listener waits" test.
#[test]
fn zt13x10_the_refresh_precedes_the_first_enqueue() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = src
        .find("\nfn record_gc_notification(")
        .expect("record_gc_notification");
    let end = start + 1 + src[start + 1..].find("\n}\n").expect("its end");
    let body = &src[start..end];
    let between = body
        .find("gc_notifications_between(before, after, cause, wall_ms)")
        .expect("the records");
    let gate = body
        .find("recs.iter().any(|r| queue.is_enabled(r.collector))")
        .expect("the listener test");
    let refresh = body
        .find("zgc_perf_refresh_before_enqueue(shared, &recs);")
        .expect("the refresh");
    let enqueue = body.find("queue.record(").expect("the enqueue");
    assert!(between < gate && gate < refresh && refresh < enqueue);
}
