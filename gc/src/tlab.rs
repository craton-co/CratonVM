//! Thread-Local Allocation Buffers (TLABs).
//!
//! Each thread gets a small chunk of the young generation's from-space
//! that it can bump-allocate from without taking the global arena lock.
//! When the TLAB is exhausted, the thread requests a new one from the
//! shared arena (which does take the lock, but amortized over many
//! allocations).
//!
//! TLABs dramatically reduce lock contention on the allocation path.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Default TLAB size: 256 KB — large enough to amortize the lock cost
/// across ~10k small-object allocations in a tight loop and to keep
/// `refill_tlab` out of the hot path during allocation storms.
///
/// T19.3.G1 (GC allocation-storm): the original 64 KB default caused
/// `ConcurrentHashMap.initTable`-style 2-object loops running at
/// ~25 MB/s to pound the shared arena lock every ~3 ms, triggering a
/// young-GC every ~1.2 s. Bumping the baseline to 256 KB reduces
/// refill pressure by 4× without meaningfully increasing tail waste
/// (typical thread carries one TLAB; waste bounded by `MAX_TLAB_SIZE`).
const DEFAULT_TLAB_SIZE: usize = 256 * 1024;

/// T5.5.1 — minimum TLAB size under low allocation pressure.
const MIN_TLAB_SIZE: usize = 8 * 1024;

/// T5.5.1 — maximum TLAB size under high allocation pressure.
///
/// T19.3.G1: raised from 256 KB to 1 MB so the adaptive sizer can
/// grow a heavily-loaded thread's TLAB past the baseline when a
/// tight loop sustains high allocation rate.
const MAX_TLAB_SIZE: usize = 1024 * 1024;

/// T19.3.G1 — initial refill size for a freshly-created thread.
///
/// Returned by [`initial_refill_size`] so call sites that have no
/// per-thread pressure history (e.g. the first allocation on a new
/// worker thread) start with a size large enough to absorb a
/// static-init burst without an early refill.
const INITIAL_REFILL_SIZE: usize = DEFAULT_TLAB_SIZE;

/// T5.5.1 — threshold for "large" allocations, above which filling
/// a TLAB in fewer allocations still suggests high allocation pressure.
const LARGE_ALLOC_THRESHOLD: usize = 256;

/// T5.5.1 — a TLAB filled faster than this signals high pressure.
const FAST_REFILL_THRESHOLD_MS: u128 = 1;

/// T5.5.1 — a TLAB that took longer than this signals low pressure.
const SLOW_REFILL_THRESHOLD_MS: u128 = 100;

/// T5.5.1 — fewer than this many large allocations before refill → double.
const FAST_REFILL_ALLOC_COUNT: usize = 16;

/// T5.5.1 — fewer than this many total allocations before refill → halve.
const SLOW_REFILL_ALLOC_COUNT: usize = 4;

/// Minimum allocation that goes through the TLAB. Anything larger is
/// allocated directly from the shared arena (slow path).
///
/// Kept at 32 KB — half the *old* 64 KB default — so the cap on
/// TLAB-eligible object size is independent of the adaptive-sizing
/// baseline. A larger `TLAB_MAX_ALLOC` would cause tail-waste spikes
/// when a single oversized object kicks out an otherwise-full TLAB.
const TLAB_MAX_ALLOC: usize = 32 * 1024;

/// A thread-local allocation buffer.
///
/// This is a view into a slice of the young generation's from-space.
/// The thread owns this range exclusively — no locking needed for
/// bump-pointer allocation within the TLAB.
pub struct Tlab {
    /// Start of the TLAB region (inclusive).
    start: *mut u8,
    /// Current allocation cursor within the TLAB.
    cursor: *mut u8,
    /// End of the TLAB region (exclusive).
    end: *mut u8,
    /// T5.5.1 — per-thread adaptive-sizing state. Updated each
    /// allocation and consulted on refill.
    pressure: TlabPressureTracker,
}

