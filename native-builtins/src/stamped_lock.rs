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
//! The stamp layout is the REAL JDK's, bit for bit — see the constant block
//! below for why that is mandatory rather than merely tidy. In summary:
//!
//! - `version: i64` — a multiple of 256, advanced by one step on every write
//!   release, so stamps issued before it stop validating. Starts at
//!   `STAMPED_ORIGIN` (256, the JDK's `ORIGIN`).
//! - the low 8 bits of a stamp are the JDK's `ABITS` mode field:
//!     - `& ABITS == WBIT` (128) ⇒ write stamp
//!     - `& ABITS == 0`          ⇒ optimistic-read stamp (zero ⇒ invalid)
//!     - `& ABITS` in `1..=RFULL` ⇒ read stamp, the value being the reader
//!       count at acquisition
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

// ---------------------------------------------------------------------------
// Stamp bit layout — THE SAME LAYOUT THE REAL JDK USES, DELIBERATELY
//
// These are transcribed from `java.util.concurrent.locks.StampedLock` in
// JDK 25's `src.zip` (lines 314-340) and must not drift from it:
//
//     private static final int  LG_READERS = 7;          // 127 readers
//     private static final long RUNIT = 1L;
//     private static final long WBIT  = 1L << LG_READERS;
//     private static final long RBITS = WBIT - 1L;
//     private static final long RFULL = RBITS - 1L;
//     private static final long ABITS = RBITS | WBIT;
//     private static final long SBITS = ~RBITS;
//     private static final long ORIGIN = WBIT << 1;
//
// WHY WE MUST MATCH IT EXACTLY, rather than pick our own flags:
// `isWriteLockStamp`, `isReadLockStamp`, `isLockStamp` and
// `isOptimisticReadStamp` are `public static boolean (long)` methods —
// PURE FUNCTIONS OF THE STAMP that touch no instance state — and no
// registrar provides them, so REAL JDK BYTECODE runs for them and decodes
// whatever `long` this backend handed out. Until 2026-08-07 this module
// encoded a write stamp as `version | 1` and a read stamp as `version | 2`.
// The JDK's write bit is 128 and its READ COUNT is the low 7 bits, so
// `version | 1` decodes as "read lock held by one reader":
//
//     isWriteLockStamp(writeLock())  -> false   (should be true)
//     isReadLockStamp (writeLock())  -> true    (should be false)
//
// which sends the canonical release idiom
//     if (isWriteLockStamp(s)) unlockWrite(s); else if (isReadLockStamp(s)) unlockRead(s);
// down the WRONG branch, so the write hold is never released. Adopting the
// JDK layout fixes all four predicates at once — and every other
// unregistered stamp reader, including ones nobody has enumerated — because
// they are decoding a stamp in the format they were written to decode.
//
// It also makes the stamps agree with the `state` word this backend already
// mirrors into the real field (`stamped_visible_state` ->
// `lib.rs::mirror_stamped_state`), which was ALREADY in the JDK's layout.
// Before this change the two disagreed with each other.
// ---------------------------------------------------------------------------

/// JDK `LG_READERS` — bits reserved for the reader count before overflow.
const LG_READERS: u32 = 7;
/// JDK `RUNIT` — the increment for one read hold.
const RUNIT: i64 = 1;
/// JDK `WBIT` — the write bit. **128**, not 1.
const WBIT: i64 = 1 << LG_READERS;
/// JDK `RBITS` — the reader-count field (the low 7 bits).
const RBITS: i64 = WBIT - 1;
/// JDK `RFULL` — the largest reader count `RBITS` can hold. Beyond this the
/// JDK spills into `readerOverflow` and keeps handing out stamps whose count
/// field reads `RFULL`; we saturate identically so a stamp never reaches
/// `RBITS` (the JDK's transient spin-lock marker) or `WBIT`.
const RFULL: i64 = RBITS - 1;
/// JDK `ABITS` — the whole mode field, `RBITS | WBIT`. `stamp & ABITS`
/// distinguishes the three stamp modes: `WBIT` = write, `0` = optimistic,
/// `1..=RFULL` = read.
const ABITS: i64 = RBITS | WBIT;
/// JDK `SBITS` — `~RBITS`. Everything a stamp carries EXCEPT the reader
/// count, so it deliberately keeps `WBIT`. This is what the JDK compares in
/// `validate` and `unlockRead`: two concurrent readers hold stamps with
/// different counts but the same `SBITS`, while a stamp that has the write
/// bit set differs under `SBITS` from an unlocked state at the same version.
const SBITS: i64 = !RBITS;

