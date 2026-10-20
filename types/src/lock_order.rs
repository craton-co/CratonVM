// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lock ordering enforcement framework.
//!
//! **This module is the canonical, in-source definition of CratonVM's global
//! lock acquisition order.** There is no separate `docs/lock-order.md`; the
//! [`LockLevel`] enum below (with its doc comments) *is* the authoritative
//! hierarchy, and the wrappers in this module are its runtime enforcement. When
//! a level is added, removed, or renumbered, update [`LockLevel`] and the
//! mapping in [`tracking::level_from_u8`] together — they are the single source
//! of truth, so there is no external document to keep in sync.
//!
//! ## The global lock acquisition order
//!
//! Locks are numbered L0 (lowest) through L10 (highest). The full hierarchy,
//! highest-first (acquired-first), is:
//!
//! | Level | Name | Lock |
//! |------:|------|------|
//! | L10 | `class_manager`   | `SharedVm::class_manager` (RwLock) — acquired first |
//! | L9  | `native_methods`  | *reserved* — `SharedVm::native_methods` holds no lock today |
//! | L8  | `heap`            | `SharedVm::heap` interior locks (in the `gc` crate) |
//! | L7  | `ref_processor`   | `SharedVm::ref_processor` (Mutex) |
//! | L6  | `monitors`        | per-object monitor registry (`MonitorTable`) |
//! | L5  | `thread_registry` | `SharedVm::thread_registry` (Mutex) |
//! | L4  | `flight_recorder` | `SharedVm::flight_recorder` (Mutex) |
//! | L3  | `cleaner_actions` | `cleaner_thread.pending_actions` (Mutex) |
//! | L2  | `native_memory`   | `SharedVm::native_memory` (Mutex) |
//! | L1  | `jvm_thread`       | `JvmThread`-local state (owning thread only) |
//! | L0  | `scratch`         | per-call scratch collections — released first |
//!
//! The integer discriminants of [`LockLevel`] match this table exactly.
//!
//! ## The rule
//!
//! The hierarchy is **descending**: a thread holding a lock at level N may
//! *only* acquire a lock at a level **strictly less than N** (i.e. a lower
//! number). Equivalently: lower number == acquired *later* and released
//! *first*; higher number == acquired *earlier* and held longer.
//!
//! Example: a thread holding `class_manager` (L10) may then acquire `heap`
//! (L8), then `monitors` (L6), then `thread_registry` (L5) — the levels
//! descend monotonically. A thread holding `monitors` (L6) must **not** acquire
//! `class_manager` (L10) or `heap` (L8); it has to release the monitor first.
//!
//! ## Enforcement strategy
//!
//! Each [`OrderedMutex`] / [`OrderedRwLock`] carries a [`LockLevel`]. When
//! enforcement is active we maintain a per-thread bit-set of currently-held
//! levels. On every acquire we assert that the attempted level is strictly
//! less than the *minimum* currently-held level — the binding constraint for
//! "descending".
//!
//! Enforcement is **always on in debug builds**. In release builds it is
//! **off by default** (a single cached `AtomicU8` load gates the whole fast
//! path, so the wrapper is effectively zero-cost) but can be **opted in at
//! runtime** by setting the environment variable `CRATONVM_LOCK_ORDER_CHECK`
//! to a truthy value (`1`, `true`, `yes`, or `on`, case-insensitive). This
//! lets production builds turn on deadlock-ordering checks without a rebuild,
//! e.g. when reproducing a suspected lock-order bug in the field. The env var
//! is read once and cached; flipping it after the first lock acquisition has
//! no effect. See [`tracking::enforced`].
//!
//! ## Usage
//!
//! Two wrapper families implement the enforcement, differing only in the
//! backing primitive and hence in guard shape:
//!
//! - [`OrderedMutex`] / [`OrderedRwLock`] wrap `std::sync` and therefore return
//!   a `LockResult` (std locks are poisonable).
//! - [`OrderedPlMutex`] / [`OrderedPlRwLock`] wrap `parking_lot` and are
//!   **drop-in replacements** for `parking_lot::Mutex` / `parking_lot::RwLock`:
//!   `.lock()` / `.read()` / `.write()` / `.read_recursive()` return the guard
//!   directly. This is what makes converting a hot VM-wide lock with hundreds
//!   of call sites a one-line change at the declaration.
//!
//! Both families drive the *same* per-thread held-level tracker, so an
//! inversion between them (e.g. an L6 monitor held across an L10
//! `class_manager` acquisition) is detected.
//!
//! For a lock whose type genuinely cannot change — one defined in a crate that
//! cannot see [`LockLevel`], or whose mutual exclusion is spread across many
//! fine-grained inner locks — [`enter_level`] returns a [`LevelScope`] that
//! records the level for the duration of the region without owning a lock.
//!
//! ### Runtime enforcement status
//!
//! Authoritative, non-aspirational list of which locks are checked vs still
//! raw:
//! - **Wired (checked):**
//!   - L10 `class_manager` — `SharedVm::class_manager` is an
//!     [`OrderedPlRwLock`]; every `.read()` / `.write()` / `.read_recursive()`
//!     call site in the workspace is checked.
//!   - L7 `ref_processor` — `SharedVm::ref_processor` is an
//!     [`OrderedPlMutex`].
//!   - L6 `monitors` — both `MonitorTable` maps
//!     ([`crate::threading::monitor::MonitorTable`]'s `monitors` and
//!     `cas_locks`) are [`OrderedMutex`].
//! - **Not wired:**
//!   - L9 `native_methods` — the level exists in the hierarchy, but there is
//!     currently **no lock instance to wrap**: `SharedVm::native_methods` is a
//!     bare `NativeMethodRegistry`, immutable after construction (all ~3,100
//!     registrations happen during `SharedVm::new`, before the VM is shared).
//!     The table row is retained so that if the registry ever becomes mutable
//!     at runtime its lock lands at L9, between `class_manager` and `heap`.
//!   - L8 `heap` — `VmHeap` and its interior locks are defined in the separate
//!     `cratonvm-gc` crate, which cannot depend on `vm::runtime::lock_order`
//!     without inverting the existing `vm -> gc` dependency edge. Call sites in
//!     `vm` that hold heap locks across other acquisitions can announce the
//!     level with [`enter_level`]; moving [`LockLevel`] down into
//!     `cratonvm-types` (visible to both crates) is the clean fix and is not
//!     attempted here.
//!   - L5..L0 are documented for ordering purposes and are not wrapped.

