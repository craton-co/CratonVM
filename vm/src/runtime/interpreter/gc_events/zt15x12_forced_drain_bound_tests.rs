use super::*;
use crate::config::{GcAlgorithm, VmConfig};


/// Source pin: the budget is read after the drain is won and before the
/// first pop, the pop loop stops at it, and the hand-off of the rest
/// comes after the drain is let go and before the re-drain.
#[test]
fn zt15x12_the_bounded_drain_hands_the_rest_off_after_letting_go() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    let start = src
        .find("pub(super) fn run_gc_notifications(")
        .expect("the drain");
    let end = start + src[start..].find("\n}\n").expect("its end");
    let body = &src[start..end];
    let won = body.find("let Some(drain) = drain else").expect("the drain won");
    let budget = body
        .find(".then(|| queue.pending_notifications())")
        .expect("the budget");
    let stop = body.find("if budget == Some(0)").expect("the stop");
    let first_pop = body.find("drain.pop()").expect("the pop");
    let let_go = body
        .find("queue.end_drain_take_missed(drain)")
        .expect("let go");
    let dropped = body.find("drop(drain);").expect("let go (rule off)");
    let hand = body
        .find("forced_drain_hands_rest_to_delivery_thread(shared, thread)")
        .expect("the hand-off");
    let again = body
        .find("run_gc_notifications_after_miss(shared, thread, forced)")
        .expect("the re-drain");
    assert!(won < budget && budget < stop && stop < first_pop);
    assert!(let_go < hand && dropped < hand && hand < again);
}
