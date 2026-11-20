// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Pre-faulting the arena just ahead of the low bump cursor (jr21 gc,
//! `docs/feature-designs/jit-ir-optimizations.md`).
//!
//! Before the first collections every TLAB carve on a fresh heap bumps into
//! memory nothing has written: committed (the `-Xms` startup commit is
//! `mprotect`ed read-write) but not yet backed, so each 4 KiB page costs its
//! allocating thread a demand-zero fault on the first store (~1 us on the
//! Linux host, ~85 small objects' worth). With an ergonomic multi-GiB heap
//! the warm-up cycles are over a gigabyte away, so the first allocation-heavy
//! row of a run paid ~2.5x per allocation (`FirstTouchRows` cold 310 against
//! 125 ns per 16-allocation call once the heap had been reused). HotSpot G1
//! pays it once, for its bounded eden; a ZGC heap that bumps through a tenth
//! of `-Xmx` before its first cycle paid it for all of that.
//!
//! The fix keeps the cost off the allocating thread: a per-heap worker
//! (`ZPrefaulter`) keeps the COMMITTED bytes up to
//! `CRATONVM_ZGC_PREFAULT_AHEAD` MiB past the low cursor populated
//! (`MADV_POPULATE_WRITE`, which faults pages in without writing a byte), so
//! a carve finds its pages already backed. It commits nothing (the JMX
//! `committed` figure does not move), and it touches only granules the commit
//! map says are committed, under the heap's `uncommit_gate` -- the lock every
//! collection and every uncommitter pass holds around its decommits and
//! resets -- so no give-back can interleave with a populate. The mutator's
//! only cost is one relaxed load per carve, and an `unpark` once per half
//! window.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use super::ZgcRealHeap;
use cratonvm_gc_core::reservation::GRANULE;

/// Bytes one worker step populates under one `uncommit_gate` hold: a
/// collection that wants the gate waits at most this much `madvise`
/// (~0.4 ms for a granule on the Linux host).
pub const ZGC_PREFAULT_STEP: usize = GRANULE;

/// The default window past the low cursor, in MiB. At the host's ~5 GB/s
/// populate rate it is ~12 ms of lead over an allocating thread.
const ZGC_PREFAULT_AHEAD_DEFAULT_MIB: usize = 8;

/// A heap restored from a boot image starts its worker this long after the
/// restore: a program that ends sooner (the restore exists to make those
/// cheap) never pays ~60 MB of populate and the matching teardown. Zero for a
/// cold boot, whose own boot takes longer than this anyway.
const ZGC_PREFAULT_RESTORED_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Delay before the worker's first pass.
pub fn start_delay(restored: bool) -> std::time::Duration {
    if restored {
        ZGC_PREFAULT_RESTORED_DELAY
    } else {
        std::time::Duration::from_millis(1000)
    }
}

/// The worker's idle sleep: it is woken by the carve path, the timeout only
/// bounds how late it notices a stop or a missed wake.
const ZGC_PREFAULT_IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// `CRATONVM_ZGC_PREFAULT_AHEAD=<MiB>` -> the window in bytes; `0` / `off` /
/// `false` / `no` turn the worker off. Unset: [`ZGC_PREFAULT_AHEAD_DEFAULT_MIB`]
/// where the populate is a single `madvise` (`linux`), off elsewhere (the
/// touch fallback is not exercised on those platforms; an explicit value
/// still turns it on). A value that is not a number keeps the default. Pure.
pub fn prefault_ahead_from(raw: Option<&str>, linux: bool) -> usize {
    let default = if linux {
        ZGC_PREFAULT_AHEAD_DEFAULT_MIB
    } else {
        0
    };
    let mib = match raw.map(|v| v.trim().to_ascii_lowercase()) {
        None => default,
        Some(v) if matches!(v.as_str(), "0" | "off" | "false" | "no") => 0,
        Some(v) => v.parse::<usize>().unwrap_or(ZGC_PREFAULT_AHEAD_DEFAULT_MIB),
    };
    mib.saturating_mul(1 << 20)
}

