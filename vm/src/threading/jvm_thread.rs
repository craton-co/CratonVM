// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread JVM execution state.
//!
//! Each Java thread has its own `JvmThread` containing:
//! - Call stack (for stack traces)
//! - Test output buffer (`printed`)
//! - Thread identity and flags
//
// T1.8.2 — production-code panic gate. Per-thread state is on the
// hottest paths in the VM; an `.unwrap()` here would panic the entire
// runtime. Test code is allowed unwraps so the gate is opt-out under
// `#[cfg(test)]`.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,)
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar as PLCondvar, Mutex as PLMutex};

use crate::classloading::resolution::InvokeCache;
use crate::runtime::frame::{Frame, FrameStack};
use crate::runtime::fx_collections::FxHashMap;
use crate::types::{ObjectRef, Value};
use cratonvm_types::ClassId;

/// Pool type for SoA locals and stack vecs: (values, tags).
pub type SoaPool = Vec<(Vec<u64>, Vec<u8>)>;

/// A two-entry per-thread cache for repeated ASCII case conversion.  Returning
/// alternating immutable results avoids the observable same-object shortcut
/// while eliminating allocation/collection in hot use-and-discard patterns.
#[derive(Clone)]
pub struct StringCaseCacheEntry {
    pub source: ObjectRef,
    pub locale: Option<ObjectRef>,
    pub upper: bool,
    pub first: ObjectRef,
    pub second: ObjectRef,
    pub next: bool,
}

#[derive(Clone)]
pub struct JitHashMapStringNodeCacheEntry {
    pub map: ObjectRef,
    pub node: ObjectRef,
    /// Exact String object passed to a HashMap or ConcurrentHashMap get. This
    /// is rooted alongside the map/node pair and avoids content scans.
    pub key_object: Option<ObjectRef>,
    pub key: String,
    pub mod_count_slot: usize,
    pub mod_count: i32,
    /// `Some` selects the ConcurrentHashMap segment seqlock validity rule;
    /// `None` retains the ordinary HashMap `modCount` rule above.
    pub chm_generation: Option<u64>,
    /// Stable identity hash of the CHM segment owning `node` when this is a
    /// CHM entry. Storing the integer avoids a second GC root per cache entry.
    pub chm_segment_id: Option<i32>,
}

// Pool size limits — prevent unbounded growth
const MAX_POOL_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// GcBlockState — blocked-region GC maintenance state (shared with registry)
// ---------------------------------------------------------------------------

/// Blocked-region GC state for one thread, shared `JvmThread` ↔ `ThreadRegistry`
/// (same pattern as `root_snapshot`).
///
/// A thread parked in a blocking native (`Object.wait`, `Thread.join`,
/// `LockSupport.park`, `ReferenceQueue.remove`, …) is excluded from the
/// stop-the-world barrier (`GcBarrier::threads_blocked`), so any number of
/// GCs can complete while it sleeps. Two things would otherwise go stale:
///
/// 1. its deposited `root_snapshot` — scanned as roots by every GC; after the
///    first missed *moving* collection the snapshot addresses point into a
///    vacated semispace, and once that space cycles back around and is
///    collected again the collector evacuates garbage "objects" through
///    them (writing forwarding state into the interior of innocent live
///    objects — the H2 TestScript stale-receiver SEGV);
/// 2. its frames — `check_post_block_gc` only applied the pointer map when a
///    STW was active at the exact wake instant; a GC that completed mid-block
///    left every local/operand-stack ref pointing at recycled from-space.
///
/// The GC initiator therefore calls
/// `ThreadRegistry::fold_pointer_map_into_blocked` (`update_all_roots`
/// step 20, under STW) for every thread whose `in_blocked_region` flag is
/// set: it remaps the thread's `root_snapshot` in place and composes the
/// GC's pointer map into `fixup` (chaining `orig → cur → new` across
/// multiple missed GCs, keyed by the address the frames still hold). On
/// wake the thread applies and clears `fixup` in `check_post_block_gc`.
/// cceres3: one precisely-tracked frame slot for the blocked window — see
/// `GcBlockState::slot_origins`.
#[derive(Clone, Copy)]
pub struct SlotOrigin {
    pub frame: u32,
    pub idx: u32,
    pub is_stack: bool,
    /// Was this slot LIVE at its frame's pc when the deposit ran — i.e. one of
    /// the roots the collector was required to keep?
    ///
    /// The write-back does not need this (healing a dead slot is harmless and
    /// keeps the frame self-consistent), but the fold's invariant check does:
    /// a DEAD local pointing into a reclaimed span is the per-bci liveness
    /// analysis working exactly as designed — `ExecutorService.invokeAll`'s
    /// `tasks` argument, scoped out at the `f.get()` the caller is parked in,
    /// hits this on every round of `probes/BlockedFrameRootProbe`. Reporting
    /// those would bury the case that matters. Operand-stack slots are always
    /// live.
    pub live: bool,
    /// Address the slot held at the blocking deposit.
    pub orig: usize,
    /// The object's current address, advanced by every GC initiator's fold
    /// through that collection's pointer map (exact per-map lookup).
    pub cur: usize,
}

pub struct GcBlockState {
    /// True from `deposit_root_snapshot` (just before the thread blocks)
    /// until the end of `check_post_block_gc` (after the fixup is applied).
    pub in_blocked_region: AtomicBool,
    /// Java-visible blocking kind while `in_blocked_region` is true:
    /// 1 = WAITING (wait/park/join), 2 = BLOCKED (monitor acquisition).
    /// The GC protocol only needs the boolean above; preserving this small
    /// distinction lets `Thread.getState()` report the JDK state correctly.
    pub java_state: AtomicU8,
    /// Composed `frame-held address → current address` map accumulated by GC
    /// initiators for every collection that completed while the thread was
    /// in a blocked region. Applied + cleared on wake.
    pub fixup: PLMutex<cratonvm_types::PointerMap>,
    /// cceres3 (WildFly boot stale-frame family): exact per-slot tracking for
    /// the blocked window. `fixup` above is keyed by the address the frames
    /// held when each object FIRST moved — a chain that breaks if any link's
    /// seed was missed (filtered snapshot, multi-block chains, recycled-address
    /// ABA), permanently stranding the slot. Each entry here instead pins down
    /// one (frame, slot) with the address it held at the blocking deposit;
    /// folds advance `cur` with an exact per-collection lookup and the wake
    /// write-back stores `cur` straight into the slot. Filled only by the
    /// flag-raising deposit; taken (and cleared) on wake.
    pub slot_origins: PLMutex<Vec<SlotOrigin>>,
}

impl GcBlockState {
    pub fn new() -> Self {
        Self {
            in_blocked_region: AtomicBool::new(false),
            java_state: AtomicU8::new(0),
            fixup: PLMutex::new(cratonvm_types::PointerMap::default()),
            slot_origins: PLMutex::new(Vec::new()),
        }
    }
}

impl Default for GcBlockState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// win_park — accurate timed parking on Windows
// ---------------------------------------------------------------------------

