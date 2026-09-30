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

/// One raw word in a BLOCKED peer's native stack that a cross-thread
/// conservative scan resolved to a heap object.
///
/// `SlotOrigin` cannot express this: it is keyed by `(frame, idx)` into the
/// INTERPRETER frames, and these words live in the peer's machine stack, below
/// or between its compiled frames, named by no oop map. They are the population
/// §12 of
/// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
/// measured as stale on essentially every relocating cycle.
#[derive(Clone, Copy)]
pub struct NativeSlotFixup {
    /// Absolute address of the word inside the peer's own stack.
    pub addr: usize,
    /// Value the word held when the scan captured it. The wake write-back
    /// refuses to store unless the word STILL reads this, which is what keeps
    /// it off a C local the native call has since reused.
    pub orig: usize,
    /// The object's current address, advanced by each collection's fold.
    pub cur: usize,
}

/// The compiled activations a thread blocked in native code stands under, as
/// a JDWP frame listing splices them into its interpreter frames (interpreter
/// round i1 wave 21, lane L3; `interpreter::capture_blocked_compiled_view`).
/// Plain data: no object reference, so no collection has to know about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockedCompiledView {
    /// The compiled (and inlined) activations, bottom first, each listed
    /// immediately below interpreter frame `below` (`frames.len()`: above the
    /// top interpreter frame).
    pub rows: Vec<BlockedCompiledRow>,
    /// Interpreter frames whose body runs compiled (an OSR'd loop, or a body
    /// entered compiled under its own frame): `(frame index, the compiled
    /// half's bytecode index or -1)`. Their `Frame` locals are the values at
    /// the compiled entry, so a listing withholds them.
    pub stale: Vec<(u32, i64)>,
}

/// The classes of a blocked thread's interpreter frames, as its blocking
/// deposit found them: the per-class half of the stale-frame census
/// (interpreter round i1 wave 24, lane L3;
/// `interpreter::obsolete_frames::prune_histories_by_census`). A thread pushes
/// no frame while it is blocked, so a class this list does not name has no
/// frame on the thread for as long as `GcBlockState::in_blocked_region`
/// stays up. Plain data: no object reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockedFrameClasses {
    /// `classes` names the class of every interpreter frame the thread had at
    /// its last flag-raising deposit, and it had no compiled activation then
    /// (whose sites translate through a class's history without a frame).
    /// `false`: unknown -- the census counts the thread for every class.
    pub exact: bool,
    /// Each class once per run of equal neighbours, bottom frame first; the
    /// allocation is reused from one deposit to the next.
    pub classes: Vec<ClassId>,
}

/// One row of [`BlockedCompiledView::rows`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockedCompiledRow {
    /// The interpreter frame (bottom-first index) this row is listed below.
    pub below: u32,
    /// `ClassId` of the method's class.
    pub class_id: u32,
    /// `debug::jdwp_method_id` of the method.
    pub method_id: u64,
    /// Bytecode index the activation stands at, or -1 when unknown.
    pub bci: i64,
}

pub struct GcBlockState {
    /// gen r5w2/roots6: raised when the owning `JvmThread` is dropped; see
    /// [`GcBlockState::owner_dropped`].
    owner_dropped: AtomicBool,
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
    /// Raw native-stack words this thread's blocked window had scanned out of
    /// it by a cross-thread conservative scan. Appended by the fold, advanced
    /// by every later fold, applied and cleared on wake. See
    /// [`NativeSlotFixup`].
    pub native_slots: PLMutex<Vec<NativeSlotFixup>>,
    /// The JDWP inspection window of a blocking native region (interpreter
    /// round i1 wave 12): 0, or the thread's `JvmThread` address with flag
    /// bits in its low three bits, open from the blocking deposit to the
    /// start of the leave while a debugger is attached. A debugger command
    /// reads a suspended blocked thread's frames through it
    /// (`interpreter::publish_frames_of_blocked_thread`); the leave waits out
    /// a reader. Not a GC field; only `experimental-debug` builds write it.
    pub debugger_inspect: std::sync::atomic::AtomicUsize,
    /// The compiled activations above or between this thread's interpreter
    /// frames, captured by the thread itself as it opens the inspection
    /// window under compiled code (interpreter round i1 wave 21, lane L3):
    /// only the thread can walk its own JIT entry chain, and the reader lists
    /// them from here. Empty otherwise; cleared when the window closes. Not a
    /// GC field; only `experimental-debug` builds write it.
    pub debugger_compiled: PLMutex<BlockedCompiledView>,
    /// The stale-frame census's input (interpreter round i1 wave 23, lane
    /// L3; `interpreter::obsolete_frames`): no frame of this thread is
    /// stamped below it, as of its last conversion pass
    /// (`convert_obsolete_frames_if_redefined`) that found no compiled
    /// activation on its stack, or of its creation. A step of a class's
    /// redefinition history retired at or below every thread's floor
    /// translates for nobody. Written only by the owning thread; read by a
    /// redefining thread through the registry, which shares this `Arc`.
    /// Not a GC field.
    pub obsolete_frame_floor: std::sync::atomic::AtomicU64,
    /// The classes of this thread's frames at its last flag-raising deposit
    /// (interpreter round i1 wave 24, lane L3; [`BlockedFrameClasses`]): while
    /// `in_blocked_region` is up, the census counts this thread's floor only
    /// for the classes it names. Written by the owning thread before it
    /// raises the flag, and marked inexact by the registry's native-block
    /// marker (which raises the flag without a deposit). Not a GC field.
    pub blocked_frame_classes: PLMutex<BlockedFrameClasses>,
    /// gcd d2/i (opt-in `CRATONVM_XT_BLOCKED_MONITOR_PROOF`): the JIT depth
    /// this thread PROVED rewritable at the flag-raising deposit of a block in
    /// the compiled `monitorenter` helper, or 0 (none, or not proven), or
    /// [`MONITOR_BLOCK_PROOF_ARMED`] (armed by the helper, deposit not yet
    /// taken). Written only by the owning thread: armed by
    /// `vm_exec::monitor_enter_blocking_from_compiled`, settled by
    /// `deposit_root_snapshot_inner` BEFORE it raises `in_blocked_region`
    /// (Release), cleared by the same helper after the wake. Read by the
    /// take-over (`ThreadRegistry::blocked_os_tids_split_by_monitor_proof`)
    /// only while `in_blocked_region` is up, i.e. while the deposit's proof
    /// still describes the thread's frames.
    pub monitor_block_proven_jit_depth: std::sync::atomic::AtomicUsize,
    /// gcd d5/f: how many relocating folds
    /// (`ThreadRegistry::fold_pointer_map_into_blocked_audited`) have targeted
    /// this thread's blocked state, ever. Bumped by the fold (inside a pause,
    /// while `in_blocked_region` is up), read by the owning thread: a JNI
    /// native in native (`native::jni`) compares it with the value it read
    /// before its deposit to prove nothing it holds has moved. Monotonic;
    /// compared, never reset.
    pub blocked_folds: std::sync::atomic::AtomicU64,
}

