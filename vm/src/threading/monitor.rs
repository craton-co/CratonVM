// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVM monitor (intrinsic lock) implementation.
//!
//! Every Java object can be used as a monitor. The JVM's `monitorenter` and
//! `monitorexit` instructions acquire and release these monitors. Monitors
//! are **reentrant**: a thread that already owns a monitor can enter it again,
//! incrementing an entry count.
//!
//! The monitor table is a global structure keyed by object identity (pointer
//! address as `usize`). Monitors are created lazily on first use.
//!
//! When multiple OS threads contend for the same monitor, `parking_lot::Condvar`
//! is used to block and wake threads. Two separate condvars are used:
//! - `entry_condvar`: wakes threads blocked on `monitorenter`
//! - `wait_condvar`: wakes threads blocked on `Object.wait()`

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use cratonvm_types::{self as types, ObjectHeader};
use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

use crate::error::{MethodCallFailed, RuntimeError, VmError};
// SECURITY FIX (V11): wire the L6 `monitors` registry through the lock-order
// enforcement framework so the documented hierarchy is actually checked at
// runtime in debug builds.
use crate::runtime::lock_order::{LockLevel, OrderedMutex};
use crate::threading::jvm_thread::ThreadId;
use crate::types::ObjectRef;

// ---------------------------------------------------------------------------
// KC16-watchdog: global stack-dump-request flag for parked Object.wait()ers
// ---------------------------------------------------------------------------
//
// The interpreter top-of-loop polls `SharedVm::stack_dump_pending()` so any
// thread executing bytecode acks the watchdog within a single dispatch.
// Threads parked in `Object.wait()` are blocked OFF the interpreter loop
// (inside `parking_lot::Condvar::wait_for`) and therefore never reach the
// top-of-loop check. This static flag lets the watchdog wake them.
//
// `SharedVm::request_stack_dump()` sets this flag in addition to its own
// per-VM flag. The wait loop in `Monitor::wait` polls it every 5ms (the
// same cadence as the interrupt flag) and exits the wait when set; the
// monitor_wait caller then sees the `SharedVm` flag and dumps frames
// through the normal path.
static STACK_DUMP_WAIT_FLAG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn stack_dump_wait_flag() -> &'static std::sync::atomic::AtomicBool {
    &STACK_DUMP_WAIT_FLAG
}

/// KC16-watchdog: signal all threads parked in `Object.wait()` to exit the
/// condvar wait and re-check their state (where the interpreter loop will
/// observe `SharedVm::stack_dump_pending()` and emit a frame snapshot).
///
/// Called by `SharedVm::request_stack_dump()`.
pub fn signal_stack_dump_to_waiters() {
    STACK_DUMP_WAIT_FLAG.store(true, std::sync::atomic::Ordering::Release);
}

/// KC16-watchdog: gated diagnostic. When `CRATONVM_DBG_MONENTER` is set, the
/// contended `Monitor::enter` loop polls the stack-dump flag and emits the
/// blocked thread's frame snapshot (deposited by `monitor_enter` in
/// `vm_exec`). This surfaces a thread deadlocked while acquiring a
/// `synchronized` monitor — otherwise invisible to the watchdog, which only
/// sees `Object.wait` / `LockSupport.park` waiters. **Default OFF**: the
/// hot contended-enter path stays byte-for-byte unchanged in normal runs
/// (plain `entry_condvar.wait`); the poll variant runs only under the flag.
/// Read once and cached so the per-enter check is a single relaxed load.
pub fn mon_enter_dump_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("CRATONVM_DBG_MONENTER").is_ok())
}

/// PERF (monitor-leak reclaim): opt-in gate for reclaiming inflated-monitor
/// registry entries whose object is dead. See `MonitorTable::remap_after_gc`
/// for the full safety rationale — in short, "absent from the GC forwarding
/// map" is a reliable *dead* signal **only for a whole-heap collection**; for
/// partial collectors (G1 young/mixed, generational minor GC) an absent key may
/// be a live in-place survivor, so dropping it would desync the registry from
/// the surviving object's `INFLATED` mark word. Until the GC can pass a
/// dead-address set (flagged cross-file follow-up), monitor reclamation is
/// **default OFF** so the registry behaviour stays byte-identical. CAS-lock
/// reclamation is unconditional (safe for all collectors) and does NOT consult
/// this flag. Read once and cached so the per-GC check is a single relaxed load.
#[inline]
fn reclaim_dead_monitors_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_RECLAIM_DEAD_MONITORS").is_some())
}

/// KC16-watchdog: callback installed by the VM that emits the current
/// thread's frame chain from a wait-site context. Set by `SharedVm`
/// during construction so `Monitor::wait` can reach the per-thread
/// frame table held in the `JvmThread` registry.
type WaitSiteDumpFn = Arc<dyn Fn(ThreadId) + Send + Sync>;
static WAIT_SITE_DUMP: std::sync::OnceLock<WaitSiteDumpFn> = std::sync::OnceLock::new();

pub fn install_wait_site_dump<F>(f: F)
where
    F: Fn(ThreadId) + Send + Sync + 'static,
{
    let _ = WAIT_SITE_DUMP.set(Arc::new(f));
}

fn emit_wait_site_frames(thread_id: ThreadId) {
    if let Some(f) = WAIT_SITE_DUMP.get() {
        f(thread_id);
    } else {
        tracing::warn!(
            target: "kc16_watchdog",
            thread_id = ?thread_id,
            "Object.wait() observed stack_dump request but no dump callback installed"
        );
    }
}

// ---------------------------------------------------------------------------
// Thin-lock fast-path helpers
// ---------------------------------------------------------------------------
//
// These functions implement the lock-word state machine on the object header
// directly, avoiding any allocation in the uncontended case.
//
// State transitions (see `types::heap_types` for the mark word layout):
//
//   NEUTRAL --CAS--> THIN_LOCKED       (try_thin_lock)
//   THIN_LOCKED(self) --CAS--> THIN_LOCKED(self, recursion+1)
//                                       (try_thin_recursive_lock)
//   THIN_LOCKED(self) --CAS--> THIN_LOCKED(self, recursion-1) or NEUTRAL
//                                       (try_thin_unlock)
//   NEUTRAL / THIN_LOCKED --CAS--> INFLATED(&Monitor)
//                                       (MonitorTable::inflate_locked)
//
// The inflation path is the only one that allocates a `Monitor`. It must
// publish the mark word via `compare_exchange` against the observed
// pre-inflation mark, *not* an unconditional store, to avoid clobbering a
// concurrent CAS (e.g. another thread releasing a thin lock back to
// NEUTRAL). `MonitorTable::inflate_locked` enforces this; the bare
// `inflate_unchecked` helper (kept for diagnostics / future reuse) is
// `pub(crate)` and documented as unsafe in that respect.

/// Attempt thin-lock acquisition via a single CAS on the mark word.
///
/// Returns `Ok(())` on success (the calling thread is now the thin-lock owner
/// with recursion = 0). Returns `Err(current_mark)` if the CAS failed -- the
/// caller can inspect the current state to decide whether to retry, recurse,
/// or inflate.
#[inline]
pub fn try_thin_lock(header: &ObjectHeader, thread_id: u32) -> Result<(), u64> {
    header
        .mark_word
        .compare_exchange(
            types::MARK_NEUTRAL,
            ObjectHeader::make_thin_locked(thread_id, 0),
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .map(|_| ())
        .map_err(|m| m)
}

/// Re-entrant thin-lock: bump the recursion counter via CAS.
///
/// On success returns `Ok(new_recursion)`. Returns `Err(current_mark)` if:
/// - the object is not in `THIN_LOCKED` state, or
/// - it is thin-locked by a *different* thread, or
/// - the recursion counter is at u8::MAX (caller must inflate to support
///   deeper nesting).
#[inline]
pub fn try_thin_recursive_lock(header: &ObjectHeader, thread_id: u32) -> Result<u8, u64> {
    loop {
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return Err(cur);
        }
        if ObjectHeader::thin_lock_owner(cur) != thread_id {
            return Err(cur);
        }
        let recursion = ObjectHeader::thin_lock_recursion(cur);
        if recursion == u8::MAX {
            return Err(cur); // overflow → must inflate
        }
        let new = ObjectHeader::make_thin_locked(thread_id, recursion + 1);
        if header
            .mark_word
            .compare_exchange(cur, new, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(recursion + 1);
        }
        // Another thread mutated the mark word (almost certainly because
        // it inflated the lock) — restart and re-classify.
    }
}

/// Symmetric counterpart to `try_thin_lock` / `try_thin_recursive_lock`:
/// drop one level of thin-lock ownership.
///
/// On success returns `Ok(Some(new_recursion))` if recursion remains > 0,
/// or `Ok(None)` if the lock was fully released (mark word now NEUTRAL).
///
/// Returns `Err(current_mark)` if the mark word is not `THIN_LOCKED` by the
/// calling thread (caller must dispatch to the inflated path or raise
/// `IllegalMonitorStateException`).
#[inline]
pub fn try_thin_unlock(header: &ObjectHeader, thread_id: u32) -> Result<Option<u8>, u64> {
    loop {
        let cur = header.mark_word.load(Ordering::Relaxed);
        if ObjectHeader::mark_state(cur) != types::MARK_THIN_LOCKED {
            return Err(cur);
        }
        if ObjectHeader::thin_lock_owner(cur) != thread_id {
            return Err(cur);
        }
        let recursion = ObjectHeader::thin_lock_recursion(cur);
        let (new, ret) = if recursion == 0 {
            // Last release → return to NEUTRAL.
            (types::MARK_NEUTRAL, None)
        } else {
            (
                ObjectHeader::make_thin_locked(thread_id, recursion - 1),
                Some(recursion - 1),
            )
        };
        if header
            .mark_word
            .compare_exchange(cur, new, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(ret);
        }
        // Lost the CAS — re-evaluate (an inflation by a concurrent thread
        // is the only realistic cause since we, the owner, are the only one
        // who can legally change a THIN_LOCKED mark word otherwise).
    }
}

/// Inflate a thin lock (or a neutral mark) to a heap-allocated `Monitor`
/// **without** CAS-protecting the publish. Visibility is restricted to the
/// crate so that the only legitimate caller is `MonitorTable::inflate_locked`,
/// which holds the registry mutex and performs its own `compare_exchange`
/// against the observed mark word before/instead of calling this helper.
///
/// # Safety / correctness contract
///
/// This helper publishes the new mark word via an **unconditional**
/// `store(Release)`. That is only sound when the caller has *already
/// confirmed*, via CAS, that the mark word still holds the value it observed
/// — otherwise this store will silently clobber a concurrent CAS (e.g. a
/// thin-lock owner releasing back to NEUTRAL, or another thread's earlier
/// inflation publishing a different `Arc<Monitor>`), corrupting the lock
/// state machine. The bare helper is therefore named `_unchecked` and is
/// not part of the public API; new call sites must instead use
/// `MonitorTable::inflate_locked`, which routes inflation through the
/// registry mutex and performs the CAS publish itself.
///
/// If `current_owner` is `Some((tid, recursion))`, the new monitor is
/// pre-acquired by that thread with the given entry count, atomically
/// transferring ownership from the thin-lock representation. This is used
/// when the current owner inflates its own lock (to support deeper recursion)
/// and also when a contending thread inflates a lock held by someone else.
///
/// Returns the leaked `Monitor` pointer. Callers are responsible for keeping
/// the `Monitor` alive (e.g. by stashing an `Arc<Monitor>` in
/// `MonitorTable::monitors`) so that the GC remap path can find it.
#[inline]
#[allow(dead_code)] // retained for future direct inflation paths; the in-tree
                    // inflation goes through `MonitorTable::inflate_locked`,
                    // which inlines the same logic under its registry mutex.
