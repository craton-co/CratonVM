use super::*;
use crate::config::{GcAlgorithm, VmConfig};


/// Source pin: the drain point takes the marking door under the rule, a
/// refused build ends the drain for good, and the re-drain comes only
/// after the drain is let go (`end_drain_take_missed` consumes it).
#[test]
fn zt13x10_the_drain_marks_and_redrains_only_after_letting_go() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = src
        .find("pub(super) fn run_gc_notifications(")
        .expect("the drain");
    let end = start + src[start..].find("\n}\n").expect("its end");
    let body = &src[start..end];
    let mark = body
        .find("queue.begin_drain_noting_miss()")
        .expect("the marking door");
    let refused = body.find("refused_any = true;").expect("the refusal");
    let take = body
        .find("queue.end_drain_take_missed(drain)")
        .expect("let go, then read the mark");
    let again = body
        .find("run_gc_notifications_after_miss(shared, thread, forced)")
        .expect("the re-drain");
    assert!(mark < refused && refused < take && take < again);
}
