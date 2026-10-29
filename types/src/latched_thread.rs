// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Worker threads whose owner stops them from a `Drop` that may run inside a
//! thread-local destructor.
//!
//! On Windows a thread's TLS destructors run inside `LdrShutdownThread`, under
//! the loader lock, and every thread that exits needs that lock to finish
//! exiting. A `JoinHandle::join` made from such a destructor waits for a worker
//! that has left its loop but cannot complete its own exit: the joiner holds the
//! lock the worker waits for, and every later thread start or exit in the
//! process queues behind them. The JNI context's thread-local holds an
//! `Arc<SharedVm>`; when it held the last one, the VM's heap was dropped there
//! and `ZgcRealHeap`'s uncommitter join froze the whole `cratonvm_vm` lib test
//! binary (`docs/feature-designs/jdk-only-mode.md`,
//! psample: `tls_callback -> drop SharedVm -> ZUncommitterHandle::stop_and_join
//! -> Thread::join`, the uncommitter parked in `LdrShutdownThread`).
//!
//! [`spawn_latched`] wraps the worker's closure so it raises a latch after the
//! closure has returned (or unwound) and every value it captured is dropped.
//! [`LatchedJoinHandle::join`] waits on that latch, which needs no lock the
//! loader holds. On Windows it then waits up to [`EXIT_GRACE`] on the thread's
//! OS handle (signalled only once the thread has fully exited, TLS callbacks
//! included) and joins it, or detaches a thread that cannot exit in that time
//! (the loader-lock case). `JoinHandle::is_finished` cannot make that call: it
//! turns true when the closure's result is stored, before the thread's
//! thread-local destructors and `LdrShutdownThread` run. Elsewhere there is no
//! loader lock and `join` joins as `JoinHandle::join` does. Either way the
//! worker runs none of its closure's code once `join` returns, which is what an
//! owner freeing the worker's data relies on.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle, Thread};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// How long [`LatchedJoinHandle::join`] lets a worker whose closure has
/// returned finish its OS-level exit before detaching it (Windows). A worker
/// outside the loader-lock case exits in microseconds.
pub const EXIT_GRACE: Duration = Duration::from_millis(100);

struct Latch<T> {
    result: Mutex<Option<std::thread::Result<T>>>,
    done: Condvar,
}

/// A worker spawned by [`spawn_latched`].
pub struct LatchedJoinHandle<T> {
    handle: JoinHandle<()>,
    latch: Arc<Latch<T>>,
}

/// Spawn `f` on `builder` with an exit latch (see the module doc).
pub fn spawn_latched<F, T>(builder: Builder, f: F) -> std::io::Result<LatchedJoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let latch = Arc::new(Latch {
        result: Mutex::new(None),
        done: Condvar::new(),
    });
    let worker_latch = Arc::clone(&latch);
    let handle = builder.spawn(move || {
        // `f` is consumed by the call: its captures are gone before the latch
        // is raised, whether it returned or unwound.
        let result = catch_unwind(AssertUnwindSafe(f));
        *worker_latch.result.lock() = Some(result);
        worker_latch.done.notify_all();
    })?;
    Ok(LatchedJoinHandle { handle, latch })
}

impl<T> LatchedJoinHandle<T> {
    /// The worker's thread.
    pub fn thread(&self) -> &Thread {
        self.handle.thread()
    }

    /// Has the worker's closure returned?
    pub fn is_finished(&self) -> bool {
        self.latch.result.lock().is_some()
    }

    /// Wait for the worker's closure to return and hand back its result (the
    /// panic payload if it unwound), as `JoinHandle::join` does. Never call it
    /// from the worker itself.
    pub fn join(self) -> std::thread::Result<T> {
        let result = {
            let mut slot = self.latch.result.lock();
            while slot.is_none() {
                self.latch.done.wait(&mut slot);
            }
            slot.take().expect("latch raised without a result")
        };
        if os_thread_exits_within(&self.handle, EXIT_GRACE) {
            let _ = self.handle.join();
        }
        result
    }
}

/// Has `handle`'s OS thread fully exited, or does it within `grace`?
#[cfg(windows)]
fn os_thread_exits_within(handle: &JoinHandle<()>, grace: Duration) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, millis: u32) -> u32;
    }
    const WAIT_OBJECT_0: u32 = 0;
    let millis = u32::try_from(grace.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: the handle is the thread's, owned by `handle`, open until it
    // is joined or dropped.
    unsafe { WaitForSingleObject(handle.as_raw_handle(), millis) == WAIT_OBJECT_0 }
}

/// No loader lock: the thread's exit takes no lock its joiner can hold.
#[cfg(not(windows))]
fn os_thread_exits_within(_handle: &JoinHandle<()>, _grace: Duration) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_returns_the_closures_value() {
        let h = spawn_latched(Builder::new(), || 41 + 1).expect("spawn");
        assert_eq!(h.join().expect("no panic"), 42);
    }

    #[test]
    fn join_returns_the_panic_payload() {
        let h =
            spawn_latched(Builder::new(), || -> i32 { panic!("latched worker") }).expect("spawn");
        let payload = h.join().expect_err("the worker unwound");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"latched worker"));
    }

    #[test]
    fn the_captures_are_dropped_before_join_returns() {
        let data = Arc::new(());
        let held = Arc::clone(&data);
        let h = spawn_latched(Builder::new(), move || drop(held)).expect("spawn");
        h.join().expect("no panic");
        assert_eq!(Arc::strong_count(&data), 1);
    }

    /// The loader-lock case: the worker's OS thread cannot finish exiting
    /// because one of ITS thread-local destructors waits for the joiner.
    /// `JoinHandle::join` would deadlock; the latched join returns after the
    /// grace and detaches the thread, which then exits. Windows only: elsewhere
    /// `join` joins (there is no loader lock to model).
    #[cfg(windows)]
    #[test]
    fn a_worker_stuck_in_its_own_exit_is_detached_after_the_grace() {
        use std::time::Instant;
        struct WaitOnDrop(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for WaitOnDrop {
            fn drop(&mut self) {
                let (flag, cv) = &*self.0;
                let mut released = flag.lock();
                while !*released {
                    cv.wait(&mut released);
                }
            }
        }
        thread_local! {
            static STUCK: std::cell::RefCell<Option<WaitOnDrop>> =
                const { std::cell::RefCell::new(None) };
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let h = spawn_latched(Builder::new(), move || {
            STUCK.with(|s| *s.borrow_mut() = Some(WaitOnDrop(worker_gate)));
            7
        })
        .expect("spawn");
        let started = Instant::now();
        assert_eq!(h.join().expect("no panic"), 7);
        assert!(started.elapsed() >= EXIT_GRACE);
        let (flag, cv) = &*gate;
        *flag.lock() = true;
        cv.notify_all();
    }
}