/// Windows timed waits are aligned to the system clock tick (15.625 ms by
/// default), and a parked thread's *deadline* inherits that alignment:
/// `WaitForSingleObject`, `SleepConditionVariableSRW`, a plain waitable timer
/// and `parking_lot`'s condvar all return at the first tick at or after the
/// requested instant, so a 50 ms wait measures 62.5 ms. `timeBeginPeriod(1)`
/// does NOT lift it — measured on this host, every one of those primitives sat
/// at p50 = 62.3 ms for a 50 ms request both before and after that call, with
/// the system-wide resolution already reported as 1 ms.
///
/// The one primitive that is accurate is a waitable timer created with
/// `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803+): p50 = 50.3 ms
/// for the same request. It is what Rust's own `std::thread::sleep` uses,
/// which is why `Thread.sleep` was already accurate here while every
/// `LockSupport.parkNanos` deadline was not.
///
/// The visible cost of the tick alignment is not a slow park — the mean is
/// right, because a scheduler like netty's re-arms from an ABSOLUTE deadline —
/// it is a *periodic* one: four short cycles then one long one. A 50 ms
/// fixed-rate task fires at 62.5, 109.4, 156.3, 203.1, 250.0 ms, i.e. gaps of
/// 46.9 ms x4 then 62.5 ms. `AutoScalingEventExecutorChooserFactoryTest`
/// polls its group every 50 ms and asserts on a state its monitor holds for
/// exactly one cycle; a 46.9 ms cycle is shorter than the poll, so the state
/// can be stepped over entirely and the test reads the NEXT one.
///
/// So the wait is bounded by a high-resolution timer and the unpark signal by
/// an auto-reset event, and both are waited on together.
/// `CRATONVM_WIN_HIRES_PARK=0` reverts to the condvar path.
#[cfg(target_os = "windows")]
pub(crate) mod win_park {
    use std::time::Duration;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(
            attrs: *mut core::ffi::c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> isize;
        fn SetEvent(handle: isize) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn CreateWaitableTimerExW(
            attrs: *mut core::ffi::c_void,
            name: *const u16,
            flags: u32,
            desired_access: u32,
        ) -> isize;
        fn SetWaitableTimer(
            timer: isize,
            due_time: *const i64,
            period: i32,
            routine: *mut core::ffi::c_void,
            arg: *mut core::ffi::c_void,
            resume: i32,
        ) -> i32;
        fn CancelWaitableTimer(timer: isize) -> i32;
        fn WaitForMultipleObjects(
            count: u32,
            handles: *const isize,
            wait_all: i32,
            millis: u32,
        ) -> u32;
    }

    const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x0000_0002;
    const TIMER_ALL_ACCESS: u32 = 0x001F_0003;
    const INFINITE: u32 = 0xFFFF_FFFF;

    /// `CRATONVM_WIN_HIRES_PARK=0` reverts every timed park to the condvar
    /// path this module replaces (the pre-2026-08-29 behaviour).
    pub(crate) fn enabled() -> bool {
        use std::sync::OnceLock;
        static G: OnceLock<bool> = OnceLock::new();
        *G.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_WIN_HIRES_PARK")
                .map(|v| v != *"0")
                .unwrap_or(true)
        })
    }

    /// An auto-reset event for one `ParkState`, or 0 if it could not be made
    /// (every caller then takes the condvar path).
    pub(crate) fn create_wake_event() -> isize {
        if !enabled() {
            return 0;
        }
        // SAFETY: a documented Win32 call with a null security descriptor and
        // a null name; it only creates a kernel object and returns its handle.
        unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) }
    }

    /// Release a handle from `create_wake_event`.
    pub(crate) fn close(handle: isize) {
        if handle != 0 {
            // SAFETY: `handle` came from `CreateEventW` above and is closed
            // exactly once, from `ParkState`'s `Drop`.
            unsafe { CloseHandle(handle) };
        }
    }

    /// Signal a parked thread's wake event. A signal delivered when nobody is
    /// waiting leaves the event set, so the next timed park returns at once —
    /// which is exactly the permit semantics `unpark` already has, and a
    /// spurious return is in any case what `LockSupport.park` allows.
    pub(crate) fn signal(handle: isize) {
        if handle != 0 {
            // SAFETY: `handle` is a live auto-reset event owned by the
            // `ParkState` this call reached through an `Arc`.
            unsafe { SetEvent(handle) };
        }
    }

    thread_local! {
        /// One high-resolution timer per parking thread, reused across parks.
        /// `-1` means "not probed yet"; `0` means the OS refused one, so the
        /// probe runs once per thread and never again.
        static HIRES_TIMER: std::cell::Cell<isize> = const { std::cell::Cell::new(-1) };
    }

    /// This thread's high-resolution timer, or 0 if the OS has none.
    fn timer() -> isize {
        HIRES_TIMER.with(|c| {
            let cached = c.get();
            if cached != -1 {
                return cached;
            }
            // SAFETY: documented Win32 call, null attributes and null name.
            let h = unsafe {
                CreateWaitableTimerExW(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
            };
            c.set(h);
            h
        })
    }

    /// Wait until `dur` elapses or `wake_event` is signalled, whichever comes
    /// first. Returns `false` when the accurate path is unavailable and the
    /// caller must fall back to the condvar.
    pub(crate) fn wait_until(wake_event: isize, dur: Duration) -> bool {
        if wake_event == 0 || !enabled() {
            return false;
        }
        let timer = timer();
        if timer == 0 {
            return false;
        }
        // A negative due time is a RELATIVE interval in 100 ns units.
        let hundred_nanos = (dur.as_nanos() / 100).min(i64::MAX as u128) as i64;
        let due: i64 = -hundred_nanos.max(1);
        // SAFETY: `timer` is this thread's live timer handle and `due` points
        // at a live local for the duration of the call.
        let armed = unsafe {
            SetWaitableTimer(
                timer,
                &due,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if armed == 0 {
            return false;
        }
        let handles = [wake_event, timer];
        // SAFETY: both handles are live for the call; `handles` is a valid
        // two-element array and the count matches.
        unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        // SAFETY: same live timer handle; cancelling an already-signalled
        // timer is defined and leaves it unsignalled for the next park.
        unsafe { CancelWaitableTimer(timer) };
        true
    }
}

// ---------------------------------------------------------------------------
// ParkState — binary semaphore for LockSupport.park() / unpark()
// ---------------------------------------------------------------------------

/// Per-thread parking permit for `LockSupport.park()` / `unpark()`.
///
/// Implements a binary semaphore: `unpark()` sets the permit, `park()` consumes
/// it (or blocks until one is available). If `unpark()` is called before
/// `park()`, the next `park()` returns immediately.
pub struct ParkState {
    mutex: PLMutex<bool>,
    condvar: PLCondvar,
    /// DBG (`CRATONVM_DBG_PARKLAT`): monotonic nanos of the most recent
    /// `unpark()` that SET the permit (0 = none). `park_interruptible`
    /// reads it on wake-with-permit and reports the unpark→wake latency
    /// when it exceeds a threshold, separating "the signal was generated
    /// late" (Java-side / protocol) from "the signal was delivered late"
    /// (VM park machinery) in the RRWL crawl/join-stall investigation.
    last_unpark_nanos: std::sync::atomic::AtomicU64,
    /// Windows only: auto-reset event this thread's timed parks wait on
    /// alongside a high-resolution timer, so the deadline is not rounded up
    /// to the 15.625 ms system tick. 0 when the event could not be created
    /// or `CRATONVM_WIN_HIRES_PARK=0` turned the path off; every timed park
    /// then takes the condvar exactly as before. See `win_park` above.
    #[cfg(target_os = "windows")]
    wake_event: isize,
}

#[cfg(target_os = "windows")]
impl Drop for ParkState {
    fn drop(&mut self) {
        win_park::close(self.wake_event);
    }
}

/// Cached `CRATONVM_DBG_PARKLAT` gate.
#[inline]
fn parklat_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_PARKLAT").is_some())
}

/// Monotonic nanos for the PARKLAT diagnostic (process-relative).
#[inline]
fn parklat_now_nanos() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}

