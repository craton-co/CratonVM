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
//! | L9  | `native_methods`  | `SharedVm::native_methods` (append-only Mutex) |
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
//! Each [`OrderedMutex`] / [`OrderedRwLock`] carries a [`LockLevel`]. In debug
//! builds we maintain a per-thread bit-set of currently-held levels. On every
//! acquire we assert that the attempted level is strictly less than the
//! *minimum* currently-held level — the binding constraint for "descending".
//! In release builds the wrapper is zero-cost (the tracking module is
//! compiled out).
//!
//! ## Usage
//!
//! Wiring is incremental, starting with the highest-level locks. As of the
//! V11 hardening pass the **L6 `monitors` registry** is wired: both internal
//! maps of [`crate::threading::monitor::MonitorTable`] (`monitors` and
//! `cas_locks`) are [`OrderedMutex`] at [`LockLevel::Monitors`].
//!
//! ### Runtime enforcement status
//!
//! Authoritative, non-aspirational list of which locks are checked vs still
//! raw:
//! - **Wired (checked):** L6 `monitors` (both `MonitorTable` maps).
//! - **Not wired (not observed by the checker):** L10 `class_manager` and
//!   L8 `heap`, for the reasons below; the remaining levels are documented
//!   here for ordering purposes but not yet wrapped.
//!
//! The two high-level locks above are **not** wired and the checker therefore
//! does not observe them:
//! - `class_manager` (L10) is a `parking_lot::RwLock` reached from ~19 modules
//!   (including FFI/JNI surfaces this pass is not permitted to touch); swapping
//!   its type would ripple guard-API changes through those files.
//! - `heap` (L8) is defined in the separate `gc` crate (`gc/src/vm_heap.rs`),
//!   which cannot depend on `vm::runtime::lock_order` without a circular crate
//!   dependency.

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
    /// L9 — `SharedVm::native_methods` (append-only Mutex).
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
// Thread-local tracking (debug builds only)
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
mod tracking {
    use super::LockLevel;
    use std::cell::Cell;

    // Bit-set of currently held lock levels for this thread.
    // Index `i` corresponds to `LockLevel` with discriminant `i`.
    thread_local! {
        static HELD: Cell<[bool; LockLevel::COUNT]> = const { Cell::new([false; LockLevel::COUNT]) };
    }

    /// Returns the *lowest* currently-held level, if any. In descending
    /// order, this is the binding constraint: the next lock acquired must be
    /// at a level strictly less than this.
    pub(super) fn lowest_held() -> Option<LockLevel> {
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
    pub(super) fn acquire(level: LockLevel) {
        HELD.with(|cell| {
            let mut arr = cell.get();
            arr[level as u8 as usize] = true;
            cell.set(arr);
        });
    }

    /// Mark a level as released by this thread.
    pub(super) fn release(level: LockLevel) {
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
}

// ---------------------------------------------------------------------------
// OrderedMutex<T>
// ---------------------------------------------------------------------------

/// A `Mutex<T>` wrapper that enforces lock-ordering discipline.
///
/// In debug builds, acquiring this lock asserts that the calling thread holds
/// no lock at an equal or **lower** [`LockLevel`] (the descending-order rule
/// documented at the top of this module). In release builds the check is
/// compiled away entirely, making this a zero-cost wrapper.
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

    /// Acquire the mutex, enforcing descending lock order in debug builds.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if the calling thread already holds a lock at
    /// an equal or *lower* level (which would invert the documented descending
    /// hierarchy and risk deadlock).
    pub fn lock(&self) -> LockResult<OrderedMutexGuard<'_, T>> {
        #[cfg(debug_assertions)]
        {
            if let Some(held) = tracking::lowest_held() {
                assert!(
                    self.level < held,
                    "{}",
                    LockOrderViolation {
                        attempted: self.level,
                        held,
                    }
                );
            }
            tracking::acquire(self.level);
        }

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
    #[allow(dead_code)]
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
        #[cfg(debug_assertions)]
        tracking::release(self.level);
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
    /// In debug builds, panics on lock-order violation (see [`OrderedMutex::lock`]).
    pub fn read(&self) -> LockResult<OrderedRwLockReadGuard<'_, T>> {
        #[cfg(debug_assertions)]
        {
            if let Some(held) = tracking::lowest_held() {
                assert!(
                    self.level < held,
                    "{}",
                    LockOrderViolation {
                        attempted: self.level,
                        held,
                    }
                );
            }
            tracking::acquire(self.level);
        }

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
    /// In debug builds, panics on lock-order violation (see [`OrderedMutex::lock`]).
    pub fn write(&self) -> LockResult<OrderedRwLockWriteGuard<'_, T>> {
        #[cfg(debug_assertions)]
        {
            if let Some(held) = tracking::lowest_held() {
                assert!(
                    self.level < held,
                    "{}",
                    LockOrderViolation {
                        attempted: self.level,
                        held,
                    }
                );
            }
            tracking::acquire(self.level);
        }

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
    #[allow(dead_code)]
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
        #[cfg(debug_assertions)]
        tracking::release(self.level);
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
    #[allow(dead_code)]
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
        #[cfg(debug_assertions)]
        tracking::release(self.level);
    }
}