/// The span the worker populates next, as arena offsets `[lo, hi)`: from the
/// later of the low `cursor` and what it already populated (`done`), at most
/// `step` bytes, never past `cursor + ahead` nor `limit` (the end of the bump
/// region). `None` when the window is already covered. Pure.
pub fn next_span(
    cursor: usize,
    done: usize,
    ahead: usize,
    step: usize,
    limit: usize,
) -> Option<(usize, usize)> {
    let lo = cursor.max(done);
    let hi = lo
        .saturating_add(step)
        .min(cursor.saturating_add(ahead))
        .min(limit);
    (hi > lo).then_some((lo, hi))
}

/// The end of the run of committed granules that starts at `lo`, clipped to
/// `hi`: `lo` itself when `lo`'s granule is not committed. Pure over the
/// commit map.
pub fn committed_run_end(bits: &[AtomicU64], lo: usize, hi: usize) -> usize {
    let mut g = lo / GRANULE;
    while g * GRANULE < hi && cratonvm_gc_core::reservation::granule_committed(bits, g) {
        g += 1;
    }
    (g * GRANULE).clamp(lo, hi.max(lo))
}

/// A running worker's handle: its stop flag and thread.
struct ZPrefaulterHandle {
    stop: Arc<AtomicBool>,
    thread: Option<cratonvm_types::latched_thread::LatchedJoinHandle<()>>,
}

