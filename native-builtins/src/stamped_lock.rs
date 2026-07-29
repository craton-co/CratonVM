// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP4.7 — Real `java.util.concurrent.locks.StampedLock` and
//! `java.util.concurrent.locks.ReentrantReadWriteLock` backends.
//!
//! Both lock kinds are implemented as a process-wide `parking_lot::Mutex` +
//! `Condvar` map keyed by a GC-stable identity of the Java lock object. The
//! `usize` key is computed by the caller (`lib.rs::gc_stable_lock_key`) from
//! `identity_hash_code` (plus a generation to break hash collisions) rather
//! than the raw heap address — a moving collector relocates the lock object,
//! so a raw-address key computed at lock time would not match the key
//! recomputed at unlock time, missing the slot (deadlock) or hitting a
//! neighbour. To this module the key is an opaque, stable `usize`. The same
//! `Mutex` guards the lock's logical state and acts as the "wait set" the
//! Condvar parks on, so check-and-block is race-free — unlike the older
//! native impl that did `monitor_enter; check; monitor_wait(5ms);
//! monitor_exit`, which dropped the state lock between the check and the
//! wait and could miss a notification.
//!
//! # StampedLock state encoding
//!
//! - `stamp: i64` — version counter. The low bit is the write flag:
//!     - even stamp ⇒ no writer active
//!     - odd  stamp ⇒ writer active
//!   Stamp begins at `STAMPED_ORIGIN` (256) to match real JDK behaviour.
//!   Every `unlockWrite` advances the even part by 1 (so two consecutive
//!   `unlockWrite`s give visibly different stamps and `validate()` after a
//!   write returns false).
//! - `readers: i32` — number of currently held read locks.
//!
//! Both fields live in one `LockState` struct guarded by one `Mutex`. The
//! `Condvar` wakes blocked readers/writers when state changes:
//!     - write → free: notifies all (multiple readers can run)
//!     - read drains to 0: notifies all (a writer may now run)
//!     - reader acquires after a writer release: no extra notify needed —
//!       readers don't block on each other
//!
//! # ReentrantReadWriteLock state encoding
//!
//! - `writer_thread: Option<u64>` — owning thread ID for the write lock.
//! - `write_holds: u32` — re-entrancy count for the writer.
//! - `read_holds: HashMap<u64, u32>` — per-thread read hold count.
//! - `total_readers: u32` — sum of `read_holds` (cached for speed).
//! - `fair: bool` — fairness mode. When fair, FIFO order is preserved by
//!   draining a small queue of "wakers" instead of letting any thread race.
//!
//! Both flavours support `tryLock` (non-blocking attempt) and `lock`
//! (blocking until acquired). Re-entrancy is supported in `RwLock` (ReadLock
//! and WriteLock both let the same thread re-enter); `StampedLock` is
//! intentionally **not** reentrant — the JDK spec forbids it.
//!
//! # Why not just use `std::sync::RwLock`?
//!
//! Java's lock semantics include features `std::sync::RwLock` lacks:
//! validatable optimistic reads, stamp-aware unlock, lock conversion,
//! per-thread reentrant counting, and fairness. We also need the lock to be
//! released **across** the unwinding of a NativeContext call — which means
//! the state must outlive any single ctx and live in a global. Hence the
//! identity-keyed map (GC-stable `usize` key supplied by the caller).

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::{Condvar, Mutex};

/// First stamp returned by a freshly initialised `StampedLock`. Matches the
/// JDK's `WBIT << 8` (= 256). Picking a non-zero origin makes the
/// "stamp != 0" gate in `validate(stamp)` actually mean something: a
/// brand-new lock immediately handed back via `tryOptimisticRead` returns
/// 256, which validates true; a write-locked one returns 0.
pub const STAMPED_ORIGIN: i64 = 256;
const JDK_WBIT: i64 = 128;

/// How much one write release advances the version.
///
/// The low 8 bits of every stamp are reserved for the mode flags below —
/// exactly as the JDK reserves its low `ABITS` — so a stamp carries BOTH
/// "which version of the lock did I observe" and "which mode do I hold".
/// Without a mode bit, `tryConvertToOptimisticRead` cannot tell a read stamp
/// from an optimistic one and would either leak a read hold or fail to
/// release one.
const VERSION_STEP: i64 = 256;
/// Set on a WRITE stamp (`writeLock` / `tryWriteLock` / a successful
/// `tryConvertToWriteLock`). Deliberately the LOW bit: `unlock(J)V` and
/// several call sites already use "odd stamp ⇒ write stamp".
const STAMP_WRITE: i64 = 1;
/// Set on a READ stamp (`readLock` / `tryReadLock` / a successful
/// `tryConvertToReadLock`).
const STAMP_READ: i64 = 2;
/// Selects the version part of a stamp (everything above the mode flags).
const VERSION_MASK: i64 = !0xFF;