impl ParkState {
    /// Create a new ParkState with no permit available.
    pub fn new() -> Self {
        Self {
            mutex: PLMutex::new(false),
            condvar: PLCondvar::new(),
            last_unpark_nanos: std::sync::atomic::AtomicU64::new(0),
            #[cfg(target_os = "windows")]
            wake_event: win_park::create_wake_event(),
        }
    }

    /// Consume the permit or block until one is available.
    ///
    /// If a permit is available (from a prior `unpark()`), consumes it and
    /// returns immediately. Otherwise, blocks until `unpark()` is called or
    /// the timeout expires.
    pub fn park(&self, timeout: Option<std::time::Duration>) {
        let mut permit = self.mutex.lock();
        if *permit {
            *permit = false;
            return;
        }
        match timeout {
            Some(dur) if !dur.is_zero() => {
                // Windows: an accurate deadline needs a high-resolution timer,
                // and staying wakeable by `unpark` needs the event waited on
                // beside it — the condvar can be neither. The lock is released
                // first because `unpark` takes it to set the permit and signals
                // the event afterwards, so a signal landing in the gap leaves
                // the auto-reset event set and the wait returns at once.
                #[cfg(target_os = "windows")]
                if win_park::enabled() && self.wake_event != 0 {
                    drop(permit);
                    let waited = win_park::wait_until(self.wake_event, dur);
                    let mut permit = self.mutex.lock();
                    if !waited {
                        // No high-resolution timer on this OS build.
                        self.condvar.wait_for(&mut permit, dur);
                    }
                    *permit = false;
                    return;
                }
                self.condvar.wait_for(&mut permit, dur);
            }
            None => {
                self.condvar.wait(&mut permit);
            }
            _ => {} // zero timeout = no-op
        }
        *permit = false; // consume permit even on timeout/spurious wakeup
    }

    /// Consume the permit or block until one is available, with interrupt awareness.
    ///
    /// Like `park()`, but periodically checks the interrupt flag so that
    /// `Thread.interrupt()` can break a parked thread out of the wait.
    pub fn park_interruptible(
        &self,
        timeout: Option<std::time::Duration>,
        interrupted: &std::sync::atomic::AtomicBool,
    ) {
        let mut permit = self.mutex.lock();
        if *permit {
            *permit = false;
            return;
        }
        // Check interrupt before blocking
        if interrupted.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let poll = std::time::Duration::from_millis(5);
        match timeout {
            Some(dur) if !dur.is_zero() => {
                let deadline = std::time::Instant::now() + dur;
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() || *permit {
                        break;
                    }
                    // Windows: same reason as `park()` above — the condvar
                    // rounds the deadline up to the 15.625 ms system tick.
                    // Slices stay (they bound how long an interrupt that did
                    // NOT also unpark can go unnoticed) but each one is now
                    // accurate, so the last slice ends ON the deadline instead
                    // of at the next tick after it.
                    //
                    // The accurate slice is the longer one on purpose. A 5 ms
                    // condvar slice does not cost a wakeup every 5 ms — the OS
                    // rounds it out to ~15.6 ms — so asking the timer for 5 ms
                    // would TRIPLE the wakeup rate of every timed park in the
                    // VM to buy interrupt latency nothing was waiting on
                    // (`thread_interrupt` already unparks, and that signal now
                    // reaches this wait through the event). 15 ms keeps both
                    // the wakeup rate and the interrupt latency where the
                    // condvar path actually had them.
                    #[cfg(target_os = "windows")]
                    let slice = if win_park::enabled() && self.wake_event != 0 {
                        std::time::Duration::from_millis(15)
                    } else {
                        poll
                    };
                    #[cfg(not(target_os = "windows"))]
                    let slice = poll;
                    let wait_time = remaining.min(slice);
                    #[cfg(target_os = "windows")]
                    let accurate = if win_park::enabled() && self.wake_event != 0 {
                        drop(permit);
                        let waited = win_park::wait_until(self.wake_event, wait_time);
                        permit = self.mutex.lock();
                        waited
                    } else {
                        false
                    };
                    #[cfg(not(target_os = "windows"))]
                    let accurate = false;
                    if !accurate {
                        self.condvar.wait_for(&mut permit, wait_time);
                    }
                    if *permit || interrupted.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
            }
            None => {
                // Untimed park — poll for interrupt or unpark
                loop {
                    self.condvar.wait_for(&mut permit, poll);
                    if *permit || interrupted.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
            }
            _ => {} // zero timeout = no-op
        }
        // DBG (CRATONVM_DBG_PARKLAT): report a tardy delivery — the wake
        // observed the permit long after the unpark that set it.
        if *permit && parklat_enabled() {
            let set_at = self
                .last_unpark_nanos
                .load(std::sync::atomic::Ordering::Acquire);
            if set_at != 0 {
                let lat_ms = parklat_now_nanos().saturating_sub(set_at) / 1_000_000;
                if lat_ms >= 50 {
                    eprintln!(
                        "[parklat] unpark->wake {lat_ms}ms (thread {:?})",
                        std::thread::current().id(),
                    );
                }
            }
        }
        *permit = false;
    }

    /// Make a permit available; unblock a parked thread.
    ///
    /// If the thread is currently parked, it will be unblocked. If not,
    /// the next call to `park()` will return immediately.
    pub fn unpark(&self) {
        let mut permit = self.mutex.lock();
        *permit = true;
        if parklat_enabled() {
            self.last_unpark_nanos
                .store(parklat_now_nanos(), std::sync::atomic::Ordering::Release);
        }
        self.condvar.notify_one();
        // A thread on the accurate path is not on the condvar; wake it too.
        #[cfg(target_os = "windows")]
        win_park::signal(self.wake_event);
    }
}

impl Default for ParkState {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a thread is a platform (OS) thread or a virtual thread (JEP 444).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    /// Traditional platform thread backed by an OS thread.
    Platform,
    /// Virtual thread (Java 21+) — lightweight, scheduled onto carrier threads.
    Virtual,
}

/// Unique identifier for a JVM thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thread-{}", self.0)
    }
}

/// Per-thread execution state.
///
/// Each Java thread (including the main thread) has its own `JvmThread`.
/// This struct holds all state that is local to a single thread of execution.
pub struct JvmThread {
    /// Unique thread identifier.
    pub thread_id: ThreadId,

    /// Human-readable thread name.
    pub name: String,

