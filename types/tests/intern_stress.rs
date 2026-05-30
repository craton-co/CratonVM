// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent stress test for `StringPool::intern_arc`.
//!
//! Spawns 16 threads that interleave two workloads:
//!
//!   * **Writers** — insert fresh, never-before-seen UUID-shaped strings
//!     into the pool.
//!   * **Readers** — re-intern previously-inserted strings, asserting
//!     the returned `Arc<str>` is pointer-identical to the original
//!     hand-off (proves no torn writes / no Arc-identity drift under
//!     contention).
//!
//! No external dependencies — UUIDs are synthesised from a per-thread
//! counter plus a monotonic global ticket so every key is unique across
//! the entire run.
//!
//! Types gap §2.3-2 from `.claude/review-2026-05-24/types.md`.

use cratonvm_types::StringPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Total threads spawned (8 inserters + 8 verifiers running concurrently).
const THREAD_COUNT: usize = 16;
/// Per-thread insert count.  Keep modest to bound test runtime while
/// still producing enough contention.
const INSERTS_PER_THREAD: usize = 200;

/// Synthesise a UUID-shaped string (32 hex chars + 4 hyphens), derived
/// from a monotonic ticket so every call yields a unique string across
/// the whole process.
fn fresh_uuid_like() -> String {
    static TICKET: AtomicU64 = AtomicU64::new(0xDEAD_BEEF_0000_0000);
    let n = TICKET.fetch_add(1, Ordering::Relaxed);
    // Two 64-bit halves: the monotonic counter and its bitwise inverse.
    // 4-2-2-2-6 grouping mimics UUID-4 layout.  The exact format is
    // immaterial — uniqueness is the only invariant the test relies on.
    let lo = n;
    let hi = !n;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hi >> 32) as u32,
        (hi >> 16) as u16,
        hi as u16,
        (lo >> 48) as u16,
        lo & 0xFFFF_FFFF_FFFF,
    )
}

#[test]
fn concurrent_intern_no_torn_writes_or_identity_drift() {
    let pool = Arc::new(StringPool::new());

    // Shared observation log — every successful insert lands here so
    // verifier threads can rediscover and re-intern the same string.
    // Vec<(content, Arc<str> handed back by the *first* insert)> —
    // any later intern of `content` must return an `Arc<str>` that is
    // `Arc::ptr_eq` to this baseline.
    type Witness = (String, Arc<str>);
    let log: Arc<Mutex<Vec<Witness>>> = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::with_capacity(THREAD_COUNT);
    for tid in 0..THREAD_COUNT {
        let pool = Arc::clone(&pool);
        let log = Arc::clone(&log);
        // Even-numbered threads write; odd-numbered threads alternate
        // between writing and reading.  This guarantees that by the
        // end every thread has both inserted and (likely) re-read.
        let h = thread::spawn(move || {
            for i in 0..INSERTS_PER_THREAD {
                // Always insert a fresh key.
                let key = fresh_uuid_like();
                let arc = pool.intern_arc(&key);
                // Witness it for later re-reads.
                {
                    let mut g = log.lock().unwrap();
                    g.push((key.clone(), Arc::clone(&arc)));
                }
                // Half the iterations also re-read a previously
                // inserted key (if any are available).  This is the
                // "torn write" probe: while one thread is in the
                // middle of an insert, another may try to re-read
                // any earlier key.
                if i % 2 == (tid & 1) {
                    // Snapshot a witness without holding the log lock
                    // across the re-intern call (the pool itself locks
                    // internally; nested locks would risk a deadlock).
                    let snapshot: Option<(String, Arc<str>)> = {
                        let g = log.lock().unwrap();
                        if g.is_empty() {
                            None
                        } else {
                            // Sample roughly the middle of the log so
                            // we hit a key that has had time to settle.
                            let idx = g.len() / 2;
                            Some(g[idx].clone())
                        }
                    };
                    if let Some((content, baseline)) = snapshot {
                        let re_read = pool.intern_arc(&content);
                        assert!(
                            Arc::ptr_eq(&baseline, &re_read),
                            "re-intern of {content:?} on tid {tid} produced a different \
                             Arc<str> allocation — torn write or identity drift",
                        );
                        // Content equality is a weaker invariant; check
                        // it anyway in case `ptr_eq` ever passes by luck.
                        assert_eq!(&*re_read, content.as_str());
                    }
                }
            }
        });
        handles.push(h);
    }

    for h in handles {
        h.join().expect("worker thread must not panic");
    }

    // Post-condition: every witnessed key, when re-interned through the
    // pool one more time, still returns the exact same `Arc<str>` we
    // captured at insert time.  This is a stronger restatement of the
    // in-loop check, run on the cooled-down pool with no concurrent
    // writers in flight.
    let g = log.lock().unwrap();
    for (content, baseline) in g.iter() {
        let re_read = pool.intern_arc(content);
        assert!(
            Arc::ptr_eq(baseline, &re_read),
            "post-cooldown re-intern of {content:?} produced a different Arc<str>"
        );
    }

    // Sanity: pool length matches the unique-content count.  Note that
    // because `fresh_uuid_like` is process-monotonic, all witnessed
    // keys are unique by construction, so the witness list length is
    // exactly the inserted-content count.
    let unique_inserted = g.len();
    assert_eq!(
        pool.len(),
        unique_inserted,
        "pool size must equal the number of distinct keys inserted"
    );
}