/// First stamp returned by a freshly initialised `StampedLock`. This is the
/// JDK's `ORIGIN = WBIT << 1` (= 256) exactly. A non-zero origin makes the
/// "stamp != 0" gate in `validate(stamp)` mean something: a brand-new lock
/// handed back via `tryOptimisticRead` returns 256, which validates true; a
/// write-locked one returns 0.
pub const STAMPED_ORIGIN: i64 = WBIT << 1;

/// How much one write release advances the version.
///
/// The JDK does not have a named constant for this: `unlockWriteState(s)`
/// is `s + WBIT`, which clears the write bit and carries into bit 8. Since
/// every unlocked state has bit 7 clear, one full lock/unlock cycle moves
/// the version by `2 * WBIT` = 256 = `ABITS + 1`. We keep the version as a
/// multiple of that so the low 8 bits are exclusively the mode field.
const VERSION_STEP: i64 = ABITS + 1;
/// Selects the version part of a stamp (everything above the mode field).
const VERSION_MASK: i64 = !ABITS;

/// `WBIT`, re-exported for `unlock(J)V`. That dispatcher's JDK arm is
/// `(stamp & WBIT) != 0`, which is deliberately WEAKER than
/// [`is_write_stamp`]'s `(stamp & ABITS) == WBIT`; keep them distinct.
pub const JDK_WBIT: i64 = WBIT;

/// JDK `StampedLock.isWriteLockStamp`: `(stamp & ABITS) == WBIT`.
#[inline]
pub fn is_write_stamp(stamp: i64) -> bool {
    stamp & ABITS == WBIT
}

/// JDK `StampedLock.isReadLockStamp`: `(stamp & RBITS) != 0`.
#[inline]
pub fn is_read_stamp(stamp: i64) -> bool {
    stamp & RBITS != 0
}

/// JDK `StampedLock.isOptimisticReadStamp`:
/// `(stamp & ABITS) == 0 && stamp != 0`.
#[inline]
pub fn is_optimistic_stamp(stamp: i64) -> bool {
    stamp & ABITS == 0 && stamp != 0
}

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

    /// The JDK `state` word for this logical state — the value the real
    /// `StampedLock.state` field is mirrored to, and the value from which
    /// every stamp below is cut. Write-held sets `WBIT`; otherwise the low
    /// 7 bits carry the reader count, saturating at `RFULL` exactly as the
    /// JDK's `tryIncReaderOverflow` does.
    #[inline]
    fn state_word(&self) -> i64 {
        if self.write_held {
            self.version | WBIT
        } else {
            self.version | self.reader_field()
        }
    }

    /// The reader count as the JDK's `RBITS` field: `0..=RFULL`.
    #[inline]
    fn reader_field(&self) -> i64 {
        i64::from(self.readers).clamp(0, RFULL)
    }

    /// A WRITE stamp: `version | WBIT`. Call only with `write_held` set —
    /// the JDK's write stamp IS the write-held state word.
    #[inline]
    fn write_stamp(&self) -> i64 {
        self.version | WBIT
    }

    /// A READ stamp: `version | reader_count`, i.e. a copy of the state word
    /// taken after this hold was counted. The JDK notes the count in a stamp
    /// "is unused other than to determine mode", but two concurrent readers
    /// really do get different stamps (1025, 1026, ...) and code that
    /// compares stamps for identity must see that.
    #[inline]
    fn read_stamp(&self) -> i64 {
        self.version | self.reader_field().max(RUNIT)
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
/// reader count and return a READ stamp (a non-zero `RBITS` field, so
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
    if is_write_stamp(stamp) {
        return state.write_held;
    }
    !state.write_held
}