// ---------------------------------------------------------------------------
// StampedLock backend
// ---------------------------------------------------------------------------

/// Logical state of one StampedLock instance.
struct StampedState {
    /// Version counter, always a multiple of [`VERSION_STEP`]. Advanced by
    /// every write release so stamps issued before it stop validating.
    version: i64,
    /// A writer currently holds the lock.
    write_held: bool,
    /// Number of currently held read locks.
    readers: i32,
}

impl StampedState {
    fn new() -> Self {
        Self {
            version: STAMPED_ORIGIN,
            write_held: false,
            readers: 0,
        }
    }

    #[inline]
    fn write_stamp(&self) -> i64 {
        self.version | STAMP_WRITE
    }

    #[inline]
    fn read_stamp(&self) -> i64 {
        self.version | STAMP_READ
    }

    /// Drop the write hold and publish a new version.
    #[inline]
    fn release_write(&mut self) {
        self.write_held = false;
        self.version = self.version.wrapping_add(VERSION_STEP);
    }
}

/// One StampedLock — the `Mutex<StampedState>` is taken on every operation
/// that inspects or mutates the lock's logical state, so `Condvar::wait`
/// can release it atomically while parking.
struct StampedLockSlot {
    state: Mutex<StampedState>,
    /// Wakes any thread blocked in `read_lock`/`write_lock`. We use a
    /// single condvar and `notify_all` on every release because the
    /// state transitions are coarse (writer goes free / readers drain to
    /// 0) and the contended-thread count is bounded by application
    /// concurrency, not lock count. `notify_one` would risk waking a
    /// reader when only writers are queued (or vice versa) and forcing
    /// another wake cycle.
    cv: Condvar,
}

impl StampedLockSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(StampedState::new()),
            cv: Condvar::new(),
        }
    }
}

fn stamped_table() -> &'static Mutex<HashMap<usize, std::sync::Arc<StampedLockSlot>>> {
    static T: OnceLock<Mutex<HashMap<usize, std::sync::Arc<StampedLockSlot>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stamped_slot(addr: usize) -> std::sync::Arc<StampedLockSlot> {
    let mut t = stamped_table().lock();
    t.entry(addr)
        .or_insert_with(|| std::sync::Arc::new(StampedLockSlot::new()))
        .clone()
}

/// Clamp a nanosecond timeout to something `Duration::from_nanos` accepts.
fn timeout_duration(nanos: i64) -> std::time::Duration {
    std::time::Duration::from_nanos(nanos.max(0) as u64)
}

/// `<init>()` — establish the slot for this lock's GC-stable identity key.
pub fn stamped_init(addr: usize) {
    // Always (re-)install a fresh slot so a recycled identity key can't
    // expose stale state. The key is a GC-stable identity hash (+generation)
    // supplied by `lib.rs::gc_stable_lock_key`, so a live lock keeps the same
    // key across moving collections; this re-insert is purely defensive
    // against a brand-new lock reusing a retired key.
    let mut t = stamped_table().lock();
    t.insert(addr, std::sync::Arc::new(StampedLockSlot::new()));
}

/// `writeLock()` — block until exclusive write access is granted, return
/// the (odd) write stamp.
pub fn stamped_write_lock(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    while state.write_held || state.readers > 0 {
        slot.cv.wait(&mut state);
    }
    state.write_held = true;
    state.write_stamp()
}

/// `tryWriteLock()` — non-blocking. Returns 0 on failure, the write stamp
/// on success.
pub fn stamped_try_write_lock(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    if state.write_held || state.readers > 0 {
        return 0;
    }
    state.write_held = true;
    state.write_stamp()
}

/// `tryWriteLock(time, unit)` — wait at most `nanos`. Returns 0 on timeout.
pub fn stamped_try_write_lock_timed(addr: usize, nanos: i64) -> i64 {
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    if nanos <= 0 {
        if state.write_held || state.readers > 0 {
            return 0;
        }
        state.write_held = true;
        return state.write_stamp();
    }
    let deadline = std::time::Instant::now() + timeout_duration(nanos);
    while state.write_held || state.readers > 0 {
        if slot.cv.wait_until(&mut state, deadline).timed_out() {
            return 0;
        }
    }
    state.write_held = true;
    state.write_stamp()
}

/// `readLock()` — block until no writer is active, then increment the
/// reader count and return a READ stamp (carries [`STAMP_READ`] so
/// `tryConvertToOptimisticRead` can tell it from an optimistic stamp).
pub fn stamped_read_lock(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    while state.write_held {
        slot.cv.wait(&mut state);
    }
    state.readers += 1;
    state.read_stamp()
}

/// `tryReadLock()` — non-blocking. Returns 0 on failure, a read stamp on success.
pub fn stamped_try_read_lock(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    if state.write_held {
        return 0;
    }
    state.readers += 1;
    state.read_stamp()
}