use std::fmt;
use std::sync::{LockResult, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

// ---------------------------------------------------------------------------
// LockLevel
// ---------------------------------------------------------------------------

/// Ordered lock levels.
///
/// These variants, and the table in this module's doc comment, *are* the
/// canonical global lock acquisition order — the integer discriminants are the
/// single source of truth. A thread holding a lock at level N may only acquire
/// locks at level **strictly less than N** (descending order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LockLevel {
    /// L0 — per-call scratch (`Vec`s, `HashMap`s built inside a single call).
    Scratch = 0,
    /// L1 — `JvmThread`-local state (owning thread only; never contended).
    JvmThread = 1,
    /// L2 — `SharedVm::native_memory` (NativeMemoryTable Mutex).
    NativeMemory = 2,
    /// L3 — `SharedVm::cleaner_thread.pending_actions` (Mutex).
    CleanerActions = 3,
    /// L4 — `SharedVm::flight_recorder` (Mutex).
    FlightRecorder = 4,
    /// L5 — `SharedVm::thread_registry` (Mutex).
    ThreadRegistry = 5,
    /// L6 — `SharedVm::monitors` (per-object Monitors via `Arc<Monitor>`).
    Monitors = 6,
    /// L7 — `SharedVm::ref_processor` (Mutex).
    RefProcessor = 7,
    /// L8 — `SharedVm::heap` interior locks.
    Heap = 8,
    /// L9 — reserved for `SharedVm::native_methods`. The registry is
    /// currently immutable after construction and holds no lock; the level
    /// is kept so a future runtime-mutable registry lands here, between
    /// `class_manager` and `heap`. See this module's enforcement-status list.
    NativeMethods = 9,
    /// L10 — `SharedVm::class_manager` (RwLock). Highest level / acquired first.
    ClassManager = 10,
}

impl LockLevel {
    /// One past the highest discriminant; size of the per-thread tracking
    /// bit-array.
    const COUNT: usize = 11;
}

impl fmt::Display for LockLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error returned when a thread attempts to acquire a lock out of order.
///
/// `held` is the *binding* (minimum) currently-held level — the one the
/// `attempted` level needed to be strictly less than.
#[derive(Debug, Clone)]
pub struct LockOrderViolation {
    /// The level the thread attempted to acquire.
    pub attempted: LockLevel,
    /// The minimum-level lock the thread currently holds. The attempt was
    /// rejected because `attempted >= held` (descending order requires
    /// `attempted < held`).
    pub held: LockLevel,
}