/// The "armed, not yet deposited" value of
/// [`GcBlockState::monitor_block_proven_jit_depth`]. Never a depth a reader
/// credits.
pub const MONITOR_BLOCK_PROOF_ARMED: usize = usize::MAX;

impl GcBlockState {
    /// gen r5w2/roots6: has the `JvmThread` that owns this state been dropped?
    ///
    /// The registry publishes raw addresses INTO that `JvmThread` (its `Tlab`,
    /// the whole struct) for the collector to read, and clears them on the
    /// thread's normal exit. A platform thread whose closure UNWINDS drops its
    /// `JvmThread` (a stack local) without that clear, and the entry stays
    /// alive until the OS thread finishes -- so a pause in between read the
    /// dead frame (`collect_reserved_tlab_tails` published whatever `(cursor,
    /// end)` it found there as a TLAB skip span). The drop now raises this
    /// flag first (`JvmThread::gc_block_state_drop_notice`), and every reader
    /// of a published
    /// address skips an entry that has it. See
    /// `docs/internal/gc/gengc-r5w1-crash5-an-unwinding-carrier-publishes-a-dropped-jvmthread-FIXED-20260927.md`.
    #[inline]
    pub fn owner_dropped(&self) -> bool {
        self.owner_dropped
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn new() -> Self {
        Self {
            owner_dropped: AtomicBool::new(false),
            in_blocked_region: AtomicBool::new(false),
            java_state: AtomicU8::new(0),
            fixup: PLMutex::new(cratonvm_types::PointerMap::default()),
            slot_origins: PLMutex::new(Vec::new()),
            native_slots: PLMutex::new(Vec::new()),
            debugger_inspect: std::sync::atomic::AtomicUsize::new(0),
            debugger_compiled: PLMutex::new(BlockedCompiledView {
                rows: Vec::new(),
                stale: Vec::new(),
            }),
            // Frames the owning thread builds from now on carry at least this
            // stamp (as `JvmThread::redefinitions_seen` starts).
            obsolete_frame_floor: std::sync::atomic::AtomicU64::new(
                cratonvm_classloading::class_redefinition_count(),
            ),
            blocked_frame_classes: PLMutex::new(BlockedFrameClasses::default()),
            monitor_block_proven_jit_depth: std::sync::atomic::AtomicUsize::new(0),
            blocked_folds: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl Default for GcBlockState {
    fn default() -> Self {
        Self::new()
    }
}

/// gen r5w2/roots6: a field of [`JvmThread`] whose `Drop` raises
/// [`GcBlockState::owner_dropped`] on the thread's shared block state, so a
/// registry reader never dereferences an address published into a
/// `JvmThread` that is gone -- including one dropped by an UNWIND, which
/// skips the normal-exit clear.
///
/// A field rather than `impl Drop for JvmThread`, so no code that moves a
/// field out of a `JvmThread` has to change. Moving the `JvmThread` (into a
/// `Box` on a yield) moves this too and drops nothing.
pub struct GcBlockStateDropNotice(Arc<GcBlockState>);

impl GcBlockStateDropNotice {
    /// A notice for `state`, which must be the owning `JvmThread`'s
    /// `gc_block_state`.
    pub fn new(state: &Arc<GcBlockState>) -> Self {
        Self(Arc::clone(state))
    }
}

impl Drop for GcBlockStateDropNotice {
    fn drop(&mut self) {
        self.0
            .owner_dropped
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl std::fmt::Debug for GcBlockStateDropNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GcBlockStateDropNotice")
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

    /// Take the permit without blocking: `true` when one was set (it is now
    /// consumed). A virtual thread's carrier park drops with it a permit that
    /// is not the thread's `LockSupport` permit (interpreter round i1 wave 28,
    /// lane L4; `VirtualThreadManager::begin_carrier_park`).
    pub fn take_permit(&self) -> bool {
        std::mem::replace(&mut *self.mutex.lock(), false)
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

/// Why a virtual thread's continuation yielded (interpreter round i1 wave 24,
/// lane L7; page
/// `interpreter-L7-an-interrupt-does-not-reach-an-unmounted-virtual-thread`).
///
/// A remounted continuation resumes AFTER the yielding native's invoke, as if
/// the native had returned normally. That is right for a park (which returns
/// on an unpark, an interrupt or spuriously), and wrong for a sleep or a
/// keyed wait woken by an interrupt: HotSpot throws `InterruptedException`
/// out of those. The remount reads this record to raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtYield {
    /// `LockSupport.park` / `parkNanos`: resumes normally, whatever woke it.
    Park,
    /// `Thread.sleep`: an interrupt throws `InterruptedException("sleep
    /// interrupted")`; any other early wake sleeps out the rest. `None`: the
    /// deadline overflowed `Instant` (a sleep-forever).
    Sleep { deadline: Option<std::time::Instant> },
    /// A wait on a VM-local key (`CountDownLatch.await`'s native): an
    /// interrupt cancels the registration and throws `InterruptedException`.
    KeyedWait { key: u64 },
}

/// A frame of a yielded continuation whose return value must be converted
/// before it reaches its caller (interpreter round i1 wave 25, lane L7; see
/// `interpreter::note_continuation_return_boundary`).
///
/// A lambda proxy's SAM call is served by a nested Rust call into the impl
/// method (`try_lambda_dispatch`), and that Rust frame converts the impl's
/// return to the SAM's (`coerce_return`: box, unbox, widen, or drop it for a
/// `void` SAM). A continuation keeps only the Java frames: when it remounts,
/// the impl frame returns straight into the frame that made the SAM call, so
/// the conversion the Rust frame would have done is recorded here and applied
/// by the interpreter's return arms instead.
#[derive(Debug, Clone)]
pub struct ContinuationReturnAdapter {
    /// Index of the impl frame in `JvmThread::frames`.
    pub frame_index: usize,
    /// That frame's `Frame::seq`: an entry whose frame has left (an exception
    /// unwound it, or a new frame reuses the index) no longer matches.
    pub frame_seq: u64,
    /// The return token the caller's call site expects (the SAM call's
    /// descriptor), e.g. `V`, `I`, `Ljava/lang/Object;`.
    pub caller_return: Box<str>,
}

/// Unique identifier for a JVM thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thread-{}", self.0)
    }
}

/// What compiled code needs to take an uncontended `monitorenter` /
/// `monitorexit` inline (both JIT tiers, `jit/src/runtime_lowering.rs`
/// `emit_inline_thin_lock`): the thin-lock owner bits to CAS into the 32-bit
/// mark word, this thread's JMX lock stack to record the acquisition in, and
/// the lease slot's `LeaseBlock` (its thin-lock count, and the inflated-monitor
/// cache the inflated arm reads). Located through
/// `JitRuntimeHelpers::monitor_block_offset_in_thread`
/// ([`JvmThread::jit_monitor_block_offset`]).
///
/// Compiled code reads `thin_owner`, `lock_stack` and `held`, writes the four
/// census words and adds to / subtracts from the count at `*held` (a plain
/// `ADD` / `SUB`: this thread is its only writer), and reads the inflated
/// cache behind it, all at `cratonvm_jit_api::JIT_MONITOR_BLOCK_*` /
/// `cratonvm_jit::runtime_lowering::LEASE_BLOCK_*`
/// (pinned below). `thin_owner == u64::MAX` means "no inline path": the thread
/// is not armed yet (armed lazily by the first helper-path `monitorenter`, see
/// `ThreadRegistry::arm_jit_monitor_block`), every lock slot of the VM is
/// leased (the helpers inflate for such a thread), it is not registered, or
/// `CRATONVM_MONITOR_FASTPATH=0`.
///
/// `thin_owner` names the thread's per-VM lock-slot LEASE
/// (`monitor::LockSlots`), not its id. A lease lasts until the thread dies
/// (`LockSlots::release`, from `release_monitors_held_by_except`, after which
/// no Java code runs on it), so an armed block can never name a slot another
/// thread holds. `held` points into the same `MonitorTable`'s `LockSlots`;
/// `registry_id` ties both to one VM (the table and the registry live side by
/// side in `SharedVm::threads`), and arming from another registry disarms
/// first. `book` owns the allocation `lock_stack` points into, so the address
/// cannot dangle while this `JvmThread` lives — it is the very `Arc` the
/// registry entry holds, so compiled code and the helpers write ONE lock
/// stack.
///
/// The four census counters (round 11 wave 7) answer "did the inline path
/// fire?", which timing alone could not: compiled monitor sites built under
/// `CRATONVM_DBG_JITC` add 1 to the matching counter on the inline path and on
/// the fall to the helper; `vm_exec::monitor_enter_blocking` and this block's
/// `Drop` print them (`[monitor-inline census]`). Without the flag no site
/// writes them and they stay zero.
#[repr(C)]
pub(crate) struct JitMonitorBlock {
    /// `MARK_THIN_LOCKED | slot << THIN_LOCK_OWNER_SHIFT`, or `u64::MAX`.
    thin_owner: u64,
    lock_stack: usize,
    /// Census counters at `JIT_MONITOR_BLOCK_{INLINE,SLOW}_{ENTERS,EXITS}_OFFSET`.
    /// Written only by this thread's compiled code (a plain `ADD`); atomics so
    /// Rust reads them as memory the emitted code may have changed behind its
    /// references. Nothing here needs a read-modify-write.
    census_inline_enters: std::sync::atomic::AtomicU64,
    census_inline_exits: std::sync::atomic::AtomicU64,
    census_slow_enters: std::sync::atomic::AtomicU64,
    census_slow_exits: std::sync::atomic::AtomicU64,
    /// Address of the lease slot's `LeaseBlock` (inside the VM's
    /// `MonitorTable`, which outlives every thread of that VM; its first word
    /// is the lessee's `u32` thin-lock count); `0` while unarmed.
    held: usize,
    /// `ThreadRegistry::registry_id` the block was armed from; `0` = unarmed.
    registry_id: u64,
    book: Option<Arc<crate::threading::thread_registry::JmxMonitorBook>>,
    /// `CRATONVM_DBG_JITC`, read once per thread: 0 = not read yet, 1 = off,
    /// 2 = on. Per thread rather than a process-wide latch.
    census: u8,
    /// Bit length of the counters' total at the last census line, so a thread
    /// prints at most one line per doubling.
    census_reported_bits: u32,
}

const _: () = {
    use cratonvm_jit_api as api;
    assert!(
        std::mem::offset_of!(JitMonitorBlock, thin_owner)
            == api::JIT_MONITOR_BLOCK_THIN_OWNER_OFFSET
    );
    assert!(std::mem::offset_of!(JitMonitorBlock, held) == api::JIT_MONITOR_BLOCK_HELD_OFFSET);
    assert!(
        std::mem::offset_of!(JitMonitorBlock, lock_stack)
            == api::JIT_MONITOR_BLOCK_LOCK_STACK_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_inline_enters)
            == api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_inline_exits)
            == api::JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_slow_enters)
            == api::JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitMonitorBlock, census_slow_exits)
            == api::JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET
    );
    assert!(std::mem::size_of::<std::sync::atomic::AtomicU64>() == 8);
};