/// `tryReadLock(time, unit)` — wait at most `nanos`. Returns 0 on timeout.
pub fn stamped_try_read_lock_timed(addr: usize, nanos: i64) -> i64 {
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    if nanos <= 0 {
        if state.write_held {
            return 0;
        }
        state.readers += 1;
        return state.read_stamp();
    }
    let deadline = std::time::Instant::now() + timeout_duration(nanos);
    while state.write_held {
        if slot.cv.wait_until(&mut state, deadline).timed_out() {
            return 0;
        }
    }
    state.readers += 1;
    state.read_stamp()
}

/// `tryOptimisticRead()` — never blocks. Returns 0 if a writer is active,
/// the current version otherwise. Caller must subsequently `validate(stamp)`
/// to confirm no writer ran in the meantime.
pub fn stamped_try_optimistic_read(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let state = slot.state.lock();
    if state.write_held {
        0
    } else {
        state.version
    }
}

/// `validate(stamp)` — true iff `stamp` still describes the lock.
///
/// Mirrors the JDK: a WRITE stamp validates while its holder still holds the
/// lock (the JDK compares `SBITS`, which includes `WBIT`); a read or
/// optimistic stamp validates while the version is unchanged and no writer
/// has taken over.
pub fn stamped_validate(addr: usize, stamp: i64) -> bool {
    if stamp == 0 {
        return false; // 0 is the "invalid" sentinel
    }
    let slot = stamped_slot(addr);
    let state = slot.state.lock();
    if (stamp & VERSION_MASK) != state.version {
        return false;
    }
    if stamp & STAMP_WRITE != 0 {
        return state.write_held;
    }
    !state.write_held
}

/// `unlockWrite(stamp)` — release the write lock, advance the version.
pub fn stamped_unlock_write(addr: usize) {
    let slot = stamped_slot(addr);
    {
        let mut state = slot.state.lock();
        if !state.write_held {
            return;
        }
        state.release_write();
    }
    // Wake any reader/writer queued behind us.
    slot.cv.notify_all();
}

/// Release a write hold without a stamp; false when none was held.
pub fn stamped_try_unstamped_unlock_write(addr: usize) -> bool {
    let slot = stamped_slot(addr);
    {
        let mut state = slot.state.lock();
        if !state.write_held {
            return false;
        }
        state.release_write();
    }
    slot.cv.notify_all();
    true
}

/// `unlockRead(stamp)` — decrement reader count; if it hits 0 wake any
/// queued writer.
pub fn stamped_unlock_read(addr: usize) {
    let slot = stamped_slot(addr);
    let drained_to_zero;
    {
        let mut state = slot.state.lock();
        if state.readers > 0 {
            state.readers -= 1;
        }
        drained_to_zero = state.readers == 0;
    }
    if drained_to_zero {
        slot.cv.notify_all();
    }
}

/// Release a read hold without a stamp; false when none was held.
pub fn stamped_try_unstamped_unlock_read(addr: usize) -> bool {
    let slot = stamped_slot(addr);
    let drained_to_zero;
    {
        let mut state = slot.state.lock();
        if state.readers <= 0 {
            return false;
        }
        state.readers -= 1;
        drained_to_zero = state.readers == 0;
    }
    if drained_to_zero {
        slot.cv.notify_all();
    }
    true
}

/// The value mirrored into the real `StampedLock.state` field so JDK bytecode
/// that merely *reads* it (`toString`, `isLocked`) sees something coherent.
pub fn stamped_visible_state(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let state = slot.state.lock();
    let base = state.version;
    if state.write_held {
        base | JDK_WBIT
    } else {
        base.saturating_add(i64::from(state.readers.clamp(0, 126)))
    }
}

/// `tryConvertToWriteLock(stamp)`.
///
/// Per the JDK: succeeds when the stamp already holds the write lock (returns
/// it unchanged), when it is the ONLY read hold (upgrade), or when it is a
/// still-valid optimistic stamp and the lock is free.
pub fn stamped_try_convert_to_write(addr: usize, stamp: i64) -> i64 {
    if stamp == 0 {
        return 0;
    }
    let slot = stamped_slot(addr);
    let mut state = slot.state.lock();
    if (stamp & VERSION_MASK) != state.version {
        return 0;
    }
    if stamp & STAMP_WRITE != 0 {
        return if state.write_held {
            state.write_stamp()
        } else {
            0
        };
    }
    if stamp & STAMP_READ != 0 {
        if state.write_held || state.readers != 1 {
            return 0;
        }
        state.readers = 0;
        state.write_held = true;
        return state.write_stamp();
    }
    // Optimistic stamp: only convertible while the lock is completely free.
    if state.write_held || state.readers != 0 {
        return 0;
    }
    state.write_held = true;
    state.write_stamp()
}