    /// Live execution frames. The last element is the currently executing frame.
    /// Frames are pushed on method entry and popped on method return.
    /// Stack traces are derived from frames on demand (in capture_stack_trace).
    ///
    /// [`FrameStack`] is a drop-in for the `Vec<Frame>` this used to be — same
    /// methods, same `Index`, same iteration, and it still coerces to
    /// `&[Frame]` — but it additionally guarantees that a frame's **address**
    /// does not change while it is on the stack, except across an explicit,
    /// observable relocation (`FrameStack::reloc_epoch`). Reserving with
    /// `FrameStack::reserve_stable(n)` rules relocation out for the next `n`
    /// pushes, which is what lets the interpreter hoist a single
    /// `*mut Frame` across the dispatch loop instead of re-indexing
    /// `thread.frames[frame_idx]` on every access. See the `FrameStack` docs
    /// for the aliasing rules that come with the raw-pointer API.
    /// The heap collection count as of the last time SOMETHING remapped this
    /// thread's frames: the initiator's own `update_all_roots`, the
    /// safepoint-arrival `apply_pointer_map_to_thread`, or the blocked-region
    /// wake write-back.
    ///
    /// `CRATONVM_DBG_VACATED_FRAMES` prints it beside the heap's current count.
    /// The two disagreeing at a slot that still names a vacated address is the
    /// difference between "a heal path ran and missed this slot" and "no heal
    /// path ran for this thread at all", which want completely different fixes.
    pub last_heal_collection: u64,

    pub frames: FrameStack,

    /// Pool of reusable locals (values, tags) pairs (avoids allocation on recursive calls).
    pub locals_pool: SoaPool,

    /// Pool of reusable stack (values, tags) pairs (avoids allocation on recursive calls).
    pub stacks_pool: SoaPool,

    /// Values printed by `tempPrint` — used by integration tests.
    pub printed: Vec<Value>,

    /// Lines printed via System.out.println — captured as plain Rust strings.
    pub printed_lines: Vec<String>,

    /// Whether this is a daemon thread.
    pub daemon: bool,

    /// Thread interrupt flag (shared with ThreadRegistry for cross-thread access).
    pub interrupted: Arc<AtomicBool>,

    /// Cached Java Thread object on the heap for this thread.
    pub java_thread_obj: Option<ObjectRef>,

    /// Parking permit for LockSupport.park()/unpark().
    pub park_state: Arc<ParkState>,

    /// Root snapshot: shared with ThreadRegistry for GC root scanning across threads.
    /// Updated at safepoints and before blocking operations.
    pub root_snapshot: Arc<parking_lot::Mutex<Vec<ObjectRef>>>,

    /// Frame trace snapshot: shared with ThreadRegistry so another thread can
    /// read this thread's Java call stack (cross-thread `Thread.getStackTrace()`
    /// / `dumpThreads()`). Published at the same blocking deposit points as
    /// `root_snapshot`, so for a parked thread it reflects where it is stuck.
    /// Line-less (see `stackwalker::capture_frames_no_lines`) to stay lock-free.
    pub frame_trace: Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>,

    /// Optional diagnostic breadcrumb shared with the thread registry. When
    /// `CRATONVM_DBG_VM_STATE=1` is set, long-running VM helper paths publish a
    /// compact state here so STW census can identify non-bytecode stalls.
    pub vm_state: Arc<parking_lot::Mutex<String>>,

    /// Opt-in root-snapshot cache (`CRATONVM_ROOTSNAP_CACHE`): per *frozen*
    /// frame, `((frame.seq, frame.exec_epoch), that frame's scanned GC roots)`,
    /// indexed parallel to `frames[0..rs_cache.len()]`. Lets
    /// `update_root_snapshot` reuse the deep, continuously-frozen frames and
    /// re-scan only the churning top. Valid only while `rs_cache_gen ==
    /// heap.collection_count()` (a GC may have moved/promoted objects,
    /// invalidating the cached addresses). The key is `(seq, exec_epoch)` — NOT
    /// `seq` alone: `seq` proves the frame was never popped, but a still-present
    /// frame can RE-EXECUTE and reassign its locals; `exec_epoch` (bumped on
    /// callee-return and local-slot writes) detects that so stale roots are never
    /// reused (see `Frame::seq` / `Frame::exec_epoch`). Empty/unused when the
    /// gate is off.
    pub rs_cache: Vec<((u64, u64), Vec<ObjectRef>)>,
    /// GC collection count at which `rs_cache` was built (move/promote generation).
    pub rs_cache_gen: u64,

    /// Blocked-region GC state: shared with ThreadRegistry so a GC initiator
    /// can maintain this thread's roots while it is parked in a blocking
    /// native (`Object.wait` / `Thread.join` / `LockSupport.park` /
    /// `ReferenceQueue.remove`). See [`GcBlockState`].
    pub gc_block_state: Arc<GcBlockState>,
    /// This thread's own `ThreadEntry::jmx_locked_synchronizers` list, fetched
    /// on the first AQS ownership transition this thread performs.
    ///
    /// `AbstractOwnableSynchronizer.setExclusiveOwnerThread` is intercepted so
    /// `ThreadInfo.getLockedSynchronizers()` has something to report, and it
    /// runs twice per uncontended `ReentrantLock.lock()`/`unlock()` pair. Going
    /// through `ThreadRegistry::threads` to find this thread's own list put a
    /// registry-wide `RwLock` read and a `ThreadId` hash on that path; the
    /// handle removes both, exactly as [`GcBlockState`] above does for the
    /// blocked-region protocol.
    ///
    /// `None` until first use, and left `None` for a thread the registry does
    /// not know — which falls back to `set_jmx_owned_synchronizer`.
    pub jmx_locked_synchronizers: Option<Arc<parking_lot::Mutex<Vec<ObjectRef>>>>,

    /// GC roots for `Value::Object` arguments popped from the operand stack into a
    /// Rust `Vec` while a registered native runs (`safe_native_call`). Those refs
    /// are no longer on the Java stack until the callee returns, so without this
    /// list a safepoint GC can collect them mid-native (Letsgo AV after CCE
    /// `enhance` returns the original `Class`).
    pub native_pin_roots: Vec<ObjectRef>,

    /// Unused objects reserved in one old-generation allocation batch for
    /// small native allocations. Keeping the pool on the owning thread makes
    /// it a normal GC root set; every GC snapshot/remap path treats these
    /// entries exactly like `native_pin_roots` until `alloc_object` hands one
    /// to a native callback.
    pub native_alloc_pool: Vec<ObjectRef>,
    /// `(class_id, slot_count)` shared by every entry in
    /// `native_alloc_pool`. `None` when the pool is empty.
    pub native_alloc_pool_layout: Option<(ClassId, usize)>,

    // ---- handle scope support (arch/handles) ----
    //
    // Slot storage backing `NativeContext::handle_root`/`handle_get` (see
    // `native_api::registry` and `cratonvm_types::handle`) — the GC-safe
    // sibling of `native_pin_roots` just above. A handle reads through its
    // slot on every access instead of caching an `ObjectRef` copy, so it
    // cannot go stale across an allocating call the way a raw pinned-and-
    // re-read local still can if a caller forgets the re-read step.
    //
    /// One entry per live handle slot. A `None` hole is a released
    /// (`handle_scope_pop`-truncated-past or otherwise unrooted) slot; root
    /// scanning and both cross-thread snapshot paths visit only `Some` entries;
    /// every ordinary and fallback pointer-map consumer rewrites those entries.
    /// Entries are appended by `handle_root` and released in bulk by
    /// `handle_scope_pop` truncating back to a recorded base — never
    /// reused mid-scope — mirroring `native_pin_roots`' own append/truncate
    /// discipline.
    pub handle_slots: Vec<Option<ObjectRef>>,
    /// Stack of `handle_slots` lengths recorded by each `handle_scope_push`,
    /// popped (and used to truncate `handle_slots`) by the matching
    /// `handle_scope_pop`. Tracking the base on the thread itself (rather
    /// than making every caller thread a base index through, as
    /// `pin_native_root`'s callers must) is what lets handle scopes nest
    /// across native call frames with a single push/pop pair each.
    pub handle_scope_bases: Vec<usize>,

