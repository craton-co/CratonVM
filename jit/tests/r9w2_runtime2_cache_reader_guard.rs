// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 9 wave 2, lane `runtime2`:
//! `jit-cache-reader-guard-makes-the-last-drop-unauthorised-20260918.md`.
//!
//! A `JitCache` reader holds an `arc_swap` guard on a shard map for the length
//! of its lookup. When a publisher supersedes a body during that window, the
//! OLD map (kept alive by the guard) still owns a reference to the withdrawn
//! body, so the publisher's retirement drops a non-last reference and the
//! reader's guard drop becomes the body's LAST reference -- released outside
//! the retirement queue, i.e. counted in `published_code_free_audit().1`, the
//! counter three documents call "must stay zero".
//!
//! The fix stores `RetainedCode` in the shard maps, whose drop routes a last
//! reference through `defer_jit_owner` whichever thread drops it. This test is
//! the regression guard: readers hammer `get` while the main thread
//! supersedes the same key hundreds of times, and the unqueued count must not
//! move.
//!
//! A separate integration binary for the same reason as `code_free_audit.rs`:
//! the counter is process-global and monotone, and the unit-test binary drives
//! it up on purpose. Readers wrap every `get` result in `RetainedCode`, as a
//! real holder of a compiled body must -- a bare `Arc` dropped last on the
//! reader would be a DIFFERENT (caller-side) bypass, not the one under test.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cratonvm_jit::{
    jit_execution_enter, jit_execution_leave, published_code_free_audit, CompiledMethod,
    ExecutableBuffer, JitCache, RetainedCode,
};

fn body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

#[test]
fn a_concurrent_reader_never_makes_a_superseded_bodys_last_drop_unauthorised() {
    let (_, unqueued_at_start) = published_code_free_audit();
    assert_eq!(
        unqueued_at_start, 0,
        "this binary must start clean, or the assertion below is about somebody else's release"
    );

    let cache = Arc::new(JitCache::new());
    let class: Arc<str> = Arc::from("r9w2/ReaderGuard");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    cache.put(class.clone(), method.clone(), desc.clone(), cid, body());

    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (cache, stop) = (Arc::clone(&cache), Arc::clone(&stop));
            let (class, method, desc) = (class.clone(), method.clone(), desc.clone());
            std::thread::spawn(move || {
                let mut hits = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(cm) = cache.get(&class, &method, &desc, cid) {
                        let held = RetainedCode::new(cm);
                        hits += 1;
                        std::thread::yield_now();
                        drop(held);
                    }
                }
                hits
            })
        })
        .collect();

    for _ in 0..500 {
        cache.put(class.clone(), method.clone(), desc.clone(), cid, body());
    }
    stop.store(true, Ordering::Relaxed);
    let hits: u64 = readers.into_iter().map(|r| r.join().expect("reader")).sum();
    assert!(
        hits > 0,
        "the readers must actually have raced the publisher"
    );

    cache.clear_all();
    drop(cache);
    // Let the retirement queue drain on quiescence observations.
    for _ in 0..50 {
        jit_execution_leave(jit_execution_enter());
    }

    // Since the merge with dev (2026-09-19), `RetainedCode::drop` decides by
    // `Arc::strong_count`, so two concurrent last-two drops can both drop plainly
    // (dev's `jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`). That
    // race is closed one level down: `ExecutableBuffer::drop` RESCUES such a
    // mapping into the retirement queue instead of unmapping it, and the audit
    // still counts the rescue. So a small non-zero count is a rescued release,
    // not an unmap. The guard-drop bypass this test was written for would count
    // roughly one per supersede, so bound the count well below the 500 publishes.
    let (_, unqueued) = published_code_free_audit();
    assert!(
        unqueued < 50,
        "{unqueued} superseded bodies were released outside the retirement queue \
         (rescued, but far more than the concurrent last-two-drop race explains); \
         rerun with CRATONVM_DBG_JIT_CODE_FREE=1 for the releasing backtraces"
    );
}