/// `tryConvertToReadLock(stamp)` — downgrade a write lock (or upgrade an
/// optimistic observation) to a read lock.
pub fn stamped_try_convert_to_read(addr: usize, stamp: i64) -> i64 {
    if stamp == 0 {
        return 0;
    }
    let slot = stamped_slot(addr);
    let new_stamp;
    let notify;
    {
        let mut state = slot.state.lock();
        if (stamp & VERSION_MASK) != state.version {
            return 0;
        }
        if stamp & STAMP_WRITE != 0 {
            if !state.write_held {
                return 0;
            }
            // Advance the version (write released) and immediately take a read.
            state.release_write();
            state.readers += 1;
            new_stamp = state.read_stamp();
            notify = true;
        } else if stamp & STAMP_READ != 0 {
            // Already a read hold — nothing to do.
            return stamp;
        } else {
            if state.write_held {
                return 0;
            }
            state.readers += 1;
            new_stamp = state.read_stamp();
            notify = false;
        }
    }
    if notify {
        slot.cv.notify_all();
    }
    new_stamp
}

/// `tryConvertToOptimisticRead(stamp)` — release whatever hold `stamp`
/// represents and hand back an observation stamp; 0 if `stamp` is stale.
///
/// **This is a release path, not an inspection.** Agroal's
/// `StampedCopyOnWriteArrayList` (io.agroal:agroal-pool) never calls
/// `unlockWrite` at all — every mutator is
/// `long stamp = lock.writeLock(); try { … } finally { optimisticStamp =
/// lock.tryConvertToOptimisticRead(stamp); }`. While this method was missing
/// from the native surface the call fell through to real JDK bytecode, which
/// reads the real `state` field this side-table backend does not drive: it
/// returned 0 and released nothing, so the write hold leaked and the next
/// `readLock()` blocked forever. That deadlocked the Keycloak 26.6.1 boot in
/// `JPAConfig.startAll` (Agroal's connection pool), with the JPA startup
/// thread and the pool's validation thread both parked in
/// `StampedCopyOnWriteArrayList.getUnderlyingArray`.
pub fn stamped_try_convert_to_optimistic(addr: usize, stamp: i64) -> i64 {
    if stamp == 0 {
        return 0;
    }
    let slot = stamped_slot(addr);
    let version;
    let notify;
    {
        let mut state = slot.state.lock();
        if (stamp & VERSION_MASK) != state.version {
            return 0;
        }
        if stamp & STAMP_WRITE != 0 {
            if !state.write_held {
                return 0;
            }
            state.release_write();
            version = state.version;
            notify = true;
        } else if stamp & STAMP_READ != 0 {
            if state.readers <= 0 {
                return 0;
            }
            state.readers -= 1;
            version = state.version;
            notify = state.readers == 0;
        } else {
            // Already an optimistic observation: still valid iff no writer
            // has taken the lock since it was issued.
            return if state.write_held { 0 } else { state.version };
        }
    }
    if notify {
        slot.cv.notify_all();
    }
    version
}

/// `isWriteLocked()` — non-blocking inspection.
pub fn stamped_is_write_locked(addr: usize) -> bool {
    let slot = stamped_slot(addr);
    let result = slot.state.lock().write_held;
    result
}

/// `isReadLocked()` — non-blocking inspection.
pub fn stamped_is_read_locked(addr: usize) -> bool {
    let slot = stamped_slot(addr);
    let result = slot.state.lock().readers > 0;
    result
}

/// `isLocked()` — held in either mode.
pub fn stamped_is_locked(addr: usize) -> bool {
    let slot = stamped_slot(addr);
    let state = slot.state.lock();
    state.write_held || state.readers > 0
}

/// `getReadLockCount()` — current number of held read locks.
pub fn stamped_get_read_lock_count(addr: usize) -> i32 {
    let slot = stamped_slot(addr);
    let result = slot.state.lock().readers;
    result
}

// ---------------------------------------------------------------------------
// ReentrantReadWriteLock backend
// ---------------------------------------------------------------------------

/// Per-thread / per-lock state for ReentrantReadWriteLock.
struct RwLockState {
    /// Owning thread ID for the write lock; `None` when no writer.
    writer_thread: Option<u64>,
    /// Re-entrancy count for the writer (only meaningful when
    /// `writer_thread.is_some()`).
    write_holds: u32,
    /// Per-thread read-hold count. A thread only appears in this map when
    /// it holds at least one read lock. Removing the entry on the last
    /// `unlock` keeps the map small.
    read_holds: HashMap<u64, u32>,
    /// Cached `read_holds.values().sum()`. Updating the sum on every
    /// lock/unlock saves a `.values().sum()` walk on the hot path of
    /// `getReadLockCount()` — small but every nanosecond matters under
    /// heavy reader contention.
    total_readers: u32,
    /// Fairness mode. Set once at construction; never modified.
    fair: bool,
    /// Number of writers blocked waiting for the lock. Used in fair mode
    /// to make readers yield to a queued writer (HotSpot convention).
    waiting_writers: u32,
}