/// `JitMonitorBlock::census`: the flag has not been read on this thread yet.
const CENSUS_UNREAD: u8 = 0;
/// `JitMonitorBlock::census`: `CRATONVM_DBG_JITC` is on.
const CENSUS_ON: u8 = 2;

impl JitMonitorBlock {
    /// The "no inline path" state every thread starts in.
    pub(crate) const fn unarmed() -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            thin_owner: u64::MAX,
            lock_stack: 0,
            census_inline_enters: AtomicU64::new(0),
            census_inline_exits: AtomicU64::new(0),
            census_slow_enters: AtomicU64::new(0),
            census_slow_exits: AtomicU64::new(0),
            held: 0,
            registry_id: 0,
            book: None,
            census: CENSUS_UNREAD,
            census_reported_bits: 0,
        }
    }

    /// Whether this block was armed from the registry `registry_id`.
    #[inline]
    pub(crate) fn is_armed_for(&self, registry_id: u64) -> bool {
        self.thin_owner != u64::MAX && self.registry_id == registry_id
    }

    /// Whether compiled code on this thread may take the inline path now.
    #[inline]
    pub(crate) fn is_armed(&self) -> bool {
        self.thin_owner != u64::MAX
    }

    /// Whether the monitor census is on for this thread (`CRATONVM_DBG_JITC`,
    /// read on the first call and kept per thread): one byte compare after that.
    #[inline]
    pub(crate) fn census_on(&mut self) -> bool {
        if self.census == CENSUS_UNREAD {
            self.census = if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                CENSUS_ON
            } else {
                1
            };
        }
        self.census == CENSUS_ON
    }

    /// `(inline enters, inline exits, helper enters, helper exits)` taken by
    /// this thread's census-compiled monitor sites.
    pub(crate) fn census_counts(&self) -> (u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.census_inline_enters.load(Relaxed),
            self.census_inline_exits.load(Relaxed),
            self.census_slow_enters.load(Relaxed),
            self.census_slow_exits.load(Relaxed),
        )
    }

    /// Whether a census line is due: the counters' total has reached a new
    /// power of two since the last line (so a thread prints O(log n) lines).
    pub(crate) fn census_due(&mut self) -> bool {
        let (a, b, c, d) = self.census_counts();
        let total = a.saturating_add(b).saturating_add(c).saturating_add(d);
        let bits = u64::BITS - total.leading_zeros();
        if bits > self.census_reported_bits {
            self.census_reported_bits = bits;
            true
        } else {
            false
        }
    }

    /// Arm the inline path: `lock_stack` must be an address inside `book`, and
    /// `lease` the calling thread's own lock-slot lease from the
    /// `MonitorTable` of the same VM (`MonitorTable::jit_thin_lease`). A null
    /// counter or lock-stack address leaves the block unarmed.
    ///
    /// Order matters only against compiled code on THIS thread, which cannot
    /// run concurrently with this call (the caller is a helper it called), so
    /// plain stores suffice. `thin_owner` is written last and is what enables
    /// it.
    pub(crate) fn arm(
        &mut self,
        lease: crate::threading::monitor::JitThinLease,
        registry_id: u64,
        book: Arc<crate::threading::thread_registry::JmxMonitorBook>,
        lock_stack: usize,
    ) {
        self.disarm();
        if lease.held == 0 || lock_stack == 0 {
            return;
        }
        self.book = Some(book);
        self.lock_stack = lock_stack;
        self.held = lease.held;
        self.registry_id = registry_id;
        self.thin_owner = u64::from(lease.owner_bits);
    }

    /// Back to the unarmed state: compiled code takes the helper path again.
    /// `thin_owner` first, which is what disables the inline path. The census
    /// counters and the flag reading survive: they describe the thread, not
    /// the arming.
    pub(crate) fn disarm(&mut self) {
        self.thin_owner = u64::MAX;
        self.lock_stack = 0;
        self.held = 0;
        self.registry_id = 0;
        self.book = None;
    }
}