impl<T: fmt::Debug> fmt::Debug for OrderedRwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.guard, f)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- LockLevel tests ----------------------------------------------------

    #[test]
    fn lock_level_ordering() {
        // Numeric ordering matches the doc's table — higher number is acquired
        // earlier in the descending hierarchy.
        assert!(LockLevel::Scratch < LockLevel::JvmThread);
        assert!(LockLevel::JvmThread < LockLevel::NativeMemory);
        assert!(LockLevel::NativeMemory < LockLevel::CleanerActions);
        assert!(LockLevel::CleanerActions < LockLevel::FlightRecorder);
        assert!(LockLevel::FlightRecorder < LockLevel::ThreadRegistry);
        assert!(LockLevel::ThreadRegistry < LockLevel::Monitors);
        assert!(LockLevel::Monitors < LockLevel::RefProcessor);
        assert!(LockLevel::RefProcessor < LockLevel::Heap);
        assert!(LockLevel::Heap < LockLevel::NativeMethods);
        assert!(LockLevel::NativeMethods < LockLevel::ClassManager);
    }

    #[test]
    fn lock_level_discriminants_match_docs() {
        // These integer values are the canonical hierarchy (see this module's
        // doc comment) and MUST not drift. If you change them, change the table
        // in the module doc too.
        assert_eq!(LockLevel::Scratch as u8, 0);
        assert_eq!(LockLevel::JvmThread as u8, 1);
        assert_eq!(LockLevel::NativeMemory as u8, 2);
        assert_eq!(LockLevel::CleanerActions as u8, 3);
        assert_eq!(LockLevel::FlightRecorder as u8, 4);
        assert_eq!(LockLevel::ThreadRegistry as u8, 5);
        assert_eq!(LockLevel::Monitors as u8, 6);
        assert_eq!(LockLevel::RefProcessor as u8, 7);
        assert_eq!(LockLevel::Heap as u8, 8);
        assert_eq!(LockLevel::NativeMethods as u8, 9);
        assert_eq!(LockLevel::ClassManager as u8, 10);
    }

    #[test]
    fn lock_level_equality() {
        assert_eq!(LockLevel::Heap, LockLevel::Heap);
        assert_ne!(LockLevel::Heap, LockLevel::ClassManager);
    }

    #[test]
    fn lock_level_display() {
        let s = format!("{}", LockLevel::Heap);
        assert_eq!(s, "Heap");
    }

    // -- OrderedMutex basic -------------------------------------------------

    #[test]
    fn mutex_single_lock_unlock() {
        let m = OrderedMutex::new(42, LockLevel::Heap);
        {
            let g = m.lock().unwrap();
            assert_eq!(*g, 42);
        }
        // Can re-acquire after drop.
        let mut g = m.lock().unwrap();
        *g = 99;
        assert_eq!(*g, 99);
    }

    #[test]
    fn mutex_level_accessor() {
        let m = OrderedMutex::new((), LockLevel::Heap);
        assert_eq!(m.level(), LockLevel::Heap);
    }

    #[test]
    fn mutex_descending_order_ok() {
        // ClassManager (10) -> Heap (8) -> Scratch (0): the canonical
        // descending acquisition the doc describes.
        let a = OrderedMutex::new(1, LockLevel::ClassManager);
        let b = OrderedMutex::new(2, LockLevel::Heap);
        let c = OrderedMutex::new(3, LockLevel::Scratch);

        let ga = a.lock().unwrap();
        let gb = b.lock().unwrap();
        let gc = c.lock().unwrap();
        assert_eq!(*ga + *gb + *gc, 6);
    }

    #[test]
    fn mutex_non_adjacent_levels_ok() {
        let a = OrderedMutex::new((), LockLevel::ClassManager);
        let b = OrderedMutex::new((), LockLevel::Scratch);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mutex_ascending_order_panics() {
        // Holding Heap (8) and then trying to acquire ClassManager (10) is the
        // forbidden inversion described in this module's doc comment.
        let a = OrderedMutex::new((), LockLevel::Heap);
        let b = OrderedMutex::new((), LockLevel::ClassManager);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // boom
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mutex_same_level_panics() {
        let a = OrderedMutex::new((), LockLevel::ThreadRegistry);
        let b = OrderedMutex::new((), LockLevel::ThreadRegistry);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // same level => violation
    }

    #[test]
    fn mutex_release_then_higher_ok() {
        let a = OrderedMutex::new((), LockLevel::Heap);
        let b = OrderedMutex::new((), LockLevel::ClassManager);

        {
            let _ga = a.lock().unwrap();
            // drop ga
        }
        // Now ClassManager is fine because nothing is held.
        let _gb = b.lock().unwrap();
    }

    // -- OrderedRwLock basic ------------------------------------------------

    #[test]
    fn rwlock_read_write() {
        let rw = OrderedRwLock::new(String::from("hello"), LockLevel::Monitors);
        {
            let r = rw.read().unwrap();
            assert_eq!(&*r, "hello");
        }
        {
            let mut w = rw.write().unwrap();
            w.push_str(" world");
        }
        {
            let r = rw.read().unwrap();
            assert_eq!(&*r, "hello world");
        }
    }

    #[test]
    fn rwlock_level_accessor() {
        let rw = OrderedRwLock::new((), LockLevel::ClassManager);
        assert_eq!(rw.level(), LockLevel::ClassManager);
    }

    #[test]
    fn rwlock_descending_read_ok() {
        let a = OrderedRwLock::new(1, LockLevel::ClassManager);
        let b = OrderedRwLock::new(2, LockLevel::Monitors);
        let ga = a.read().unwrap();
        let gb = b.read().unwrap();
        assert_eq!(*ga + *gb, 3);
    }

    #[test]
    fn rwlock_descending_write_ok() {
        let a = OrderedRwLock::new(1, LockLevel::ClassManager);
        let b = OrderedRwLock::new(2, LockLevel::Heap);
        let _ga = a.write().unwrap();
        let _gb = b.write().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_ascending_read_panics() {
        let a = OrderedRwLock::new((), LockLevel::Heap);
        let b = OrderedRwLock::new((), LockLevel::ClassManager);
        let _ga = a.read().unwrap();
        let _gb = b.read().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_ascending_write_panics() {
        let a = OrderedRwLock::new((), LockLevel::Monitors);
        let b = OrderedRwLock::new((), LockLevel::ClassManager);
        let _ga = a.write().unwrap();
        let _gb = b.write().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_same_level_panics() {
        let a = OrderedRwLock::new((), LockLevel::Monitors);
        let b = OrderedRwLock::new((), LockLevel::Monitors);
        let _ga = a.read().unwrap();
        let _gb = b.write().unwrap();
    }

    // -- Mixed mutex + rwlock -----------------------------------------------

    #[test]
    fn mixed_rwlock_then_mutex_descending_ok() {
        // The textbook combo from this module: class_manager (RwLock, L10) held,
        // then heap (Mutex, L8) acquired.
        let rw = OrderedRwLock::new((), LockLevel::ClassManager);
        let m = OrderedMutex::new((), LockLevel::Heap);
        let _gr = rw.write().unwrap();
        let _gm = m.lock().unwrap();
    }

    #[test]
    fn mixed_mutex_then_rwlock_descending_ok() {
        let m = OrderedMutex::new((), LockLevel::Heap);
        let rw = OrderedRwLock::new((), LockLevel::Monitors);
        let _gm = m.lock().unwrap();
        let _gr = rw.read().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mixed_mutex_then_rwlock_ascending_panics() {
        // The exact "Forbidden: monitor -> class manager" case from this module.
        let m = OrderedMutex::new((), LockLevel::Monitors);
        let rw = OrderedRwLock::new((), LockLevel::ClassManager);
        let _gm = m.lock().unwrap();
        let _gr = rw.read().unwrap();
    }

    // -- LockOrderViolation error type --------------------------------------

    #[test]
    fn violation_display() {
        let v = LockOrderViolation {
            attempted: LockLevel::ClassManager,
            held: LockLevel::Heap,
        };
        let s = format!("{}", v);
        assert!(s.contains("lock order violation"));
        assert!(s.contains("ClassManager"));
        assert!(s.contains("Heap"));
    }

    #[test]
    fn violation_is_error() {
        let v = LockOrderViolation {
            attempted: LockLevel::ClassManager,
            held: LockLevel::Monitors,
        };
        let e: &dyn std::error::Error = &v;
        assert!(e.to_string().contains("lock order violation"));
    }

    // -- Thread isolation ---------------------------------------------------

    #[test]
    fn separate_threads_independent() {
        use std::sync::Arc;
        use std::thread;

        // Two threads each acquire the *same* level independently -- no
        // violation because tracking is per-thread.
        let m1 = Arc::new(OrderedMutex::new((), LockLevel::Heap));
        let m2 = Arc::new(OrderedMutex::new((), LockLevel::Heap));

        let m1c = Arc::clone(&m1);
        let m2c = Arc::clone(&m2);

        let t1 = thread::spawn(move || {
            let _g = m1c.lock().unwrap();
        });
        let t2 = thread::spawn(move || {
            let _g = m2c.lock().unwrap();
        });

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn full_descending_chain() {
        // Mirrors the descending hierarchy documented in this module from L10
        // down to L0. Acquiring in this order must succeed.
        let locks: Vec<OrderedMutex<usize>> = vec![
            OrderedMutex::new(10, LockLevel::ClassManager),
            OrderedMutex::new(9, LockLevel::NativeMethods),
            OrderedMutex::new(8, LockLevel::Heap),
            OrderedMutex::new(7, LockLevel::RefProcessor),
            OrderedMutex::new(6, LockLevel::Monitors),
            OrderedMutex::new(5, LockLevel::ThreadRegistry),
            OrderedMutex::new(4, LockLevel::FlightRecorder),
            OrderedMutex::new(3, LockLevel::CleanerActions),
            OrderedMutex::new(2, LockLevel::NativeMemory),
            OrderedMutex::new(1, LockLevel::JvmThread),
            OrderedMutex::new(0, LockLevel::Scratch),
        ];

        let guards: Vec<_> = locks.iter().map(|l| l.lock().unwrap()).collect();
        let sum: usize = guards.iter().map(|g| **g).sum();
        assert_eq!(sum, 10 + 9 + 8 + 7 + 6 + 5 + 4 + 3 + 2 + 1 + 0);
    }

    // -- Smoke test: every documented L -> L transition succeeds ------------
    //
    // This module's hierarchy permits combinations such as:
    //   class_manager (L10) -> heap (L8) -> ref_processor (L7)
    //   class_manager (L10) -> monitors (L6) -> thread_registry (L5)
    //   heap (L8)          -> monitors (L6) -> thread_registry (L5)
    //
    // The smoke test below exercises each adjacent pair in the module's table
    // (acquire the higher-level lock, then acquire the lower-level lock, then
    // drop both) to prove the wrappers and the doc agree.

    #[test]
    fn smoke_every_adjacent_descending_pair() {
        // Pairs are (higher-level, lower-level). Each must be acquirable
        // higher-then-lower without tripping the assertion.
        let pairs: &[(LockLevel, LockLevel)] = &[
            (LockLevel::ClassManager, LockLevel::NativeMethods),
            (LockLevel::NativeMethods, LockLevel::Heap),
            (LockLevel::Heap, LockLevel::RefProcessor),
            (LockLevel::RefProcessor, LockLevel::Monitors),
            (LockLevel::Monitors, LockLevel::ThreadRegistry),
            (LockLevel::ThreadRegistry, LockLevel::FlightRecorder),
            (LockLevel::FlightRecorder, LockLevel::CleanerActions),
            (LockLevel::CleanerActions, LockLevel::NativeMemory),
            (LockLevel::NativeMemory, LockLevel::JvmThread),
            (LockLevel::JvmThread, LockLevel::Scratch),
        ];

        for (high, low) in pairs {
            let outer = OrderedMutex::new(*high as u8, *high);
            let inner = OrderedMutex::new(*low as u8, *low);
            let _go = outer.lock().unwrap();
            let _gi = inner.lock().unwrap();
            assert_eq!(*_go, *high as u8);
            assert_eq!(*_gi, *low as u8);
            // Both drop here, in inner-then-outer order (LIFO).
        }
    }

    #[test]
    fn smoke_canonical_doc_examples() {
        // Example: "GC stops the world" — heap (L8) holds, then
        // monitors (L6), then thread_registry (L5).
        {
            let heap = OrderedMutex::new((), LockLevel::Heap);
            let monitors = OrderedMutex::new((), LockLevel::Monitors);
            let registry = OrderedMutex::new((), LockLevel::ThreadRegistry);
            let _h = heap.lock().unwrap();
            let _m = monitors.lock().unwrap();
            let _r = registry.lock().unwrap();
        }

        // Example: "interpreter calls into the heap" — heap (L8) -> ref_processor (L7).
        {
            let heap = OrderedMutex::new((), LockLevel::Heap);
            let refp = OrderedMutex::new((), LockLevel::RefProcessor);
            let _h = heap.lock().unwrap();
            let _r = refp.lock().unwrap();
        }

        // Example: class_manager (L10) -> heap (L8).
        {
            let cm = OrderedRwLock::new((), LockLevel::ClassManager);
            let heap = OrderedMutex::new((), LockLevel::Heap);
            let _c = cm.write().unwrap();
            let _h = heap.lock().unwrap();
        }
    }

    // -- Debug trait ---------------------------------------------------------

    #[test]
    fn debug_impls() {
        let m = OrderedMutex::new(42_i32, LockLevel::Heap);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("OrderedMutex"));
        assert!(dbg.contains("Heap"));

        let rw = OrderedRwLock::new(7_i32, LockLevel::ClassManager);
        let dbg = format!("{:?}", rw);
        assert!(dbg.contains("OrderedRwLock"));
        assert!(dbg.contains("ClassManager"));
    }

    // -- V11 wiring invariants (monitors registry) --------------------------

    // SECURITY FIX (V11): the real `MonitorTable` now holds two L6 registries
    // (`monitors` and `cas_locks`). The checker must forbid nesting one inside
    // the other (equal level is not strictly descending). This reproduces the
    // exact shape of the bug that `remap_after_gc` was restructured to avoid.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn v11_two_monitor_level_registries_must_not_nest() {
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let cas_locks = OrderedMutex::new((), LockLevel::Monitors);
        let _m = monitors.lock().unwrap();
        let _c = cas_locks.lock().unwrap(); // same level (L6) => violation
    }

    // SECURITY FIX (V11): acquiring a lower-level lock *after* the L6 monitors
    // registry is the documented, allowed direction and must NOT trip.
    #[test]
    fn v11_monitors_then_lower_ok() {
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let scratch = OrderedMutex::new((), LockLevel::Scratch); // L0 < L6
        let _m = monitors.lock().unwrap();
        let _s = scratch.lock().unwrap();
    }

    // SECURITY FIX (V11): holding the L6 monitors registry and then reaching
    // *up* for class_manager (L10) is the canonical forbidden monitor ->
    // class_manager inversion documented in this module. Even though
    // class_manager itself is not yet wrapped, the check fires the moment any
    // higher-level OrderedRwLock is acquired under a held monitor.
    #[test]
    #[should_panic(expected = "lock order violation")]
    fn v11_monitors_then_classmanager_inverts() {
        let monitors = OrderedMutex::new((), LockLevel::Monitors);
        let class_manager = OrderedRwLock::new((), LockLevel::ClassManager);
        let _m = monitors.lock().unwrap();
        let _c = class_manager.write().unwrap(); // L10 under L6 => violation
    }

    #[test]
    fn guard_debug() {
        let m = OrderedMutex::new(99, LockLevel::Heap);
        let g = m.lock().unwrap();
        let dbg = format!("{:?}", g);
        assert!(dbg.contains("99"));
    }
}
