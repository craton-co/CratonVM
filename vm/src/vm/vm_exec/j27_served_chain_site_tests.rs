//! JIT round j27 wave 4, lane MHROWS2: a noted chain is taken only by the
//! raise of its class at the call it was noted at.
use super::{served_chain_for_raise, NotedServedRaiseChain, ServedRaiseCallSite};
use cratonvm_native_api::ServedRaiseChain;

const CCE: &str = "java/lang/ClassCastException";

fn noted(depth: usize, pc: usize) -> Option<Box<NotedServedRaiseChain>> {
    Some(Box::new(NotedServedRaiseChain {
        chain: Box::new(ServedRaiseChain {
            throwable_class: CCE,
            hops: Vec::new(),
        }),
        site: ServedRaiseCallSite { depth, pc: Some(pc) },
    }))
}

fn at(depth: usize, pc: usize) -> ServedRaiseCallSite {
    ServedRaiseCallSite { depth, pc: Some(pc) }
}

#[test]
fn the_raise_at_the_noting_call_takes_its_chain() {
    let taken = served_chain_for_raise(noted(3, 17), CCE, at(3, 17), true);
    assert_eq!(taken.map(|c| c.throwable_class), Some(CCE));
}

/// The stale case: the noting native returned without the funnel's raise;
/// a later `ClassCastException` at another call must not list its frames.
#[test]
fn a_raise_at_another_call_drops_a_stale_chain() {
    assert!(served_chain_for_raise(noted(3, 17), CCE, at(3, 21), true).is_none());
    assert!(served_chain_for_raise(noted(3, 17), CCE, at(4, 17), true).is_none());
    // `CRATONVM_SERVED_RAISE_CHAIN_SITE=0`: the pre-j27 rule.
    assert!(served_chain_for_raise(noted(3, 17), CCE, at(3, 21), false).is_some());
}

#[test]
fn a_raise_of_another_class_drops_the_chain() {
    let npe = "java/lang/NullPointerException";
    assert!(served_chain_for_raise(noted(3, 17), npe, at(3, 17), true).is_none());
    assert!(served_chain_for_raise(noted(3, 17), npe, at(3, 17), false).is_none());
    assert!(served_chain_for_raise(None, CCE, at(3, 17), true).is_none());
}