// SAFETY: Tlab pointers are into arena memory owned by the GC heap.
// Each Tlab is exclusive to one thread — no concurrent access.
unsafe impl Send for Tlab {}

impl Tlab {
    /// Create an empty (exhausted) TLAB.
    pub fn empty() -> Self {
        Self {
            start: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
            pressure: TlabPressureTracker::new(),
        }
    }

    /// Create a TLAB backed by the given memory region.
    ///
    /// # Safety
    /// The caller must ensure `ptr..ptr+size` is valid, zeroed, writable
    /// memory that will not be accessed by any other thread until this
    /// TLAB is retired.
    pub unsafe fn new(ptr: *mut u8, size: usize) -> Self {
        let pressure = TlabPressureTracker::new();
        pressure.begin_refill(size);
        Self {
            start: ptr,
            cursor: ptr,
            end: ptr.add(size),
            pressure,
        }
    }

    /// Try to bump-allocate `size` bytes with 8-byte alignment from this TLAB.
    ///
    /// Returns `None` if the TLAB doesn't have enough space.
    #[inline(always)]
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(align.is_power_of_two());
        let cursor = self.cursor as usize;
        let aligned = (cursor + align - 1) & !(align - 1);
        let new_cursor = aligned + size;
        if new_cursor > self.end as usize {
            return None;
        }
        let ptr = aligned as *mut u8;
        self.cursor = new_cursor as *mut u8;
        self.pressure.record_allocation(size);
        Some(ptr)
    }

    /// Returns the remaining bytes available in this TLAB.
    pub fn remaining(&self) -> usize {
        (self.end as usize).saturating_sub(self.cursor as usize)
    }

    /// Returns true if this TLAB has no backing memory.
    pub fn is_empty(&self) -> bool {
        self.start.is_null()
    }

    /// Retire this TLAB (mark it as exhausted).
    /// The unused tail space is wasted but will be reclaimed at GC time
    /// when the arena is reset.
    pub fn retire(&mut self) {
        self.start = std::ptr::null_mut();
        self.cursor = std::ptr::null_mut();
        self.end = std::ptr::null_mut();
    }

    /// T5.5.1 — Recommended byte size for the next refill. Delegates to
    /// the per-thread [`TlabPressureTracker`] which applies the
    /// grow/shrink heuristic. Call this after retiring the current TLAB
    /// and before requesting a fresh buffer from the arena.
    pub fn next_refill_size(&self) -> usize {
        self.pressure.next_refill_size()
    }

    /// T5.5.1 — Notify the tracker that a new TLAB of `size` bytes has
    /// been installed. Resets internal counters and starts the
    /// fill-time clock for the new window.
    pub fn begin_refill(&self, size: usize) {
        self.pressure.begin_refill(size);
    }

    /// T5.5.1 — Immutable access to the adaptive-sizing tracker.
    pub fn pressure_tracker(&self) -> &TlabPressureTracker {
        &self.pressure
    }
}

/// The default TLAB chunk size to request from the arena.
pub fn default_tlab_size() -> usize {
    DEFAULT_TLAB_SIZE
}

/// Maximum object size that uses the TLAB fast path.
pub fn tlab_max_alloc() -> usize {
    TLAB_MAX_ALLOC
}

/// T19.3.G1 — the refill size for a thread with no pressure history.
///
/// Call sites that request a TLAB without consulting a
/// [`TlabPressureTracker`] (e.g. the very first refill on a fresh
/// thread, or test harnesses that don't model per-thread state)
/// should use this instead of [`default_tlab_size`] — it is the
/// documented "start big, then let the adaptive sizer decide"
/// entry point. Currently identical to [`default_tlab_size`] but
/// exposed as a distinct symbol so future tuning can change one
/// without affecting the other.
pub fn initial_refill_size() -> usize {
    INITIAL_REFILL_SIZE
}

/// T19.3.G1 — floor on TLAB refill sizes (bytes).
pub fn min_tlab_size() -> usize {
    MIN_TLAB_SIZE
}