/// `unlockWrite(stamp)` — release the write lock, advance the version.
///
/// Returns `false` when `stamp` does not name this lock's current write
/// hold; the caller must then raise `IllegalMonitorStateException`. The JDK
/// guard is `if (state != stamp || (stamp & WBIT) == 0L) throw`, i.e. an
/// EXACT state-word match — so a stale write stamp cannot release a later
/// writer's hold. This used to ignore the stamp entirely and release
/// whatever was held, which silently broke mutual exclusion.
pub fn stamped_unlock_write(addr: usize, stamp: i64) -> bool {
    let slot = stamped_slot(addr);
    {
        let mut state = slot.state.lock();
        if !is_write_stamp(stamp) || !state.write_held || state.state_word() != stamp {
            return false;
        }
        state.release_write();
    }
    // Wake any reader/writer queued behind us.
    slot.cv.notify_all();
    true
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
///
/// Returns `false` when `stamp` does not name an outstanding read hold; the
/// caller must then raise `IllegalMonitorStateException`. The JDK's guard is
/// `(stamp & RBITS) != 0` plus `(state & SBITS) == (stamp & SBITS)` plus
/// `(state & RBITS) != 0` — note it compares only the version, NOT the
/// reader count, because concurrent readers legitimately hold stamps with
/// different counts.
pub fn stamped_unlock_read(addr: usize, stamp: i64) -> bool {
    let slot = stamped_slot(addr);
    let drained_to_zero;
    {
        let mut state = slot.state.lock();
        if !is_read_stamp(stamp)
            || (stamp & SBITS) != (state.state_word() & SBITS)
            || state.readers <= 0
        {
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
/// that merely *reads* it (`toString`, `getReadLockCount(long)`, a fallen-
/// through `validate`) sees something coherent. Since 2026-08-07 the stamps
/// handed out are cut from this same word, so the two can no longer disagree.
pub fn stamped_visible_state(addr: usize) -> i64 {
    let slot = stamped_slot(addr);
    let state = slot.state.lock();
    state.state_word()
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
    if is_write_stamp(stamp) {
        return if state.write_held {
            state.write_stamp()
        } else {
            0
        };
    }
    if is_read_stamp(stamp) {
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
        if is_write_stamp(stamp) {
            if !state.write_held {
                return 0;
            }
            // Advance the version (write released) and immediately take a read.
            state.release_write();
            state.readers += 1;
            new_stamp = state.read_stamp();
            notify = true;
        } else if is_read_stamp(stamp) {
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
        if is_write_stamp(stamp) {
            if !state.write_held {
                return 0;
            }
            state.release_write();
            version = state.version;
            notify = true;
        } else if is_read_stamp(stamp) {
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
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
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

    /// The regression this whole module was re-encoded for (2026-08-07).
    ///
    /// Measured on HotSpot 25.0.3 with `SLBits.java`:
    ///     first writeLock() = 384, & 255 = 128
    ///     isWriteLockStamp -> true, isReadLockStamp -> false
    ///     first readLock()  & 255 = 1  (the reader COUNT, not a flag)
    ///     tryOptimisticRead & 255 = 0
    ///
    /// `isWriteLockStamp` and friends are unregistered `public static`
    /// methods, so REAL JDK BYTECODE decodes these values. If this test ever
    /// goes red the canonical release idiom takes the wrong branch again and
    /// write holds leak.
    #[test]
    fn stamped_write_stamp_uses_the_real_jdk_bit_layout() {
        let a = fresh_addr();
        stamped_init(a);
        let s = stamped_write_lock(a);
        assert_eq!(s, STAMPED_ORIGIN | WBIT, "first write stamp must be 384");
        assert_eq!(s & ABITS, WBIT, "write stamp mode field must be WBIT");
        assert!(is_write_stamp(s), "JDK isWriteLockStamp must say write");
        assert!(!is_read_stamp(s), "JDK isReadLockStamp must NOT say read");
        assert!(!is_optimistic_stamp(s));
        assert!(stamped_unlock_write(a, s));
    }

    #[test]
    fn stamped_read_and_optimistic_stamps_use_the_real_jdk_bit_layout() {
        let a = fresh_addr();
        stamped_init(a);
        let o = stamped_try_optimistic_read(a);
        assert_eq!(o, STAMPED_ORIGIN);
        assert_eq!(o & ABITS, 0, "optimistic stamp mode field must be 0");
        assert!(is_optimistic_stamp(o));
        assert!(!is_read_stamp(o) && !is_write_stamp(o));

        // Two concurrent read holds get DIFFERENT stamps whose low bits are
        // the reader count, exactly like the JDK (513, 514, ...).
        let r1 = stamped_read_lock(a);
        let r2 = stamped_read_lock(a);
        assert_eq!(r1 & ABITS, 1);
        assert_eq!(r2 & ABITS, 2);
        assert_eq!(r2 - r1, RUNIT);
        for r in [r1, r2] {
            assert!(is_read_stamp(r));
            assert!(!is_write_stamp(r));
            assert!(!is_optimistic_stamp(r));
        }
        assert!(stamped_unlock_read(a, r2));
        assert!(stamped_unlock_read(a, r1));
    }

    #[test]
    fn stamped_unlock_write_advances_stamp() {
        let a = fresh_addr();
        stamped_init(a);
        let s0 = stamped_try_optimistic_read(a);
        let w = stamped_write_lock(a);
        assert!(stamped_unlock_write(a, w));
        let s1 = stamped_try_optimistic_read(a);
        assert!(s1 > s0, "stamp must advance: {} -> {}", s0, s1);
        // HotSpot advances the version by exactly one step per write release.
        assert_eq!(s1 - s0, VERSION_STEP);
        assert_eq!(s1 & ABITS, 0, "post-unlock stamp must be optimistic");
    }

    /// A stale or foreign write stamp must NOT release a live write hold —
    /// the JDK's guard is an exact state-word match. Before 2026-08-07
    /// `unlockWrite` ignored its stamp entirely.
    #[test]
    fn stamped_unlock_write_rejects_a_stale_stamp() {
        let a = fresh_addr();
        stamped_init(a);
        let w0 = stamped_write_lock(a);
        assert!(stamped_unlock_write(a, w0));
        // Same stamp, second time: the version has moved on.
        assert!(!stamped_unlock_write(a, w0));
        // A live hold taken by "someone else" is not releasable with w0.
        let w1 = stamped_write_lock(a);
        assert!(!stamped_unlock_write(a, w0));
        assert!(stamped_is_write_locked(a), "stale stamp must not release");
        // Nor with a read or optimistic stamp.
        assert!(!stamped_unlock_write(a, w1 & VERSION_MASK));
        assert!(!stamped_unlock_write(a, 0));
        assert!(stamped_unlock_write(a, w1));
    }

    #[test]
    fn stamped_unlock_read_rejects_a_non_read_stamp() {
        let a = fresh_addr();
        stamped_init(a);
        let r = stamped_read_lock(a);
        assert!(!stamped_unlock_read(a, 0), "zero is never a read stamp");
        assert!(
            !stamped_unlock_read(a, r & VERSION_MASK),
            "an optimistic stamp holds nothing"
        );
        // WBIT is inside SBITS, so setting it makes the stamp name a
        // different lock state — the JDK rejects it and so must we.
        assert!(
            !stamped_unlock_read(a, r | WBIT),
            "SBITS mismatch must be rejected"
        );
        assert_eq!(stamped_get_read_lock_count(a), 1);
        assert!(stamped_unlock_read(a, r));
        assert!(!stamped_unlock_read(a, r), "no hold left to release");
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
        let w = stamped_write_lock(a);
        assert!(stamped_unlock_write(a, w));
        assert!(!stamped_validate(a, s));
    }

    #[test]
    fn stamped_try_write_blocked_by_reader() {
        let a = fresh_addr();
        stamped_init(a);
        let r = stamped_read_lock(a);
        assert_eq!(stamped_try_write_lock(a), 0);
        assert!(stamped_unlock_read(a, r));
    }

    #[test]
    fn stamped_try_read_blocked_by_writer() {
        let a = fresh_addr();
        stamped_init(a);
        let w = stamped_write_lock(a);
        assert_eq!(stamped_try_read_lock(a), 0);
        assert!(stamped_unlock_write(a, w));
    }

    #[test]
    fn stamped_convert_to_write_succeeds_with_one_reader() {
        let a = fresh_addr();
        stamped_init(a);
        let r = stamped_read_lock(a);
        let w = stamped_try_convert_to_write(a, r);
        assert!(w != 0);
        assert!(is_write_stamp(w));
        assert!(stamped_is_write_locked(a));
        assert_eq!(stamped_get_read_lock_count(a), 0);
        assert!(stamped_unlock_write(a, w));
    }

    #[test]
    fn stamped_convert_to_write_fails_with_two_readers() {
        let a = fresh_addr();
        stamped_init(a);
        let r1 = stamped_read_lock(a);
        let r2 = stamped_read_lock(a);
        assert_eq!(stamped_try_convert_to_write(a, r1), 0);
        assert!(stamped_unlock_read(a, r1));
        assert!(stamped_unlock_read(a, r2));
    }

    #[test]
    fn stamped_writer_blocks_then_releases() {
        // Cross-thread test: take write, spawn reader (blocks), release
        // write, reader makes progress.
        let a = fresh_addr();
        stamped_init(a);
        let w = stamped_write_lock(a);
        let join = std::thread::spawn(move || {
            let s = stamped_read_lock(a);
            assert!(stamped_unlock_read(a, s));
            s
        });
        // give the reader a moment to be queued
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(stamped_unlock_write(a, w));
        let stamp = join.join().unwrap();
        // The reader's stamp is a read stamp at the POST-release version.
        assert!(is_read_stamp(stamp));
        assert_eq!(stamp & VERSION_MASK, (w & VERSION_MASK) + VERSION_STEP);
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
                    // A writer cannot advance the version while a read hold
                    // is outstanding, so this stamp is still current.
                    assert!(stamped_unlock_read(a, s), "read stamp went stale");
                }
            }));
        }
        // Writer churns the lock alongside the readers.
        for _ in 0..50 {
            let w = stamped_write_lock(a);
            assert!(stamped_unlock_write(a, w));
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
        assert!(is_read_stamp(r));
        assert!(stamped_unlock_read(a, r));
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
        let w = stamped_try_write_lock(a);
        assert!(w != 0);
        assert!(stamped_unlock_write(a, w));
    }

    #[test]
    fn stamped_convert_to_optimistic_on_stale_stamp_is_zero() {
        let a = fresh_addr();
        stamped_init(a);
        let s = stamped_try_optimistic_read(a);
        let w = stamped_write_lock(a);
        assert!(stamped_unlock_write(a, w));
        assert_eq!(stamped_try_convert_to_optimistic(a, s), 0);
        assert_eq!(stamped_try_convert_to_optimistic(a, 0), 0);
    }

    #[test]
    fn stamped_read_and_optimistic_stamps_are_distinguishable() {
        let a = fresh_addr();
        stamped_init(a);
        let opt = stamped_try_optimistic_read(a);
        let rd = stamped_read_lock(a);
        assert_ne!(
            opt, rd,
            "an optimistic stamp must not look like a read stamp"
        );
        // Converting the OPTIMISTIC stamp must not steal the reader's hold.
        assert!(stamped_try_convert_to_optimistic(a, opt) != 0);
        assert_eq!(stamped_get_read_lock_count(a), 1);
        assert!(stamped_unlock_read(a, rd));
    }

    #[test]
    fn stamped_timed_try_write_lock_times_out_under_a_reader() {
        let a = fresh_addr();
        stamped_init(a);
        let r = stamped_read_lock(a);
        let t0 = std::time::Instant::now();
        assert_eq!(stamped_try_write_lock_timed(a, 20_000_000), 0);
        assert!(t0.elapsed() >= std::time::Duration::from_millis(15));
        assert!(stamped_unlock_read(a, r));
        let w = stamped_try_write_lock_timed(a, 20_000_000);
        assert!(w != 0);
        assert!(stamped_unlock_write(a, w));
    }

    #[test]
    fn stamped_convert_to_read_downgrades_a_write_hold() {
        let a = fresh_addr();
        stamped_init(a);
        let w = stamped_write_lock(a);
        let r = stamped_try_convert_to_read(a, w);
        assert!(r != 0);
        assert!(is_read_stamp(r));
        assert!(!stamped_is_write_locked(a));
        assert_eq!(stamped_get_read_lock_count(a), 1);
        assert!(stamped_unlock_read(a, r));
    }

    #[test]
    fn stamped_is_locked_covers_both_modes() {
        let a = fresh_addr();
        stamped_init(a);
        assert!(!stamped_is_locked(a));
        let r = stamped_read_lock(a);
        assert!(stamped_is_locked(a));
        assert!(stamped_unlock_read(a, r));
        let w = stamped_write_lock(a);
        assert!(stamped_is_locked(a));
        assert!(stamped_unlock_write(a, w));
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
