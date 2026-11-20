use super::method_admits_chains;
use std::cell::Cell;

fn ask(is_synchronized: bool, self_lock: bool, switch_on: bool) -> (bool, u32) {
    let reads = Cell::new(0);
    let admitted = method_admits_chains(is_synchronized, self_lock, || {
        reads.set(reads.get() + 1);
        switch_on
    });
    (admitted, reads.get())
}

/// Plain methods always plan chains and never read the switch; a
/// synchronized one only with the switch on and not self-locking.
#[test]
fn only_a_synchronized_method_asks_the_census_switch() {
    assert_eq!(ask(false, false, false), (true, 0));
    assert_eq!(ask(false, true, false), (true, 0));
    assert_eq!(ask(true, false, false), (false, 1));
    assert_eq!(ask(true, false, true), (true, 1));
    assert_eq!(ask(true, true, true), (false, 0));
}
