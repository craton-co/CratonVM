use super::*;

const A: usize = 0x1000_0000;
const B: usize = 0x5000_0000;
const LEN: usize = 0x4000_0000;
const MASK: usize = !(0x10_0000 - 1);

fn fresh() -> ([AtomicUsize; 5], AtomicUsize) {
    (
        [
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ],
        AtomicUsize::new(0),
    )
}

fn publish(words: &[AtomicUsize; 5], owner: &AtomicUsize, base: usize) {
    publish_jit_g1_barrier_in(words, owner, base, LEN, MASK, base + LEN, 9);
}

fn owned_by(words: &[AtomicUsize; 5], owner: &AtomicUsize, base: usize) -> bool {
    owner.load(Ordering::SeqCst) == base
        && words[0].load(Ordering::SeqCst) == base
        && words[1].load(Ordering::SeqCst) == LEN
        && words[2].load(Ordering::SeqCst) == MASK
        && words[3].load(Ordering::SeqCst) == base + LEN
        && words[4].load(Ordering::SeqCst) == 9
}

/// Not live for a new compile, and the run-time test the emitters make
/// (`((obj - base) ^ (val - base)) & mask`) is zero only for one object.
fn poisoned(words: &[AtomicUsize; 5]) -> bool {
    let base = words[0].load(Ordering::SeqCst);
    let mask = words[2].load(Ordering::SeqCst);
    let same = |o: usize, v: usize| (o.wrapping_sub(base) ^ v.wrapping_sub(base)) & mask == 0;
    words[1].load(Ordering::SeqCst) == 0
        && base == 0
        && mask == JIT_G1_BARRIER_POISON_MASK
        && !same(A + 8, A + 16)
        && !same(A + 0x10_0000 - 8, A + 0x10_0000 + 8)
        && !same(B + 8, A + 8)
        && same(A + 8, A + 8)
}

#[test]
fn a_lone_collector_owns_the_table_and_its_drop_frees_the_claim() {
    let (words, owner) = fresh();
    publish(&words, &owner, A);
    assert!(owned_by(&words, &owner, A));
    // A non-owner's teardown changes nothing.
    clear_jit_g1_barrier_owned_by_in(&words, &owner, B);
    assert!(owned_by(&words, &owner, A));
    clear_jit_g1_barrier_owned_by_in(&words, &owner, A);
    assert!(poisoned(&words), "teardown poisons, never zeroes the mask");
    assert_eq!(owner.load(Ordering::SeqCst), 0, "the claim is free again");
    // A later collector (A gone) publishes normally.
    publish(&words, &owner, B);
    assert!(owned_by(&words, &owner, B));
    clear_jit_g1_barrier_owned_by_in(&words, &owner, B);
    assert!(poisoned(&words));
    assert_eq!(owner.load(Ordering::SeqCst), 0);
}

#[test]
fn two_live_collectors_poison_the_table_for_good_in_both_drop_orders() {
    for a_first in [true, false] {
        let (words, owner) = fresh();
        publish(&words, &owner, A);
        publish(&words, &owner, B);
        assert!(poisoned(&words), "the second claimant poisons (a_first={a_first})");
        assert_eq!(owner.load(Ordering::SeqCst), JIT_G1_BARRIER_CONTENDED);
        let (first, second) = if a_first { (A, B) } else { (B, A) };
        clear_jit_g1_barrier_owned_by_in(&words, &owner, first);
        assert!(poisoned(&words), "after one drop (a_first={a_first})");
        assert_eq!(owner.load(Ordering::SeqCst), JIT_G1_BARRIER_CONTENDED);
        clear_jit_g1_barrier_owned_by_in(&words, &owner, second);
        assert!(poisoned(&words), "after both drops (a_first={a_first})");
        assert_eq!(owner.load(Ordering::SeqCst), JIT_G1_BARRIER_CONTENDED);
        // Sticky: a third collector does not revive the table.
        publish(&words, &owner, A);
        assert!(poisoned(&words), "contention is sticky (a_first={a_first})");
    }
}

/// A table already contended (by collectors since dropped) stays
/// poisoned for a new publisher, and its teardown leaves it so.
#[test]
fn a_contended_table_stays_poisoned_for_a_new_publisher() {
    let (words, owner) = fresh();
    owner.store(JIT_G1_BARRIER_CONTENDED, Ordering::SeqCst);
    publish(&words, &owner, A);
    assert!(poisoned(&words));
    clear_jit_g1_barrier_owned_by_in(&words, &owner, A);
    assert!(poisoned(&words));
    assert_eq!(owner.load(Ordering::SeqCst), JIT_G1_BARRIER_CONTENDED);
}
