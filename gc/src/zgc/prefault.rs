// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Pre-faulting the arena just ahead of the low bump cursor (jr21 gc,
//! `docs/internal/fixed-bugs/jr21-fresh-heap-first-touch-charges-the-first-allocating-row-FIXED-20261027.md`).
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
use crate::reservation::GRANULE;

/// Bytes one worker step populates under one `uncommit_gate` hold: a
/// collection that wants the gate waits at most this much `madvise`
/// (~0.4 ms for a granule on the Linux host).
pub(crate) const ZGC_PREFAULT_STEP: usize = GRANULE;

/// The default window past the low cursor, in MiB. At the host's ~5 GB/s
/// populate rate it is ~12 ms of lead over an allocating thread.
const ZGC_PREFAULT_AHEAD_DEFAULT_MIB: usize = 64;

/// The worker's idle sleep: it is woken by the carve path, the timeout only
/// bounds how late it notices a stop or a missed wake.
const ZGC_PREFAULT_IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// `CRATONVM_ZGC_PREFAULT_AHEAD=<MiB>` -> the window in bytes; `0` / `off` /
/// `false` / `no` turn the worker off. Unset: [`ZGC_PREFAULT_AHEAD_DEFAULT_MIB`]
/// where the populate is a single `madvise` (`linux`), off elsewhere (the
/// touch fallback is not exercised on those platforms; an explicit value
/// still turns it on). A value that is not a number keeps the default. Pure.
pub(crate) fn prefault_ahead_from(raw: Option<&str>, linux: bool) -> usize {
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
pub(crate) fn next_span(
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
pub(crate) fn committed_run_end(bits: &[AtomicU64], lo: usize, hi: usize) -> usize {
    let mut g = lo / GRANULE;
    while g * GRANULE < hi && crate::reservation::granule_committed(bits, g) {
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
pub(crate) struct ZPrefault {
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
    pub(crate) fn new() -> Self {
        let raw = cratonvm_types::flags::runtime_var("CRATONVM_ZGC_PREFAULT_AHEAD").ok();
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
        // Latched, as the uncommitter: the heap can be dropped in a
        // thread-local destructor, where an OS join deadlocks on Windows.
        let spawned = cratonvm_types::latched_thread::spawn_latched(
            std::thread::Builder::new().name("ZPrefaulter".to_string()),
            move || loop {
                if thread_stop.load(Ordering::Acquire) {
                    return;
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
    pub(crate) fn note_low_cursor(&self, cursor: usize) {
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
    pub(crate) fn prefault_pass(&self) -> usize {
        let pf = &self.counters.fp.prefault;
        let ahead = pf.ahead.load(Ordering::Relaxed);
        let mut populated = 0usize;
        while ahead != 0 {
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
                crate::reservation::pre_touch_range((self.arena_base + lo) as *mut u8, hi - lo)
            };
            pf.upto.store(hi, Ordering::Relaxed);
        }
        pf.bytes.fetch_add(populated, Ordering::Relaxed);
        populated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    #[test]
    fn the_window_defaults_on_linux_only_and_parses_mib() {
        assert_eq!(prefault_ahead_from(None, true), 64 * MIB);
        assert_eq!(prefault_ahead_from(None, false), 0);
        assert_eq!(prefault_ahead_from(Some("16"), false), 16 * MIB);
        assert_eq!(prefault_ahead_from(Some(" 8 "), true), 8 * MIB);
        for off in ["0", "off", "false", "no", "OFF"] {
            assert_eq!(prefault_ahead_from(Some(off), true), 0, "{off}");
        }
        assert_eq!(prefault_ahead_from(Some("lots"), true), 64 * MIB);
    }

    #[test]
    fn the_next_span_starts_at_the_later_of_cursor_and_done() {
        // Fresh: from the cursor, one step.
        assert_eq!(
            next_span(100, 0, 64 * MIB, 2 * MIB, 1 << 40),
            Some((100, 100 + 2 * MIB))
        );
        // Already populated past the cursor: continue from there.
        assert_eq!(
            next_span(100, 10 * MIB, 64 * MIB, 2 * MIB, 1 << 40),
            Some((10 * MIB, 12 * MIB))
        );
        // The window is covered.
        assert_eq!(next_span(0, 64 * MIB, 64 * MIB, 2 * MIB, 1 << 40), None);
        // The last step is clipped to the window...
        assert_eq!(
            next_span(0, 63 * MIB, 64 * MIB, 2 * MIB, 1 << 40),
            Some((63 * MIB, 64 * MIB))
        );
        // ... and to the bump region's end.
        assert_eq!(next_span(0, 0, 64 * MIB, 2 * MIB, MIB), Some((0, MIB)));
        assert_eq!(next_span(5 * MIB, 0, 64 * MIB, 2 * MIB, 5 * MIB), None);
        // No window, nothing to do.
        assert_eq!(next_span(0, 0, 0, 2 * MIB, 1 << 40), None);
    }

    #[test]
    fn the_committed_run_stops_at_the_first_uncommitted_granule() {
        // Granules 0..3 committed, 3 not, 4 committed.
        let bits: Vec<AtomicU64> = vec![AtomicU64::new(0b10111)];
        let g = GRANULE;
        assert_eq!(committed_run_end(&bits, 0, 10 * g), 3 * g);
        assert_eq!(committed_run_end(&bits, g + 5, 2 * g), 2 * g);
        assert_eq!(committed_run_end(&bits, 3 * g, 5 * g), 3 * g);
        assert_eq!(committed_run_end(&bits, 4 * g, 5 * g), 5 * g);
        // Past the map: nothing is committed.
        assert_eq!(committed_run_end(&bits, 64 * g, 65 * g), 64 * g);
    }

    /// The behaviour a user sees: on a reserved heap with a startup commit, a
    /// carve wakes nothing synchronously, but a pass populates the committed
    /// window past the cursor, stops at the window, and the bytes stay zero.
    #[test]
    fn a_pass_populates_the_committed_window_ahead_of_the_cursor() {
        let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB);
        heap.counters
            .fp
            .prefault
            .ahead
            .store(8 * MIB, Ordering::Relaxed);
        let cursor = heap.arena.lock().used_low_for_compaction();
        let got = heap.prefault_pass();
        assert!(got > 0, "populated nothing");
        assert!(got <= 8 * MIB + GRANULE, "populated past the window: {got}");
        assert_eq!(heap.prefaulted_bytes(), got);
        let upto = heap.counters.fp.prefault.upto.load(Ordering::Relaxed);
        assert!(
            upto >= cursor + 8 * MIB,
            "window not covered: {upto} from {cursor}"
        );
        // Covered: a second pass has nothing to do.
        assert_eq!(heap.prefault_pass(), 0);
        // Contents unchanged: the populated bytes still read zero.
        let base = heap.arena_base as *const u8;
        for off in (cursor..upto).step_by(4096) {
            // SAFETY: committed (the startup commit) and inside the arena.
            assert_eq!(unsafe { *base.add(off) }, 0, "byte at {off}");
        }
    }

    /// Nothing past the commit: a heap whose startup commit ends below the
    /// window populates up to the commit and stops there.
    #[test]
    fn a_pass_never_crosses_into_uncommitted_granules() {
        let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 4 * MIB);
        heap.counters
            .fp
            .prefault
            .ahead
            .store(64 * MIB, Ordering::Relaxed);
        let committed_before = heap.os_committed_bytes();
        // `commit_initial_zgc` splits a 4 MiB `-Xms` into 2 MiB at the low
        // end and 2 MiB at the high end: the pass covers the low part only.
        let got = heap.prefault_pass();
        assert!(got > 0, "populated nothing of the low startup commit");
        let upto = heap.counters.fp.prefault.upto.load(Ordering::Relaxed);
        assert_eq!(upto, 2 * MIB, "stopped short of, or past, the low startup commit");
        assert_eq!(
            heap.os_committed_bytes(),
            committed_before,
            "the worker committed memory"
        );
    }

    /// A collection holding the gate makes the pass yield at once.
    #[test]
    fn a_pass_yields_to_a_held_uncommit_gate() {
        let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB);
        heap.counters
            .fp
            .prefault
            .ahead
            .store(8 * MIB, Ordering::Relaxed);
        let gate = heap.counters.fp.uncommit_gate.lock();
        assert_eq!(heap.prefault_pass(), 0);
        drop(gate);
        assert!(heap.prefault_pass() > 0);
    }

    /// The carve hook requests a wake only when the window has fallen below
    /// half its size ahead of the cursor, and only once until the worker runs.
    #[test]
    fn the_carve_hook_requests_once_per_half_window() {
        let heap = ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB);
        let pf = &heap.counters.fp.prefault;
        pf.ahead.store(8 * MIB, Ordering::Relaxed);
        pf.upto.store(10 * MIB, Ordering::Relaxed);
        heap.note_low_cursor(MIB);
        assert!(
            !pf.requested.load(Ordering::Relaxed),
            "9 MiB ahead is above half"
        );
        // 7 MiB ahead: short of the full window, still above half.
        heap.note_low_cursor(3 * MIB);
        assert!(
            !pf.requested.load(Ordering::Relaxed),
            "7 MiB ahead is above half"
        );
        heap.note_low_cursor(7 * MIB);
        assert!(
            pf.requested.load(Ordering::Relaxed),
            "3 MiB ahead is below half"
        );
        // Off: never requests.
        pf.requested.store(false, Ordering::Relaxed);
        pf.ahead.store(0, Ordering::Relaxed);
        heap.note_low_cursor(9 * MIB);
        assert!(!pf.requested.load(Ordering::Relaxed));
    }

    /// The worker itself: started, it populates the window without being
    /// asked by a carve (its first pass), and stops on request.
    #[test]
    fn the_worker_populates_and_stops() {
        let heap = Arc::new(ZgcRealHeap::with_capacity_and_initial(256 * MIB, 64 * MIB));
        heap.counters
            .fp
            .prefault
            .ahead
            .store(8 * MIB, Ordering::Relaxed);
        if !heap.start_prefaulter() {
            // A store without a commit map (no reservation on this host).
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while heap.prefaulted_bytes() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(heap.prefaulted_bytes() > 0, "the worker populated nothing");
        heap.stop_prefaulter();
        assert!(heap.counters.fp.prefault.handle.lock().is_none());
    }
}
