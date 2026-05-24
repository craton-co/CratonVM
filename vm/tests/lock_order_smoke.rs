// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Smoke coverage for the `OrderedMutex` / `OrderedRwLock` lock-order
//! wrappers (review §2.3, item 3).
//!
//! Walks the `LockLevel` enum and asserts, for every L → L′ transition,
//! that the wrapper's debug-build assertion fires iff the documented
//! hierarchy in `docs/lock-order.md` says the transition is forbidden.
//!
//! ## Wrapper status — read first
//!
//! As of this commit, `OrderedMutex` / `OrderedRwLock` are **not wired
//! into any production callsite**. `SharedVm` still uses raw
//! `std::sync::Mutex` / `RwLock` for `class_manager`, `heap`,
//! `monitors`, etc. The wrappers live in
//! `vm/src/runtime/lock_order.rs` as a forward-looking enforcement
//! mechanism, but the *runtime* lock-order policy across `SharedVm` is
//! currently un-enforced — a future commit (C6 follow-up) wires
//! `OrderedMutex<T>` into the actual fields.
//!
//! Until that wiring lands, this test exercises the wrapper API in
//! isolation. It asserts that:
//!
//! - Every allowed L → L′ transition (per the wrapper's documented
//!   acquisition order) succeeds.
//! - Every forbidden transition panics with `lock order violation`.
//! - Drop-then-acquire returns to a clean state (no residual
//!   "currently held" bookkeeping leaks across guards).
//!
//! TODO: once `OrderedMutex` consumers land in `SharedVm`, add a
//! second `lock_order_e2e.rs` that drives the real fields with
//! intentionally inverted call sequences and asserts the same
//! violation surfaces from a real interpreter path.
//!
//! ## Hierarchy interpretation
//!
//! The wrapper in this worktree implements **ascending** acquisition:
//! a thread holding a lock at level N may only acquire a lock at
//! level **strictly greater than N**. The numeric mapping is:
//!
//! `HeapLock(0) < ClassLoader(1) < MonitorPool(2) < ThreadList(3) < JitCache(4) < Safepoint(5)`
//!
//! Doc-vs-code direction mismatch (review §1.1 B1, `docs/lock-order.md`
//! is descending L10 → L0) is tracked separately — this file asserts
//! what the *code* enforces today so a future inversion is caught by
//! both the existing unit tests in `lock_order.rs` and this
//! integration-test surface.

#![allow(clippy::unwrap_used)]

use std::panic::{catch_unwind, AssertUnwindSafe};

use cratonvm_vm::runtime::lock_order::{LockLevel, OrderedMutex, OrderedRwLock};

/// All 6 levels in ascending order.
const ALL_LEVELS: [LockLevel; 6] = [
    LockLevel::HeapLock,
    LockLevel::ClassLoader,
    LockLevel::MonitorPool,
    LockLevel::ThreadList,
    LockLevel::JitCache,
    LockLevel::Safepoint,
];

/// Try to acquire `m_inner` while holding `m_outer`. Returns `Ok(())`
/// if both acquires succeed; `Err(panic_message)` if the inner acquire
/// trips the lock-order assertion.
///
/// Runs the inner acquire under `catch_unwind` so the harness can
/// continue exercising the next transition after a deliberate
/// violation. Each call runs on a fresh OS thread so the per-thread
/// `tracking::HELD` bit-set is guaranteed clean — without this, a
/// `should_panic` in a forbidden transition would leave the outer
/// lock's bit set, polluting every subsequent transition on the same
/// thread (`tracking::release` only fires on `Drop`, which the panic
/// short-circuits past).
fn attempt_transition(outer: LockLevel, inner: LockLevel) -> Result<(), String> {
    // Spawn a fresh thread so the per-thread `HELD` state is empty,
    // independent of whatever prior transitions ran in the test
    // harness. The join returns the wrapped result.
    std::thread::spawn(move || {
        let m_outer = OrderedMutex::new((), outer);
        let m_inner = OrderedMutex::new((), inner);

        let _g_outer = m_outer.lock().unwrap();
        let inner_result =
            catch_unwind(AssertUnwindSafe(|| m_inner.lock().expect("poison-free")));
        match inner_result {
            Ok(_g_inner) => Ok(()),
            Err(payload) => {
                let msg = payload_to_string(payload);
                Err(msg)
            }
        }
    })
    .join()
    .expect("transition probe thread panicked outside the inner-lock attempt")
}