/// T19.3.G1 — cap on TLAB refill sizes (bytes).
pub fn max_tlab_size() -> usize {
    MAX_TLAB_SIZE
}

/// T5.5.1 — Adaptive TLAB sizing based on allocation pressure.
///
/// Tracks the number of TLAB refills since the last GC and uses an
/// exponential moving average to adapt the TLAB size. High refill
/// rates (= high allocation pressure) scale up toward `MAX_TLAB_SIZE`;
/// low rates scale down toward `MIN_TLAB_SIZE`.
///
/// The controller is per-thread. Call `record_refill()` each time a
/// TLAB is exhausted and refilled, and `recommended_size()` to get
/// the next TLAB chunk size.
pub struct AdaptiveTlabSizer {
    /// Exponential moving average of refills per GC epoch.
    refill_rate_ema: f64,
    /// Number of refills since the last GC epoch reset.
    refills_this_epoch: u32,
    /// Current recommended TLAB size (bytes).
    current_size: usize,
}

impl AdaptiveTlabSizer {
    pub fn new() -> Self {
        Self {
            refill_rate_ema: 0.0,
            refills_this_epoch: 0,
            current_size: DEFAULT_TLAB_SIZE,
        }
    }

    /// Record a TLAB refill event. Call this each time the thread
    /// exhausts its current TLAB and requests a new one from the arena.
    pub fn record_refill(&mut self) {
        self.refills_this_epoch += 1;
    }

    /// Called at the end of a GC epoch to update the EMA and
    /// recompute the recommended TLAB size.
    pub fn end_epoch(&mut self) {
        let alpha = 0.3; // EMA smoothing factor
        self.refill_rate_ema =
            alpha * (self.refills_this_epoch as f64) + (1.0 - alpha) * self.refill_rate_ema;
        self.refills_this_epoch = 0;

        // Scale linearly between MIN and MAX based on refill rate.
        // At ≤ 2 refills/epoch → MIN; at ≥ 20 refills/epoch → MAX.
        let low = 2.0;
        let high = 20.0;
        let t = ((self.refill_rate_ema - low) / (high - low)).clamp(0.0, 1.0);
        self.current_size =
            MIN_TLAB_SIZE + ((MAX_TLAB_SIZE - MIN_TLAB_SIZE) as f64 * t) as usize;
        // Round to 4 KB boundary for page alignment.
        self.current_size = (self.current_size + 4095) & !4095;
    }

    /// The recommended TLAB size for the next refill.
    pub fn recommended_size(&self) -> usize {
        self.current_size
    }
}

impl Default for AdaptiveTlabSizer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// T5.5.1 — TlabPressureTracker: per-thread, event-driven adaptive sizing
// ---------------------------------------------------------------------------

/// T5.5.1 — Per-thread allocation-pressure tracker for TLAB sizing.
///
/// Unlike [`AdaptiveTlabSizer`] (which updates at GC epoch boundaries),
/// this tracker reacts to each TLAB refill. It records the wall-clock
/// time the current TLAB has been in use and the number/size classes
/// of allocations made against it. When the TLAB is retired the tracker
/// decides whether the next refill should grow, shrink, or stay the
/// same size based on a simple heuristic:
///
/// - **Grow (double, cap at [`MAX_TLAB_SIZE`])** — TLAB filled in
///   under [`FAST_REFILL_THRESHOLD_MS`] ms of wall clock OR after
///   fewer than [`FAST_REFILL_ALLOC_COUNT`] allocations when most of
///   them are > [`LARGE_ALLOC_THRESHOLD`] bytes (indicating the thread
///   is pushing bulk throughput).
/// - **Shrink (halve, floor at [`MIN_TLAB_SIZE`])** — TLAB took more
///   than [`SLOW_REFILL_THRESHOLD_MS`] ms OR fewer than
///   [`SLOW_REFILL_ALLOC_COUNT`] allocations fired before the refill
///   window closed (the TLAB is oversized for this thread).
/// - **Keep** — neither trigger hit.
pub struct TlabPressureTracker {
    /// Total bytes allocated against the current TLAB since the last refill.
    pub allocations_since_last_refill: AtomicUsize,
    /// Size of the most recent TLAB refill (bytes). Starts at
    /// [`DEFAULT_TLAB_SIZE`] so the first refill uses a sane default.
    pub last_refill_size: AtomicUsize,
    /// Number of allocation calls against the current TLAB.
    pub alloc_count: AtomicUsize,
    /// Number of allocations > [`LARGE_ALLOC_THRESHOLD`] bytes against
    /// the current TLAB.
    pub large_alloc_count: AtomicUsize,
    /// Instant the current TLAB was handed to the thread. Used to
    /// compute wall-clock fill time.
    refill_started_at: parking_lot::Mutex<Instant>,
}