impl RwLockState {
    fn new(fair: bool) -> Self {
        Self {
            writer_thread: None,
            write_holds: 0,
            read_holds: HashMap::new(),
            total_readers: 0,
            fair,
            waiting_writers: 0,
        }
    }

    #[inline]
    fn write_held_by_other(&self, tid: u64) -> bool {
        match self.writer_thread {
            Some(o) if o != tid => true,
            _ => false,
        }
    }

    #[inline]
    fn write_held_by(&self, tid: u64) -> bool {
        self.writer_thread == Some(tid)
    }
}

struct RwLockSlot {
    state: Mutex<RwLockState>,
    cv: Condvar,
}

fn rw_table() -> &'static Mutex<HashMap<usize, std::sync::Arc<RwLockSlot>>> {
    static T: OnceLock<Mutex<HashMap<usize, std::sync::Arc<RwLockSlot>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rw_slot(addr: usize, fair: bool) -> std::sync::Arc<RwLockSlot> {
    let mut t = rw_table().lock();
    t.entry(addr)
        .or_insert_with(|| {
            std::sync::Arc::new(RwLockSlot {
                state: Mutex::new(RwLockState::new(fair)),
                cv: Condvar::new(),
            })
        })
        .clone()
}

/// `<init>()` / `<init>(boolean)` — initialise the slot. Idempotent: a
/// repeated init under the same address is treated as fresh state.
pub fn rw_init(addr: usize, fair: bool) {
    let mut t = rw_table().lock();
    t.insert(
        addr,
        std::sync::Arc::new(RwLockSlot {
            state: Mutex::new(RwLockState::new(fair)),
            cv: Condvar::new(),
        }),
    );
}

/// Returns true if a reader can proceed. Encodes the fairness rule:
/// in fair mode, a queued writer takes precedence over a fresh reader,
/// EXCEPT when the calling thread is itself the writer (downgrade path)
/// or already holds a read lock (re-entrant read).
fn reader_can_proceed(state: &RwLockState, tid: u64) -> bool {
    // Already a re-entrant read → always allowed (otherwise we'd deadlock
    // a thread that holds a read lock and tries to take another).
    if state.read_holds.contains_key(&tid) {
        return true;
    }
    // No writer & (unfair OR no queued writer) → reader runs.
    if state.write_held_by(tid) {
        return true; // downgrade path: holding write, take a read
    }
    if state.writer_thread.is_some() {
        return false;
    }
    if state.fair && state.waiting_writers > 0 {
        return false;
    }
    true
}

/// `ReentrantReadWriteLock$ReadLock.lock()` — block until allowed, then
/// increment per-thread + total reader counts.
pub fn rw_read_lock(parent_addr: usize, tid: u64) {
    let slot = rw_slot(parent_addr, false);
    let mut state = slot.state.lock();
    while !reader_can_proceed(&state, tid) {
        slot.cv.wait(&mut state);
    }
    *state.read_holds.entry(tid).or_insert(0) += 1;
    state.total_readers += 1;
}

/// `tryLock()` for the read lock. Non-blocking.
pub fn rw_try_read_lock(parent_addr: usize, tid: u64) -> bool {
    let slot = rw_slot(parent_addr, false);
    let mut state = slot.state.lock();
    // tryLock ignores the fairness queue per JDK spec — it's a barging
    // attempt by design.
    if state.write_held_by_other(tid) {
        return false;
    }
    *state.read_holds.entry(tid).or_insert(0) += 1;
    state.total_readers += 1;
    true
}

/// `ReentrantReadWriteLock$ReadLock.unlock()`.
pub fn rw_read_unlock(parent_addr: usize, tid: u64) {
    let slot = rw_slot(parent_addr, false);
    let drained_to_zero;
    {
        let mut state = slot.state.lock();
        let entry = match state.read_holds.get_mut(&tid) {
            Some(c) => c,
            None => return, // unlock without lock — silently no-op
        };
        if *entry > 1 {
            *entry -= 1;
        } else {
            state.read_holds.remove(&tid);
        }
        if state.total_readers > 0 {
            state.total_readers -= 1;
        }
        drained_to_zero = state.total_readers == 0;
    }
    if drained_to_zero {
        slot.cv.notify_all();
    }
}