impl fmt::Display for LockOrderViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lock order violation: attempted to acquire {:?} (level {}) while holding {:?} (level {}); descending order requires attempted < held",
            self.attempted, self.attempted as u8, self.held, self.held as u8
        )
    }
}

impl std::error::Error for LockOrderViolation {}

// ---------------------------------------------------------------------------
// Thread-local tracking
// ---------------------------------------------------------------------------

pub mod tracking {
    use super::LockLevel;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU8, Ordering};

    // Cached enforcement decision. 0 = undetermined, 1 = off, 2 = on. Reading
    // it is a single relaxed atomic load on the lock fast path, so when
    // enforcement is off the per-acquire cost is negligible.
    static ENFORCE: AtomicU8 = AtomicU8::new(0);

    /// Whether lock-order enforcement is active for this process.
    ///
    /// In debug builds this is always `true`. In release builds it defaults to
    /// `false` but can be opted in at runtime by setting the environment
    /// variable `CRATONVM_LOCK_ORDER_CHECK` to a truthy value (`1`, `true`,
    /// `yes`, or `on`, case-insensitive). The decision is computed on the first
    /// call and cached; later changes to the environment have no effect.
    #[inline]
    pub fn enforced() -> bool {
        match ENFORCE.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = compute_enforced();
                // Idempotent: every caller computes the same value, so a racing
                // store is harmless. Use a plain store rather than CAS.
                ENFORCE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// Turn enforcement on for the rest of this process, unconditionally.
    ///
    /// Exists for **test binaries only**. `cargo test --release` builds the
    /// suite with `debug_assertions` off, so [`enforced`] defaults to `false`
    /// and every `#[should_panic(expected = "lock order violation")]` test in
    /// `vm/src/runtime/lock_order.rs` stopped panicking and failed — 18
    /// failures that looked like real regressions in every `--release` test
    /// run and had to be re-triaged as noise each time. Calling this at the
    /// top of those tests makes a release run exercise the same code path a
    /// debug run does.
    ///
    /// It can only ever *enable* checking (never disable it), and the check
    /// itself is a pure assertion over a per-thread bit-set, so flipping it on
    /// mid-process cannot introduce a false positive: a lock acquired before
    /// the flip simply was not recorded, which can only cause a violation to
    /// be *missed*, not invented.
    #[doc(hidden)]
    pub fn force_enable_for_testing() {
        ENFORCE.store(2, Ordering::Relaxed);
    }

    fn compute_enforced() -> bool {
        // Always enforce in debug builds; the env opt-in is for release.
        if cfg!(debug_assertions) {
            return true;
        }
        match crate::flags::runtime_var("CRATONVM_LOCK_ORDER_CHECK") {
            Ok(v) => {
                let v = v.trim();
                v.eq_ignore_ascii_case("1")
                    || v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("on")
            }
            Err(_) => false,
        }
    }

    // Bit-set of currently held lock levels for this thread.
    // Index `i` corresponds to `LockLevel` with discriminant `i`.
    thread_local! {
        static HELD: Cell<[bool; LockLevel::COUNT]> = const { Cell::new([false; LockLevel::COUNT]) };
    }

    /// Returns the *lowest* currently-held level, if any. In descending
    /// order, this is the binding constraint: the next lock acquired must be
    /// at a level strictly less than this.
    pub fn lowest_held() -> Option<LockLevel> {
        HELD.with(|cell| {
            let arr = cell.get();
            for i in 0..LockLevel::COUNT {
                if arr[i] {
                    return Some(level_from_u8(i as u8));
                }
            }
            None
        })
    }

    /// Mark a level as held by this thread.
    pub fn acquire(level: LockLevel) {
        HELD.with(|cell| {
            let mut arr = cell.get();
            arr[level as u8 as usize] = true;
            cell.set(arr);
        });
    }

    /// Mark a level as released by this thread.
    pub fn release(level: LockLevel) {
        HELD.with(|cell| {
            let mut arr = cell.get();
            arr[level as u8 as usize] = false;
            cell.set(arr);
        });
    }

    fn level_from_u8(v: u8) -> LockLevel {
        match v {
            0 => LockLevel::Scratch,
            1 => LockLevel::JvmThread,
            2 => LockLevel::NativeMemory,
            3 => LockLevel::CleanerActions,
            4 => LockLevel::FlightRecorder,
            5 => LockLevel::ThreadRegistry,
            6 => LockLevel::Monitors,
            7 => LockLevel::RefProcessor,
            8 => LockLevel::Heap,
            9 => LockLevel::NativeMethods,
            10 => LockLevel::ClassManager,
            _ => unreachable!(),
        }
    }

    /// Check the descending-order invariant for `level` against the locks
    /// currently held by this thread, then record `level` as held.
    ///
    /// No-op (and skips touching the thread-local) when enforcement is off, so
    /// the only cost on the disabled fast path is the cached [`enforced`] load.
    ///
    /// # Panics
    ///
    /// Panics with a [`super::LockOrderViolation`] message if `level` is not
    /// strictly less than the minimum currently-held level.
    #[inline]
    pub fn check_and_acquire(level: LockLevel) {
        if !enforced() {
            return;
        }
        if let Some(held) = lowest_held() {
            assert!(
                level < held,
                "{}",
                super::LockOrderViolation {
                    attempted: level,
                    held,
                }
            );
        }
        acquire(level);
    }

    /// Mirror of [`check_and_acquire`] for guard drop: releases `level` iff
    /// enforcement is active. Must be paired with a `check_and_acquire(level)`
    /// — `enforced()` is cached, so both observe the same decision.
    #[inline]
    pub fn release_if_enforced(level: LockLevel) {
        if enforced() {
            release(level);
        }
    }

    /// Whether this thread currently holds a lock at `level`.
    #[inline]
    pub fn is_held(level: LockLevel) -> bool {
        HELD.with(|cell| cell.get()[level as u8 as usize])
    }

    /// Like [`check_and_acquire`], but reports whether the level was actually
    /// recorded so the caller's guard can decide whether to release it on drop.
    ///
    /// Returns `false` (and does nothing) when enforcement is off, `true` after
    /// a successful check-and-record.
    ///
    /// # Panics
    ///
    /// Panics with a [`super::LockOrderViolation`] message if `level` is not
    /// strictly less than the minimum currently-held level.
    #[inline]
    pub fn check_and_acquire_tracked(level: LockLevel) -> bool {
        if !enforced() {
            return false;
        }
        if let Some(held) = lowest_held() {
            assert!(
                level < held,
                "{}",
                super::LockOrderViolation {
                    attempted: level,
                    held,
                }
            );
        }
        acquire(level);
        true
    }

    /// Reentrant variant of [`check_and_acquire_tracked`] for deliberately
    /// recursive acquisitions (`parking_lot::RwLock::read_recursive`).
    ///
    /// Re-entering a lock this thread already holds introduces no new edge in
    /// the wait-for graph, so when `level` is already recorded we neither check
    /// nor re-record (returning `false`, so the guard leaves the outer
    /// acquisition's record intact). When `level` is *not* held this behaves
    /// exactly like [`check_and_acquire_tracked`].
    ///
    /// # Panics
    ///
    /// Panics on a descending-order violation when `level` is not already held.
    #[inline]
    pub fn check_and_acquire_reentrant(level: LockLevel) -> bool {
        if !enforced() {
            return false;
        }
        if is_held(level) {
            return false;
        }
        if let Some(held) = lowest_held() {
            assert!(
                level < held,
                "{}",
                super::LockOrderViolation {
                    attempted: level,
                    held,
                }
            );
        }
        acquire(level);
        true
    }
}

