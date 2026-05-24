// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lock ordering enforcement framework.
//!
//! Prevents deadlocks by enforcing a strict acquisition order on mutexes and
//! read-write locks. Each lock is assigned a [`LockLevel`]. A thread may only
//! acquire a lock whose level is *strictly greater* than any level it already
//! holds. Violations are caught at runtime in debug builds via thread-local
//! tracking; in release builds the wrapper is zero-cost.

use std::fmt;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, LockResult};

// ---------------------------------------------------------------------------
// LockLevel
// ---------------------------------------------------------------------------

/// Ordered lock levels. Lower numeric value == acquired first.
///
/// The total order is:
/// `HeapLock < ClassLoader < MonitorPool < ThreadList < JitCache < Safepoint`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LockLevel {
    /// GC heap mutex.
    HeapLock = 0,
    /// Class loading lock.
    ClassLoader = 1,
    /// Object monitor pool.
    MonitorPool = 2,
    /// Thread registry.
    ThreadList = 3,
    /// JIT code cache.
    JitCache = 4,
    /// Safepoint coordination.
    Safepoint = 5,
}

impl LockLevel {
    const COUNT: usize = 6;
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
#[derive(Debug, Clone)]
pub struct LockOrderViolation {
    /// The level the thread attempted to acquire.
    pub attempted: LockLevel,
    /// The highest level the thread currently holds.
    pub held: LockLevel,
}

impl fmt::Display for LockOrderViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lock order violation: attempted to acquire {:?} (level {}) while holding {:?} (level {})",
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