pub(crate) fn inflate_unchecked(
    header: &ObjectHeader,
    monitor: Arc<Monitor>,
    current_owner: Option<(u32, u8)>,
) -> *mut Monitor {
    if let Some((tid, recursion)) = current_owner {
        monitor.enter_with_recursion(ThreadId(tid as u64), (recursion as u32) + 1);
    }
    // Cast through *const to *mut — `Arc::as_ptr` only exposes the const
    // form, but the mark word stores an opaque address tag, not a reference
    // that gets dereferenced through this pointer.
    let raw = Arc::as_ptr(&monitor) as *mut Monitor;
    // SAFETY: Monitor is naturally 8-byte aligned (it contains a Mutex which
    // has at least pointer alignment); the low 2 bits are therefore zero and
    // safe to use as the state tag.
    let new_mark = ObjectHeader::make_inflated(raw as usize);
    // NOTE: unconditional store — see the function-level doc for the
    // CAS-correctness contract. Direct callers outside
    // `MonitorTable::inflate_locked` will violate the lock state machine.
    header.mark_word.store(new_mark, Ordering::Release);
    raw
}

/// Look up an `&ObjectHeader` from an `ObjectRef`. Mirrors the pattern used by
/// the GC (`cratonvm_gc::heap::Heap::get_header`) — the first
/// `HEADER_SIZE` bytes of every heap allocation are a `repr(C)` `ObjectHeader`.
#[inline]
fn header_of(obj_ref: ObjectRef) -> &'static ObjectHeader {
    // SAFETY: `ObjectRef` is constructed only from live, properly aligned heap
    // allocations whose first bytes are an `ObjectHeader`. The lifetime is
    // bounded by the GC, which scans monitor state at safepoints.
    unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
}

/// Truncate a `ThreadId(u64)` to the 32-bit field stored in the thin-lock
/// owner slot. Thread ids that exceed `u32::MAX` cannot be represented in the
/// thin lock and force inflation; in practice the JVM never reaches that many
/// live threads.
#[inline]
fn tid_to_u32(tid: ThreadId) -> Option<u32> {
    if tid.0 <= u32::MAX as u64 {
        Some(tid.0 as u32)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Monitor — per-object lock
// ---------------------------------------------------------------------------

/// A single JVM monitor (intrinsic lock).
///
/// Each monitor tracks its owner thread and re-entry count. Two condvars
/// separate the two kinds of blocking: monitor entry contention and
/// `Object.wait()`/`notify()`.
pub struct Monitor {
    state: Mutex<MonitorState>,
    /// Wakes threads blocked on `monitorenter` (waiting to acquire the lock).
    entry_condvar: Condvar,
    /// Wakes threads blocked on `Object.wait()`.
    wait_condvar: Condvar,
}

/// The mutable state protected by a monitor's mutex.
struct MonitorState {
    /// The thread that currently owns this monitor, or `None` if unlocked.
    owner: Option<ThreadId>,
    /// Re-entry count. Incremented on each `monitorenter`, decremented on
    /// `monitorexit`. The monitor is released when this reaches 0.
    entry_count: u32,
    /// Round-7 HIGH (vm #5): JFR enter/exit event-pair consistency.
    ///
    /// JFR can be enabled or disabled at any moment (`cratonvm_jfr::is_enabled()`
    /// flips via a global AtomicBool). The interpreter's `Monitorenter` path
    /// snapshots `is_enabled()` *before* acquiring the lock and only emits the
    /// JFR monitor-enter event if that snapshot was true. The corresponding
    /// `Monitorexit` path would naturally re-check `is_enabled()` — but if JFR
    /// *enabled* between the enter snapshot and the exit, the exit-side check
    /// would emit an exit event for which no matching enter event was ever
    /// recorded (orphan exit), and downstream JFR consumers correlate enter
    /// and exit events by `(thread_id, monitor_addr)` so an orphan exit
    /// corrupts their per-monitor wait-time aggregation.
    ///
    /// To keep the enter/exit pair atomic with respect to JFR state, we stash
    /// the enter-time decision on the monitor itself: the interpreter sets
    /// this flag via [`Monitor::set_jfr_enter_recorded`] right after it emits
    /// the enter event, and the exit path consults
    /// [`Monitor::take_jfr_enter_recorded`] to decide whether to emit the
    /// matching exit event. Only the outermost reentrant enter records (so a
    /// nested re-acquire of the same monitor by the same thread doesn't
    /// produce a spurious paired event); when `entry_count` returns to 0 the
    /// flag is cleared so the next acquisition starts fresh.
    jfr_enter_recorded: bool,
}

impl Monitor {
    /// Create a new, unlocked monitor.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(MonitorState {
                owner: None,
                entry_count: 0,
                jfr_enter_recorded: false,
            }),
            entry_condvar: Condvar::new(),
            wait_condvar: Condvar::new(),
        }
    }

    /// Pre-acquire this monitor for `thread_id` at the given entry count.
    ///
    /// Used exclusively by the inflation handoff (`inflate_unchecked` /
    /// `MonitorTable::inflate_locked`) to atomically transfer ownership
    /// from the thin-lock representation to a freshly created `Monitor`.
    /// The monitor must be brand new (no other thread can observe it yet
    /// because the mark word still points at the thin state), so this is
    /// a pure local initialization — no condvar signalling needed.
    pub(crate) fn enter_with_recursion(&self, thread_id: ThreadId, entry_count: u32) {
        debug_assert!(entry_count >= 1, "entry_count must be at least 1");
        let mut state = self.state.lock();
        debug_assert!(state.owner.is_none(), "Monitor must be fresh");
        state.owner = Some(thread_id);
        state.entry_count = entry_count;
    }

    /// Returns true if this monitor is currently owned by the given
    /// thread. Non-blocking inspection — used by `Thread.holdsLock`.
    fn is_held_by(&self, thread_id: ThreadId) -> bool {
        let state = self.state.lock();
        state.owner == Some(thread_id)
    }

    /// Non-blocking inspection of the current owner, if any. Used at the
    /// contention point (`vm_exec::monitor_enter_blocking`) to detect a
    /// monitor whose owner has since died — see
    /// `MonitorTable::release_monitors_held_by` for why a one-time sweep at
    /// thread-death time isn't sufficient by itself: a monitor that was
    /// still a thin lock (never contended) when its owning thread died gets
    /// inflated *later*, by whichever thread next contends it, and that
    /// inflation pre-seeds the new `Monitor`'s owner from the stale mark
    /// word — the death-time sweep can't have found an object that wasn't
    /// inflated yet.
    pub(crate) fn current_owner(&self) -> Option<ThreadId> {
        self.state.lock().owner
    }

    /// PERF (monitor-leak reclaim): returns true iff this monitor is fully
    /// idle — unowned and with a zero entry count. A monitor that is held,
    /// re-entered, or in the middle of a `wait()` (which temporarily clears
    /// `owner` but keeps a non-zero `saved_count` *only on the stack*, and is
    /// represented to the registry by a live extra `Arc` clone held by the
    /// blocked thread — see the `strong_count` guard at the reclaim site)
    /// reports `false`. Used by `remap_after_gc` to decide whether a
    /// dead-object monitor entry can be dropped without losing lock state.
    ///
    /// Note: idleness alone does NOT prove the object is dead — the reclaim
    /// site additionally requires (a) the object is absent from the GC's live
    /// forwarding map for a whole-heap collection and (b) `Arc::strong_count`
    /// proves the registry holds the only reference (no thread is parked in
    /// `block_enter`/`wait` on a clone). All three together are required.
    #[inline]
    fn is_idle(&self) -> bool {
        let state = self.state.lock();
        state.owner.is_none() && state.entry_count == 0
    }

    /// Acquire this monitor for the given thread.
    ///
    /// If the monitor is unowned, the thread becomes the owner.
    /// If the monitor is already owned by this thread, the entry count is
    /// incremented (reentrant lock).
    /// If the monitor is owned by another thread, this blocks until the
    /// monitor is released.
    fn enter(&self, thread_id: ThreadId) {
        let mut state = self.state.lock();
        // Wait until the monitor is either unowned or owned by us.
        if mon_enter_dump_enabled() {
            // Gated diagnostic only (CRATONVM_DBG_MONENTER): poll on a 5ms
            // cadence so a watchdog stack-dump request can surface a thread
            // deadlocked here. Emits the blocked thread's frames once (the
            // snapshot was deposited by `monitor_enter` before this call).
            let mut dumped = false;
            while state.owner.is_some() && state.owner != Some(thread_id) {
                self.entry_condvar
                    .wait_for(&mut state, std::time::Duration::from_millis(5));
                if !dumped && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire) {
                    emit_wait_site_frames(thread_id);
                    dumped = true;
                }
            }
        } else {
            // Normal path — unchanged: block on the condvar until released.
            while state.owner.is_some() && state.owner != Some(thread_id) {
                self.entry_condvar.wait(&mut state);
            }
        }
        match state.owner {
            None => {
                // Unowned — acquire
                state.owner = Some(thread_id);
                state.entry_count = 1;
            }
            Some(_) => {
                // Reentrant — already owned by this thread
                state.entry_count += 1;
            }
        }
    }

    /// Non-blocking acquire: `true` ⇒ acquired (fresh or re-entrant),
    /// `false` ⇒ owned by another thread (the caller must take the
    /// GC-blocked contended path — see `MonitorTable::enter_or_contend`).
    fn try_enter(&self, thread_id: ThreadId) -> bool {
        let mut state = self.state.lock();
        match state.owner {
            None => {
                state.owner = Some(thread_id);
                state.entry_count = 1;
                true
            }
            Some(owner) if owner == thread_id => {
                state.entry_count += 1;
                true
            }
            Some(_) => false,
        }
    }

    /// Blocking acquire of a CONTENDED monitor handed out by
    /// `MonitorTable::enter_or_contend`. The caller MUST have marked itself
    /// GC-blocked first (deposit roots + `GcBarrier::enter_blocked`): the
    /// current owner may be parked at a GC safepoint waiting for
    /// `gc_complete`, so a contender that still counts in the barrier's
    /// `expected` wedges the whole VM (the H2 TestScript three-way STW
    /// deadlock: owner waits GC, contender waits owner, GC waits contender).
    pub(crate) fn block_enter(&self, thread_id: ThreadId) {
        self.enter(thread_id);
    }

    /// Release this monitor for the given thread.
    ///
    /// Decrements the entry count. When it reaches 0, the monitor is released
    /// and becomes unowned.
    ///
    /// Returns `Err` if the calling thread does not own the monitor
    /// (`IllegalMonitorStateException`).
    pub(crate) fn exit(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let mut state = self.state.lock();
        match state.owner {
            Some(owner) if owner == thread_id => {
                // B6: JVMS §6.5 monitorexit contract — if the entry count is
                // already zero at the moment of the exit attempt, the calling
                // thread does NOT logically own the monitor and must observe
                // `IllegalMonitorStateException`. Check BEFORE the decrement
                // so the failure mode is the spec-defined IMSE rather than a
                // silent saturating wrap. The saturating-sub below becomes
                // defense-in-depth against the same race (frame-unwind release
                // racing a manual `monitorexit`) but no longer hides the bug.
                if state.entry_count == 0 {
                    return Err(MonitorError::NotOwner);
                }
                state.entry_count = state.entry_count.saturating_sub(1);
                if state.entry_count == 0 {
                    state.owner = None;
                    // Round-7 HIGH (vm #5): clear the JFR enter-event flag at
                    // the same instant the monitor becomes unowned. The next
                    // acquirer (potentially a different thread) starts with
                    // a fresh `jfr_enter_recorded = false` and the
                    // interpreter's enter-time snapshot governs whether
                    // the next enter/exit pair is JFR-tracked.
                    state.jfr_enter_recorded = false;
                    // Wake one thread waiting to enter this monitor
                    self.entry_condvar.notify_one();
                }
                Ok(())
            }
            _ => Err(MonitorError::NotOwner),
        }
    }

    /// Forcibly release this monitor if it is (still) owned by `thread_id`,
    /// regardless of entry count. Used only when `thread_id` has already
    /// terminated (`ThreadRegistry::mark_dead`) — a dead thread can never
    /// call `monitorexit` for a monitor it happened to be holding when it
    /// exited (e.g. interrupted out of a blocking native call while inside
    /// a `synchronized` block), so without this every future `monitorenter`
    /// on that object blocks forever on `entry_condvar`. Wakes ALL entry
    /// waiters (not just one, unlike a normal `exit`) since we don't know
    /// how many threads are parked and each must re-check for itself.
    /// Returns `true` if a release actually happened (diagnostic only).
    pub(crate) fn force_release_if_owned_by(&self, thread_id: ThreadId) -> bool {
        let mut state = self.state.lock();
        if state.owner == Some(thread_id) {
            state.owner = None;
            state.entry_count = 0;
            state.jfr_enter_recorded = false;
            self.entry_condvar.notify_all();
            true
        } else {
            false
        }
    }

    /// Round-7 HIGH (vm #5): record that the interpreter emitted a JFR
    /// `monitor_enter` event for the current outermost acquisition of this
    /// monitor. Called from the `Monitorenter` opcode handler right after the
    /// event is pushed to the flight recorder, so the matching `Monitorexit`
    /// can decide whether to emit an exit event without re-checking the
    /// (potentially-flipped-since) global JFR enable flag.
    ///
    /// No-op unless the caller currently owns the monitor — defensive against
    /// a stale snapshot in a racing interpreter thread.
    pub(crate) fn set_jfr_enter_recorded(&self, thread_id: ThreadId) {
        let mut state = self.state.lock();
        if state.owner == Some(thread_id) {
            state.jfr_enter_recorded = true;
        }
    }

    /// Round-7 HIGH (vm #5): peek the JFR enter-recorded flag for the current
    /// owner. Returns `true` only if the interpreter actually emitted a
    /// matching `monitor_enter` event for the live acquisition — the natural
    /// gate for emitting a paired `monitor_exit` event without producing an
    /// orphan one when JFR turned on between the two opcodes.
    pub(crate) fn jfr_enter_recorded(&self) -> bool {
        let state = self.state.lock();
        state.jfr_enter_recorded
    }

    /// Object.wait() — release the monitor and block until notified.
    ///
    /// The calling thread must own this monitor. The entry count is saved,
    /// ownership is released, and the thread blocks on `wait_condvar`.
    /// When woken (by `notify`/`notifyAll`), it re-acquires the monitor
    /// with the original entry count restored.
    ///
    /// If `timeout_ms` is Some(millis) with millis > 0, the wait is bounded.
    ///
    /// If `interrupted` is provided, the wait periodically checks the flag
    /// and returns early if the thread has been interrupted (needed because
    /// Thread.interrupt() sets a flag but cannot directly wake a condvar).
    fn wait(
        &self,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<bool, MonitorError> {
        let mut state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }

        // Save and release
        let saved_count = state.entry_count;
        state.owner = None;
        state.entry_count = 0;
        self.entry_condvar.notify_one();

        // Block on wait_condvar with periodic interrupt checks.
        // We use short timed waits so that Thread.interrupt() (which only sets
        // a flag) can wake us within a bounded interval.
        //
        // KC16-watchdog: also poll the global stack-dump flag so a thread
        // parked in Object.wait() (e.g. AsyncFutureTask.await ->
        // EnhancedQueueExecutor handoff) observes the watchdog's request
        // and dumps its frame chain from the wait site. Without this
        // poll, a thread that entered wait() before the watchdog fired
        // sits forever in `wait_condvar.wait()` and ack_count stays at
        // 0, leaving the watchdog's only signal as the misleading
        // "main thread is in native (Rust) code" banner.
        //
        // Important: observing the dump flag does NOT consume it and
        // does NOT cause `wait()` to return spuriously. After emitting
        // one frame snapshot per wait call we set a local "already
        // dumped" guard and re-park; this preserves Java semantics
        // (a notify is still required to return) while letting the
        // watchdog see at least one snapshot before it aborts.
        let poll_interval = std::time::Duration::from_millis(5);
        let mut was_interrupted = false;
        let mut frames_dumped = false;
        match timeout_ms {
            Some(ms) if ms > 0 => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let wait_time = remaining.min(poll_interval);
                    let result = self.wait_condvar.wait_for(&mut state, wait_time);
                    if let Some(flag) = interrupted {
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
                            // LOST-WAKEUP FIX: a notify_one() wakes exactly ONE
                            // waiter. If this thread was both notified and
                            // interrupted, breaking out here to throw
                            // InterruptedException would CONSUME that single
                            // notification — another waiter the notify was meant
                            // for would never wake (lost wakeup). `wait_for`
                            // reports `!timed_out()` when a signal (notify /
                            // notifyAll / spurious) woke us within the slice; in
                            // that case forward the notification to one other
                            // waiter so the JLS/HotSpot guarantee that a notify
                            // wakes *some* waiter still holds. A spurious wakeup
                            // can trigger an extra notify_one() with no waiter to
                            // receive it, which is harmless (condvars do not
                            // accumulate permits). Done while still holding the
                            // monitor state lock, exactly like notify().
                            if !result.timed_out() {
                                self.wait_condvar.notify_one();
                            }
                            break;
                        }
                    }
                    // Woken by notify()/notifyAll() (or a spurious wakeup) before
                    // the poll slice elapsed — return so the caller can re-check
                    // its condition, exactly like the untimed branch below.
                    // Without this, timed Object.wait(timeout) ignored notify and
                    // always slept the FULL timeout: Thread.join(millis) waited
                    // the entire timeout after the joined thread already died, and
                    // ExecutorService.awaitTermination / timed Condition.await
                    // slept the whole duration after being signalled.
                    if !result.timed_out() {
                        break;
                    }
                    if !frames_dumped
                        && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire)
                    {
                        emit_wait_site_frames(thread_id);
                        frames_dumped = true;
                    }
                }
            }
            _ => {
                // Untimed wait. Loop with periodic interrupt checks until
                // notify() wakes us or the interrupt flag is set.
                if let Some(flag) = interrupted {
                    loop {
                        let result = self.wait_condvar.wait_for(&mut state, poll_interval);
                        if flag.load(std::sync::atomic::Ordering::Acquire) {
                            was_interrupted = true;
                            // LOST-WAKEUP FIX (see the timed branch above): if a
                            // notify woke us in the same slice we observed the
                            // interrupt, forward the single notification to one
                            // other waiter so it is not swallowed by the
                            // InterruptedException throw. `!timed_out()` means a
                            // signal (notify/notifyAll/spurious) arrived within
                            // the poll slice. Re-notify under the held state lock.
                            if !result.timed_out() {
                                self.wait_condvar.notify_one();
                            }
                            break;
                        }
                        if !frames_dumped
                            && stack_dump_wait_flag().load(std::sync::atomic::Ordering::Acquire)
                        {
                            emit_wait_site_frames(thread_id);
                            frames_dumped = true;
                        }
                        // If the condvar was signalled (not timed out), break
                        // to allow the caller to re-check its condition.
                        if !result.timed_out() {
                            break;
                        }
                    }
                } else {
                    // No interrupt flag — use a real untimed wait (for unit tests etc.)
                    self.wait_condvar.wait(&mut state);
                }
            }
        }

        // Re-acquire: wait until monitor is unowned or owned by us
        while state.owner.is_some() && state.owner != Some(thread_id) {
            self.entry_condvar.wait(&mut state);
        }
        state.owner = Some(thread_id);
        state.entry_count = saved_count;

        Ok(was_interrupted)
    }

    /// Object.notify() — wake one thread waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    fn notify(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }
        self.wait_condvar.notify_one();
        Ok(())
    }

    /// Object.notifyAll() — wake all threads waiting on this monitor.
    ///
    /// The calling thread must own this monitor.
    pub(crate) fn notify_all(&self, thread_id: ThreadId) -> Result<(), MonitorError> {
        let state = self.state.lock();
        if state.owner != Some(thread_id) {
            return Err(MonitorError::NotOwner);
        }
        self.wait_condvar.notify_all();
        Ok(())
    }
}