// ---------------------------------------------------------------------------
// OrderedMutex<T>
// ---------------------------------------------------------------------------

/// A `Mutex<T>` wrapper that enforces lock-ordering discipline.
///
/// When enforcement is active, acquiring this lock asserts that the calling
/// thread holds no lock at an equal or **lower** [`LockLevel`] (the
/// descending-order rule documented at the top of this module). Enforcement is
/// always on in debug builds and opt-in at runtime in release builds via
/// `CRATONVM_LOCK_ORDER_CHECK`; when off, the only per-acquire cost is a single
/// cached atomic load (see [`tracking::enforced`]).
pub struct OrderedMutex<T> {
    inner: Mutex<T>,
    level: LockLevel,
}

// Manual impls because Mutex<T> is Send+Sync but we store a LockLevel too.
unsafe impl<T: Send> Send for OrderedMutex<T> {}
unsafe impl<T: Send> Sync for OrderedMutex<T> {}

impl<T> OrderedMutex<T> {
    /// Create a new ordered mutex at the given lock level.
    pub fn new(value: T, level: LockLevel) -> Self {
        Self {
            inner: Mutex::new(value),
            level,
        }
    }

    /// Acquire the mutex, enforcing descending lock order when enforcement is
    /// active (always in debug builds; in release when
    /// `CRATONVM_LOCK_ORDER_CHECK` is set — see this module's docs).
    ///
    /// # Panics
    ///
    /// When enforcement is active, panics if the calling thread already holds a
    /// lock at an equal or *lower* level (which would invert the documented
    /// descending hierarchy and risk deadlock).
    pub fn lock(&self) -> LockResult<OrderedMutexGuard<'_, T>> {
        tracking::check_and_acquire(self.level);

