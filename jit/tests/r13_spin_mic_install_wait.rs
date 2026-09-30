// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 13 wave 10, lane spin (lane mega8's proposal M8-2): `JitMICSlot::update`'s
//! wait for another writer's `INSTALLING_CLASS_ID` publication is bounded.
//!
//! Before the fix the wait was `std::hint::spin_loop()` with no bound, no yield
//! and no poll: a slot whose installing writer never stored its class id would
//! hang every later miss at the site, silently, in Rust code the watchdog
//! reports only as "compiled code or a long native call"
//! (`r13w10-orch-codecache-discard-churn-compatible-spin-hang-FIXED-20260928.md`).
//! `CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND` (default on) makes the waiter yield and
//! then give up without publishing; `0` restores the unbounded spin, and these
//! tests then skip.

use cratonvm_jit::JitMICSlot;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// `JitMICSlot::INSTALLING_CLASS_ID` (crate-private): all ones.
const INSTALLING: u32 = u32::MAX;

fn bound_on() -> bool {
    !matches!(
        std::env::var("CRATONVM_JIT_MIC_INSTALL_WAIT_BOUND").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Run `update(7, ..)` on its own thread and answer whether it returned within
/// `limit`. A thread that never returns is left behind (the harness exits
/// without joining it), so a regression fails the assertion instead of hanging
/// the test binary.
fn update_returns_within(slot: &Arc<JitMICSlot>, limit: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let slot = Arc::clone(slot);
    std::thread::spawn(move || {
        slot.update(7, "r13spin/Seven", 0, false, false);
        let _ = tx.send(());
    });
    rx.recv_timeout(limit).is_ok()
}

#[test]
fn a_slot_left_installing_forever_does_not_hang_the_next_miss() {
    if !bound_on() {
        return;
    }
    let slot = Arc::new(JitMICSlot::new());
    // A writer that CASed the slot and never stored its class id.
    slot.cached_class_id.store(INSTALLING, Ordering::Release);
    assert!(
        update_returns_within(&slot, Duration::from_secs(60)),
        "JitMICSlot::update spun forever on a slot another writer left INSTALLING"
    );
    // It gave up WITHOUT publishing: the guard still matches no receiver and
    // there is no entry to call.
    assert_eq!(slot.cached_class_id.load(Ordering::Acquire), INSTALLING);
    assert_eq!(slot.cached_entry(), (0, false));

    // Once the holder finishes (here: empties the slot), the next miss
    // publishes as usual.
    slot.cached_class_id
        .store(JitMICSlot::EMPTY_CLASS_ID, Ordering::Release);
    slot.update(7, "r13spin/Seven", 0, false, false);
    assert_eq!(slot.cached_class_id.load(Ordering::Acquire), 7);
}

#[test]
fn a_holder_that_publishes_another_class_ends_the_wait() {
    if !bound_on() {
        return;
    }
    let slot = Arc::new(JitMICSlot::new());
    slot.cached_class_id.store(INSTALLING, Ordering::Release);
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = {
        let slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            slot.update(7, "r13spin/Seven", 0, false, false);
            let _ = tx.send(());
        })
    };
    std::thread::sleep(Duration::from_millis(1));
    // The holder's final store: class 9 owns the slot for its lifetime.
    slot.cached_class_id.store(9, Ordering::Release);
    assert!(
        rx.recv_timeout(Duration::from_secs(60)).is_ok(),
        "the waiter did not return after the holder published"
    );
    waiter.join().expect("waiter");
    // Whether the waiter saw the store or gave up first, a populated slot is
    // never retargeted.
    assert_eq!(slot.cached_class_id.load(Ordering::Acquire), 9);
}