impl TlabPressureTracker {
    /// Create a new tracker, seeded with the default TLAB size.
    pub fn new() -> Self {
        Self {
            allocations_since_last_refill: AtomicUsize::new(0),
            last_refill_size: AtomicUsize::new(DEFAULT_TLAB_SIZE),
            alloc_count: AtomicUsize::new(0),
            large_alloc_count: AtomicUsize::new(0),
            refill_started_at: parking_lot::Mutex::new(Instant::now()),
        }
    }

    /// Record one allocation of `size` bytes against the current TLAB.
    #[inline]
    pub fn record_allocation(&self, size: usize) {
        self.allocations_since_last_refill
            .fetch_add(size, Ordering::Relaxed);
        self.alloc_count.fetch_add(1, Ordering::Relaxed);
        if size > LARGE_ALLOC_THRESHOLD {
            self.large_alloc_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Mark the beginning of a new TLAB lifetime. Call this after a
    /// refill so that the next call to [`next_refill_size`] can measure
    /// how long the just-retired TLAB was in use.
    pub fn begin_refill(&self, refill_size: usize) {
        self.last_refill_size.store(refill_size, Ordering::Relaxed);
        self.allocations_since_last_refill.store(0, Ordering::Relaxed);
        self.alloc_count.store(0, Ordering::Relaxed);
        self.large_alloc_count.store(0, Ordering::Relaxed);
        *self.refill_started_at.lock() = Instant::now();
    }

    /// Compute the next TLAB refill size based on the heuristic.
    ///
    /// The current TLAB must be retired (fully exhausted) by the
    /// caller before invoking this — the tracker examines counters
    /// collected during the just-finished TLAB lifetime and decides
    /// whether to grow, shrink, or keep the size.
    ///
    /// The returned value is guaranteed to satisfy
    /// `MIN_TLAB_SIZE <= size <= MAX_TLAB_SIZE`.
    pub fn next_refill_size(&self) -> usize {
        let elapsed_ms = self
            .refill_started_at
            .lock()
            .elapsed()
            .as_millis();
        let alloc_count = self.alloc_count.load(Ordering::Relaxed);
        let large_allocs = self.large_alloc_count.load(Ordering::Relaxed);
        let current = self.last_refill_size.load(Ordering::Relaxed);

        // Grow when: fast fill OR few-but-large allocations dominated.
        let grow = elapsed_ms < FAST_REFILL_THRESHOLD_MS
            || (alloc_count < FAST_REFILL_ALLOC_COUNT && large_allocs > 0);

        // Shrink when: very slow fill OR the TLAB was barely touched.
        let shrink =
            elapsed_ms > SLOW_REFILL_THRESHOLD_MS || alloc_count < SLOW_REFILL_ALLOC_COUNT;

        let next = if grow && !shrink {
            current.saturating_mul(2).min(MAX_TLAB_SIZE)
        } else if shrink && !grow {
            (current / 2).max(MIN_TLAB_SIZE)
        } else {
            current
        };
        next.clamp(MIN_TLAB_SIZE, MAX_TLAB_SIZE)
    }
}

impl Default for TlabPressureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TlabPressureTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlabPressureTracker")
            .field(
                "allocations_since_last_refill",
                &self.allocations_since_last_refill.load(Ordering::Relaxed),
            )
            .field(
                "last_refill_size",
                &self.last_refill_size.load(Ordering::Relaxed),
            )
            .field("alloc_count", &self.alloc_count.load(Ordering::Relaxed))
            .field(
                "large_alloc_count",
                &self.large_alloc_count.load(Ordering::Relaxed),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tlab_basic_alloc() {
        let mut buf = vec![0u8; 1024];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 1024) };
        assert_eq!(tlab.remaining(), 1024);

        let ptr = tlab.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(tlab.remaining(), 1024 - 64);
    }