        match self.inner.lock() {
            Ok(guard) => Ok(OrderedMutexGuard {
                guard,
                level: self.level,
            }),
            Err(poison) => {
                // Even on poison we acquired the OS lock, so tracking is correct.
                let guard = OrderedMutexGuard {
                    guard: poison.into_inner(),
                    level: self.level,
                };
                Err(std::sync::PoisonError::new(guard))
            }
        }
    }

    /// Returns the lock level of this mutex.
    pub fn level(&self) -> LockLevel {
        self.level
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedMutex")
            .field("level", &self.level)
            .field("inner", &self.inner)
            .finish()
    }
}

/// RAII guard for [`OrderedMutex`]. Releases the lock level on drop in debug
/// builds.
pub struct OrderedMutexGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    level: LockLevel,
}

impl<T> std::ops::Deref for OrderedMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for OrderedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for OrderedMutexGuard<'_, T> {
    fn drop(&mut self) {
        tracking::release_if_enforced(self.level);
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedMutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

// ---------------------------------------------------------------------------
// OrderedRwLock<T>
// ---------------------------------------------------------------------------

/// An `RwLock<T>` wrapper with the same descending lock-ordering enforcement
/// as [`OrderedMutex`].
pub struct OrderedRwLock<T> {
    inner: RwLock<T>,
    level: LockLevel,
}

unsafe impl<T: Send> Send for OrderedRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for OrderedRwLock<T> {}

impl<T> OrderedRwLock<T> {
    /// Create a new ordered read-write lock at the given level.
    pub fn new(value: T, level: LockLevel) -> Self {
        Self {
            inner: RwLock::new(value),
            level,
        }
    }

    /// Acquire the lock for reading.
    ///
    /// # Panics
    ///
    /// When enforcement is active, panics on lock-order violation (see
    /// [`OrderedMutex::lock`]).
    pub fn read(&self) -> LockResult<OrderedRwLockReadGuard<'_, T>> {
        tracking::check_and_acquire(self.level);

        match self.inner.read() {
            Ok(guard) => Ok(OrderedRwLockReadGuard {
                guard,
                level: self.level,
            }),
            Err(poison) => {
                let guard = OrderedRwLockReadGuard {
                    guard: poison.into_inner(),
                    level: self.level,
                };
                Err(std::sync::PoisonError::new(guard))
            }
        }
    }

    /// Acquire the lock for writing.
    ///
    /// # Panics
    ///
    /// When enforcement is active, panics on lock-order violation (see
    /// [`OrderedMutex::lock`]).
    pub fn write(&self) -> LockResult<OrderedRwLockWriteGuard<'_, T>> {
        tracking::check_and_acquire(self.level);

        match self.inner.write() {
            Ok(guard) => Ok(OrderedRwLockWriteGuard {
                guard,
                level: self.level,
            }),
            Err(poison) => {
                let guard = OrderedRwLockWriteGuard {
                    guard: poison.into_inner(),
                    level: self.level,
                };
                Err(std::sync::PoisonError::new(guard))
            }
        }
    }

    /// Returns the lock level of this rwlock.
    pub fn level(&self) -> LockLevel {
        self.level
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedRwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedRwLock")
            .field("level", &self.level)
            .field("inner", &self.inner)
            .finish()
    }
}

/// RAII read guard for [`OrderedRwLock`].
pub struct OrderedRwLockReadGuard<'a, T> {
    guard: RwLockReadGuard<'a, T>,
    level: LockLevel,
}

impl<T> std::ops::Deref for OrderedRwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> Drop for OrderedRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        tracking::release_if_enforced(self.level);
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedRwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

