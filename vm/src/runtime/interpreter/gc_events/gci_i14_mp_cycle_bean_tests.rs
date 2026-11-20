use super::*;
use cratonvm_gc::gc_metrics::{GcNotificationRecord, JMX_G1_CONCURRENT_COLLECTOR};

fn rec(collector: &'static str, id: u64, start: std::time::Instant) -> GcNotificationRecord {
    GcNotificationRecord {
        collector,
        action: "end of concurrent GC pause",
        cause: "No GC",
        id,
        start,
        end: start + std::time::Duration::from_millis(10),
        timestamp_ms: 0,
        before: Vec::new(),
        after: Vec::new(),
    }
}

#[test]
fn gci_i14_mp_the_initial_mark_books_no_bean_under_the_opt_in() {
    let p = NonCollectionPause::G1InitialMark;
    assert!(non_collection_pause_books_a_bean(p, || false), "default: as before");
    assert!(!non_collection_pause_books_a_bean(p, || true), "opt-in: HotSpot's young pause counted it");
    // The remark keeps its bean either way; the flag is not even asked.
    let r = NonCollectionPause::G1Remark;
    assert!(non_collection_pause_books_a_bean(r, || panic!("not asked for the remark")));
    // A pause with no bean books none.
    assert!(!non_collection_pause_books_a_bean(NonCollectionPause::HeapDump, || true));
}

#[test]
fn gci_i14_mp_a_remark_and_cleanup_are_two_notifications() {
    let t0 = std::time::Instant::now();
    let cut = t0 + std::time::Duration::from_millis(6);
    let out = split_concurrent_remark_records(vec![rec(JMX_G1_CONCURRENT_COLLECTOR, 4, t0)], cut);
    assert_eq!(out.len(), 2);
    assert_eq!((out[0].id, out[1].id), (3, 4), "Remark then Cleanup");
    assert_eq!(out[0].end, cut);
    assert_eq!(out[1].start, cut);
    assert_eq!(out[1].end, t0 + std::time::Duration::from_millis(10));
    // Another bean's record, or a count that cannot hold two, passes through.
    let young = split_concurrent_remark_records(vec![rec("G1 Young Generation", 7, t0)], cut);
    assert_eq!(young.len(), 1);
    let one = split_concurrent_remark_records(vec![rec(JMX_G1_CONCURRENT_COLLECTOR, 1, t0)], cut);
    assert_eq!(one.len(), 1);
    // A cut past the pause is clamped into it.
    let late = t0 + std::time::Duration::from_secs(1);
    let out = split_concurrent_remark_records(vec![rec(JMX_G1_CONCURRENT_COLLECTOR, 2, t0)], late);
    assert_eq!(out[0].end, out[1].end);
}