    /// Returns the highest currently-held level, if any.
    pub(super) fn highest_held() -> Option<LockLevel> {
        HELD.with(|cell| {
            let arr = cell.get();
            for i in (0..LockLevel::COUNT).rev() {
                if arr[i] {
                    // SAFETY: discriminant values 0..COUNT map exactly to variants.
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
            0 => LockLevel::HeapLock,
            1 => LockLevel::ClassLoader,
            2 => LockLevel::MonitorPool,
            3 => LockLevel::ThreadList,
            4 => LockLevel::JitCache,
            5 => LockLevel::Safepoint,
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
/// no lock at an equal or higher [`LockLevel`]. In release builds the check
/// is compiled away entirely, making this a zero-cost wrapper.
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

    /// Acquire the mutex, enforcing lock ordering in debug builds.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if the calling thread already holds a lock at
    /// an equal or higher level (this would risk deadlock).
    pub fn lock(&self) -> LockResult<OrderedMutexGuard<'_, T>> {
        #[cfg(debug_assertions)]
        {
            if let Some(held) = tracking::highest_held() {
                assert!(
                    self.level > held,
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

/// An `RwLock<T>` wrapper with the same lock-ordering enforcement as
/// [`OrderedMutex`].
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
    /// In debug builds, panics on lock-order violation.
    pub fn read(&self) -> LockResult<OrderedRwLockReadGuard<'_, T>> {
        #[cfg(debug_assertions)]
        {
            if let Some(held) = tracking::highest_held() {
                assert!(
                    self.level > held,
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
    /// In debug builds, panics on lock-order violation.
    pub fn write(&self) -> LockResult<OrderedRwLockWriteGuard<'_, T>> {
        #[cfg(debug_assertions)]
        {
            if let Some(held) = tracking::highest_held() {
                assert!(
                    self.level > held,
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
        assert!(LockLevel::HeapLock < LockLevel::ClassLoader);
        assert!(LockLevel::ClassLoader < LockLevel::MonitorPool);
        assert!(LockLevel::MonitorPool < LockLevel::ThreadList);
        assert!(LockLevel::ThreadList < LockLevel::JitCache);
        assert!(LockLevel::JitCache < LockLevel::Safepoint);
    }

    #[test]
    fn lock_level_discriminants() {
        assert_eq!(LockLevel::HeapLock as u8, 0);
        assert_eq!(LockLevel::ClassLoader as u8, 1);
        assert_eq!(LockLevel::MonitorPool as u8, 2);
        assert_eq!(LockLevel::ThreadList as u8, 3);
        assert_eq!(LockLevel::JitCache as u8, 4);
        assert_eq!(LockLevel::Safepoint as u8, 5);
    }

    #[test]
    fn lock_level_equality() {
        assert_eq!(LockLevel::HeapLock, LockLevel::HeapLock);
        assert_ne!(LockLevel::HeapLock, LockLevel::Safepoint);
    }

    #[test]
    fn lock_level_display() {
        let s = format!("{}", LockLevel::HeapLock);
        assert_eq!(s, "HeapLock");
    }

    // -- OrderedMutex basic -------------------------------------------------

    #[test]
    fn mutex_single_lock_unlock() {
        let m = OrderedMutex::new(42, LockLevel::HeapLock);
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
        let m = OrderedMutex::new((), LockLevel::JitCache);
        assert_eq!(m.level(), LockLevel::JitCache);
    }

    #[test]
    fn mutex_ascending_order_ok() {
        let a = OrderedMutex::new(1, LockLevel::HeapLock);
        let b = OrderedMutex::new(2, LockLevel::ClassLoader);
        let c = OrderedMutex::new(3, LockLevel::Safepoint);

        let ga = a.lock().unwrap();
        let gb = b.lock().unwrap();
        let gc = c.lock().unwrap();
        assert_eq!(*ga + *gb + *gc, 6);
    }

    #[test]
    fn mutex_non_adjacent_levels_ok() {
        let a = OrderedMutex::new((), LockLevel::HeapLock);
        let b = OrderedMutex::new((), LockLevel::Safepoint);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mutex_descending_order_panics() {
        let a = OrderedMutex::new((), LockLevel::Safepoint);
        let b = OrderedMutex::new((), LockLevel::HeapLock);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // boom
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mutex_same_level_panics() {
        let a = OrderedMutex::new((), LockLevel::ThreadList);
        let b = OrderedMutex::new((), LockLevel::ThreadList);
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // same level => violation
    }

    #[test]
    fn mutex_release_then_lower_ok() {
        let a = OrderedMutex::new((), LockLevel::Safepoint);
        let b = OrderedMutex::new((), LockLevel::HeapLock);

        {
            let _ga = a.lock().unwrap();
            // drop ga
        }
        // Now HeapLock is fine because nothing is held.
        let _gb = b.lock().unwrap();
    }

    // -- OrderedRwLock basic ------------------------------------------------

    #[test]
    fn rwlock_read_write() {
        let rw = OrderedRwLock::new(String::from("hello"), LockLevel::MonitorPool);
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
        let rw = OrderedRwLock::new((), LockLevel::ClassLoader);
        assert_eq!(rw.level(), LockLevel::ClassLoader);
    }

    #[test]
    fn rwlock_ascending_read_ok() {
        let a = OrderedRwLock::new(1, LockLevel::HeapLock);
        let b = OrderedRwLock::new(2, LockLevel::MonitorPool);
        let ga = a.read().unwrap();
        let gb = b.read().unwrap();
        assert_eq!(*ga + *gb, 3);
    }

    #[test]
    fn rwlock_ascending_write_ok() {
        let a = OrderedRwLock::new(1, LockLevel::HeapLock);
        let b = OrderedRwLock::new(2, LockLevel::JitCache);
        let _ga = a.write().unwrap();
        let _gb = b.write().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_descending_read_panics() {
        let a = OrderedRwLock::new((), LockLevel::JitCache);
        let b = OrderedRwLock::new((), LockLevel::HeapLock);
        let _ga = a.read().unwrap();
        let _gb = b.read().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_descending_write_panics() {
        let a = OrderedRwLock::new((), LockLevel::Safepoint);
        let b = OrderedRwLock::new((), LockLevel::ClassLoader);
        let _ga = a.write().unwrap();
        let _gb = b.write().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn rwlock_same_level_panics() {
        let a = OrderedRwLock::new((), LockLevel::MonitorPool);
        let b = OrderedRwLock::new((), LockLevel::MonitorPool);
        let _ga = a.read().unwrap();
        let _gb = b.write().unwrap();
    }

    // -- Mixed mutex + rwlock -----------------------------------------------

    #[test]
    fn mixed_mutex_then_rwlock_ascending_ok() {
        let m = OrderedMutex::new((), LockLevel::HeapLock);
        let rw = OrderedRwLock::new((), LockLevel::ThreadList);
        let _gm = m.lock().unwrap();
        let _gr = rw.read().unwrap();
    }

    #[test]
    fn mixed_rwlock_then_mutex_ascending_ok() {
        let rw = OrderedRwLock::new((), LockLevel::ClassLoader);
        let m = OrderedMutex::new((), LockLevel::Safepoint);
        let _gr = rw.write().unwrap();
        let _gm = m.lock().unwrap();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn mixed_mutex_then_rwlock_descending_panics() {
        let m = OrderedMutex::new((), LockLevel::Safepoint);
        let rw = OrderedRwLock::new((), LockLevel::HeapLock);
        let _gm = m.lock().unwrap();
        let _gr = rw.read().unwrap();
    }

    // -- LockOrderViolation error type --------------------------------------

    #[test]
    fn violation_display() {
        let v = LockOrderViolation {
            attempted: LockLevel::HeapLock,
            held: LockLevel::Safepoint,
        };
        let s = format!("{}", v);
        assert!(s.contains("lock order violation"));
        assert!(s.contains("HeapLock"));
        assert!(s.contains("Safepoint"));
    }

    #[test]
    fn violation_is_error() {
        let v = LockOrderViolation {
            attempted: LockLevel::HeapLock,
            held: LockLevel::JitCache,
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
        let m1 = Arc::new(OrderedMutex::new((), LockLevel::HeapLock));
        let m2 = Arc::new(OrderedMutex::new((), LockLevel::HeapLock));

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
    fn full_ascending_chain() {
        let locks: Vec<OrderedMutex<usize>> = vec![
            OrderedMutex::new(0, LockLevel::HeapLock),
            OrderedMutex::new(1, LockLevel::ClassLoader),
            OrderedMutex::new(2, LockLevel::MonitorPool),
            OrderedMutex::new(3, LockLevel::ThreadList),
            OrderedMutex::new(4, LockLevel::JitCache),
            OrderedMutex::new(5, LockLevel::Safepoint),
        ];

        let guards: Vec<_> = locks.iter().map(|l| l.lock().unwrap()).collect();
        let sum: usize = guards.iter().map(|g| **g).sum();
        assert_eq!(sum, 15);
    }

    // -- Debug trait ---------------------------------------------------------

    #[test]
    fn debug_impls() {
        let m = OrderedMutex::new(42_i32, LockLevel::HeapLock);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("OrderedMutex"));
        assert!(dbg.contains("HeapLock"));

        let rw = OrderedRwLock::new(7_i32, LockLevel::JitCache);
        let dbg = format!("{:?}", rw);
        assert!(dbg.contains("OrderedRwLock"));
        assert!(dbg.contains("JitCache"));
    }

    #[test]
    fn guard_debug() {
        let m = OrderedMutex::new(99, LockLevel::HeapLock);
        let g = m.lock().unwrap();
        let dbg = format!("{:?}", g);
        assert!(dbg.contains("99"));
    }
}