impl ZPrefaulterHandle {
    fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            // The worker may be the one dropping the heap (it upgraded the
            // last strong reference); never join yourself.
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

/// Per-heap pre-fault state. Lives in the cold counters box: the carve path
/// reads `ahead` and `upto` only.
pub struct ZPrefault {
    /// The window in bytes; `0` = off (read once per heap).
    ahead: AtomicUsize,
    /// Arena offset below which the worker has populated, back to the cursor
    /// it last saw.
    upto: AtomicUsize,
    /// A wake is outstanding; the carve path unparks once per request.
    requested: AtomicBool,
    /// Bytes populated (engagement: `prefault=` on `[GC] zgc-footprint-spans:`).
    bytes: AtomicUsize,
    /// The worker's thread, for the carve path's `unpark`.
    worker: std::sync::OnceLock<std::thread::Thread>,
    handle: Mutex<Option<ZPrefaulterHandle>>,
}

impl ZPrefault {
    pub fn new() -> Self {
        let raw = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok();
        Self {
            ahead: AtomicUsize::new(prefault_ahead_from(
                raw.as_deref(),
                cfg!(target_os = "linux"),
            )),
            upto: AtomicUsize::new(0),
            requested: AtomicBool::new(false),
            bytes: AtomicUsize::new(0),
            worker: std::sync::OnceLock::new(),
            handle: Mutex::new(None),
        }
    }
}

impl ZgcRealHeap {
    /// Start this heap's pre-fault worker (see the module comment). Per heap,
    /// never a process global. Starts nothing when the window is `0`, the
    /// store is not a reservation with a commit map, or the heap is on
    /// explicit huge pages (a populate would take pool pages).
    pub fn start_prefaulter(self: &Arc<Self>) -> bool {
        let pf = &self.counters.fp.prefault;
        if pf.ahead.load(Ordering::Relaxed) == 0 {
            return false;
        }
        {
            let arena = self.arena.lock();
            if arena.commit_bits().is_none() || arena.is_explicit_huge_pages() {
                return false;
            }
        }
        let mut slot = pf.handle.lock();
        if slot.is_some() {
            return true;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let weak = Arc::downgrade(self);
        let not_before = std::time::Instant::now()
            + start_delay(
                cratonvm_types::image_layout::mode() == cratonvm_types::image_layout::ImageMode::Restored,
            );
        // Latched, as the uncommitter: the heap can be dropped in a
        // thread-local destructor, where an OS join deadlocks on Windows.
        let spawned = cratonvm_types::latched_thread::spawn_latched(
            std::thread::Builder::new().name("ZPrefaulter".to_string()),
            move || loop {
                if thread_stop.load(Ordering::Acquire) {
                    return;
                }
                // A carve's wake-up ends the park early: sleep out the rest.
                let now = std::time::Instant::now();
                if now < not_before {
                    std::thread::park_timeout(not_before - now);
                    continue;
                }
                let Some(heap) = weak.upgrade() else {
                    return;
                };
                heap.counters
                    .fp
                    .prefault
                    .requested
                    .store(false, Ordering::Release);
                let _ = heap.prefault_pass();
                drop(heap);
                std::thread::park_timeout(ZGC_PREFAULT_IDLE_WAIT);
            },
        );
        match spawned {
            Ok(thread) => {
                let _ = pf.worker.set(thread.thread().clone());
                *slot = Some(ZPrefaulterHandle {
                    stop,
                    thread: Some(thread),
                });
                true
            }
            Err(e) => {
                tracing::warn!(
                    target: "cratonvm::gc",
                    error = %e,
                    "zgc: could not spawn the pre-fault worker; carves fault their own pages",
                );
                false
            }
        }
    }

    /// Stop this heap's pre-fault worker, if it runs, and wait for it.
    pub fn stop_prefaulter(&self) {
        let handle = self.counters.fp.prefault.handle.lock().take();
        if let Some(handle) = handle {
            handle.stop_and_join();
        }
    }

    /// Bytes the pre-fault worker has populated on this heap.
    pub fn prefaulted_bytes(&self) -> usize {
        self.counters.fp.prefault.bytes.load(Ordering::Relaxed)
    }

    /// The carve path's hook: the low cursor is now `cursor`. Wakes the
    /// worker once the populated window has fallen below half its size ahead
    /// of the cursor. One relaxed load when nothing is due.
    #[inline]
    pub fn note_low_cursor(&self, cursor: usize) {
        let pf = &self.counters.fp.prefault;
        let ahead = pf.ahead.load(Ordering::Relaxed);
        if ahead == 0 || pf.upto.load(Ordering::Relaxed) >= cursor.saturating_add(ahead / 2) {
            return;
        }
        if !pf.requested.swap(true, Ordering::AcqRel) {
            if let Some(worker) = pf.worker.get() {
                worker.unpark();
            }
        }
    }

    /// Populate the window ahead of the low cursor, one
    /// [`ZGC_PREFAULT_STEP`] per `uncommit_gate` hold, until it is covered,
    /// the next granule is not committed, or a collection holds the gate.
    /// Returns the bytes populated. The worker's body; tests call it directly.
    pub fn prefault_pass(&self) -> usize {
        let pf = &self.counters.fp.prefault;
        let ahead = pf.ahead.load(Ordering::Relaxed);
        let mut populated = 0usize;
        if ahead == 0 {
            return 0;
        }
        loop {
            // A collection or an uncommitter pass holds the gate around every
            // decommit and reset: yield to it, the next wake retries.
            let Some(_gate) = self.counters.fp.uncommit_gate.try_lock() else {
                break;
            };
            let (cursor, limit, bits) = {
                let arena = self.arena.lock();
                let cursor = arena.used_low_for_compaction();
                (
                    cursor,
                    cursor + arena.low_bump_headroom(),
                    arena.commit_bits(),
                )
            };
            let Some(bits) = bits else {
                break;
            };
            let done = pf.upto.load(Ordering::Relaxed);
            let Some((lo, hi)) = next_span(cursor, done, ahead, ZGC_PREFAULT_STEP, limit) else {
                break;
            };
            let hi = committed_run_end(&bits, lo, hi);
            if hi <= lo {
                break;
            }
            // SAFETY: `[lo, hi)` lies in committed granules of this heap's
            // arena (the commit map, read under `uncommit_gate`, which every
            // decommit and reset holds), and the arena stays mapped while
            // `self` is borrowed. The populate never changes a byte, so the
            // allocators that may carve into the span meanwhile see their own
            // stores (see `pre_touch_range`).
            populated += unsafe {
                cratonvm_gc_core::reservation::pre_touch_range((self.arena_base + lo) as *mut u8, hi - lo)
            };
            pf.upto.store(hi, Ordering::Relaxed);
        }
        pf.bytes.fetch_add(populated, Ordering::Relaxed);
        populated
    }
}

#[cfg(test)]
mod tests;