    /// Object result from the last `safe_native_call`: either an object return
    /// value before the interpreter pushes it onto the operand stack, or a
    /// native-thrown Java exception before the interpreter routes it through a
    /// catch handler. Covers the cross-thread GC window after the call returns.
    pub native_pending_return: Option<ObjectRef>,

    /// Bounded JIT cache for immutable String keys in exact HashMaps. Entries
    /// are normal thread roots and are cleared lazily on a structural change.
    pub jit_hashmap_string_node_cache: Vec<JitHashMapStringNodeCacheEntry>,
    pub string_case_cache: Vec<StringCaseCacheEntry>,

    /// Thread-local invoke cache — maps (caller_class, cp_index) to resolved targets.
    /// No locking needed since each thread owns its cache.
    ///
    /// The JIT arm holds [`cratonvm_jit::RetainedCode`] rather than a bare
    /// `Arc<CompiledMethod>`. This cache is evicted by the thread that
    /// dispatches through it — `get` auto-evicts a stale entry, `put`
    /// replaces one, `evict`/`clear` drop whole call sites — and those
    /// evictions can run *while a frame of the evicted body is on this very
    /// stack*. Once the JIT cache has retired the body, this entry is its last
    /// owner, so a plain `Arc` drop would `munmap` the code under the
    /// thread's own return address. Measured on
    /// `BasicErrorControllerIntegrationTests`: released at
    /// `active_jit_executions` = 2 and 3.
    pub invoke_cache: InvokeCache<cratonvm_jit::RetainedCode>,

    /// Per-thread resolved-field site cache — the interpreter's "resolved
    /// constant pool" for `getfield`/`putfield`/`getstatic`/`putstatic`.
    ///
    /// The authoritative `SharedVm::resolution_cache` already memoizes
    /// `(referencing class, cp index) -> ResolvedField`, but reaching it costs an
    /// `OrderedPlRwLock` read, and `resolve_field_ref_loader_aware` then
    /// *revalidates* every hit by re-deriving the field-owning class from its
    /// name — two `String` allocations, two further `class_manager` read
    /// acquisitions and a full `resolve_class_loader_aware`. This side table
    /// skips all of it for the sites where that revalidation is a tautology.
    /// See [`crate::runtime::interpreter::site_cache`] for the validity
    /// argument — read it before adding a `put` call site.
    pub field_sites: crate::runtime::interpreter::FieldSiteCache,
    /// Quickened instance-field sites: the receiver class the site last
    /// resolved against and the compact-layout offset / storage kind of the
    /// field in that class, so the `getfield` / `putfield` fast arms can
    /// load or store without resolving anything. Same key and epoch
    /// validation as `field_sites`; filled by the slow handler on the access
    /// that resolved the field.
    pub fast_field_sites: crate::runtime::interpreter::FastFieldSiteCache,

    /// Per-thread resolved-method site cache — the same "resolved constant
    /// pool" for the `(descriptor, num_params)` pair that the argument-popping
    /// helpers need. On the inline-cache HIT path those used to call
    /// `resolve_method_ref` for two of its four return values, paying a
    /// `resolution_cache` read lock, a hash probe and three `Arc<str>`
    /// clone/drop pairs per invoke.
    pub method_sites: crate::runtime::interpreter::MethodSiteCache,

    /// Per-thread resolved `new`-site cache — the same "resolved constant pool"
    /// for the class an allocation site allocates.
    ///
    /// The interpreter's `new` handler re-derived its answer on every single
    /// execution: a `String` allocated for the class name, a full
    /// `resolve_class_loader_aware`, and FOUR `class_manager` read
    /// acquisitions (name, access check, initialization, field count). At
    /// netty's `AdaptivePoolingAllocator` allocation rate that is the
    /// per-allocation cost the throughput page asked to explain. See
    /// [`crate::runtime::interpreter::site_cache::ClassSiteCache`] for which
    /// sites are admissible and what a hit is allowed to skip.
    pub class_sites: crate::runtime::interpreter::ClassSiteCache,

    /// Per-thread resolved `checkcast`/`instanceof` target classes. Separate
    /// from `class_sites` on purpose — see `CastSiteCache`'s docs for why a
    /// shared table would let a `checkcast` fill answer a `new`.
    pub cast_sites: crate::runtime::interpreter::CastSiteCache,

    /// Per-thread memo for the interface receiver-selection re-check that
    /// `execute_invokevirtual_cached` performs on every `invokeinterface`
    /// cache hit. See `IfaceSelectSiteCache` for what it stores and why the
    /// value has to carry the receiver class as well as the site.
    pub iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache,

    /// Thread-local cache for the vtable-fast native-shadow guard.
    ///
    /// On invoke-cache misses, `execute_invokevirtual_vtable_fast` checks whether
    /// the receiver class or one of its native-shadowed ancestors should cede
    /// dispatch to the native/intrinsic path. HQL/lambda-heavy workloads can miss
    /// the invoke cache repeatedly, and the old guard paid a native-registry hash
    /// lookup for every ancestor on every miss.
    ///
    /// Keyed by `(receiver_class_id, hierarchy_redefine_fingerprint,
    /// method_name_hash, descriptor_hash)`. A redefine changes the
    /// fingerprint of the affected class hierarchy, so those entries
    /// self-invalidate without disabling this cache process-wide.
    pub native_shadow_cache: FxHashMap<(u32, u64, u64, u64), bool>,

    /// Whether this is a platform or virtual thread (JEP 444, Java 21).
    pub kind: ThreadKind,

    /// Pin depth for virtual threads (JEP 491).
    ///
    /// Each monitor enter increments this; each monitor exit decrements.
    /// When non-zero on a virtual thread, the thread is "pinned" to its
    /// carrier — attempting to park/sleep will keep the carrier blocked
    /// and emit a `jdk.VirtualThreadPinned` JFR event.
    pub pin_count: u32,

    /// Last pin reason (for JFR event classification).
    /// "Monitor", "NativeMethod", "ClassInit", or empty when not pinned.
    pub pin_reason: &'static str,

    /// Scoped value binding stack (JEP 446, Java 25).
    /// Each entry is (key_id, key_ref, bound_value). Searched top-to-bottom.
    ///
    /// Round-9 GC fix: `key_ref` is the `ScopedValue` ObjectRef that owns
    /// `key_id`. When set, the GC root scanner reports it so the key
    /// object cannot be collected while a binding for it is live (only
    /// the value used to be retained; the key itself could be reclaimed
    /// while user code still observed the binding via
    /// `ScopedValue.isBound()` / `Carrier.get`).  `None` for legacy
    /// callers that pre-date the with-key API — those paths fall back to
    /// the value-only behaviour and are no worse than before.
    pub scoped_values: Vec<(u64, Option<ObjectRef>, Value)>,

    /// Thread-local allocation buffer for lock-free young-gen allocation.
    pub tlab: cratonvm_gc::Tlab,

    /// Per-thread **shadow stack** of live object references for precise,
    /// rewritable GC roots inside JIT-compiled code (see
    /// `cratonvm_gc::shadow_stack`). JIT code pushes live oops onto it before
    /// a GC-capable call and reloads them after; the collector marks and
    /// rewrites the pushed slots precisely, which lets a *moving* young-gen
    /// collection run safely while JIT frames are live. Unallocated (empty)
    /// until the thread first enters JIT code with the mechanism enabled.
    pub shadow_stack: cratonvm_gc::shadow_stack::ShadowStack,