/// Extract the human-readable message from a `catch_unwind` payload.
/// The payload type is the canonical `Box<dyn Any + Send + 'static>`
/// returned by the std lib; we downcast against the two cases the
/// standard `assert!` macro can produce (`&'static str` for literal
/// messages, `String` for formatted ones).
fn payload_to_string(p: Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// ---------------------------------------------------------------------------
// Allowed transitions — `outer < inner` per ascending discipline.
// ---------------------------------------------------------------------------

#[test]
fn all_strictly_ascending_pairs_succeed() {
    // For every (outer, inner) with outer < inner, both acquires must
    // succeed without any panic.
    let mut checked = 0usize;
    for (i, outer) in ALL_LEVELS.iter().enumerate() {
        for inner in &ALL_LEVELS[i + 1..] {
            let result = attempt_transition(*outer, *inner);
            assert!(
                result.is_ok(),
                "ascending pair {outer:?} → {inner:?} should be allowed; \
                 wrapper rejected with: {:?}",
                result.err(),
            );
            checked += 1;
        }
    }
    // 6 levels: C(6,2) = 15 ordered pairs with outer < inner.
    assert_eq!(checked, 15, "expected 15 ascending pairs, checked {checked}");
}

#[test]
fn full_ascending_chain_acquires_all_six_levels() {
    // The maximal allowed chain: every level acquired in order, each
    // held simultaneously.
    let mutexes: Vec<OrderedMutex<u8>> = ALL_LEVELS
        .iter()
        .enumerate()
        .map(|(i, lvl)| OrderedMutex::new(i as u8, *lvl))
        .collect();
    // RAII guards held in a vec so all six are alive at the assert.
    let guards: Vec<_> = mutexes.iter().map(|m| m.lock().unwrap()).collect();
    let sum: u32 = guards.iter().map(|g| u32::from(**g)).sum();
    // 0 + 1 + 2 + 3 + 4 + 5 = 15.
    assert_eq!(sum, 15);
}

#[test]
fn mixed_mutex_then_rwlock_ascending_ok() {
    // Cross-type guard interaction still respects ordering.
    let m = OrderedMutex::new(7_i32, LockLevel::HeapLock);
    let rw = OrderedRwLock::new(11_i32, LockLevel::Safepoint);
    let g_m = m.lock().unwrap();
    let g_rw = rw.write().unwrap();
    assert_eq!(*g_m + *g_rw, 18);
}

// ---------------------------------------------------------------------------
// Forbidden transitions — `outer >= inner` per ascending discipline.
// ---------------------------------------------------------------------------

#[test]
fn all_strictly_descending_pairs_panic() {
    // For every (outer, inner) with outer > inner, the inner acquire
    // must trip the wrapper's lock-order assertion.
    let mut checked = 0usize;
    for (i, outer) in ALL_LEVELS.iter().enumerate() {
        for inner in &ALL_LEVELS[..i] {
            let result = attempt_transition(*outer, *inner);
            let msg = result.expect_err(&format!(
                "descending pair {outer:?} → {inner:?} should be rejected, but no panic fired",
            ));
            assert!(
                msg.contains("lock order violation"),
                "panic message for {outer:?} → {inner:?} should mention \
                 'lock order violation'; got: {msg:?}",
            );
            checked += 1;
        }
    }
    // 15 descending pairs (symmetric with the ascending count).
    assert_eq!(checked, 15, "expected 15 descending pairs, checked {checked}");
}

#[test]
fn same_level_pairs_panic() {
    // Equal-level acquires also violate the strict-greater rule.
    for lvl in ALL_LEVELS {
        let result = attempt_transition(lvl, lvl);
        let msg = result.expect_err(&format!(
            "same-level pair {lvl:?} → {lvl:?} should be rejected, but no panic fired",
        ));
        assert!(
            msg.contains("lock order violation"),
            "panic message for {lvl:?} → {lvl:?} should mention 'lock order violation'; got: {msg:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Release / re-acquire — the tracking bit-set is cleared on Drop.
// ---------------------------------------------------------------------------

#[test]
fn drop_then_reacquire_clears_tracking() {
    // After releasing a high-level lock, a thread is free to take a
    // *lower-level* lock — which would otherwise be a violation.
    let high = OrderedMutex::new((), LockLevel::Safepoint);
    let low = OrderedMutex::new((), LockLevel::HeapLock);
    {
        let _g = high.lock().unwrap();
        // _g drops here.
    }
    // Tracking has now released Safepoint; HeapLock must succeed.
    let _g_low = low.lock().unwrap();
}