/// RAII write guard for [`OrderedRwLock`].
pub struct OrderedRwLockWriteGuard<'a, T> {
    guard: RwLockWriteGuard<'a, T>,
    level: LockLevel,
}

impl<T> std::ops::Deref for OrderedRwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for OrderedRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for OrderedRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        tracking::release_if_enforced(self.level);
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedRwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

// ---------------------------------------------------------------------------
// Enforcement query
// ---------------------------------------------------------------------------

/// Whether lock-order enforcement is active in this process.
///
/// Always `true` in debug builds. In release builds it is `true` only when
/// `CRATONVM_LOCK_ORDER_CHECK` was set to a truthy value (`1`, `true`, `yes`,
/// `on`) before the first lock acquisition — the decision is computed once and
/// cached, so flipping the variable later has no effect.
///
/// Exposed so tests and diagnostics can tell "the ordering assertion held" from
/// "the ordering assertion was never evaluated".
#[inline]
pub fn enforcement_active() -> bool {
    tracking::enforced()
}

// ---------------------------------------------------------------------------
// Level scopes (for locks whose *type* cannot be changed)
// ---------------------------------------------------------------------------

/// RAII scope that records "this thread holds a lock at `level`" in the
/// per-thread order tracker without owning any lock itself.
///
/// This is the escape hatch for locks whose type cannot be changed — either
/// because they live in a crate that cannot see [`LockLevel`] (the `gc` crate's
/// heap interior locks, see this module's docs) or because the mutual exclusion
/// is spread over many fine-grained inner locks (the monitor table). Wrapping
/// the lock itself in [`OrderedMutex`] / [`OrderedPlRwLock`] is always
/// preferable: a scope is only as accurate as the discipline of its callers.
#[must_use = "a LevelScope must be held for the whole locked region"]
pub struct LevelScope {
    level: LockLevel,
    /// Whether this scope actually recorded `level`. False when enforcement is
    /// off, so `Drop` never clears a level it did not set.
    tracked: bool,
}

impl LevelScope {
    /// The level recorded by this scope.
    pub fn level(&self) -> LockLevel {
        self.level
    }
}

impl Drop for LevelScope {
    #[inline]
    fn drop(&mut self) {
        if self.tracked {
            tracking::release(self.level);
        }
    }
}

impl fmt::Debug for LevelScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LevelScope")
            .field("level", &self.level)
            .field("tracked", &self.tracked)
            .finish()
    }
}

/// Enter a [`LevelScope`] at `level`, enforcing the descending-order rule.
///
/// # Panics
///
/// When enforcement is active, panics if the calling thread already holds a
/// lock at an equal or *lower* level.
#[inline]
pub fn enter_level(level: LockLevel) -> LevelScope {
    LevelScope {
        level,
        tracked: tracking::check_and_acquire_tracked(level),
    }
}

// ---------------------------------------------------------------------------
// parking_lot-backed wrappers (drop-in)
// ---------------------------------------------------------------------------
//
// DESIGN NOTE — why a second family of wrappers.
//
// `OrderedMutex`/`OrderedRwLock` above wrap `std::sync`, whose guards come
// wrapped in a `LockResult` because std locks are poisonable. CratonVM's hot
// VM-wide locks (`class_manager`, `ref_processor`, …) are `parking_lot` locks,
// whose `.read()`/`.write()`/`.lock()` return the guard *directly*.
//
// Converting `SharedVm::class_manager` to the std-backed `OrderedRwLock` would
// therefore mean adding `.unwrap()` (or `.expect(..)`) to ~615 call sites
// across `vm`, `vm-cli` and `libcratonvm` — a mechanical but enormous diff that
// buries the actual change, and it would also silently swap parking_lot's
// fairness and `read_recursive` behaviour for std's. So the types below are
// instead **API-compatible drop-in replacements for `parking_lot::Mutex` /
// `parking_lot::RwLock`**: same method names, same "guard returned directly"
// shape, `Deref`/`DerefMut` to the protected value. Converting a lock is then a
// one-line change at the field declaration plus one at the constructor, and
// every call site compiles unchanged.

/// A `parking_lot::Mutex<T>` wrapper that enforces lock-ordering discipline.
///
/// Drop-in for `parking_lot::Mutex`: [`lock`](Self::lock) returns the guard
/// directly (no `LockResult`). The ordering check happens inside `lock`, gated
/// on [`tracking::enforced`] — always on in debug builds, opt-in in release via
/// `CRATONVM_LOCK_ORDER_CHECK`. When off, the added cost is one cached relaxed
/// atomic load per acquire.
pub struct OrderedPlMutex<T> {
    inner: parking_lot::Mutex<T>,
    level: LockLevel,
}