    /// T1.5.1 — pending asynchronous exception to deliver at the next
    /// safepoint.
    ///
    /// When `Some(throwable)`, the next call to `safepoint_check`
    /// clears this field and raises the throwable so it propagates
    /// through the interpreter's normal exception table walk.
    /// Used by `Thread.stop0` and by any future API that needs to
    /// deliver a Throwable to a running thread without corrupting
    /// stack state.
    ///
    /// The field is owned by the target thread; cross-thread setters
    /// (the `Thread.stop0` native) go through the thread registry
    /// which takes a brief lock and then sets this via an
    /// `AsyncExceptionSlot` (see `thread_registry.rs`).
    ///
    /// The stored `ObjectRef` must point to a live `Throwable`
    /// subclass. The field is cleared by the safepoint before raise,
    /// so a single `stop()` call delivers exactly once.
    pub pending_async_exception: Option<ObjectRef>,

    /// The JIT's out-of-band **pending Java exception**, awaiting the
    /// interpreter's post-JIT drain.
    ///
    /// A compiled method cannot return a throwable through the JIT ABI, so
    /// every helper that raises one (`jit_alloc_oom`, the class-init guards,
    /// `handle_jit_dispatch_error`, `jit_throw_exception`, the stack-overflow
    /// guards, …) stashes it here and returns the `i64::MIN` deopt sentinel;
    /// `take_all_jit_signals` / `take_jit_pending_exception` drain it and route
    /// it through the method's exception table. The *non-reference* siblings of
    /// this signal (`athrow_bci`, `aioobe`, `arithmetic`, `npe`, `npe_action`,
    /// `deopt`) stay in the `JIT_SIGNALS` thread-local; only this one is a heap
    /// reference, and that is why it lives here instead.
    ///
    /// **It is a `JvmThread` field for exactly one reason: GC reachability.**
    /// It used to be `JitSignals::exception`, a `Cell<Option<ObjectRef>>`
    /// inside a `thread_local!`. A collecting thread cannot reach another
    /// thread's TLS, and the `VM_ROOT_SOURCES` callbacks all run on the
    /// collector, so the stashed throwable was neither scanned nor remapped —
    /// an unrooted live reference across a window that includes a *guaranteed*
    /// collection in the `OutOfMemoryError` case (`jit_alloc_oom` stashes the
    /// OOME precisely because the heap is exhausted). As a `JvmThread` field it
    /// sits next to `pending_async_exception` in both halves of the root
    /// machinery: `memory/roots.rs` §10 pushes it, `memory/gc.rs` §10 rewrites
    /// it.
    ///
    /// A collection between the stash and the drain therefore keeps the
    /// throwable alive and hands the drain its post-move address.
    /// See `fixed-bugs/jit-signals-root-gap.md`.
    pub jit_pending_exception: Option<ObjectRef>,

    /// The uncaught throwable the launcher is about to render, parked here for
    /// the duration of SHUTDOWN HOOK execution.
    ///
    /// The launcher gets an `ObjectRef` out of `MethodCallFailed::
    /// ExceptionThrown`, then runs shutdown hooks BEFORE rendering it (HotSpot
    /// runs hooks on the uncaught path too). A hook is arbitrary Java: it
    /// allocates, and it can collect. Held only in a Rust local across that
    /// call the throwable is reachable from nothing the collector can see, so
    /// a collection during the hooks reclaims it — and the render then reads a
    /// zeroed header, which decodes as `ClassId(0)`, which IS
    /// `java.lang.Object`.
    ///
    /// That is `bug-h2-testopenclose-throwable-is-java-lang-object-20260829.md`:
    /// `Exception in thread "main" java/lang/Object` with no message and no
    /// frames, because the whole object is gone. H2's own
    /// `OnExitDatabaseCloser` hook closes a database from the hook, which is
    /// as much allocation as the window needs.
    ///
    /// Same two halves as `jit_pending_exception` above — `memory/roots.rs`
    /// §10 pushes it, `memory/gc.rs` rewrites it — so the throwable survives
    /// the hooks AND the launcher gets its post-move address.
    pub uncaught_exception_pending: Option<ObjectRef>,

    /// T17.Δ.3 — JVMTI single-step enable for this thread.
    ///
    /// When set, the interpreter's per-instruction dispatch fires
    /// `JvmtiEventKind::SingleStep` once for each bytecode.  Set by
    /// `JvmtiEnv::set_event_notification_mode(Enable, SingleStep, Some(tid))`
    /// and cleared by the corresponding `Disable` call.
    ///
    /// Stored as `AtomicBool` because the flag may be flipped from a
    /// debugger thread while the owning thread is executing bytecode; the
    /// owning thread reads it with `Ordering::Relaxed` on the dispatch-loop
    /// hot path (single-thread view of its own flag is always consistent).
    pub single_step_enabled: AtomicBool,

    /// T17.Δ.5 — per-thread JVMTI frame-pop requests.
    ///
    /// Each entry is a frame depth (index into `frames`) for which
    /// `NotifyFramePop` was called.  When the interpreter is about to drop
    /// a frame it consults this list; if the frame's depth matches any
    /// registered request, `FramePop` fires and the entry is removed.
    ///
    /// Implemented as a small `Vec<u32>` because in practice only a handful
    /// of frames are marked at any given time (debugger "step out" uses
    /// one; profilers typically use zero).  Linear scan cost is negligible
    /// next to the frame-pop work.
    pub frame_pop_requests: Vec<u32>,
}