impl Drop for JitMonitorBlock {
    /// The census's last word for a thread that runs no helper-path
    /// `monitorenter` after its hot loop (a worker that locks inline and then
    /// ends): the only line that reports its totals.
    fn drop(&mut self) {
        if self.census != CENSUS_ON {
            return;
        }
        let (ie, ix, se, sx) = self.census_counts();
        if (ie | ix | se | sx) == 0 {
            return;
        }
        eprintln!(
            "[monitor-inline census] thread-end thin_owner={:#x} inline_enters={ie} \
             inline_exits={ix} helper_enters={se} helper_exits={sx}",
            self.thin_owner
        );
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
    /// gen r5w2/roots6: raises `gc_block_state`'s `owner_dropped` when this
    /// `JvmThread` is dropped. See [`GcBlockStateDropNotice`].
    pub gc_block_state_drop_notice: GcBlockStateDropNotice,
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

    /// The exception the interpreter's unwinder last posted a debugger
    /// `Exception` event for (JDWP or JVMTI; interpreter round i1 wave 11),
    /// as a JNI weak global handle, `0` for none. An exception reaches the
    /// unwinder again at every interpreter entry it unwinds through; this
    /// tells those from a new throw (HotSpot's
    /// `JvmtiThreadState::is_exception_detected`). Freed and cleared when an
    /// interpreter handler catches (`interpreter::note_exception_caught_if_armed`),
    /// replaced by the next reported exception. A weak handle, so it is in
    /// neither per-thread root list.
    pub reported_exception: u64,

    /// The address of the innermost native callback this thread runs through
    /// the native-call funnel (`vm_exec::safe_native_call_impl`), recorded only
    /// while a debugger is attached, `0` for none (interpreter round i1 wave
    /// 17, `interpreter::enter_running_native`). Read by the JDWP
    /// blocked-thread reader to name an overridable native a virtual call
    /// selected (`interpreter::blocked_native_method`). Plain data, in
    /// neither per-thread root list.
    pub running_native: usize,

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

    /// The class declaring the JNI native this thread is running, set and
    /// restored by `vm_exec.rs`'s `JniContextGuard::install_for_native`.
    /// JNI `FindClass` resolves through its loader, as HotSpot's
    /// `jni_FindClass` does (the native's own holder is the caller class).
    pub jni_native_class: Option<ClassId>,

    /// The JNI natives this thread is running, outermost first, as
    /// `(interpreter depth at the call, declaring class, key)` (the key: the JNI
    /// function pointer called, or a method-event post's tagged hash;
    /// interpreter round i1 wave 42, lane L1): pushed and popped around each
    /// JNI native call by `jvmti::native_env::NativeFrameRow` (natives nest
    /// through upcalls), read by the JVMTI stack functions
    /// (`jvmti::native_env::frames_of`), which list a native method's frame
    /// at location -1 as HotSpot does. Plain data, in neither per-thread root
    /// list. The fourth member is the thread's JIT entry-chain length at the
    /// call (`jit::conservative_roots::current_thread_jit_depth`; wave 44,
    /// lane L3): a compiled method the native's own upcall entered is listed
    /// above it (`stackwalker::capture_trace_with_anchor_positions`).
    pub jni_native_frames: Vec<(usize, ClassId, u64, u32)>,

    /// The reflective calls (`Method.invoke`, `Constructor.newInstance`) this
    /// thread is inside, outermost first: pushed and truncated around each by
    /// the natives that serve them (`NativeExceptionAccess::
    /// enter_reflective_call`), read by the stack captures, which list the
    /// JDK frames HotSpot shows for such a call
    /// (`stackwalker::reflective_splices`; interpreter round i1 wave 43, lane
    /// L3). Plain data, in neither per-thread root list.
    pub reflective_calls: Vec<crate::runtime::stackwalker::ReflectiveCallRow>,

    /// Bitmask of the `ReflectiveCallKind`s whose accessor class this thread
    /// has already asked the class manager to load (bit 0 `MethodInvoke`,
    /// bit 1 `ConstructorNewInstance`): a reflective call's JDK frames name
    /// it, so it must be loaded before a capture can list them.
    pub reflective_accessors_loaded: u8,

    /// The JDK frames of each reflective-call kind as this thread's captures
    /// last named them (`stackwalker::ReflectiveFramesNamed`), reused while no
    /// class is redefined.
    pub reflective_frames_named: crate::runtime::stackwalker::ReflectiveFramesNamed,

    /// Bounded JIT cache for immutable String keys in exact HashMaps. Entries
    /// are cleared lazily on a structural change, and wholesale when
    /// [`Self::jit_memo_epoch`] goes stale.
    pub jit_hashmap_string_node_cache: Vec<JitHashMapStringNodeCacheEntry>,
    pub string_case_cache: Vec<StringCaseCacheEntry>,
    /// The GC-pause generation (`GcBarrier::gc_generation`) the two JIT memo
    /// caches above were last validated at. A probe or put, and the safepoint
    /// publish, at a later generation clear both first — so a cache the thread
    /// has not touched since the last pause stops keeping a dropped `HashMap`
    /// or `String` alive, and no entry is ever served across a pause. Present
    /// entries are always roots. See `memory::roots::validate_jit_memo_caches`.
    pub jit_memo_epoch: u64,

    /// This thread's `ThreadLocal` values while it is NOT mounted on an OS
    /// thread: a virtual thread between continuation slices. The intrinsics
    /// keep the RUNNING thread's map in an OS-thread `thread_local!`;
    /// `vm_exec::resume_virtual_continuation` swaps this state in at every
    /// mount and back out at every unmount, so a virtual thread's values
    /// neither leak to the next virtual thread on the carrier nor get lost when
    /// it migrates. Always empty for a platform thread. Object values are JNI
    /// global roots already, so this needs no scan.
    pub detached_thread_locals: cratonvm_native_builtins::phases_early::DetachedThreadLocals,

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

    /// `cratonvm_jit::jit_withdrawn_bodies()` when this thread last released
    /// the withdrawn bodies its dispatch caches pin. See
    /// `jit_bridge::release_withdrawn_code_owners`.
    pub jit_withdrawals_seen: u64,

    /// `cratonvm_classloading::class_redefinition_count()` when this thread
    /// last moved its frames that run a replaced method body onto the
    /// translated one (`interpreter::obsolete_frames`, interpreter round i1
    /// wave 19, lane L3). One load and a compare at every safepoint and
    /// blocking-region exit until a class is redefined.
    pub redefinitions_seen: u64,

    /// `Some(withdrawn)` while this thread runs a batch of redefinitions
    /// (`NativeContext::begin_redefinition_batch`, one
    /// `Instrumentation.retransformClasses` / `redefineClasses` call):
    /// `JitCache::bodies_withdrawn_by_redefinition` when it opened. Each
    /// redefinition in it defers its loop-exit handshake, and the batch's end
    /// takes one if any body was withdrawn since (interpreter round i1 wave
    /// 25, lane L6).
    pub(crate) redefinition_batch: Option<u64>,

    /// Round 14 wave 6 (lane trace5; TR5-2): this thread's last bottom
    /// `Thread.run` decision (`vm_exec::prepend_thread_run_standin_frame`),
    /// so a throw on an executor worker skips the class-manager lock. `None`
    /// until the first such capture.
    pub(crate) thread_run_bottom_memo: Option<crate::runtime::stackwalker::ThreadRunBottomMemo>,

    /// Round 14 wave 6 (lane trace5; TR5-3): `Some(timed)` from a served
    /// `Thread.join(long)` about to raise `InterruptedException`
    /// (`NativeThreadAccess::set_served_timed_join_hint`); taken by the next
    /// throwable capture on this thread.
    pub(crate) served_timed_join_hint: Option<bool>,
    // The code those frames ran before they were moved is kept in each frame
    // (`ReplacedBody::retired_code`), not here: interpreter round i1 wave 22,
    // lane L3 retired `retired_obsolete_code`, which kept it until the thread
    // ended.

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
    /// Quickened `getstatic` sites: the resolved field of a site whose
    /// declaring class was already initialized when it was filled, so a hit
    /// skips resolution AND the class-initialization check. Same key and epoch
    /// validation as `field_sites`; admission rule in
    /// `interpreter::field_fast::fill_static_site`.
    pub static_field_sites: crate::runtime::interpreter::FieldSiteCache,

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

    /// Per-thread numeric `ldc` / `ldc2_w` constants, so a warm site takes
    /// neither the `class_manager` nor the `resolution_cache` read lock. See
    /// `site_cache::PrimitiveConstantSiteCache`.
    pub prim_const_sites: crate::runtime::interpreter::site_cache::PrimitiveConstantSiteCache,

    /// Per-thread memo for the interface receiver-selection re-check that
    /// `execute_invokevirtual_cached` performs on every `invokeinterface`
    /// cache hit. See `IfaceSelectSiteCache` for what it stores and why the
    /// value has to carry the receiver class as well as the site.
    pub iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache,

    /// Per-thread copy of the linked JDK-factory `invokedynamic` sites, so a
    /// warm site is executed without the `resolution_cache` lock and hash.
    /// See `runtime::invokedynamic::IndySiteCache`.
    pub indy_sites: crate::runtime::invokedynamic::IndySiteCache,

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

    /// Why this virtual thread's continuation last yielded, written by the
    /// yielding native's `NativeContext` call (`vt_park_for`,
    /// `vt_sleep_for`, `vt_wait_on_key`) and taken by the remount
    /// (`interpreter::resume_continuation`). See [`VtYield`].
    pub vt_yield: VtYield,

    /// Return conversions a remounted continuation still owes, one per impl
    /// frame a lambda dispatch entered through a nested Rust call (wave 25,
    /// lane L7). Empty on every platform thread and on almost every virtual
    /// one; the interpreter's return arms test `is_empty()` only. See
    /// [`ContinuationReturnAdapter`].
    pub continuation_return_adapters: Vec<ContinuationReturnAdapter>,

    /// Whether this thread's JMX book holds a frame-monitor publication
    /// (`ThreadRegistry::publish_jmx_frame_monitors`), so a deposit with no
    /// monitor held skips the registry entirely (wave 24, lane L7).
    /// Owner-only; atomic only so the `&self` deposit path can write it.
    pub jmx_frame_monitors_published: AtomicBool,

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

    /// The class ids of the `checkcast` failure being raised on this thread:
    /// the receiver's class and the resolved target's, for the
    /// `ClassCastException` message funnel, which otherwise resolves each
    /// operand by NAME and cannot tell two loaders' same-named classes apart
    /// (interpreter round i1 wave 32, `exceptions::note_cast_operands`). The
    /// two ids packed high/low, `u64::MAX` when empty; atomic because the
    /// funnel holds the thread shared, and it takes the note.
    pub cast_operands_note: std::sync::atomic::AtomicU64,

    /// The VM-initiated `loadClass` drives in flight on this thread,
    /// innermost last: `(referencing class id, internal name, referencing
    /// loader's native id or u32::MAX)`. Read by
    /// `interpreter::constants::drive_defining_loader_load_named` for its
    /// re-entry guard, its `ClassCircularityError` and its parallel-capable
    /// re-ask nesting. On the thread, not in a `thread_local!`, so the
    /// rows are this VM's by construction: two VMs driven on one OS thread
    /// (an embedder, or a native of one VM calling into another) no longer
    /// see each other's class ids (interpreter round i1 wave 40, lane L5).
    pub loader_drives_in_flight: Vec<(u32, String, u32)>,

    /// Thread-local allocation buffer for lock-free young-gen allocation.
    pub tlab: cratonvm_gc::Tlab,

    /// The `tlab.thread_allocated_bytes()` total at which the interpreter's
    /// `maybe_gc` next asks the heap-occupancy triggers; 0 = at the next call.
    /// Reset to 0 by a TLAB refill and by an allocation outside the TLAB. See
    /// `interpreter::gc_and_alloc::occupancy_poll_due`.
    pub gc_occupancy_poll_at: u64,

    /// Per-thread **shadow stack** of live object references for precise,
    /// rewritable GC roots inside JIT-compiled code (see
    /// `cratonvm_gc::shadow_stack`). JIT code pushes live oops onto it before
    /// a GC-capable call and reloads them after; the collector marks and
    /// rewrites the pushed slots precisely, which lets a *moving* young-gen
    /// collection run safely while JIT frames are live. Unallocated (empty)
    /// until the thread first enters JIT code with the mechanism enabled.
    pub shadow_stack: cratonvm_gc::shadow_stack::ShadowStack,

    /// The inline thin-lock's per-thread block (owner id + JMX lock stack),
    /// read by compiled `monitorenter`/`monitorexit` at
    /// [`Self::jit_monitor_block_offset`]. Unarmed until this thread's first
    /// helper-path `monitorenter`; see [`JitMonitorBlock`].
    pub(crate) jit_monitor_block: JitMonitorBlock,

    /// The compiled-recursion native-stack floor of the OS thread this thread
    /// last ran compiled code on (round 13 wave 4, lane mega3, proposal
    /// M13-3): written by `jit::helpers::set_jit_thread` whenever it publishes
    /// this thread, and read by compiled inline-cache stack checks through the
    /// `JIT_THREAD` mirror at `JitRuntimeHelpers::jit_stack_floor_offset_in_thread`
    /// (`CMP RSP, [thread + off]; JBE <helper>`). `0` never trips.
    pub(crate) jit_stack_floor: usize,

    /// Extra compiled self-recursion budget (bytes) of a thread whose carrier
    /// `thread_start` grew for a `stackSize` / `-Xss` request: the growth over
    /// the default carrier. 0 for every other thread. Read by
    /// `jit::helpers::compute_self_call_stack_floor` (gcd d6/f item 2).
    pub(crate) self_call_extra_budget: usize,

    /// gce e2/t: `CRATONVM_DBG_XT_FORCE_TAKEOVER`, read the first time this
    /// thread's compiled poll finds a pause (never on a thread that does not),
    /// plus the pause it is declining its compiled polls for. See
    /// `jit::xt_root_scan::ForceTakeover`.
    pub(crate) xt_force_takeover: crate::jit::xt_root_scan::ForceTakeover,

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
    /// sits next to `java_thread_obj` in both halves of the root
    /// machinery: `memory/roots.rs` §10 pushes it, `memory/gc.rs` §10 rewrites
    /// it.
    ///
    /// A collection between the stash and the drain therefore keeps the
    /// throwable alive and hands the drain its post-move address.
    /// See `jit-signals-root-gap.md`.
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

    /// Byte offset of the [`JitMonitorBlock`] from the start of `JvmThread`,
    /// published to the JIT as `JitRuntimeHelpers::monitor_block_offset_in_thread`.
    /// A const `offset_of!`, unlike the two lazily-probed offsets above: the
    /// workspace MSRV (1.88) has had it since 1.77.
    pub(crate) const fn jit_monitor_block_offset() -> usize {
        std::mem::offset_of!(JvmThread, jit_monitor_block)
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
        let gc_block_state = Arc::new(GcBlockState::new());
        Self {
            gc_block_state_drop_notice: GcBlockStateDropNotice::new(&gc_block_state),
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
            gc_block_state,
            jmx_locked_synchronizers: None,
            native_pin_roots: Vec::new(),
            reported_exception: 0,
            running_native: 0,
            native_alloc_pool: Vec::new(),
            native_alloc_pool_layout: None,
            handle_slots: Vec::new(),
            handle_scope_bases: Vec::new(),
            native_pending_return: None,
            jni_native_class: None,
            jni_native_frames: Vec::new(),
            reflective_calls: Vec::new(),
            reflective_accessors_loaded: 0,
            reflective_frames_named: Default::default(),
            jit_hashmap_string_node_cache: Vec::new(),
            string_case_cache: Vec::new(),
            jit_memo_epoch: 0,
            detached_thread_locals: Default::default(),
            invoke_cache: InvokeCache::new(),
            jit_withdrawals_seen: 0,
            // Frames this thread builds from now on index the current pools.
            redefinitions_seen: cratonvm_classloading::class_redefinition_count(),
            redefinition_batch: None,
            thread_run_bottom_memo: None,
            served_timed_join_hint: None,
            field_sites: crate::runtime::interpreter::FieldSiteCache::new(),
            fast_field_sites: crate::runtime::interpreter::FastFieldSiteCache::new(),
            static_field_sites: crate::runtime::interpreter::FieldSiteCache::new(),
            method_sites: crate::runtime::interpreter::MethodSiteCache::new(),
            class_sites: crate::runtime::interpreter::ClassSiteCache::new(),
            cast_sites: crate::runtime::interpreter::CastSiteCache::new(),
            prim_const_sites:
                crate::runtime::interpreter::site_cache::PrimitiveConstantSiteCache::new(),
            iface_select_sites: crate::runtime::interpreter::IfaceSelectSiteCache::new(),
            indy_sites: crate::runtime::invokedynamic::IndySiteCache::new(),
            native_shadow_cache: FxHashMap::default(),
            kind: ThreadKind::Platform,
            pin_count: 0,
            pin_reason: "",
            vt_yield: VtYield::Park,
            continuation_return_adapters: Vec::new(),
            jmx_frame_monitors_published: AtomicBool::new(false),
            scoped_values: Vec::new(),
            cast_operands_note: std::sync::atomic::AtomicU64::new(u64::MAX),
            loader_drives_in_flight: Vec::new(),
            tlab: cratonvm_gc::Tlab::empty(),
            gc_occupancy_poll_at: 0,
            shadow_stack: cratonvm_gc::shadow_stack::ShadowStack::empty(),
            jit_monitor_block: JitMonitorBlock::unarmed(),
            jit_stack_floor: 0,
            self_call_extra_budget: 0,
            xt_force_takeover: crate::jit::xt_root_scan::ForceTakeover::new(),
            jit_pending_exception: None,
            uncaught_exception_pending: None,
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
    /// `docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md`.
    ///
    /// Returns `false` when there was no frame to pop, so callers keep the
    /// `if let Some(..)` shape the `pop()` form gave them.
    #[inline]
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
        self.recycle_top_frame_pooled(operand_stack_pool, tag_pool)
    }

    /// [`Self::recycle_top_frame_in_place`] under
    /// `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`: harvest the top frame's buffers
    /// into the pools and drop its husk. Out of line so every return keeps
    /// only the one-line retire (interpreter round i1 wave 32).
    #[inline(never)]
    fn recycle_top_frame_pooled(
        &mut self,
        operand_stack_pool: &crate::runtime::alloc_fastpath::VecPool<u64>,
        tag_pool: &crate::runtime::alloc_fastpath::VecPool<u8>,
    ) -> bool {
        let depth = self.frames.len();
        let Some(top) = self.frames.last_mut() else {
            return false;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = top.take_pool_parts_in_place();
        // `truncate_hard` runs `Drop` on the element where it sits and never
        // moves a surviving frame — the address-stability contract
        // `FrameStack` exists for. The husk's remaining fields (metadata
        // `Arc`s, the code, `osr_attempt_counts`) are released by that drop
        // exactly as they were by the old move. Plain `truncate` (used here
        // until interpreter round i1 wave 37) only lowers the depth since
        // frames are retired in place, so every pop under this switch left an
        // emptied husk holding its method's `Arc`s until the next push at
        // that depth -- the husk `truncate_hard` exists to drop.
        self.frames.truncate_hard(depth - 1);
        // A slab-windowed frame (interpreter round i1 wave 29,
        // `runtime::slot_slab`) hands back four EMPTY `Vec`s: routing them
        // would poison the pools, as `harvest_retired_slot` explains.
        if local_vals.capacity() == 0 && stack_vals.capacity() == 0 {
            return true;
        }
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

    /// Hand the retired slot's pooled buffers to the thread pools and leave
    /// the slot where it is, a husk that the next cached install rebuilds in
    /// a slot-slab window (`FrameStack::retired_slot_takes_a_window`'s husk
    /// rule). Interpreter round i1 wave 32: a slot that first held a pooled
    /// frame used to reset its own four buffers on every later call at its
    /// depth, and that is most slots on a call-heavy workload. Unlike
    /// [`Self::harvest_retired_slot`] nothing above it is trimmed, and the
    /// buffers are pooled, not freed, so a by-value push at this depth finds
    /// them again.
    #[cold]
    #[inline(never)]
    pub fn convert_retired_slot_to_window(&mut self) {
        let Some(slot) = self.frames.retired_slot_mut() else {
            return;
        };
        let (local_vals, local_tags, stack_vals, stack_tags) = slot.take_pool_parts_in_place();
        if local_vals.capacity() == 0 && stack_vals.capacity() == 0 {
            return;
        }
        if self.locals_pool.len() < MAX_POOL_SIZE {
            self.locals_pool.push((local_vals, local_tags));
            self.stacks_pool.push((stack_vals, stack_tags));
        }
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

    /// The monitor census counters are where compiled code adds to them (the
    /// pinned `JIT_MONITOR_BLOCK_*` offsets), `census_counts` reads those
    /// words, a disarm keeps them, and `census_due` fires once per doubling.
    #[test]
    fn monitor_census_counters_sit_at_the_offsets_compiled_code_bumps() {
        use cratonvm_jit_api as api;
        let mut block = JitMonitorBlock::unarmed();
        let base = &mut block as *mut JitMonitorBlock as *mut u8;
        let bump = |off: usize, by: u64| {
            // SAFETY: `off` is one of the four pinned counter offsets of the
            // `#[repr(C)]` block, each an 8-aligned `AtomicU64`; this is the
            // plain `ADD QWORD [thread + block + off]` the JIT emits.
            unsafe {
                let p = base.add(off) as *mut u64;
                p.write(p.read() + by);
            }
        };
        bump(api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET, 1);
        bump(api::JIT_MONITOR_BLOCK_INLINE_EXITS_OFFSET, 2);
        bump(api::JIT_MONITOR_BLOCK_SLOW_ENTERS_OFFSET, 3);
        bump(api::JIT_MONITOR_BLOCK_SLOW_EXITS_OFFSET, 4);
        assert_eq!(block.census_counts(), (1, 2, 3, 4));
        block.disarm();
        assert_eq!(
            block.census_counts(),
            (1, 2, 3, 4),
            "disarm keeps the census"
        );
        assert!(!block.is_armed());
        // Total 10 (bit length 4): due once, then not again until 16.
        assert!(block.census_due());
        assert!(!block.census_due());
        let base = &mut block as *mut JitMonitorBlock as *mut u8;
        // SAFETY: as above.
        unsafe {
            let p = base.add(api::JIT_MONITOR_BLOCK_INLINE_ENTERS_OFFSET) as *mut u64;
            p.write(p.read() + 6);
        }
        assert!(block.census_due(), "16 is a new doubling");
        // Never read the flag here: `Drop` must stay silent for this block.
        assert_eq!(block.census, CENSUS_UNREAD);
    }
}