/// Monitor operation error.
#[derive(Debug)]
pub(crate) enum MonitorError {
    /// The current thread does not own the monitor.
    NotOwner,
}

// ---------------------------------------------------------------------------
// MonitorTable — global table of monitors keyed by object identity
// ---------------------------------------------------------------------------

/// Global table of JVM monitors, keyed by object pointer address.
///
/// Each Java object can be used as a monitor. With the thin-lock fast path
/// the *uncontended* and *re-entrant single-thread* cases NEVER allocate a
/// `Monitor` — the lock state lives entirely in the object's `mark_word`.
/// A heavyweight `Monitor` is only created when contention forces inflation
/// (or when `Object.wait`/`notify` is used, which thin locks do not support).
///
/// The `monitors` map is therefore a *fallback registry* for inflated
/// monitors:
///   * it keeps each `Arc<Monitor>` alive after inflation (the mark word
///     only stores a raw pointer);
///   * it provides the lookup needed by `remap_after_gc` so monitors follow
///     their object across compaction.
///
/// The fast path (`try_thin_lock`, `try_thin_recursive_lock`,
/// `try_thin_unlock`) NEVER touches this map.
pub struct MonitorTable {
    /// Inflated-monitor registry — keys are object pointer addresses.
    /// Populated lazily on inflation; consulted by GC remap only.
    /// T10.9.B: FxHashMap — keys are object pointer addresses (internal).
    ///
    // SECURITY FIX (V11): this registry lock is the `monitors` lock at
    // hierarchy level L6 (`docs/lock-order.md`). It is wrapped in
    // `OrderedMutex` at `LockLevel::Monitors` so that, in debug builds, any
    // attempt to acquire a *lower*-or-equal-level lock first and then this
    // registry trips the descending-order debug assertion. All accesses are
    // confined to this file (the field is private), so the wrapper change has
    // no blast radius outside `monitor.rs`.
    monitors: OrderedMutex<FxHashMap<usize, Arc<Monitor>>>,
    /// Per-object CAS locks for compareAndSwap operations.
    /// Provides mutual exclusion for non-atomic CAS emulation on Value slots.
    /// T10.9.B: FxHashMap — object pointer addresses (internal).
    ///
    // SECURITY FIX (V11): the CAS-lock *registry* (the outer map) is also part
    // of the L6 `monitors` subsystem; wrap it at `LockLevel::Monitors` too. The
    // inner per-object `Arc<Mutex<()>>` stays a plain `parking_lot::Mutex`: it
    // is an L6-internal sub-lock with no global ordering constraints and is
    // never held while acquiring another tracked lock.
    cas_locks: OrderedMutex<FxHashMap<usize, Arc<Mutex<()>>>>,
}

impl MonitorTable {
    /// Create an empty monitor table.
    pub fn new() -> Self {
        Self {
            // SECURITY FIX (V11): both registries live at L6 (`monitors`).
            monitors: OrderedMutex::new(FxHashMap::default(), LockLevel::Monitors),
            cas_locks: OrderedMutex::new(FxHashMap::default(), LockLevel::Monitors),
        }
    }

    /// Look up the inflated `Monitor` for `obj_ref`, returning `None` if the
    /// object has never been inflated.
    fn lookup_inflated(&self, obj_ref: ObjectRef) -> Option<Arc<Monitor>> {
        let key = obj_ref.as_ptr() as usize;
        // SECURITY FIX (V11): OrderedMutex::lock() returns a LockResult; the
        // registry is never poisoned (no panic is held across it), so unwrap.
        let monitors = self.monitors.lock().expect("monitors registry poisoned");
        monitors.get(&key).cloned()
    }