impl JvmThread {
    /// Byte offset of the `tlab` field from the start of `JvmThread`.
    ///
    /// Used by the JIT (`jit/src/x64.rs` `new` opcode) to emit an inline
    /// TLAB bump-pointer fast path. The JIT loads the per-thread TLAB
    /// cursor/end at `JvmThread base + TLAB_OFFSET + Tlab::CURSOR/END_OFFSET`.
    ///
    /// **MSRV note**: this crate targets Rust 1.75. `core::mem::offset_of!`
    /// is `const` only on 1.77+, so we compute the offset lazily on first
    /// access via a sentinel `JvmThread::default()` and cache it in a
    /// [`OnceLock`]. The computation is a single subtraction of two
    /// `&raw const` pointers — no allocation beyond the one-time default
    /// thread.
    ///
    /// Callers should treat the returned value as effectively `const` and
    /// cache it themselves (e.g. into a `JitRuntimeHelpers` field at
    /// helper-table init).
    pub fn tlab_offset() -> usize {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<usize> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let t = JvmThread::default();
            let base = &t as *const JvmThread as usize;
            let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
            tlab_addr - base
        })
    }

    /// Byte offset of the `shadow_stack` field from the start of `JvmThread`.
    ///
    /// Used by the JIT (`jit/src/x64.rs`) to emit the inline shadow-stack push
    /// at GC-capable safepoints: the codegen loads/stores the shadow `top` at
    /// `JvmThread base + shadow_stack_offset() + ShadowStack::TOP_OFFSET`.
    /// Computed lazily like [`Self::tlab_offset`] (MSRV: no const `offset_of!`).
    pub fn shadow_stack_offset() -> usize {
        use std::sync::OnceLock;
        static OFFSET: OnceLock<usize> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let t = JvmThread::default();
            let base = &t as *const JvmThread as usize;
            let ss_addr = &t.shadow_stack as *const cratonvm_gc::shadow_stack::ShadowStack as usize;
            ss_addr - base
        })
    }

    /// Whether VM-state breadcrumbs should be published.
    #[inline]
    pub fn vm_state_diagnostics_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VM_STATE").is_some()
        })
    }

    /// Publish a short diagnostic state for STW census. No-op unless enabled.
    #[inline]
    pub fn set_vm_state<S: Into<String>>(&self, state: S) {
        if Self::vm_state_diagnostics_enabled() {
            *self.vm_state.lock() = state.into();
        }
    }

    /// Create a new thread with the given id and name.
    pub fn new(thread_id: ThreadId, name: &str) -> Self {
        Self {
            thread_id,
            name: name.to_string(),
            last_heal_collection: 0,
            frames: FrameStack::new(),
            locals_pool: Vec::new(),
            stacks_pool: Vec::new(),
            printed: Vec::new(),
            printed_lines: Vec::new(),
            daemon: false,
            interrupted: Arc::new(AtomicBool::new(false)),
            java_thread_obj: None,
            park_state: Arc::new(ParkState::new()),
            root_snapshot: Arc::new(parking_lot::Mutex::new(Vec::new())),
            frame_trace: Arc::new(parking_lot::Mutex::new(Vec::new())),
            vm_state: Arc::new(parking_lot::Mutex::new(String::new())),
            rs_cache: Vec::new(),
            rs_cache_gen: 0,
            gc_block_state: Arc::new(GcBlockState::new()),
            jmx_locked_synchronizers: None,
            native_pin_roots: Vec::new(),
            native_alloc_pool: Vec::new(),
            native_alloc_pool_layout: None,
            handle_slots: Vec::new(),
            handle_scope_bases: Vec::new(),
            native_pending_return: None,
            jit_hashmap_string_node_cache: Vec::new(),
            string_case_cache: Vec::new(),
            invoke_cache: InvokeCache::new(),
            field_sites: crate::runtime::interpreter::FieldSiteCache::new(),
            fast_field_sites: crate::runtime::interpreter::FastFieldSiteCache::new(),
            method_sites: crate::runtime::interpreter::MethodSiteCache::new(),
            class_sites: crate::runtime::interpreter::ClassSiteCache::new(),
            cast_sites: crate::runtime::interpreter::CastSiteCache::new(),
            iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache::new(),
            native_shadow_cache: FxHashMap::default(),
            kind: ThreadKind::Platform,
            pin_count: 0,
            pin_reason: "",
            scoped_values: Vec::new(),
            tlab: cratonvm_gc::Tlab::empty(),
            shadow_stack: cratonvm_gc::shadow_stack::ShadowStack::empty(),
            pending_async_exception: None,
            jit_pending_exception: None,
            uncaught_exception_pending: None,
            single_step_enabled: AtomicBool::new(false),
            frame_pop_requests: Vec::new(),
        }
    }

    /// Return a popped frame's Vec allocations to the pool for reuse.
    ///
    /// Overflow (thread-owned pool already at `MAX_POOL_SIZE`) is offered to
    /// the per-OS-thread SoA pool in `runtime::frame`, which backs the
    /// *non-pooled* constructors `Frame::new` / `Frame::new_from_arcs` — the
    /// latter being the hot uncached invoke path, which previously allocated
    /// and zero-filled four `Vec`s per frame. Buffers that pool declines
    /// (cap reached, or oversized) are dropped exactly as before, so this is
    /// never worse than the old behaviour.
    ///
    /// The thread-owned `locals_pool` / `stacks_pool` keep first claim, so the
    /// cached invoke path's pool dynamics are unchanged.
    pub fn recycle_frame(&mut self, frame: Frame) {
        if self.locals_pool.len() < MAX_POOL_SIZE {
            frame.recycle(&mut self.locals_pool, &mut self.stacks_pool);
            return;
        }
        let (local_vals, local_tags, stack_vals, stack_tags) = frame.take_pool_parts();
        crate::runtime::frame::offer_frame_parts_to_tls_pool(
            local_vals, local_tags, stack_vals, stack_tags,
        );
    }

    /// T10.7 — recycle a frame, spilling any overflow into the VM-wide
    /// `VecPool`s on `SharedVm`.
    ///
    /// Semantics:
    ///   * When the thread-local `locals_pool` / `stacks_pool` has room, the
    ///     frame's Vecs are retained there exactly as before (zero locking).
    ///   * Once the thread-local pool is full (`MAX_POOL_SIZE`), we keep the
    ///     frame's Vecs alive by handing them to the VM-wide shared pools
    ///     (`operand_stack_pool` for the `Vec<u64>` halves and `tag_pool` for
    ///     the `Vec<u8>` halves).  Sibling threads later pull from the shared
    ///     pool when their own thread-local pool is empty (see
    ///     `refill_pools_from_shared`).
    ///
    /// This preserves allocations across thread boundaries without impacting
    /// the hot path — the shared mutex is only touched on the cold
    /// "thread-local pool full" edge.
    pub fn recycle_frame_with_shared(
        &mut self,
        frame: Frame,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) {
        // Split the frame's inner Vecs out of it exactly once.
        let (local_vals, local_tags, stack_vals, stack_tags) = frame.take_pool_parts();
        self.route_pool_parts(
            local_vals,
            local_tags,
            stack_vals,
            stack_tags,
            operand_stack_pool,
            tag_pool,
        );
    }

    /// [`Self::recycle_frame_with_shared`] for a frame that is still sitting in
    /// its `FrameStack` slot: harvest its four pooled `Vec`s in place, then let
    /// `FrameStack::truncate` drop the husk where it lies.
    ///
    /// # Why
    ///
    /// `Frame` is a ~300-byte by-value struct and the return path used to move
    /// it three times — out of the buffer (`FrameStack::pop`), into
    /// `recycle_frame_with_shared`, and again into `take_pool_parts` — to
    /// arrive at four `Vec` headers. `perf` on the interpreted-invoke probe put
    /// `memcpy` under `Vec::pop<Frame>` and `pop_and_recycle_frame_with_reason`
    /// at 6.97% of the invoke arm, inside a frame-lifecycle group worth ~24.7%
    /// of it. See
    /// `known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`.
    ///
    /// Returns `false` when there was no frame to pop, so callers keep the
    /// `if let Some(..)` shape the `pop()` form gave them.
    pub fn recycle_top_frame_in_place(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) -> bool {
        if self.frames.is_empty() {
            return false;
        }
        if !crate::runtime::env_cache::no_frame_slot_reuse() {
            // Retire the frame where it lies. Its four buffers stay in the
            // slot, and the next call at this depth is rebuilt in them by
            // `FrameStack::push_cached_compact_reusing` -- no pool round
            // trip, no `Vec` headers moved, no `Frame` moved. Nothing above
            // the new depth is live, so nothing scans what it left.
            //
            // A by-value push at this depth still harvests the slot first
            // (see `push_frame_and_fire_entry`), so the pooled constructors
            // keep the recycling they have always had.
            return self.frames.retire_top();
        }
        let depth = self.frames.len();
        let Some(top) = self.frames.last_mut() else {
            return false;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = top.take_pool_parts_in_place();
        // `truncate` runs `Drop` on the element where it sits and never moves a
        // surviving frame — the address-stability contract `FrameStack` exists
        // for. The husk's remaining fields (metadata `Arc`s, `monitor_on_exit`)
        // are released by that drop exactly as they were by the old move.
        self.frames.truncate(depth - 1);
        self.route_pool_parts(
            local_vals,
            local_tags,
            stack_vals,
            stack_tags,
            operand_stack_pool,
            tag_pool,
        );
        true
    }

    /// Thread-local pool first, VM-wide pools once it is full. Shared by both
    /// recycle entry points so the routing policy cannot drift between them.
    /// Move a retired frame slot's buffers into the thread pools, so an
    /// ordinary by-value push may overwrite the slot without losing them.
    ///
    /// No-op when no slot is retired, which is the case on every first call at
    /// a given depth and whenever `CRATONVM_JIT_NO_FRAME_SLOT_REUSE` is set.
    pub fn harvest_retired_slot(&mut self) {
        let Some(slot) = self.frames.retired_slot_mut() else {
            return;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = slot.take_pool_parts_in_place();
        self.frames.trim_retired();
        // A husk whose buffers were already harvested (the pooled recycle path,
        // i.e. `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`) hands back four EMPTY `Vec`s.
        // Pushing those into the pools poisons them: the next frame build pops
        // a zero-capacity buffer and reallocates from scratch, which measured
        // as a 2-3x regression with `ret_recycle` at 427 cycles against 17.
        if local_vals.capacity() == 0 && stack_vals.capacity() == 0 {
            return;
        }
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
        }
    }

    fn route_pool_parts(
        &mut self,
        local_vals: Vec<u64>,
        local_tags: Vec<u8>,
        stack_vals: Vec<u64>,
        stack_tags: Vec<u8>,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) {
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
            return;
        }
        // Thread-local pool is full — spill into the shared pools.
        operand_stack_pool.release(local_vals);
        tag_pool.release(local_tags);
        operand_stack_pool.release(stack_vals);
        tag_pool.release(stack_tags);
    }

    /// T10.7 — replenish the thread-local `locals_pool` / `stacks_pool` from
    /// the VM-wide shared pools when empty.
    ///
    /// Invoked just before a frame-push that will consume pooled Vecs so the
    /// shared-pool mutex is touched only on the cold refill path, not per call.
    pub fn refill_pools_from_shared(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
        max_locals: usize,
        max_stack: usize,
    ) {
        if self.locals_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_locals);
            let tags = tag_pool.acquire(max_locals);
            self.locals_pool.push((vals, tags));
        }
        if self.stacks_pool.is_empty() {
            let vals = operand_stack_pool.acquire(max_stack);
            let tags = tag_pool.acquire(max_stack);
            self.stacks_pool.push((vals, tags));
        }
    }
}