/// `ReentrantReadWriteLock$WriteLock.lock()` — block until exclusive
/// access is granted (no readers, no other writer).
pub fn rw_write_lock(parent_addr: usize, tid: u64) {
    let slot = rw_slot(parent_addr, false);
    let mut state = slot.state.lock();
    // Re-entrant write: already held by us → just bump count.
    if state.write_held_by(tid) {
        state.write_holds += 1;
        return;
    }
    // Otherwise queue and wait. Increment `waiting_writers` so fair-mode
    // readers know to yield. Decrement on every wakeup, even if we don't
    // win the race (recheck and increment again before waiting).
    state.waiting_writers += 1;
    while state.writer_thread.is_some() || state.total_readers > 0 {
        slot.cv.wait(&mut state);
    }
    state.waiting_writers -= 1;
    state.writer_thread = Some(tid);
    state.write_holds = 1;
}

/// `tryLock()` on the write lock. Non-blocking.
pub fn rw_try_write_lock(parent_addr: usize, tid: u64) -> bool {
    let slot = rw_slot(parent_addr, false);
    let mut state = slot.state.lock();
    if state.write_held_by(tid) {
        state.write_holds += 1;
        return true;
    }
    if state.writer_thread.is_some() || state.total_readers > 0 {
        return false;
    }
    state.writer_thread = Some(tid);
    state.write_holds = 1;
    true
}

/// `ReentrantReadWriteLock$WriteLock.unlock()`.
pub fn rw_write_unlock(parent_addr: usize, tid: u64) {
    let slot = rw_slot(parent_addr, false);
    let released;
    {
        let mut state = slot.state.lock();
        // Spec: throws IllegalMonitorStateException if not held by current
        // thread. We just no-op here; the JDK bytecode wrapper class can
        // still see the post-condition through subsequent isHeldByCurrent
        // checks.
        if !state.write_held_by(tid) {
            return;
        }
        if state.write_holds > 1 {
            state.write_holds -= 1;
            released = false;
        } else {
            state.write_holds = 0;
            state.writer_thread = None;
            released = true;
        }
    }
    if released {
        slot.cv.notify_all();
    }
}

/// `ReentrantReadWriteLock$WriteLock.isHeldByCurrentThread()`.
pub fn rw_write_is_held(parent_addr: usize, tid: u64) -> bool {
    let slot = rw_slot(parent_addr, false);
    let result = slot.state.lock().write_held_by(tid);
    result
}

/// Number of held read locks (across all threads).
pub fn rw_read_count(parent_addr: usize) -> i32 {
    let slot = rw_slot(parent_addr, false);
    let result = slot.state.lock().total_readers as i32;
    result
}