impl<T> OrderedPlMutex<T> {
    /// Create a new ordered mutex at `level`.
    pub const fn new(value: T, level: LockLevel) -> Self {
        Self {
            inner: parking_lot::Mutex::new(value),
            level,
        }
    }

    /// The lock level of this mutex.
    #[inline]
    pub fn level(&self) -> LockLevel {
        self.level
    }

    /// Acquire the mutex.
    ///
    /// # Panics
    ///
    /// When enforcement is active, panics if the calling thread already holds a
    /// lock at an equal or *lower* [`LockLevel`].
    #[inline]
    pub fn lock(&self) -> OrderedPlMutexGuard<'_, T> {
        let tracked = tracking::check_and_acquire_tracked(self.level);
        OrderedPlMutexGuard {
            guard: self.inner.lock(),
            level: self.level,
            tracked,
        }
    }

    /// Try to acquire the mutex without blocking. The order check runs (and can
    /// panic) only when the underlying `try_lock` actually succeeded.
    #[inline]
    pub fn try_lock(&self) -> Option<OrderedPlMutexGuard<'_, T>> {
        let guard = self.inner.try_lock()?;
        let tracked = tracking::check_and_acquire_tracked(self.level);
        Some(OrderedPlMutexGuard {
            guard,
            level: self.level,
            tracked,
        })
    }

    /// Whether the mutex is currently locked by anyone.
    #[inline]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    /// Direct access to the protected value via `&mut self` — no lock is taken,
    /// hence no order check.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Consume the mutex, returning the protected value.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedPlMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedPlMutex")
            .field("level", &self.level)
            .field("inner", &self.inner)
            .finish()
    }
}

/// RAII guard for [`OrderedPlMutex`]. Drop-in for `parking_lot::MutexGuard`.
#[must_use = "if unused the mutex is immediately unlocked"]
pub struct OrderedPlMutexGuard<'a, T> {
    guard: parking_lot::MutexGuard<'a, T>,
    level: LockLevel,
    tracked: bool,
}

impl<T> std::ops::Deref for OrderedPlMutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for OrderedPlMutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for OrderedPlMutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        if self.tracked {
            tracking::release(self.level);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedPlMutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

/// A `parking_lot::RwLock<T>` wrapper that enforces lock-ordering discipline.
///
/// Drop-in for `parking_lot::RwLock`: [`read`](Self::read),
/// [`write`](Self::write) and [`read_recursive`](Self::read_recursive) return
/// their guards directly. See the design note above for why this family exists
/// alongside the std-backed [`OrderedRwLock`].
pub struct OrderedPlRwLock<T> {
    inner: parking_lot::RwLock<T>,
    level: LockLevel,
}

impl<T> OrderedPlRwLock<T> {
    /// Create a new ordered rwlock at `level`.
    pub const fn new(value: T, level: LockLevel) -> Self {
        Self {
            inner: parking_lot::RwLock::new(value),
            level,
        }
    }

    /// The lock level of this rwlock.
    #[inline]
    pub fn level(&self) -> LockLevel {
        self.level
    }

    /// Acquire shared (read) access.
    ///
    /// # Panics
    ///
    /// When enforcement is active, panics if the calling thread already holds a
    /// lock at an equal or *lower* [`LockLevel`] — including *this* lock, since
    /// `parking_lot`'s plain `read()` is not reentrant. Use
    /// [`read_recursive`](Self::read_recursive) for a deliberately reentrant
    /// read.
    #[inline]
    pub fn read(&self) -> OrderedPlRwLockReadGuard<'_, T> {
        let tracked = tracking::check_and_acquire_tracked(self.level);
        OrderedPlRwLockReadGuard {
            guard: self.inner.read(),
            level: self.level,
            tracked,
        }
    }