impl Default for JvmThread {
    /// Creates a default "main" thread with id 0.
    /// Used with `std::mem::take` for borrow-splitting.
    fn default() -> Self {
        Self::new(ThreadId(0), "main")
    }
}

impl std::fmt::Debug for JvmThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JvmThread")
            .field("thread_id", &self.thread_id)
            .field("name", &self.name)
            .field("frames_depth", &self.frames.len())
            .field("daemon", &self.daemon)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JIT contract — `JvmThread::tlab_offset()` must report the actual
    /// byte offset of the `tlab` field. The JIT-emitted inline bump in
    /// `jit/src/x64.rs` reads `[thread + tlab_offset + CURSOR_OFFSET]`,
    /// so a wrong answer here corrupts the heap.
    #[test]
    fn tlab_offset_matches_field_address() {
        let t = JvmThread::default();
        let base = &t as *const JvmThread as usize;
        let tlab_addr = &t.tlab as *const cratonvm_gc::Tlab as usize;
        let expected = tlab_addr - base;
        assert_eq!(
            JvmThread::tlab_offset(),
            expected,
            "JvmThread::tlab_offset() out of sync with the actual field offset"
        );
        // Caching: a second call must return the same value (OnceLock).
        assert_eq!(JvmThread::tlab_offset(), expected);
    }

    #[test]
    fn thread_creation() {
        let thread = JvmThread::new(ThreadId(1), "worker-1");
        assert_eq!(thread.thread_id, ThreadId(1));
        assert_eq!(thread.name, "worker-1");
        assert!(thread.frames.is_empty());
        assert!(thread.printed.is_empty());
        assert!(!thread.daemon);
    }

    #[test]
    fn thread_default_is_main() {
        let thread = JvmThread::default();
        assert_eq!(thread.thread_id, ThreadId(0));
        assert_eq!(thread.name, "main");
    }

    #[test]
    fn thread_id_display() {
        assert_eq!(format!("{}", ThreadId(0)), "Thread-0");
        assert_eq!(format!("{}", ThreadId(42)), "Thread-42");
    }

    #[test]
    fn park_state_unpark_before_park() {
        let ps = ParkState::new();
        // Unpark first → next park returns immediately
        ps.unpark();
        ps.park(Some(std::time::Duration::from_millis(100)));
        // If park didn't return immediately, the test would timeout
    }

    #[test]
    fn park_state_park_with_timeout() {
        let ps = ParkState::new();
        let start = std::time::Instant::now();
        ps.park(Some(std::time::Duration::from_millis(50)));
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Park should have blocked for ~50ms, got {}ms",
            elapsed.as_millis()
        );
    }

    /// A timed park must end near its deadline, not at the next system tick.
    ///
    /// On Windows every condvar/`WaitForSingleObject`-shaped wait is rounded up
    /// to the 15.625 ms clock tick, so this 50 ms park measured 62.5 ms — four
    /// short cycles then one long one for anything re-arming from an absolute
    /// deadline, which is what walked `AutoScalingEventExecutorChooserFactoryTest`
    /// past the state it asserts on. `win_park` bounds it with a
    /// high-resolution waitable timer instead. The 40 ms floor of
    /// `park_state_park_with_timeout` above cannot see any of that; this is the
    /// ceiling that can.
    ///
    /// The bound is deliberately loose (a loaded CI box can delay any wakeup)
    /// and still an order of magnitude tighter than the 15.6 ms quantum: 62 ms
    /// was the OLD median, not an outlier, so a regression fails this on the
    /// median run rather than needing an unlucky one. Non-Windows hosts have
    /// no such rounding and pass it for free.
    #[test]
    fn park_state_timed_park_does_not_overshoot_to_the_system_tick() {
        let ps = ParkState::new();
        // One warm-up park: the first one on a thread creates the
        // high-resolution timer, and that is not what is being measured.
        ps.park(Some(std::time::Duration::from_millis(5)));
        let mut best = std::time::Duration::from_secs(1);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            ps.park(Some(std::time::Duration::from_millis(50)));
            best = best.min(start.elapsed());
        }
        assert!(
            best >= std::time::Duration::from_millis(45),
            "a 50 ms park returned after only {best:?}"
        );
        assert!(
            best < std::time::Duration::from_millis(58),
            "a 50 ms park took {best:?} at best -- the deadline is being rounded \
             up to the 15.625 ms system tick (62.5 ms was the pre-fix median)"
        );
    }

    #[test]
    fn park_state_unpark_wakes_parked_thread() {
        let ps = Arc::new(ParkState::new());
        let ps2 = ps.clone();
        let handle = std::thread::spawn(move || {
            // This will block until unparked
            ps2.park(Some(std::time::Duration::from_secs(5)));
        });
        // Give the thread time to park
        std::thread::sleep(std::time::Duration::from_millis(20));
        ps.unpark();
        handle.join().unwrap();
    }
}