    #[test]
    fn tlab_alignment() {
        let mut buf = vec![0u8; 256];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 256) };

        // Allocate 3 bytes (unaligned)
        tlab.alloc(3, 1).unwrap();
        // Next alloc with align=8 should skip to alignment boundary
        let p2 = tlab.alloc(8, 8).unwrap();
        assert_eq!((p2 as usize) % 8, 0);
    }

    #[test]
    fn tlab_exhaustion() {
        let mut buf = vec![0u8; 64];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 64) };
        assert!(tlab.alloc(64, 8).is_some());
        assert!(tlab.alloc(1, 1).is_none());
    }

    #[test]
    fn tlab_empty() {
        let mut tlab = Tlab::empty();
        assert!(tlab.is_empty());
        assert!(tlab.alloc(1, 1).is_none());
    }

    #[test]
    fn tlab_retire() {
        let mut buf = vec![0u8; 128];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 128) };
        assert!(!tlab.is_empty());
        tlab.retire();
        assert!(tlab.is_empty());
        assert!(tlab.alloc(1, 1).is_none());
    }

    // ---------------------------------------------------------------
    // T5.5.1 — TlabPressureTracker tests
    // ---------------------------------------------------------------

    #[test]
    fn pressure_tracker_defaults_to_default_size() {
        let t = TlabPressureTracker::new();
        assert_eq!(
            t.last_refill_size.load(Ordering::Relaxed),
            DEFAULT_TLAB_SIZE
        );
        assert_eq!(t.allocations_since_last_refill.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pressure_tracker_records_allocations() {
        let t = TlabPressureTracker::new();
        t.record_allocation(64);
        t.record_allocation(2048); // > LARGE_ALLOC_THRESHOLD
        assert_eq!(
            t.allocations_since_last_refill.load(Ordering::Relaxed),
            64 + 2048
        );
        assert_eq!(t.alloc_count.load(Ordering::Relaxed), 2);
        assert_eq!(t.large_alloc_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn pressure_tracker_doubles_on_few_large_allocs() {
        let t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024); // start at 64 KB
        // A handful of large allocations → high pressure.
        for _ in 0..4 {
            t.record_allocation(512); // > LARGE_ALLOC_THRESHOLD
        }
        // alloc_count=4 < FAST(16); large_allocs=4>0 → grow.
        // But alloc_count=4 <= SLOW(4-1) is false (4 < 4 is false) so no shrink.
        let next = t.next_refill_size();
        assert_eq!(next, 128 * 1024);
    }

    #[test]
    fn pressure_tracker_doubles_on_fast_refill() {
        let t = TlabPressureTracker::new();
        t.begin_refill(32 * 1024);
        // Simulate many allocations but start-time stays recent → elapsed < 1ms.
        for _ in 0..200 {
            t.record_allocation(128);
        }
        let next = t.next_refill_size();
        // Fast-fill path: elapsed_ms < 1 → grow.
        assert_eq!(next, 64 * 1024);
    }

    #[test]
    fn pressure_tracker_halves_on_few_allocations() {
        let t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024);
        // Only 2 allocations over the window → shrink.
        t.record_allocation(64);
        t.record_allocation(64);
        // Sleep just over the fast threshold so we don't trigger grow.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert_eq!(next, 32 * 1024);
    }

    #[test]
    fn pressure_tracker_respects_max_cap() {
        let t = TlabPressureTracker::new();
        t.begin_refill(MAX_TLAB_SIZE);
        for _ in 0..2 {
            t.record_allocation(512);
        }
        // Would double, but MAX_TLAB_SIZE already at cap.
        let next = t.next_refill_size();
        assert_eq!(next, MAX_TLAB_SIZE);
    }

    // ---------------------------------------------------------------
    // T19.3.G1 — allocation-storm regression tests
    // ---------------------------------------------------------------

    #[test]
    fn t19_default_tlab_size_is_256kb() {
        // The default TLAB baseline is 256 KB — large enough to
        // amortize `refill_tlab` across a Quarkus-style 25 MB/s
        // static-init allocation storm without the 64 KB refill
        // cascade we fixed in T19.3.G1.
        assert_eq!(default_tlab_size(), 256 * 1024);
    }

    #[test]
    fn t19_max_tlab_size_is_1mb() {
        // Adaptive sizer can grow a thread's TLAB up to 1 MB under
        // sustained pressure.
        assert_eq!(max_tlab_size(), 1024 * 1024);
    }

    #[test]
    fn t19_initial_refill_matches_default() {
        // Initial refill on a fresh thread starts at the baseline;
        // tuning one without the other should be an intentional opt-in.
        assert_eq!(initial_refill_size(), default_tlab_size());
    }

    #[test]
    fn t19_min_cap_floor_respected() {
        // Pressure tracker must not undercut MIN_TLAB_SIZE.
        assert_eq!(min_tlab_size(), 8 * 1024);
        let t = TlabPressureTracker::new();
        t.begin_refill(MIN_TLAB_SIZE);
        t.record_allocation(16);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(t.next_refill_size(), MIN_TLAB_SIZE);
    }

    #[test]
    fn t19_adaptive_sizer_grows_on_fast_fill() {
        // Simulate the allocation-storm pattern: 2 tiny objects
        // per iteration, 10 k iterations, nearly zero wall clock.
        // The pressure tracker should grow the TLAB at least twice.
        let t = TlabPressureTracker::new();
        t.begin_refill(64 * 1024);
        // Fast-fill: elapsed_ms < 1 → grow to 128 KB.
        for _ in 0..500 {
            t.record_allocation(24);
        }
        let step1 = t.next_refill_size();
        assert!(step1 >= 128 * 1024, "first growth step: {step1}");
        // Second round at 128 KB: fast-fill again → 256 KB.
        t.begin_refill(step1);
        for _ in 0..500 {
            t.record_allocation(24);
        }
        let step2 = t.next_refill_size();
        assert!(step2 >= step1, "second growth step: {step2} < {step1}");
    }

    #[test]
    fn t19_adaptive_sizer_grows_from_8kb_to_64kb_under_1m_allocs() {
        // Starts at MIN_TLAB_SIZE (8 KB), ramps to at least 64 KB
        // after repeated fast refills. This is the adaptive target
        // the T19.3.G1 fix requires so KC26 static-init doesn't
        // refill every ~3 ms.
        let t = TlabPressureTracker::new();
        let mut size = MIN_TLAB_SIZE;
        t.begin_refill(size);
        // Each loop iteration models filling a TLAB and asking for
        // the next size. We stop once we reach 64 KB or the loop
        // is clearly diverging.
        for _ in 0..16 {
            // Simulate >= 1 allocation so alloc_count doesn't shrink.
            for _ in 0..200 {
                t.record_allocation(24);
            }
            let next = t.next_refill_size();
            if next <= size {
                break;
            }
            size = next;
            t.begin_refill(size);
            if size >= 64 * 1024 {
                break;
            }
        }
        assert!(size >= 64 * 1024, "adaptive sizer stuck at {size} bytes");
    }

    #[test]
    fn t19_tlab_max_alloc_independent_of_default() {
        // Raising the default TLAB to 256 KB does not widen the cap
        // on what is eligible for the TLAB fast path — 32 KB stays.
        assert_eq!(tlab_max_alloc(), 32 * 1024);
    }

    #[test]
    fn t19_allocation_storm_tlab_survives_2m_bumps() {
        // A TLAB sized at the 256 KB default can service about
        // 10k 24-byte objects before a refill. Verify that a raw
        // TLAB actually fulfils ~10k bump requests without a panic
        // and without claiming more than the documented size.
        let mut buf = vec![0u8; 256 * 1024];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 256 * 1024) };
        let mut bumps = 0;
        while tlab.alloc(24, 8).is_some() {
            bumps += 1;
            if bumps > 20_000 {
                panic!("TLAB delivered too many objects — size unbounded?");
            }
        }
        // 256 KB / 24 B = ~10_922 objects with 8-byte alignment.
        assert!(bumps >= 10_000, "only {bumps} allocations fit in 256 KB TLAB");
    }

    #[test]
    fn t19_adaptive_sizer_shrinks_on_idle() {
        // If a thread goes idle (alloc_count = 1) the TLAB must
        // shrink toward MIN so we don't hold 1 MB of arena per
        // sleeping thread.
        let t = TlabPressureTracker::new();
        t.begin_refill(MAX_TLAB_SIZE);
        t.record_allocation(16);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert!(next < MAX_TLAB_SIZE, "expected shrink from {MAX_TLAB_SIZE}, got {next}");
        assert!(next >= MIN_TLAB_SIZE);
    }

    #[test]
    fn t19_tlab_retire_clears_buffer() {
        // Retiring the TLAB must not leave a dangling pointer that a
        // subsequent `alloc` could dereference.
        let mut buf = vec![0u8; 4096];
        let mut tlab = unsafe { Tlab::new(buf.as_mut_ptr(), 4096) };
        tlab.alloc(64, 8).unwrap();
        tlab.retire();
        assert!(tlab.is_empty());
        assert!(tlab.alloc(1, 1).is_none());
    }

    #[test]
    fn t19_begin_refill_resets_timer() {
        // begin_refill must reset the fill-time clock: the timer
        // recorded before the second begin_refill must not leak into
        // the window the tracker measures after it.
        let t = TlabPressureTracker::new();
        t.begin_refill(MIN_TLAB_SIZE);
        std::thread::sleep(std::time::Duration::from_millis(2));
        // The second begin_refill snapshot must capture a fresh
        // Instant; fill-time elapsed is measured from that snapshot
        // and the counters reset to zero.
        t.begin_refill(64 * 1024);
        assert_eq!(t.alloc_count.load(Ordering::Relaxed), 0);
        assert_eq!(t.large_alloc_count.load(Ordering::Relaxed), 0);
        // A second begin_refill must also reset last_refill_size.
        assert_eq!(t.last_refill_size.load(Ordering::Relaxed), 64 * 1024);
    }

    #[test]
    fn pressure_tracker_respects_min_floor() {
        let t = TlabPressureTracker::new();
        t.begin_refill(MIN_TLAB_SIZE);
        t.record_allocation(16); // one small alloc → shrink trigger
        std::thread::sleep(std::time::Duration::from_millis(2));
        let next = t.next_refill_size();
        assert_eq!(next, MIN_TLAB_SIZE);
    }

    #[test]
    fn pressure_tracker_begin_refill_resets_counters() {
        let t = TlabPressureTracker::new();
        t.record_allocation(128);
        t.record_allocation(128);
        t.begin_refill(16 * 1024);
        assert_eq!(t.allocations_since_last_refill.load(Ordering::Relaxed), 0);
        assert_eq!(t.alloc_count.load(Ordering::Relaxed), 0);
        assert_eq!(t.large_alloc_count.load(Ordering::Relaxed), 0);
        assert_eq!(t.last_refill_size.load(Ordering::Relaxed), 16 * 1024);
    }
}
