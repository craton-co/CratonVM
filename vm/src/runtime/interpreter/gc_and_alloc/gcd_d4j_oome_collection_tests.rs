//! gcd d4/j: the native funnel's collection owed after a heap OOME, and
//! the majors that decide one.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::atomic::Ordering;

fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

/// A heap OOME arms a debt for its raising thread; the VM-limit refusal
/// arms none; another VM owes nothing; G1 also records the debt. gcd d5/q: this
/// test used to assert that the first funnel call on the fresh heap pays;
/// a fresh heap has its old generation free (`recovered`), so the debt is
/// now CLEARED there without a collection -- the paying arms are the pure
/// `the_debt_is_the_raising_threads_and_is_paid_at_most_twice`.
#[test]
fn a_heap_oome_owes_the_funnel_one_collection() {
    let shared = vm(GcAlgorithm::Generational);
    let raiser = JvmThread::new(ThreadId(7), "d4j-raiser");
    let debt = || {
        shared
            .mem
            .alloc_ladder
            .oome_native_debt
            .load(Ordering::Relaxed)
    };
    assert!(
        !native_call_owes_oome_major(&shared, &raiser),
        "nothing owed at start"
    );
    note_heap_oome_raised(&shared, &raiser, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
    assert_eq!(debt(), 0, "the VM limit owes nothing");
    note_heap_oome_raised(&shared, &raiser, "Java heap space");
    if !cratonvm_types::flags().gc.gc_overhead_progress {
        assert_eq!(debt(), 0, "`CRATONVM_GC_OVERHEAD_PROGRESS=0` arms nothing");
        return;
    }
    assert_eq!(debt(), oome_debt_key(&raiser) | OOME_DEBT_PAYMENTS);
    let other = vm(GcAlgorithm::Generational);
    assert!(!native_call_owes_oome_major(&other, &raiser), "per VM");
    let bystander = JvmThread::new(ThreadId(8), "d4j-bystander");
    assert!(
        !native_call_owes_oome_major(&shared, &bystander),
        "the raiser's debt"
    );
    assert_ne!(debt(), 0, "a bystander leaves it in place");
    // The fresh heap's old generation is empty: nothing to collect for.
    assert!(!native_call_owes_oome_major(&shared, &raiser));
    assert_eq!(debt(), 0, "cleared, not paid");
    let g1 = vm(GcAlgorithm::G1);
    note_heap_oome_raised(&g1, &raiser, "Java heap space");
    assert_eq!(
        g1.mem.alloc_ladder.oome_native_debt.load(Ordering::Relaxed),
        oome_debt_key(&raiser) | OOME_DEBT_PAYMENTS
    );
}

/// gcd d5/q: the pure debt rule. Only the raising thread's key pays; a
/// heap that recovered clears it; a full heap pays at most twice.
#[test]
fn the_debt_is_the_raising_threads_and_is_paid_at_most_twice() {
    let key = 5u64 << 2;
    let word = key | OOME_DEBT_PAYMENTS;
    assert_eq!(
        oome_debt_decision(0, key, false),
        (false, 0),
        "nothing owed"
    );
    assert_eq!(
        oome_debt_decision(word, 6 << 2, false),
        (false, word),
        "another thread's"
    );
    assert_eq!(
        oome_debt_decision(word, key, true),
        (false, 0),
        "recovered: cleared"
    );
    let (pay, next) = oome_debt_decision(word, key, false);
    assert!(pay);
    assert_eq!(next, key | 1, "one payment left after a futile one");
    assert_eq!(
        oome_debt_decision(next, key, false),
        (true, 0),
        "the last payment"
    );
    assert_eq!(
        oome_debt_decision(key, key, false),
        (false, 0),
        "an exhausted word clears"
    );
}

/// gcf f1/o: on Generational the payment is the deciding majors, as the
/// funnel's call was (it collects on this sole thread, and leaves no major
/// request behind). The G1 / ZGC arms are measured by the probe rows.
#[test]
fn gcf_f1o_the_generational_payment_is_the_deciding_majors() {
    let shared = vm(GcAlgorithm::Generational);
    let mut thread = JvmThread::new(ThreadId(0), "f1o-pay");
    let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    pay_native_oome_debt(&shared, &mut thread);
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > before);
    assert!(!cratonvm_gc::gc_quiescence::major_gc_requested());
}

/// The deciding majors run on this (sole) thread and collect.
#[test]
fn the_deciding_majors_collect_on_this_thread() {
    let shared = vm(GcAlgorithm::Generational);
    let mut thread = JvmThread::new(ThreadId(0), "d4j-majors");
    let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    assert!(majors_to_decide_oome(&shared, &mut thread, "d4j-test"));
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > before);
    assert!(
        !cratonvm_gc::gc_quiescence::major_gc_requested(),
        "no request is left behind"
    );
}