    /// Acquire shared access, tolerating a read this thread already holds.
    ///
    /// `parking_lot::RwLock::read_recursive` exists precisely so a thread that
    /// may already hold a read guard can take another one without deadlocking
    /// against a queued writer. Re-entering a lock the thread *already* holds
    /// adds no new edge to the wait-for graph, so when this level is already
    /// recorded the order check is skipped and the guard does not re-record it.
    /// If the level is *not* already held, the full descending check applies.
    ///
    /// # Panics
    ///
    /// When enforcement is active and this level is not already held, panics on
    /// a descending-order violation exactly like [`read`](Self::read).
    #[inline]
    pub fn read_recursive(&self) -> OrderedPlRwLockReadGuard<'_, T> {
        let tracked = tracking::check_and_acquire_reentrant(self.level);
        OrderedPlRwLockReadGuard {
            guard: self.inner.read_recursive(),
            level: self.level,
            tracked,
        }
    }

    /// Acquire exclusive (write) access.
    ///
    /// # Panics
    ///
    /// When enforcement is active, panics on a descending-order violation.
    #[inline]
    pub fn write(&self) -> OrderedPlRwLockWriteGuard<'_, T> {
        let tracked = tracking::check_and_acquire_tracked(self.level);
        OrderedPlRwLockWriteGuard {
            guard: self.inner.write(),
            level: self.level,
            tracked,
        }
    }

    /// Try to acquire shared access without blocking.
    #[inline]
    pub fn try_read(&self) -> Option<OrderedPlRwLockReadGuard<'_, T>> {
        let guard = self.inner.try_read()?;
        let tracked = tracking::check_and_acquire_tracked(self.level);
        Some(OrderedPlRwLockReadGuard {
            guard,
            level: self.level,
            tracked,
        })
    }

    /// Try to acquire shared access without blocking **and without consulting
    /// the lock-order graph**.
    ///
    /// [`Self::try_read`] still calls `check_and_acquire_tracked`, which
    /// *panics* on a descending-order acquisition. That is the right behaviour
    /// for ordinary code — an out-of-order acquisition is a latent deadlock and
    /// should fail loudly in development. It is the wrong behaviour for the
    /// `System.exit` census path, which runs on whichever Java thread called
    /// `System.exit`, holding whatever locks that thread happened to hold, and
    /// whose job is to write a diagnostic file and then let the process die.
    /// Panicking there converts "a diagnostic file is missing" into "the exit
    /// path aborts".
    ///
    /// Skipping the check is sound **only because this cannot block**: the
    /// order graph exists to rule out wait-for cycles, and a non-blocking
    /// acquisition adds no edge to that graph. It returns `None` where a
    /// blocking acquire would have waited. Do not add a blocking sibling of
    /// this method.
    #[inline]
    pub fn try_read_untracked(&self) -> Option<OrderedPlRwLockReadGuard<'_, T>> {
        let guard = self.inner.try_read()?;
        Some(OrderedPlRwLockReadGuard {
            guard,
            level: self.level,
            tracked: false,
        })
    }

    /// Try to acquire exclusive access without blocking.
    #[inline]
    pub fn try_write(&self) -> Option<OrderedPlRwLockWriteGuard<'_, T>> {
        let guard = self.inner.try_write()?;
        let tracked = tracking::check_and_acquire_tracked(self.level);
        Some(OrderedPlRwLockWriteGuard {
            guard,
            level: self.level,
            tracked,
        })
    }

    /// Whether the lock is currently held (shared or exclusive) by anyone.
    #[inline]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    /// Direct access to the protected value via `&mut self` — no lock is taken,
    /// hence no order check.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }

    /// Consume the lock, returning the protected value.
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedPlRwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedPlRwLock")
            .field("level", &self.level)
            .field("inner", &self.inner)
            .finish()
    }
}

/// RAII read guard for [`OrderedPlRwLock`].
#[must_use = "if unused the lock is immediately released"]
pub struct OrderedPlRwLockReadGuard<'a, T> {
    guard: parking_lot::RwLockReadGuard<'a, T>,
    level: LockLevel,
    tracked: bool,
}

impl<T> std::ops::Deref for OrderedPlRwLockReadGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> Drop for OrderedPlRwLockReadGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        if self.tracked {
            tracking::release(self.level);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedPlRwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

/// RAII write guard for [`OrderedPlRwLock`].
#[must_use = "if unused the lock is immediately released"]
pub struct OrderedPlRwLockWriteGuard<'a, T> {
    guard: parking_lot::RwLockWriteGuard<'a, T>,
    level: LockLevel,
    tracked: bool,
}

impl<T> std::ops::Deref for OrderedPlRwLockWriteGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for OrderedPlRwLockWriteGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for OrderedPlRwLockWriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        if self.tracked {
            tracking::release(self.level);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedPlRwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