/// Whether any thread currently holds the write lock.
pub fn rw_is_write_locked(parent_addr: usize) -> bool {
    let slot = rw_slot(parent_addr, false);
    let result = slot.state.lock().writer_thread.is_some();
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Each test allocates a unique synthetic "object address" so the
    /// global tables stay disjoint between tests.
    fn fresh_addr() -> usize {
        static N: AtomicUsize = AtomicUsize::new(0x1000);
        // Spread addresses out by 64 to dodge any chance of accidental
        // collision with cache-line-rounded keys elsewhere.
        N.fetch_add(64, Ordering::SeqCst)
    }

    // --- StampedLock ---

    #[test]
    fn stamped_init_returns_origin() {
        let a = fresh_addr();
        stamped_init(a);
        assert_eq!(stamped_try_optimistic_read(a), STAMPED_ORIGIN);
    }

    #[test]
    fn stamped_write_lock_yields_odd_stamp() {
        let a = fresh_addr();
        stamped_init(a);
        let s = stamped_write_lock(a);
        assert!(s & 1 != 0, "write stamp should be odd");
        stamped_unlock_write(a);
    }

    #[test]
    fn stamped_unlock_write_advances_stamp() {
        let a = fresh_addr();
        stamped_init(a);
        let s0 = stamped_try_optimistic_read(a);
        stamped_write_lock(a);
        stamped_unlock_write(a);
        let s1 = stamped_try_optimistic_read(a);
        assert!(s1 > s0, "stamp must advance: {} -> {}", s0, s1);
        assert_eq!(s1 & 1, 0, "post-unlock stamp must be even");
    }

    #[test]
    fn stamped_validate_zero_is_invalid() {
        let a = fresh_addr();
        stamped_init(a);
        assert!(!stamped_validate(a, 0));
    }

    #[test]
    fn stamped_validate_after_write_fails() {
        let a = fresh_addr();
        stamped_init(a);
        let s = stamped_try_optimistic_read(a);
        stamped_write_lock(a);
        stamped_unlock_write(a);
        assert!(!stamped_validate(a, s));
    }

    #[test]
    fn stamped_try_write_blocked_by_reader() {
        let a = fresh_addr();
        stamped_init(a);
        stamped_read_lock(a);
        assert_eq!(stamped_try_write_lock(a), 0);
        stamped_unlock_read(a);
    }

    #[test]
    fn stamped_try_read_blocked_by_writer() {
        let a = fresh_addr();
        stamped_init(a);
        stamped_write_lock(a);
        assert_eq!(stamped_try_read_lock(a), 0);
        stamped_unlock_write(a);
    }

    #[test]
    fn stamped_convert_to_write_succeeds_with_one_reader() {
        let a = fresh_addr();
        stamped_init(a);
        let r = stamped_read_lock(a);
        let w = stamped_try_convert_to_write(a, r);
        assert!(w != 0);
        assert!(stamped_is_write_locked(a));
        assert_eq!(stamped_get_read_lock_count(a), 0);
        stamped_unlock_write(a);
    }

    #[test]
    fn stamped_convert_to_write_fails_with_two_readers() {
        let a = fresh_addr();
        stamped_init(a);
        let r1 = stamped_read_lock(a);
        let _r2 = stamped_read_lock(a);
        assert_eq!(stamped_try_convert_to_write(a, r1), 0);
        stamped_unlock_read(a);
        stamped_unlock_read(a);
    }

    #[test]
    fn stamped_writer_blocks_then_releases() {
        // Cross-thread test: take write, spawn reader (blocks), release
        // write, reader makes progress.
        let a = fresh_addr();
        stamped_init(a);
        stamped_write_lock(a);
        let join = std::thread::spawn(move || {
            let s = stamped_read_lock(a);
            stamped_unlock_read(a);
            s
        });
        // give the reader a moment to be queued
        std::thread::sleep(std::time::Duration::from_millis(20));
        stamped_unlock_write(a);
        let stamp = join.join().unwrap();
        // Reader's stamp must be the post-unlock even stamp.
        assert!(stamp & 1 == 0);
    }

    #[test]
    fn stamped_no_lost_wakeup_under_contention() {
        // Stress the wait/notify edge: many readers contending against a
        // single writer that bursts on/off. If the older
        // monitor_enter/wait/exit pattern lost a notification, this would
        // hang past the test timeout. With Mutex+Condvar held across the
        // check, every waker is observed.
        let a = fresh_addr();
        stamped_init(a);
        let mut handles = vec![];
        for _ in 0..8 {
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let s = stamped_read_lock(a);
                    let _ = s;
                    stamped_unlock_read(a);
                }
            }));
        }
        // Writer churns the lock alongside the readers.
        for _ in 0..50 {
            stamped_write_lock(a);
            stamped_unlock_write(a);
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(stamped_get_read_lock_count(a), 0);
    }

    /// The Agroal `StampedCopyOnWriteArrayList` shape: `writeLock()` is
    /// released ONLY by `tryConvertToOptimisticRead(stamp)`. Before this was
    /// implemented natively the write hold leaked and the next `readLock()`
    /// blocked forever (Keycloak 26.6.1 boot deadlock in JPAConfig.startAll).
    #[test]
    fn stamped_convert_to_optimistic_releases_the_write_lock() {
        let a = fresh_addr();
        stamped_init(a);
        let w = stamped_write_lock(a);
        assert!(stamped_is_write_locked(a));
        let obs = stamped_try_convert_to_optimistic(a, w);
        assert!(obs != 0, "converting a held write stamp must succeed");
        assert!(!stamped_is_write_locked(a), "write lock must be released");
        assert!(stamped_validate(a, obs));
        // The whole point: a reader must now get through without blocking.
        let r = stamped_read_lock(a);
        assert!(r & 1 == 0);
        stamped_unlock_read(a);
    }

    #[test]
    fn stamped_convert_to_optimistic_releases_a_read_hold() {
        let a = fresh_addr();
        stamped_init(a);
        let r = stamped_read_lock(a);
        assert_eq!(stamped_get_read_lock_count(a), 1);
        let obs = stamped_try_convert_to_optimistic(a, r);
        assert!(obs != 0);
        assert_eq!(stamped_get_read_lock_count(a), 0);
        // A writer can now take the lock without blocking.
        assert!(stamped_try_write_lock(a) != 0);
        stamped_unlock_write(a);
    }

    #[test]
    fn stamped_convert_to_optimistic_on_stale_stamp_is_zero() {
        let a = fresh_addr();
        stamped_init(a);
        let s = stamped_try_optimistic_read(a);
        stamped_write_lock(a);
        stamped_unlock_write(a);
        assert_eq!(stamped_try_convert_to_optimistic(a, s), 0);
        assert_eq!(stamped_try_convert_to_optimistic(a, 0), 0);
    }

    #[test]
    fn stamped_read_and_optimistic_stamps_are_distinguishable() {
        let a = fresh_addr();
        stamped_init(a);
        let opt = stamped_try_optimistic_read(a);
        let rd = stamped_read_lock(a);
        assert_ne!(opt, rd, "an optimistic stamp must not look like a read stamp");
        // Converting the OPTIMISTIC stamp must not steal the reader's hold.
        assert!(stamped_try_convert_to_optimistic(a, opt) != 0);
        assert_eq!(stamped_get_read_lock_count(a), 1);
        stamped_unlock_read(a);
    }

    #[test]
    fn stamped_timed_try_write_lock_times_out_under_a_reader() {
        let a = fresh_addr();
        stamped_init(a);
        stamped_read_lock(a);
        let t0 = std::time::Instant::now();
        assert_eq!(stamped_try_write_lock_timed(a, 20_000_000), 0);
        assert!(t0.elapsed() >= std::time::Duration::from_millis(15));
        stamped_unlock_read(a);
        assert!(stamped_try_write_lock_timed(a, 20_000_000) != 0);
        stamped_unlock_write(a);
    }

    #[test]
    fn stamped_convert_to_read_downgrades_a_write_hold() {
        let a = fresh_addr();
        stamped_init(a);
        let w = stamped_write_lock(a);
        let r = stamped_try_convert_to_read(a, w);
        assert!(r != 0);
        assert!(!stamped_is_write_locked(a));
        assert_eq!(stamped_get_read_lock_count(a), 1);
        stamped_unlock_read(a);
    }

    #[test]
    fn stamped_is_locked_covers_both_modes() {
        let a = fresh_addr();
        stamped_init(a);
        assert!(!stamped_is_locked(a));
        stamped_read_lock(a);
        assert!(stamped_is_locked(a));
        stamped_unlock_read(a);
        stamped_write_lock(a);
        assert!(stamped_is_locked(a));
        stamped_unlock_write(a);
        assert!(!stamped_is_locked(a));
    }

    // --- ReentrantReadWriteLock ---

    #[test]
    fn rw_basic_read_lock_unlock() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_read_lock(a, 1);
        assert_eq!(rw_read_count(a), 1);
        rw_read_unlock(a, 1);
        assert_eq!(rw_read_count(a), 0);
    }

    #[test]
    fn rw_basic_write_lock_unlock() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_write_lock(a, 1);
        assert!(rw_is_write_locked(a));
        assert!(rw_write_is_held(a, 1));
        assert!(!rw_write_is_held(a, 2));
        rw_write_unlock(a, 1);
        assert!(!rw_is_write_locked(a));
    }

    #[test]
    fn rw_reentrant_write() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_write_lock(a, 1);
        rw_write_lock(a, 1); // same thread re-enters
        assert!(rw_write_is_held(a, 1));
        rw_write_unlock(a, 1);
        assert!(rw_write_is_held(a, 1)); // still held
        rw_write_unlock(a, 1);
        assert!(!rw_is_write_locked(a));
    }

    #[test]
    fn rw_reentrant_read() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_read_lock(a, 1);
        rw_read_lock(a, 1); // same thread re-enters
        assert_eq!(rw_read_count(a), 2);
        rw_read_unlock(a, 1);
        rw_read_unlock(a, 1);
        assert_eq!(rw_read_count(a), 0);
    }

    #[test]
    fn rw_try_write_blocked_by_reader_from_other_thread() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_read_lock(a, 1);
        assert!(!rw_try_write_lock(a, 2));
        rw_read_unlock(a, 1);
        assert!(rw_try_write_lock(a, 2));
        rw_write_unlock(a, 2);
    }

    #[test]
    fn rw_try_read_blocked_by_writer_from_other_thread() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_write_lock(a, 1);
        assert!(!rw_try_read_lock(a, 2));
        rw_write_unlock(a, 1);
        assert!(rw_try_read_lock(a, 2));
        rw_read_unlock(a, 2);
    }

    #[test]
    fn rw_writer_blocks_on_readers_and_releases() {
        let a = fresh_addr();
        rw_init(a, false);
        rw_read_lock(a, 1);
        let h = std::thread::spawn(move || {
            rw_write_lock(a, 2);
            rw_write_unlock(a, 2);
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        rw_read_unlock(a, 1);
        h.join().unwrap();
        assert!(!rw_is_write_locked(a));
    }

    #[test]
    fn rw_concurrent_readers_no_serialization() {
        let a = fresh_addr();
        rw_init(a, false);
        let n = 4;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
        let mut handles = vec![];
        for tid in 0..n as u64 {
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                rw_read_lock(a, tid + 1);
                // All readers must be holding simultaneously here.
                b.wait();
                let count = rw_read_count(a);
                rw_read_unlock(a, tid + 1);
                count
            }));
        }
        let counts: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // At the barrier rendezvous all 4 readers were holding.
        assert!(
            counts.iter().any(|c| *c == n as i32),
            "expected at least one observation of {} concurrent readers, got {:?}",
            n,
            counts
        );
        assert_eq!(rw_read_count(a), 0);
    }
}
