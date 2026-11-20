// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::threading::gc_barrier::GcBarrier;
use crate::threading::jvm_thread::ThreadId;

#[test]
fn sync_owed_poll_page_is_idempotent_and_preserves_stw_ordering() {
    let barrier = GcBarrier::new();
    let flag = AtomicBool::new(false);

    barrier.sync_owed_poll_page(&|| flag.load(Ordering::SeqCst));
    assert!(!barrier.owed_poll_armed.load(Ordering::SeqCst));

    flag.store(true, Ordering::SeqCst);
    barrier.sync_owed_poll_page(&|| flag.load(Ordering::SeqCst));
    barrier.sync_owed_poll_page(&|| flag.load(Ordering::SeqCst));
    assert!(barrier.owed_poll_armed.load(Ordering::SeqCst));

    // An intervening STW pause must not clear `owed_poll_armed`.
    assert!(barrier.request_stw(ThreadId(1), 1));
    barrier.complete_gc(cratonvm_types::PointerMap::default());
    assert!(barrier.owed_poll_armed.load(Ordering::SeqCst));

    flag.store(false, Ordering::SeqCst);
    barrier.sync_owed_poll_page(&|| flag.load(Ordering::SeqCst));
    barrier.sync_owed_poll_page(&|| flag.load(Ordering::SeqCst));
    assert!(!barrier.owed_poll_armed.load(Ordering::SeqCst));
}

#[test]
fn racing_latch_and_take_converge_under_poll_sync_hook() {
    let barrier = Arc::new(GcBarrier::new());
    let state = Arc::new(cratonvm_gc::ConcurrentGcState::new());
    {
        let w_barrier = Arc::downgrade(&barrier);
        let w_state = Arc::downgrade(&state);
        state.set_poll_sync_hook(Box::new(move || {
            if let (Some(b), Some(s)) = (w_barrier.upgrade(), w_state.upgrade()) {
                b.sync_owed_poll_page(&|| s.inline_start_requested());
            }
        }));
    }

    let mut joins = Vec::new();
    for _ in 0..3 {
        let s = Arc::clone(&state);
        joins.push(std::thread::spawn(move || {
            for _ in 0..1_000 {
                let _ = s.latch_inline_start_request();
            }
        }));
    }
    {
        let s = Arc::clone(&state);
        joins.push(std::thread::spawn(move || {
            for _ in 0..1_000 {
                let _ = s.take_inline_start_request();
            }
        }));
    }
    for j in joins {
        j.join().unwrap();
    }
    assert_eq!(
        barrier.owed_poll_armed.load(Ordering::SeqCst),
        state.inline_start_requested()
    );
    let _ = state.take_inline_start_request();
    assert!(!barrier.owed_poll_armed.load(Ordering::SeqCst));
    assert!(!state.inline_start_requested());
}