    /// Force inflation of the lock for `obj_ref`. If the object already has
    /// an inflated monitor (either via a prior CAS-published mark word or via
    /// our fallback registry), that one is returned. Otherwise a new
    /// `Monitor` is allocated, pre-acquired with the *currently observed*
    /// thin-lock owner (if any), registered, and published into the mark
    /// word.
    ///
    /// Re-snapshots the mark word under the registry mutex so that the
    /// pre-acquire reflects reality at publish time (avoiding stale
    /// `current_owner` data from a CAS-loser caller).
    ///
    /// Returns `Err(IllegalStateException)` only on the pathological case
    /// where the mark word reads `INFLATED` but the registry has no entry
    /// for the object. Under correct concurrent operation this is
    /// impossible — every INFLATED publish in this module CAS-flips the
    /// mark word **and** inserts into `self.monitors` under the same
    /// registry mutex (see C8 / C9). The Err variant therefore signals
    /// memory corruption or a non-conforming inflation path; previously
    /// this case silently leaked the original `Arc<Monitor>` and broke
    /// per-object identity (a thread that already held the old monitor
    /// would re-enter a fresh unowned monitor → eventual IMSE on exit).
    fn inflate_locked(
        &self,
        obj_ref: ObjectRef,
        header: &ObjectHeader,
    ) -> Result<Arc<Monitor>, MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        // SECURITY FIX (V11): OrderedMutex::lock() returns a LockResult.
        let mut monitors = self.monitors.lock().expect("monitors registry poisoned");
        loop {
            let cur = header.mark_word.load(Ordering::Acquire);
            match ObjectHeader::mark_state(cur) {
                s if s == types::MARK_INFLATED => {
                    if let Some(m) = monitors.get(&key).cloned() {
                        return Ok(m);
                    }
                    // Inflated mark but no registry entry — pathological.
                    // We hold `self.monitors.lock()` and every legitimate
                    // INFLATED publish in this module inserts under that
                    // same mutex, so this branch is unreachable under
                    // correct concurrent operation. Refuse to recover by
                    // synthesising a fresh monitor: doing so would leak
                    // the prior `Arc<Monitor>` *and* break per-object
                    // identity (any thread that already held the old
                    // monitor would re-enter a fresh unowned monitor and
                    // later raise IMSE on exit). Surface the invariant
                    // violation as a runtime error so the caller decides
                    // whether to abort or unwind.
                    return Err(MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::IllegalStateException {
                            message: format!(
                                "monitor mark inflated but registry entry missing \
                                 (data race or memory corruption) at obj={key:#x}"
                            ),
                        },
                    )));
                }
                s if s == types::MARK_THIN_LOCKED => {
                    let owner = ObjectHeader::thin_lock_owner(cur);
                    let recursion = ObjectHeader::thin_lock_recursion(cur);
                    let monitor = Arc::new(Monitor::new());
                    monitor.enter_with_recursion(ThreadId(owner as u64), (recursion as u32) + 1);
                    let new_mark = ObjectHeader::make_inflated(Arc::as_ptr(&monitor) as usize);
                    // Publish atomically — if the CAS loses, the original
                    // owner mutated the word (either recursive bump or
                    // release). Drop the local monitor and retry.
                    if header
                        .mark_word
                        .compare_exchange(cur, new_mark, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        monitors.insert(key, monitor.clone());
                        return Ok(monitor);
                    }
                    // CAS lost; loop to re-snapshot. The pre-acquired Monitor
                    // is dropped (no other reference exists yet).
                }
                _ => {
                    // NEUTRAL (or reserved). Create an unowned monitor and
                    // publish it; the caller will `enter` it normally.
                    let monitor = Arc::new(Monitor::new());
                    let new_mark = ObjectHeader::make_inflated(Arc::as_ptr(&monitor) as usize);
                    if header
                        .mark_word
                        .compare_exchange(cur, new_mark, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        monitors.insert(key, monitor.clone());
                        return Ok(monitor);
                    }
                    // CAS lost — retry.
                }
            }
        }
    }

    /// Acquire the monitor for the given object on behalf of the given thread.
    ///
    /// Blocking variant — prefer `enter_or_contend` from interpreter/native
    /// call sites so the contended wait can be wrapped in the GC-blocked
    /// protocol (an unmarked contended wait is counted in the STW barrier's
    /// `expected` and deadlocks the collector against a safepoint-parked
    /// owner — the H2 TestScript three-way wedge).
    pub fn enter(&self, obj_ref: ObjectRef, thread_id: ThreadId) {
        if let Some(m) = self.enter_or_contend(obj_ref, thread_id) {
            m.block_enter(thread_id);
        }
    }

    /// Acquire the monitor if possible WITHOUT blocking; on contention,
    /// return the inflated `Monitor` for the caller to block on (after
    /// marking itself GC-blocked — see `Monitor::block_enter`).
    ///
    /// Fast paths (no allocation):
    /// * NEUTRAL          → CAS to THIN_LOCKED (uncontended uncrossed lock).
    /// * THIN_LOCKED(self) → bump recursion (re-entrant single-thread lock).
    ///
    /// Slow paths (inflate to a real `Monitor`):
    /// * THIN_LOCKED(other) → inflate transferring ownership, then try-enter.
    /// * THIN_LOCKED(self) at recursion = 255 → inflate, then try-enter.
    /// * INFLATED         → dispatch to `Monitor::try_enter`.
    ///
    /// `None` ⇒ acquired. `Some(m)` ⇒ contended; caller must
    /// `m.block_enter(thread_id)` (the monitor may have been released in
    /// the interim — `block_enter` then acquires immediately).
    pub(crate) fn enter_or_contend(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Option<Arc<Monitor>> {
        // Fall back to the legacy heavyweight path if the ThreadId doesn't fit
        // in the 32-bit thin-lock owner field.
        let tid32 = match tid_to_u32(thread_id) {
            Some(t) => t,
            None => {
                let m = self.inflate_for_legacy(obj_ref);
                if m.try_enter(thread_id) {
                    return None;
                }
                return Some(m);
            }
        };

        let header = header_of(obj_ref);

        // ── Fast path 1: NEUTRAL → THIN_LOCKED via single CAS. ─────────────
        match try_thin_lock(header, tid32) {
            Ok(()) => return None,
            Err(_) => { /* fall through with up-to-date classification below */ }
        }

        loop {
            let cur = header.mark_word.load(Ordering::Acquire);
            match ObjectHeader::mark_state(cur) {
                s if s == types::MARK_NEUTRAL => {
                    // Raced with another exit() — retry the fast path once.
                    if try_thin_lock(header, tid32).is_ok() {
                        return None;
                    }
                    // Lost the race again → inflate to avoid livelock.
                    // `inflate_locked` only Errs on the impossible "mark
                    // INFLATED but registry empty" invariant violation; panic
                    // here surfaces the corruption rather than the previous
                    // silent-leak recovery (see C8).
                    let m = self
                        .inflate_locked(obj_ref, header)
                        .expect("monitor inflation invariant: registry/mark-word desync");
                    if m.try_enter(thread_id) {
                        return None;
                    }
                    return Some(m);
                }
                s if s == types::MARK_THIN_LOCKED => {
                    let owner = ObjectHeader::thin_lock_owner(cur);
                    if owner == tid32 {
                        // ── Fast path 2: re-entrant thin lock. ──────────
                        match try_thin_recursive_lock(header, tid32) {
                            Ok(_) => return None,
                            Err(err_mark) => {
                                if ObjectHeader::mark_state(err_mark) == types::MARK_THIN_LOCKED
                                    && ObjectHeader::thin_lock_owner(err_mark) == tid32
                                    && ObjectHeader::thin_lock_recursion(err_mark) == u8::MAX
                                {
                                    // Recursion overflow — inflate. Inflation
                                    // pre-acquires with entry_count =
                                    // recursion+1 = 256, capturing our prior
                                    // re-entrant acquisitions. Now bump once
                                    // more (re-entrant try_enter always
                                    // succeeds) to record the current
                                    // attempted acquisition.
                                    let m = self.inflate_locked(obj_ref, header).expect(
                                        "monitor inflation invariant: registry/mark-word desync",
                                    );
                                    if m.try_enter(thread_id) {
                                        return None;
                                    }
                                    return Some(m);
                                }
                                // Otherwise the state changed under us; reclassify.
                                continue;
                            }
                        }
                    } else {
                        // ── Slow path: contended thin lock → inflate. ───
                        // `inflate_locked` re-snapshots under its mutex and
                        // pre-acquires for whatever owner the mark word
                        // currently shows (or none if it has since gone
                        // NEUTRAL). try_enter succeeds if it has since been
                        // released; otherwise the caller blocks GC-marked.
                        let m = self
                            .inflate_locked(obj_ref, header)
                            .expect("monitor inflation invariant: registry/mark-word desync");
                        if m.try_enter(thread_id) {
                            return None;
                        }
                        return Some(m);
                    }
                }
                s if s == types::MARK_INFLATED => {
                    // ── Slow path: already inflated → dispatch directly. ─
                    if let Some(m) = self.lookup_inflated(obj_ref) {
                        if m.try_enter(thread_id) {
                            return None;
                        }
                        return Some(m);
                    }
                    // Registry miss (should not happen): re-inflate. The
                    // window between the inflating thread's mark-word CAS
                    // and its registry insert is closed by the registry
                    // mutex inside `inflate_locked`, so this branch only
                    // fires on a true invariant violation — see C8.
                    //
                    // GC-audit finding 1(b) tripwire (2026-07-10): this
                    // branch is the prime suspect for the MTChurn
                    // lost-wakeup pile-up. If a pause's monitor-registry
                    // remap races a thread the STW quota hole let run
                    // mid-collection, that thread can miss here and
                    // RE-INFLATE a SECOND Monitor for the same Java object
                    // — every waiter parked on the first is orphaned (the
                    // gdb-captured 5-waiters-none-woken picture). Loud,
                    // rate-limited, and counted so an MTChurn round can
                    // confirm or kill the hypothesis cheaply.
                    {
                        static REINFLATE_MISSES: std::sync::atomic::AtomicUsize =
                            std::sync::atomic::AtomicUsize::new(0);
                        let n = REINFLATE_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if n < 8 {
                            tracing::warn!(
                                obj = obj_ref.as_ptr() as usize,
                                occurrence = n + 1,
                                "monitor registry miss with INFLATED mark word — \
                                 re-inflating a second Monitor for this object \
                                 (audit finding 1(b) tripwire: waiters on the \
                                 original monitor are now orphaned)"
                            );
                        }
                    }
                    let m = self
                        .inflate_locked(obj_ref, header)
                        .expect("monitor inflation invariant: registry/mark-word desync");
                    if m.try_enter(thread_id) {
                        return None;
                    }
                    return Some(m);
                }
                _ => {
                    // Reserved state 0b11 — should never occur. Fall back to
                    // inflation as the safest recovery.
                    let m = self
                        .inflate_locked(obj_ref, header)
                        .expect("monitor inflation invariant: registry/mark-word desync");
                    if m.try_enter(thread_id) {
                        return None;
                    }
                    return Some(m);
                }
            }
        }
    }

    /// Force the object's monitor into inflated form and acquire it if
    /// possible without blocking. Returns the stable monitor handle and whether
    /// the caller must block on it.
    ///
    /// This is used by thread termination: once it has the `Arc<Monitor>`, the
    /// final `mark_dead`/`notifyAll`/`exit` sequence no longer needs to
    /// re-lookup the monitor through the Java `Thread` object's raw address,
    /// which may be remapped by a concurrent moving GC.
    pub(crate) fn enter_inflated_or_contend(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Result<(Arc<Monitor>, bool), MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        let contended = !monitor.try_enter(thread_id);
        Ok((monitor, contended))
    }

    /// Get or create a monitor without touching the mark word — used only
    /// for the legacy fallback when a `ThreadId` exceeds `u32::MAX`.
    fn inflate_for_legacy(&self, obj_ref: ObjectRef) -> Arc<Monitor> {
        let key = obj_ref.as_ptr() as usize;
        // SECURITY FIX (V11): OrderedMutex::lock() returns a LockResult.
        let mut monitors = self.monitors.lock().expect("monitors registry poisoned");
        monitors
            .entry(key)
            .or_insert_with(|| Arc::new(Monitor::new()))
            .clone()
    }

    /// Release the monitor for the given object on behalf of the given thread.
    ///
    /// Fast paths (no allocation):
    /// * THIN_LOCKED(self) at recursion>0 → CAS recursion-1.
    /// * THIN_LOCKED(self) at recursion=0 → CAS back to NEUTRAL.
    ///
    /// Slow path:
    /// * INFLATED → dispatch to `Monitor::exit`.
    ///
    /// Returns `Err(MethodCallFailed)` with `IllegalMonitorStateException` if
    /// the calling thread does not own the monitor.
    pub fn exit(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        let key = obj_ref.as_ptr() as usize;
        let header = header_of(obj_ref);

        if let Some(tid32) = tid_to_u32(thread_id) {
            // ── Fast path: thin-lock release. ──────────────────────────────
            match try_thin_unlock(header, tid32) {
                Ok(_) => return Ok(()),
                Err(cur) => {
                    if ObjectHeader::mark_state(cur) == types::MARK_INFLATED {
                        // Fall through to inflated dispatch below.
                    } else {
                        // Not held by us (thin, but different owner; or
                        // neutral) — raise IMSE.
                        self.dbg_monexit_forensics(obj_ref, thread_id, cur, "thin-arm");
                        return Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::IllegalMonitorStateException {
                                message: format!(
                                    "thread {thread_id} does not own the monitor for object at {key:#x}"
                                ),
                            },
                        )));
                    }
                }
            }
        }

        // ── Slow path: inflated monitor dispatch. ──────────────────────────
        let monitor = self.lookup_inflated(obj_ref);
        match monitor {
            Some(m) => m.exit(thread_id).map_err(|MonitorError::NotOwner| {
                self.dbg_monexit_forensics(
                    obj_ref,
                    thread_id,
                    header.mark_word.load(Ordering::Acquire),
                    "inflated-notowner",
                );
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} does not own the monitor for object at {key:#x}"
                        ),
                    },
                ))
            }),
            None => {
                // No monitor exists for this object — thread never entered it
                self.dbg_monexit_forensics(
                    obj_ref,
                    thread_id,
                    header.mark_word.load(Ordering::Acquire),
                    "registry-miss",
                );
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "monitorexit on object at {key:#x} that was never entered"
                        ),
                    },
                )))
            }
        }
    }

    /// GC-audit finding 1(b) forensics (gated: `CRATONVM_DBG_MONEXIT`, default
    /// OFF, zero cost when unset). On a monitorexit IMSE, dump everything
    /// needed to classify the failure shape post-hoc: the raw mark word and
    /// its decode (NEUTRAL = zeroed/stale header, THIN by another tid =
    /// identity confusion, INFLATED = owner mismatch), the object header's
    /// class_id/num_slots words (a zeroed pair = the "zeroed live object"
    /// recycled-slot shape), and the registry's view of the inflated monitor.
    #[cold]
    fn dbg_monexit_forensics(&self, obj_ref: ObjectRef, tid: ThreadId, mark: u64, arm: &str) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("CRATONVM_DBG_MONEXIT").is_some()) {
            return;
        }
        let key = obj_ref.as_ptr() as usize;
        let state = ObjectHeader::mark_state(mark);
        let (class_id, num_slots) = unsafe {
            let p = obj_ref.as_ptr() as *const u8;
            (
                std::ptr::read(p as *const u32),
                std::ptr::read(p.add(16) as *const u32),
            )
        };
        let (reg_hit, reg_owner, reg_count) = {
            let monitors = self.monitors.lock().expect("monitors registry poisoned");
            match monitors.get(&key) {
                Some(m) => {
                    let st = m.state.lock();
                    (true, st.owner, st.entry_count)
                }
                None => (false, None, 0),
            }
        };
        eprintln!(
            "[MONEXIT-IMSE] arm={arm} tid={} obj={key:#x} mark={mark:#x} state={} thin_owner={} thin_rec={} class_id={class_id} num_slots={num_slots} registry_hit={reg_hit} reg_owner={reg_owner:?} reg_entry_count={reg_count}",
            tid.0,
            match state {
                s if s == types::MARK_NEUTRAL => "NEUTRAL",
                s if s == types::MARK_THIN_LOCKED => "THIN",
                s if s == types::MARK_INFLATED => "INFLATED",
                _ => "RESERVED",
            },
            ObjectHeader::thin_lock_owner(mark),
            ObjectHeader::thin_lock_recursion(mark),
        );
    }

    /// Round-7 HIGH (vm #5): record on the monitor for `obj_ref` that the
    /// interpreter has emitted a JFR `monitor_enter` event for the current
    /// outermost acquisition by `thread_id`. The exit-side companion is
    /// [`jfr_enter_recorded`].
    ///
    /// If the monitor has not been inflated yet (the uncontended fast path
    /// served the enter via the mark-word thin lock), this forces inflation
    /// so the flag has a stable home. Inflation is the expected price for a
    /// monitor that the JFR consumer is interested in — the JFR-enabled gate
    /// means we're already on the cold path.
    pub fn set_jfr_enter_recorded(&self, obj_ref: ObjectRef, thread_id: ThreadId) {
        // The only failure mode `ensure_inflated` can surface is the
        // pathological "mark INFLATED but registry empty" invariant
        // violation from `inflate_locked` (C8). This API is `()`-returning
        // (the JFR consumer has no error channel here), so we panic to
        // surface the corruption rather than silently leak as before.
        let monitor = self
            .ensure_inflated(obj_ref, thread_id)
            .expect("monitor inflation invariant: registry/mark-word desync");
        monitor.set_jfr_enter_recorded(thread_id);
    }

    /// Round-7 HIGH (vm #5): query whether a JFR `monitor_enter` event was
    /// emitted for the current outermost acquisition of the monitor for
    /// `obj_ref`. Returns `false` if the monitor has not been inflated (in
    /// which case the enter event could not have been recorded — the flag
    /// lives on the heavyweight `Monitor`, never on the thin lock).
    pub fn jfr_enter_recorded(&self, obj_ref: ObjectRef) -> bool {
        match self.lookup_inflated(obj_ref) {
            Some(m) => m.jfr_enter_recorded(),
            None => false,
        }
    }

    /// Ensure the object's lock is inflated and return the heavyweight
    /// `Monitor`. If the calling thread holds the thin lock, ownership is
    /// transferred atomically.
    ///
    /// Used by `wait`/`notify`/`notifyAll`, which require a heavyweight
    /// monitor (thin locks have no condvars). Propagates the
    /// `Err(IllegalStateException)` that `inflate_locked` raises on the
    /// pathological "INFLATED mark but missing registry entry" case (see
    /// C8) so callers can surface it as a normal runtime error rather
    /// than a silent leak.
    fn ensure_inflated(
        &self,
        obj_ref: ObjectRef,
        _thread_id: ThreadId,
    ) -> Result<Arc<Monitor>, MethodCallFailed> {
        let header = header_of(obj_ref);
        let cur = header.mark_word.load(Ordering::Acquire);
        if ObjectHeader::mark_state(cur) == types::MARK_INFLATED {
            if let Some(m) = self.lookup_inflated(obj_ref) {
                return Ok(m);
            }
        }
        // `inflate_locked` re-snapshots the mark word under its registry
        // mutex so the pre-acquire (if any) reflects the current owner.
        self.inflate_locked(obj_ref, header)
    }

    /// Perform `Object.wait()` on the monitor for the given object.
    ///
    /// The calling thread must own the monitor. It releases ownership, blocks
    /// until notified (or timed out), then re-acquires the monitor.
    ///
    /// If `interrupted` is provided, the wait will periodically check the flag
    /// and return `Ok(true)` if the thread was interrupted during the wait.
    pub fn wait(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
        timeout_ms: Option<u64>,
        interrupted: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<bool, MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor
            .wait(thread_id, timeout_ms, interrupted)
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} called wait() without owning the monitor"
                        ),
                    },
                ))
            })
    }

    /// Perform `Object.notify()` on the monitor for the given object.
    ///
    /// Wakes one thread waiting on this monitor. The calling thread must own it.
    pub fn notify(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> Result<(), MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor.notify(thread_id).map_err(|MonitorError::NotOwner| {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalMonitorStateException {
                    message: format!(
                        "thread {thread_id} called notify() without owning the monitor"
                    ),
                },
            ))
        })
    }

    /// Perform `Object.notifyAll()` on the monitor for the given object.
    ///
    /// Wakes all threads waiting on this monitor. The calling thread must own it.
    pub fn notify_all(
        &self,
        obj_ref: ObjectRef,
        thread_id: ThreadId,
    ) -> Result<(), MethodCallFailed> {
        let monitor = self.ensure_inflated(obj_ref, thread_id)?;
        monitor
            .notify_all(thread_id)
            .map_err(|MonitorError::NotOwner| {
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::IllegalMonitorStateException {
                        message: format!(
                            "thread {thread_id} called notifyAll() without owning the monitor"
                        ),
                    },
                ))
            })
    }

    /// T1.6.7 — Implements `Thread.holdsLock(Object)`.
    ///
    /// Returns `true` if the given thread currently owns the monitor for
    /// `obj_ref`. Returns `false` if the object has never been entered or
    /// is currently owned by a different thread.
    ///
    /// This is a non-blocking inspection — it briefly takes the monitor
    /// state lock to read the owner field but never waits for entry.
    pub fn holds(&self, obj_ref: ObjectRef, thread_id: ThreadId) -> bool {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => {
                if let Some(tid32) = tid_to_u32(thread_id) {
                    ObjectHeader::thin_lock_owner(mark) == tid32
                } else {
                    false
                }
            }
            s if s == types::MARK_INFLATED => {
                let key = obj_ref.as_ptr() as usize;
                let monitor = {
                    // SECURITY FIX (V11): OrderedMutex::lock() -> LockResult.
                    let monitors = self.monitors.lock().expect("monitors registry poisoned");
                    monitors.get(&key).cloned()
                };
                match monitor {
                    Some(m) => m.is_held_by(thread_id),
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// Execute a closure while holding a per-object CAS lock.
    ///
    /// Provides mutual exclusion for non-atomic compare-and-swap emulation.
    /// Each object gets its own lock (lazily created), so CAS operations on
    /// different objects do not contend.
    pub fn with_cas_lock<F, R>(&self, obj_ref: ObjectRef, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let key = obj_ref.as_ptr() as usize;
        let lock = {
            // SECURITY FIX (V11): OrderedMutex::lock() -> LockResult.
            let mut cas = self.cas_locks.lock().expect("cas_locks registry poisoned");
            cas.entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock();
        f()
    }

    /// Test-only inspection helper: return the current owner `ThreadId` of
    /// the monitor for `obj_ref`, or `None` if the monitor is currently
    /// unowned.
    ///
    /// For the thin-lock fast path the answer is derived from the mark word
    /// directly (no allocation, no inflation). For the inflated path we look
    /// up the registered `Arc<Monitor>` and read its owner field under the
    /// monitor's state mutex. Returns `None` if the object has never been
    /// entered.
    ///
    /// Used by `vm/tests/monitor_stress.rs` to assert the final monitor
    /// state after a multi-threaded stress run completes. Not on the hot
    /// path of the interpreter / native dispatch.
    pub fn current_owner(&self, obj_ref: ObjectRef) -> Option<ThreadId> {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => {
                Some(ThreadId(u64::from(ObjectHeader::thin_lock_owner(mark))))
            }
            s if s == types::MARK_INFLATED => {
                let key = obj_ref.as_ptr() as usize;
                let monitor = {
                    // SECURITY FIX (V11): OrderedMutex::lock() -> LockResult.
                    let monitors = self.monitors.lock().expect("monitors registry poisoned");
                    monitors.get(&key).cloned()
                };
                monitor.and_then(|m| {
                    let st = m.state.lock();
                    st.owner
                })
            }
            _ => None,
        }
    }

    /// Test-only inspection helper: return the current re-entry count of
    /// the monitor for `obj_ref`. Returns `0` if the monitor is unowned,
    /// has never been entered, or is currently in the NEUTRAL mark state.
    ///
    /// Thin-locked monitors report `recursion + 1` (the mark word encodes
    /// the *additional* recursive acquires beyond the initial one, so a
    /// freshly thin-locked object has recursion=0 / entry_count=1).
    /// Inflated monitors report the raw `entry_count` field.
    ///
    /// Used by `vm/tests/monitor_stress.rs` to assert balanced
    /// enter/exit pairs after multi-threaded contention.
    pub fn entry_count(&self, obj_ref: ObjectRef) -> u32 {
        let header = header_of(obj_ref);
        let mark = header.mark_word.load(Ordering::Acquire);
        match ObjectHeader::mark_state(mark) {
            s if s == types::MARK_THIN_LOCKED => {
                u32::from(ObjectHeader::thin_lock_recursion(mark)) + 1
            }
            s if s == types::MARK_INFLATED => {
                let key = obj_ref.as_ptr() as usize;
                let monitor = {
                    // SECURITY FIX (V11): OrderedMutex::lock() -> LockResult.
                    let monitors = self.monitors.lock().expect("monitors registry poisoned");
                    monitors.get(&key).cloned()
                };
                monitor.map_or(0, |m| m.state.lock().entry_count)
            }
            _ => 0,
        }
    }

    /// Remap monitor keys after GC has moved objects.
    ///
    /// Takes a mapping from old pointer addresses to new pointer addresses.
    /// Re-keys the internal HashMap so monitors remain associated with the
    /// correct (now-relocated) objects.
    ///
    /// PERF (monitor-leak reclaim): historically every inflated `Monitor` and
    /// every per-object `cas_lock` was inserted on first use and **never
    /// removed**, so the registries grew monotonically for the VM lifetime —
    /// each moving GC's remap then walked an ever-larger table even though most
    /// keyed objects were long dead. This pass reclaims the entries that can be
    /// dropped *without ever losing live lock state*, keeping the table bounded
    /// and the remap cost proportional to the live (not historical) population.
    ///
    /// SAFETY of reclamation (why this never drops a live monitor):
    ///
    /// * `cas_locks` are **always** safe to prune when idle. A CAS lock has no
    ///   back-reference from the object header (unlike an inflated monitor,
    ///   whose address is published into the mark word), so removing it is
    ///   invisible to the rest of the VM — `with_cas_lock` simply re-creates an
    ///   equivalent fresh `Mutex` on the next access. We drop an entry only when
    ///   `Arc::strong_count == 1`, i.e. the registry holds the *sole* reference
    ///   and no thread is currently inside `with_cas_lock` holding a clone. An
    ///   entry that is in use (`strong_count > 1`) is always retained/re-keyed.
    ///
    /// * Inflated `monitors` are reclaimed only when **all** of the following
    ///   hold, which together prove the monitor is dead *and* unreferenced:
    ///     1. the object's old address is **absent** from `pointer_map`;
    ///     2. `Arc::strong_count == 1` — the registry holds the only reference,
    ///        so no thread is parked in `block_enter`/`wait` on a clone;
    ///     3. the monitor `is_idle()` — unowned with a zero entry count.
    ///   Condition (1) is a reliable *dead* signal **only for a whole-heap
    ///   collection** (the semi-space copying collector forwards *every* live
    ///   object into `pointer_map`, so absence ⇔ dead). For **partial**
    ///   collectors (G1 young/mixed, the generational minor GC) `pointer_map`
    ///   lists only *moved* objects; a live old-gen / non-CSet survivor stays in
    ///   place and is legitimately absent. Dropping such an entry would be a
    ///   correctness bug — the survivor's mark word still reads `INFLATED`, so
    ///   the next `enter`/`lookup_inflated` would miss the registry and trip the
    ///   hardened "INFLATED mark but registry entry missing" panic (the same
    ///   class of premature-reclaim bug documented for BUG-V and the WildFly
    ///   weak-`ClassLoader` referent). Because this method cannot tell which
    ///   collector invoked it, monitor reclamation is **gated behind the opt-in
    ///   `CRATONVM_RECLAIM_DEAD_MONITORS` flag** and the default build re-keys
    ///   monitors exactly as before (byte-identical behaviour). The conditions
    ///   (2)+(3) are belt-and-braces: even under the flag, a held/contended/
    ///   waiting monitor is *never* removed.
    ///
    ///   CROSS-FILE FOLLOW-UP (flagged, not done here — out of edit scope): to
    ///   reclaim dead monitors safely under *partial* collectors too, the GC
    ///   would need to pass a dead-address set (or a "whole-heap collection"
    ///   bool) through `cratonvm_gc::collector::MonitorCleanup::remap_after_gc`.
    ///   That requires editing `gc/src/collector.rs`, the four `remap_after_gc`
    ///   call sites (`gc/src/heap.rs`, `gc/src/g1.rs`, `gc/src/gen_heap.rs`),
    ///   and the impl in this file — left to the owner of those files.
    /// Release every inflated monitor still owned by `thread_id`. Called
    /// once from `ThreadRegistry::mark_dead` when a Java thread terminates.
    ///
    /// A thread normally releases every monitor it holds via ordinary
    /// `monitorexit` bytecode (including on the exceptional path, via the
    /// method's exception table) before it can ever finish running — but a
    /// thread that is interrupted or otherwise torn down while blocked
    /// inside a *native* call made from within a `synchronized` region never
    /// executes that bytecode. Without this sweep, such a monitor stays
    /// "held" by a thread ID that will never call `exit`/`notify` again,
    /// and every future `monitorenter` on that same object blocks forever.
    /// Only inflated monitors are covered (this is a registry walk, not a
    /// heap scan) — an uncontended thin lock still held by a dead thread is
    /// a separate, rarer gap (nothing else was contending it, so nothing
    /// else is blocked on it either).
    pub fn release_monitors_held_by(&self, thread_id: ThreadId) {
        self.release_monitors_held_by_except(thread_id, None);
    }

    /// Same as [`Self::release_monitors_held_by`], but leaves `except` alone
    /// even if `thread_id` currently owns it.
    ///
    /// Used by the `Thread.join()` termination-notify sequence (WP4.1,
    /// `vm_exec.rs`'s `thread_start` spawn closure): the terminating thread
    /// deliberately acquires and holds its own Java `Thread` mirror's monitor
    /// across `mark_dead`/this sweep so it can safely `notify_all()` any
    /// `Thread.join()` waiters afterward. Without this exclusion, the blanket
    /// sweep force-releases that monitor too (it IS owned by `thread_id` at
    /// this point) — `state.owner` goes back to `None` — so the subsequent
    /// `Monitor::notify_all`/`exit` calls both fail `NotOwner` (silently
    /// discarded by the `let _ =` caller) and `wait_condvar.notify_all()` is
    /// never invoked. Every joiner parked in `Object.wait()` then hangs
    /// forever even though the joined thread's `alive` flag is already false
    /// (the exact "worker alive=false, joiner stuck in `Thread.join()`"
    /// lost-wakeup signature).
    pub fn release_monitors_held_by_except(&self, thread_id: ThreadId, except: Option<&Arc<Monitor>>) {
        let monitors = self.monitors.lock().expect("monitors registry poisoned");
        for monitor in monitors.values() {
            if let Some(exc) = except {
                if Arc::ptr_eq(monitor, exc) {
                    continue;
                }
            }
            monitor.force_release_if_owned_by(thread_id);
        }
    }

    pub fn remap_after_gc(&self, pointer_map: &std::collections::HashMap<usize, usize>) {
        if pointer_map.is_empty() {
            return;
        }
        // PERF: read the monitor-reclaim opt-in once and cache it — the per-GC
        // check then costs a single relaxed load. Default OFF keeps the
        // monitor-registry behaviour byte-identical to before (see the safety
        // note above: absence-from-`pointer_map` is only a reliable dead signal
        // for a whole-heap collector).
        let reclaim_monitors = reclaim_dead_monitors_enabled();

        // SECURITY FIX (V11): both `monitors` and `cas_locks` are wrapped at
        // the SAME hierarchy level (L6). The lock-order checker forbids holding
        // one and then acquiring the other (equal level => not strictly
        // descending). Scope each guard so only one L6 registry lock is held at
        // a time; the two maps are independent so this is purely additive
        // safety with no behavioural change.
        {
            let mut monitors = self.monitors.lock().expect("monitors registry poisoned");
            // PERF: pre-size the rebuilt map to the live count so the
            // re-insertion loop never rehashes mid-walk. With reclamation off
            // the live count equals the drained count; with it on it is a tight
            // upper bound. `drain()` empties the map in place and lets us
            // `insert` back into the same (now-empty) allocation.
            let entries: Vec<(usize, Arc<Monitor>)> = monitors.drain().collect();
            for (old_key, mut monitor) in entries {
                match pointer_map.get(&old_key).copied() {
                    Some(new_key) => {
                        // Object survived AND moved — must re-key (a stale key
                        // would desync the registry from the copied mark word,
                        // see BUG-V). Always retained.
                        monitors.insert(new_key, monitor);
                    }
                    None => {
                        // Absent from the forwarding map. Under a whole-heap
                        // collection this means the object is dead; under a
                        // partial collection it may be a live in-place
                        // survivor. Reclaim ONLY when explicitly enabled AND
                        // the monitor is provably idle and uniquely referenced
                        // (no waiter/owner). Otherwise re-key in place (old_key
                        // unchanged) exactly as the original code did.
                        let reclaimable = reclaim_monitors
                            // `get_mut` succeeds iff this is the unique strong
                            // reference (no other `Arc<Monitor>` clone is held
                            // by a blocked/owning thread). Equivalent to
                            // `strong_count == 1 && weak_count == 0` but checked
                            // without a separate atomic load.
                            && Arc::get_mut(&mut monitor).is_some()
                            && monitor.is_idle();
                        if !reclaimable {
                            // Keep the entry under its (unchanged) address.
                            monitors.insert(old_key, monitor);
                        }
                        // else: drop `monitor` here — the registry's sole
                        // `Arc` is released, freeing the heavyweight Monitor.
                    }
                }
            }
        }

        // Also remap CAS locks (separate L6 critical section).
        //
        // PERF: CAS locks carry no object-header back-reference, so an idle one
        // is always safe to drop and is transparently re-created by the next
        // `with_cas_lock`. Prune every entry that survived-as-dead (absent from
        // `pointer_map`) AND is unreferenced (`strong_count == 1`), independent
        // of the monitor-reclaim flag — this is unconditionally correct for all
        // collectors. A surviving-and-moved lock is re-keyed; an in-use lock
        // (`strong_count > 1`, i.e. a thread is inside `with_cas_lock`) is kept.
        {
            let mut cas = self.cas_locks.lock().expect("cas_locks registry poisoned");
            let cas_entries: Vec<(usize, Arc<Mutex<()>>)> = cas.drain().collect();
            for (old_key, mut lock) in cas_entries {
                match pointer_map.get(&old_key).copied() {
                    Some(new_key) => {
                        // Object moved — re-key so a future CAS on the same
                        // (relocated) object reuses the same lock.
                        cas.insert(new_key, lock);
                    }
                    None => {
                        // Absent. For cas_locks this is *always* safe to treat
                        // as reclaimable when unreferenced, even under a partial
                        // collector: dropping a live-but-idle object's cas_lock
                        // only forces a cheap lazy re-create on the next CAS, it
                        // never desyncs any header state. Keep it only if a
                        // thread is currently using it.
                        if Arc::get_mut(&mut lock).is_none() {
                            cas.insert(old_key, lock);
                        }
                        // else: drop the sole `Arc` — reclaimed.
                    }
                }
            }
        }
    }
}

impl Default for MonitorTable {
    fn default() -> Self {
        Self::new()
    }
}

impl cratonvm_gc::MonitorCleanup for MonitorTable {
    fn remap_after_gc(&self, pointer_map: &std::collections::HashMap<usize, usize>) {
        self.remap_after_gc(pointer_map);
    }

    /// Whole-heap dead-address prune (ZGC backend — see the trait doc).
    /// `dead` is EXACT (every element's object was just swept), so removal
    /// is unconditional: a thread still blocked on a dead object's monitor
    /// holds its own `Arc<Monitor>` clone (dropping the registry entry
    /// cannot free it under that thread), and no future locker can exist
    /// for a dead object — while a NEW object reusing the address MUST get
    /// a fresh monitor, not the dead object's.
    fn prune_dead(&self, dead: &[usize]) {
        if dead.is_empty() {
            return;
        }
        {
            let mut monitors = self.monitors.lock().expect("monitors registry poisoned");
            for d in dead {
                monitors.remove(d);
            }
        }
        {
            let mut cas = self.cas_locks.lock().expect("cas_locks registry poisoned");
            for d in dead {
                cas.remove(d);
            }
        }
    }
}

impl std::fmt::Debug for MonitorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SECURITY FIX (V11): OrderedMutex::lock() -> LockResult.
        let count = self
            .monitors
            .lock()
            .expect("monitors registry poisoned")
            .len();
        f.debug_struct("MonitorTable")
            .field("active_monitors", &count)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::memory::heap::Heap;

    /// Helper to create a test object on the heap.
    ///
    /// Leaks the backing `Heap`: dropping it here would free its two
    /// (multi-MB) arenas out from under the returned `ObjectRef`, which
    /// every caller dereferences well after this function returns. That was
    /// a real, reproducible use-after-free — SIGSEGV inside
    /// `MonitorTable::enter_inflated_or_contend` -> `ensure_inflated` ->
    /// `header_of` reading the (freed) mark word — that crashed the test
    /// binary partway through this module's suite once the freed arena
    /// mapping got unmapped/reused. A tiny capacity keeps the per-call leak
    /// negligible (this helper is called from ~20 tests).
    fn test_object() -> ObjectRef {
        let heap: &'static Heap = Box::leak(Box::new(Heap::with_capacity(4096)));
        heap.alloc_object(ClassId::new(0), 0)
    }

    #[test]
    fn monitor_enter_exit_basic() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and exit should succeed
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    /// Regression guard for the Jetty `Deflater.end()`/`DeflaterPool.end()`
    /// hang: a thread that dies while holding an inflated monitor (e.g.
    /// interrupted/torn down while blocked in a native call made from
    /// inside a `synchronized` region) must not permanently starve every
    /// future `monitorenter` on that object. Without
    /// `release_monitors_held_by`, thread B here would block forever.
    #[test]
    fn dead_thread_owned_monitor_is_released_and_future_enters_succeed() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid_a = ThreadId(1);
        let tid_b = ThreadId(2);

        let (monitor, contended) = table.enter_inflated_or_contend(obj, tid_a).expect("inflate");
        assert!(!contended, "fresh monitor should be acquired immediately");
        assert!(monitor.is_held_by(tid_a));

        // Thread A "dies" without ever calling monitorexit.
        table.release_monitors_held_by(tid_a);
        assert!(!monitor.is_held_by(tid_a));

        // A different thread must now be able to acquire the same object's
        // monitor without blocking.
        table.enter(obj, tid_b);
        assert!(monitor.is_held_by(tid_b));
        assert!(table.exit(obj, tid_b).is_ok());
    }

    #[test]
    fn release_monitors_held_by_is_a_no_op_for_monitors_owned_by_other_threads() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid_a = ThreadId(1);
        let tid_b = ThreadId(2);

        let (monitor, _) = table.enter_inflated_or_contend(obj, tid_a).expect("inflate");
        // Releasing a thread that owns nothing here must not disturb A's hold.
        table.release_monitors_held_by(tid_b);
        assert!(monitor.is_held_by(tid_a));
        assert!(table.exit(obj, tid_a).is_ok());
    }

    #[test]
    fn monitor_reentrant() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter twice, exit twice
        table.enter(obj, tid);
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_reentrant_deep() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter 10 times, exit 10 times
        for _ in 0..10 {
            table.enter(obj, tid);
        }
        for _ in 0..10 {
            assert!(table.exit(obj, tid).is_ok());
        }
    }

    #[test]
    fn monitor_exit_without_enter_fails() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Exit without entering should fail
        let result = table.exit(obj, tid);
        assert!(result.is_err());
    }

    #[test]
    fn monitor_exit_wrong_thread_fails() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid1 = ThreadId(1);
        let tid2 = ThreadId(2);

        // Thread 1 enters
        table.enter(obj, tid1);

        // Thread 2 tries to exit — should fail
        let result = table.exit(obj, tid2);
        assert!(result.is_err());

        // Thread 1 can still exit
        assert!(table.exit(obj, tid1).is_ok());
    }

    #[test]
    fn monitor_different_objects_independent() {
        let heap = Heap::new();
        let obj1 = heap.alloc_object(ClassId::new(0), 0);
        let obj2 = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid = ThreadId(1);

        // Enter both objects
        table.enter(obj1, tid);
        table.enter(obj2, tid);

        // Exit them independently
        assert!(table.exit(obj2, tid).is_ok());
        assert!(table.exit(obj1, tid).is_ok());
    }

    #[test]
    fn monitor_reenter_after_release() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and exit
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());

        // Enter again — should work
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_remap_after_gc() {
        // With thin locks, the mark word travels with the object bytes during
        // a real GC compaction (the GC must copy the entire ObjectHeader). The
        // `remap_after_gc` path only matters for the INFLATED-monitor registry,
        // so we first force inflation via wait(), then verify the registry
        // entry follows the object to its new address.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter and force inflation by calling wait() (which requires a
        // heavyweight monitor with condvars). Use a tiny timeout so the test
        // is not slow.
        table.enter(obj, tid);
        table.wait(obj, tid, Some(1), None).unwrap();
        // Now the object's mark word is INFLATED and the registry holds the
        // Arc<Monitor>.

        // Simulate GC moving the object to a new address, AND copy the mark
        // word bytes (this is what a real semi-space copy does).
        let old_addr = obj.as_ptr() as usize;
        let heap2 = Heap::new();
        let new_obj = heap2.alloc_object(ClassId::new(0), 0);
        let new_addr = new_obj.as_ptr() as usize;

        // Copy mark word from old to new (mimicking GC byte copy of the header).
        let old_mark = header_of(obj).mark_word.load(Ordering::Acquire);
        header_of(new_obj)
            .mark_word
            .store(old_mark, Ordering::Release);

        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(old_addr, new_addr);
        table.remap_after_gc(&pointer_map);

        // Exit using the NEW address should succeed — the registry was
        // re-keyed so the inflated Monitor is found.
        assert!(table.exit(new_obj, tid).is_ok());
    }

    #[test]
    fn inflated_handle_survives_object_remap_for_thread_exit_notify() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        let (monitor, contended) = table.enter_inflated_or_contend(obj, tid).expect("inflate");
        assert!(!contended, "fresh monitor should be acquired immediately");

        let old_addr = obj.as_ptr() as usize;
        let heap2 = Heap::new();
        let new_obj = heap2.alloc_object(ClassId::new(0), 0);
        let new_addr = new_obj.as_ptr() as usize;
        let old_mark = header_of(obj).mark_word.load(Ordering::Acquire);
        header_of(new_obj)
            .mark_word
            .store(old_mark, Ordering::Release);

        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(old_addr, new_addr);
        table.remap_after_gc(&pointer_map);

        monitor.notify_all(tid).expect("notify via stable handle");
        monitor.exit(tid).expect("exit via stable handle");
        table.enter(new_obj, tid);
        assert!(table.exit(new_obj, tid).is_ok());
    }

    /// PERF (monitor-leak reclaim): an idle, unreferenced CAS lock whose object
    /// is absent from the GC forwarding map is dropped from the registry
    /// (unconditional — no env flag), bounding the table. The next CAS on the
    /// same object transparently re-creates the lock, so behaviour is preserved.
    #[test]
    fn cas_lock_idle_dead_entry_is_reclaimed() {
        let table = MonitorTable::new();
        let obj = test_object();

        // Create a CAS lock for `obj`, then let the guard drop so the registry
        // holds the sole `Arc` (strong_count == 1).
        table.with_cas_lock(obj, || {});

        // A non-empty pointer_map that does NOT mention `obj` simulates a
        // whole-heap collection in which `obj` was not forwarded (= dead).
        // (An empty map early-returns; use a dummy unrelated remap so the body
        // actually runs.)
        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(0xdead_0000usize, 0xbeef_0000usize);
        table.remap_after_gc(&pointer_map);

        // The lock for `obj` was reclaimed: a fresh CAS still works (it lazily
        // re-creates the lock), proving no state was lost.
        let mut ran = false;
        table.with_cas_lock(obj, || ran = true);
        assert!(
            ran,
            "CAS lock must be transparently re-created after reclaim"
        );
    }

    /// PERF safety: with monitor reclamation at its DEFAULT (off), an inflated
    /// monitor whose object is absent from a *partial*-collection forwarding map
    /// (a live in-place survivor) is RETAINED and still usable. This guards
    /// against the premature-reclaim / registry-desync class of bug (BUG-V).
    #[test]
    fn inflated_monitor_absent_from_map_is_retained_by_default() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Force inflation via wait() (heavyweight monitor required).
        table.enter(obj, tid);
        table.wait(obj, tid, Some(1), None).unwrap();

        // Partial GC: pointer_map mentions some *other* object, not `obj`
        // (which survived in place). Default flag is off → must NOT reclaim.
        let mut pointer_map = std::collections::HashMap::new();
        pointer_map.insert(0xfeed_0000usize, 0xface_0000usize);
        table.remap_after_gc(&pointer_map);

        // The monitor is still registered under the unchanged address: the
        // owning thread can still exit it (a dropped entry would yield IMSE
        // "never entered").
        assert!(
            table.exit(obj, tid).is_ok(),
            "live in-place monitor must survive a partial-GC remap by default"
        );
    }

    #[test]
    fn monitor_contention_two_threads() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let counter = Arc::new(AtomicU32::new(0));

        // Thread 1 grabs the monitor, increments counter, releases
        let table1 = table.clone();
        let counter1 = counter.clone();
        let h1 = std::thread::spawn(move || {
            for _ in 0..100 {
                table1.enter(obj, ThreadId(1));
                let val = counter1.load(Ordering::SeqCst);
                counter1.store(val + 1, Ordering::SeqCst);
                table1.exit(obj, ThreadId(1)).unwrap();
            }
        });

        // Thread 2 does the same
        let table2 = table.clone();
        let counter2 = counter.clone();
        let h2 = std::thread::spawn(move || {
            for _ in 0..100 {
                table2.enter(obj, ThreadId(2));
                let val = counter2.load(Ordering::SeqCst);
                counter2.store(val + 1, Ordering::SeqCst);
                table2.exit(obj, ThreadId(2)).unwrap();
            }
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // Without proper locking, a non-atomic read-modify-write would lose increments
        assert_eq!(counter.load(Ordering::SeqCst), 200);
    }

    #[test]
    fn monitor_contention_reentrant() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());

        let table1 = table.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            table1.enter(obj, ThreadId(1)); // reentrant
            std::thread::sleep(std::time::Duration::from_millis(10));
            table1.exit(obj, ThreadId(1)).unwrap();
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        let table2 = table.clone();
        let h2 = std::thread::spawn(move || {
            // Give thread 1 a head start
            std::thread::sleep(std::time::Duration::from_millis(2));
            table2.enter(obj, ThreadId(2)); // should block until thread 1 exits
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn monitor_wait_notify() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());
        let flag = Arc::new(AtomicBool::new(false));

        // Consumer: waits for notify
        let table1 = table.clone();
        let flag1 = flag.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            // Wait until producer notifies
            while !flag1.load(Ordering::Acquire) {
                table1.wait(obj, ThreadId(1), Some(50), None).unwrap();
            }
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        // Producer: sets flag and notifies
        let table2 = table.clone();
        let flag2 = flag.clone();
        let h2 = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            table2.enter(obj, ThreadId(2));
            flag2.store(true, Ordering::Release);
            table2.notify(obj, ThreadId(2)).unwrap();
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn monitor_wait_timeout_expires() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid = ThreadId(1);

        table.enter(obj, tid);
        // Wait with a short timeout — nobody notifies, so it should return after timeout
        let start = std::time::Instant::now();
        table.wait(obj, tid, Some(50), None).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 40,
            "Wait should have blocked for ~50ms"
        );
        table.exit(obj, tid).unwrap();
    }

    #[test]
    fn monitor_notify_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        // Try to notify without owning the monitor
        let result = table.notify(obj, ThreadId(1));
        assert!(result.is_err());
    }

    #[test]
    fn monitor_wait_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        // Try to wait without owning the monitor
        let result = table.wait(obj, ThreadId(1), None, None);
        assert!(result.is_err());
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn monitor_enter_exit_same_thread_repeated() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Multiple enter/exit cycles on the same monitor
        for _ in 0..50 {
            table.enter(obj, tid);
            assert!(table.exit(obj, tid).is_ok());
        }
    }

    #[test]
    fn monitor_reentrant_count_two() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        // Enter twice
        table.enter(obj, tid);
        table.enter(obj, tid);

        // First exit decrements count but monitor is still owned
        assert!(table.exit(obj, tid).is_ok());

        // Exiting again fully releases
        assert!(table.exit(obj, tid).is_ok());

        // Third exit should fail (no longer owned)
        assert!(table.exit(obj, tid).is_err());
    }

    #[test]
    fn monitor_state_after_full_exit() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid1 = ThreadId(1);
        let tid2 = ThreadId(2);

        // Thread 1 enters and exits
        table.enter(obj, tid1);
        assert!(table.exit(obj, tid1).is_ok());

        // Monitor is now unowned; thread 2 can enter
        table.enter(obj, tid2);
        assert!(table.exit(obj, tid2).is_ok());
    }

    #[test]
    fn monitor_table_creation_defaults() {
        let table = MonitorTable::new();
        // Default table should have no active monitors
        let debug_str = format!("{:?}", table);
        assert!(debug_str.contains("active_monitors"));
        assert!(debug_str.contains("0"));
    }

    #[test]
    fn monitor_table_default_trait() {
        let table = MonitorTable::default();
        let obj = test_object();
        let tid = ThreadId(1);

        // Should work the same as MonitorTable::new()
        table.enter(obj, tid);
        assert!(table.exit(obj, tid).is_ok());
    }

    #[test]
    fn monitor_notify_all_without_ownership_fails() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();

        let result = table.notify_all(obj, ThreadId(1));
        assert!(result.is_err());
    }

    #[test]
    fn monitor_cas_lock_basic() {
        let table = MonitorTable::new();
        let obj = test_object();

        // CAS lock should execute the closure and return its result
        let result = table.with_cas_lock(obj, || 42);
        assert_eq!(result, 42);
    }

    #[test]
    fn monitor_remap_empty_map_is_noop() {
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(1);

        table.enter(obj, tid);
        let empty_map = std::collections::HashMap::new();
        table.remap_after_gc(&empty_map);
        // Monitor should still be accessible with original address
        assert!(table.exit(obj, tid).is_ok());
    }

    // ── Thin-lock fast-path tests ──────────────────────────────────────────

    /// Returns true if `obj` is currently THIN_LOCKED in its mark word.
    fn is_thin_locked(obj: ObjectRef) -> bool {
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        ObjectHeader::mark_state(mark) == types::MARK_THIN_LOCKED
    }

    /// Returns true if `obj` is currently INFLATED.
    fn is_inflated(obj: ObjectRef) -> bool {
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        ObjectHeader::mark_state(mark) == types::MARK_INFLATED
    }

    /// Returns the active monitor count in the table's fallback registry.
    fn monitor_registry_len(table: &MonitorTable) -> usize {
        // SECURITY FIX (V11): registry is now an OrderedMutex (LockResult).
        table
            .monitors
            .lock()
            .expect("monitors registry poisoned")
            .len()
    }

    #[test]
    fn thin_lock_uncontended() {
        // Single thread enter+exit must take the thin-lock fast path: no
        // Monitor allocation, no registry entry, and the mark word returns
        // to NEUTRAL on release.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(7);

        // Pre-enter: NEUTRAL.
        assert!(!is_thin_locked(obj));
        assert!(!is_inflated(obj));
        assert_eq!(monitor_registry_len(&table), 0);

        // Enter: thin-locked, no allocation.
        table.enter(obj, tid);
        assert!(is_thin_locked(obj), "fast path must use thin lock");
        assert!(!is_inflated(obj), "uncontended path must not inflate");
        assert_eq!(
            monitor_registry_len(&table),
            0,
            "fast path must not touch the Monitor registry"
        );

        // Exit: back to NEUTRAL, still no allocation.
        table.exit(obj, tid).unwrap();
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::mark_state(mark),
            types::MARK_NEUTRAL,
            "release must restore NEUTRAL state"
        );
        assert_eq!(monitor_registry_len(&table), 0);
    }

    #[test]
    fn thin_lock_recursive() {
        // 5 nested acquisitions on the same thread should all use the thin
        // recursive fast path. After all matching exits the mark returns to
        // NEUTRAL with no Monitor allocated.
        let table = MonitorTable::new();
        let obj = test_object();
        let tid = ThreadId(42);

        for expected_rec in 0..5u8 {
            table.enter(obj, tid);
            let mark = header_of(obj).mark_word.load(Ordering::Acquire);
            assert_eq!(ObjectHeader::mark_state(mark), types::MARK_THIN_LOCKED);
            assert_eq!(ObjectHeader::thin_lock_owner(mark), tid.0 as u32);
            assert_eq!(
                ObjectHeader::thin_lock_recursion(mark),
                expected_rec,
                "recursion field must reflect the depth"
            );
        }
        assert_eq!(monitor_registry_len(&table), 0, "no inflation allowed");

        for expected_rec_after in (0..5u8).rev() {
            table.exit(obj, tid).unwrap();
            let mark = header_of(obj).mark_word.load(Ordering::Acquire);
            if expected_rec_after == 0 {
                assert_eq!(ObjectHeader::mark_state(mark), types::MARK_NEUTRAL);
            } else {
                assert_eq!(ObjectHeader::mark_state(mark), types::MARK_THIN_LOCKED);
                assert_eq!(
                    ObjectHeader::thin_lock_recursion(mark),
                    expected_rec_after - 1
                );
            }
        }

        assert_eq!(monitor_registry_len(&table), 0);
    }

    #[test]
    fn thin_lock_inflates_on_contention() {
        // Two threads contend on the same object. The first arrival takes
        // the thin lock; the second arrival must inflate to a heavyweight
        // Monitor. After both finish, the registry must contain exactly
        // one inflated monitor.
        use std::sync::Barrier;

        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = Arc::new(MonitorTable::new());

        // Barrier to ensure both threads are running before contention starts.
        let barrier = Arc::new(Barrier::new(2));

        // Thread 1 takes the lock and holds it long enough that thread 2
        // arrives and must inflate.
        let table1 = table.clone();
        let barrier1 = barrier.clone();
        let h1 = std::thread::spawn(move || {
            table1.enter(obj, ThreadId(1));
            // Mark word should be THIN_LOCKED for tid 1 here -- but thread 2
            // is about to race in and inflate it.
            barrier1.wait();
            std::thread::sleep(std::time::Duration::from_millis(20));
            table1.exit(obj, ThreadId(1)).unwrap();
        });

        // Thread 2: arrives after thread 1 has the thin lock. The
        // contended-thin-lock path inflates and then blocks on entry.
        let table2 = table.clone();
        let barrier2 = barrier.clone();
        let h2 = std::thread::spawn(move || {
            barrier2.wait();
            // Brief delay to ensure thread 1 is still inside the critical
            // section when we attempt to enter.
            std::thread::sleep(std::time::Duration::from_millis(2));
            table2.enter(obj, ThreadId(2));
            table2.exit(obj, ThreadId(2)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // After contention, the object must have been inflated and the
        // monitor registered.
        assert_eq!(
            monitor_registry_len(&table),
            1,
            "contention must produce exactly one inflated monitor"
        );
        // The final state should be INFLATED (mark word permanently points
        // at the Monitor — thin-lock inflation is one-way per object).
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::mark_state(mark),
            types::MARK_INFLATED,
            "inflated mark word should persist after contention"
        );
    }

    /// Regression guard for the Jetty `Deflater.end()`/`DeflaterPool.end()`
    /// hang: a monitor that was still an uncontended THIN lock when its
    /// owning thread died (so `release_monitors_held_by` never saw it — it
    /// wasn't inflated yet) gets inflated *later* by whichever thread next
    /// contends it, and inflation pre-seeds the new `Monitor`'s owner from
    /// the stale mark word. `current_owner`/`force_release_if_owned_by` are
    /// what `vm_exec::monitor_enter_blocking` uses to detect and clear that
    /// dead-owner seed at the contention point, right after inflation,
    /// before blocking — this test exercises that exact mechanism directly.
    #[test]
    fn contended_inflation_of_a_dead_threads_thin_lock_is_recoverable() {
        let heap = Heap::new();
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let table = MonitorTable::new();
        let tid_dead = ThreadId(1);
        let tid_b = ThreadId(2);

        // Thread "dead" takes the (uncontended) thin lock and never releases
        // it — simulating termination while blocked in a native call made
        // from inside the synchronized region.
        table.enter(obj, tid_dead);
        let mark = header_of(obj).mark_word.load(Ordering::Acquire);
        assert_eq!(
            ObjectHeader::mark_state(mark),
            types::MARK_THIN_LOCKED,
            "uncontended enter should stay a thin lock"
        );

        // A live thread contends the same object. `enter_or_contend` must
        // inflate (nothing has released the thin lock) and pre-seed the new
        // Monitor's owner from the dead thread's ID — the exact zombie state
        // this fix targets.
        let m = table
            .enter_or_contend(obj, tid_b)
            .expect("contended thin lock must inflate, not silently succeed");
        assert_eq!(m.current_owner(), Some(tid_dead));

        // The dead-owner check + force-release (mirroring
        // `monitor_enter_blocking`'s contention-point check).
        assert!(m.force_release_if_owned_by(tid_dead));
        assert_eq!(m.current_owner(), None);

        // Thread B can now acquire immediately instead of blocking forever.
        m.block_enter(tid_b);
        assert_eq!(m.current_owner(), Some(tid_b));
        assert!(m.exit(tid_b).is_ok());
    }
}
