//! Deferred starts (Phase 5 fast boot, work package WP3).
//!
//! A boot image is a dump of the capture process's memory at snapshot point
//! P1. OS threads cannot be imaged: a thread that exists at P1 is a stack and
//! a kernel task the restored process does not have, so in a capture run
//! (`cratonvm_types::image_layout::capturing()`) no OS thread other than the
//! launcher thread and `main-vm` may exist at P1.
//!
//! Every boot-time thread starter therefore goes through this per-VM list
//! ([`SharedVm::deferred_starts`]):
//!
//! * not capturing (every normal run, and a restored run): the starter runs
//!   at once, on the calling thread, exactly as it did before this list
//!   existed -- one code path, no allocation, no lock;
//! * capturing: the starter is queued, and [`run_deferred_starts`] runs the
//!   queue, in the order it was filled, once the image has been mapped back.
//!
//! The queued closures live in the imaged arena. That is sound because they
//! own only `Arc`/owned data and function pointers (the binary is loaded at
//! the same base in the capture and the restored process); a closure must
//! never capture a borrowed pointer into a thread stack.
//!
//! Per VM, never a process global (AGENTS.md): two VMs in one process keep
//! two lists, and a test VM's list dies with it.

use crate::vm::SharedVm;
use std::sync::Arc;

/// One queued start: what [`DeferredStarts`] keeps while capturing.
type DeferredStartFn = Box<dyn FnOnce(&Arc<SharedVm>) + Send>;

/// A value a deferred starter produces, for a caller that has to own the
/// result (an RAII guard such as the launcher's signal handlers). Empty until
/// the starter has run; dropping the last handle drops the value.
pub type DeferredSlot<T> = Arc<parking_lot::Mutex<Option<T>>>;

/// This VM's queue of thread starts held back for a boot image's capture.
#[derive(Default)]
pub struct DeferredStarts {
    pending: parking_lot::Mutex<Vec<(Box<str>, DeferredStartFn)>>,
}

impl std::fmt::Debug for DeferredStarts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredStarts")
            .field("pending", &self.pending_names())
            .finish()
    }
}

impl DeferredStarts {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` while starts are held back: the process is a capture run.
    #[inline]
    pub fn holding() -> bool {
        cratonvm_types::image_layout::capturing()
    }

    /// Run `start` now, or -- in a capture run -- queue it under `name` for
    /// [`run_deferred_starts`]. `true` when it ran now. For a starter that
    /// needs no VM handle (one called before the `SharedVm` exists, such as
    /// the ZGC heap's own threads).
    #[inline]
    pub fn start_or_defer<F>(&self, name: &str, start: F) -> bool
    where
        F: FnOnce() + Send + 'static,
    {
        if !Self::holding() {
            start();
            return true;
        }
        self.defer(name, Box::new(move |_: &Arc<SharedVm>| start()));
        false
    }

    /// [`Self::start_or_defer`] for a starter that takes this VM's owning
    /// handle; the restored run passes the same VM to it.
    #[inline]
    pub fn start_or_defer_with_vm<F>(&self, shared: &Arc<SharedVm>, name: &str, start: F) -> bool
    where
        F: FnOnce(&Arc<SharedVm>) + Send + 'static,
    {
        if !Self::holding() {
            start(shared);
            return true;
        }
        self.defer(name, Box::new(start));
        false
    }

    /// [`Self::start_or_defer_with_vm`] for a starter whose result the caller
    /// keeps: the slot holds it once the starter has run (at once outside a
    /// capture run, so a normal run's slot is filled on return).
    pub fn start_or_defer_into<T, F>(
        &self,
        shared: &Arc<SharedVm>,
        name: &str,
        start: F,
    ) -> DeferredSlot<T>
    where
        T: Send + 'static,
        F: FnOnce(&Arc<SharedVm>) -> T + Send + 'static,
    {
        let slot: DeferredSlot<T> = Arc::new(parking_lot::Mutex::new(None));
        let filled = Arc::clone(&slot);
        self.start_or_defer_with_vm(shared, name, move |vm| {
            let value = start(vm);
            *filled.lock() = Some(value);
        });
        slot
    }

    fn defer(&self, name: &str, start: DeferredStartFn) {
        tracing::debug!(name, "boot image capture: thread start deferred");
        self.pending.lock().push((name.into(), start));
    }

    /// The names of the starts still queued, in queue order.
    pub fn pending_names(&self) -> Vec<String> {
        self.pending
            .lock()
            .iter()
            .map(|(name, _)| name.to_string())
            .collect()
    }

    /// How many starts are queued.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }
}

/// Start every thread `shared`'s boot deferred, in the order it deferred them;
/// the count started. Called by the restore path once the image is mapped and
/// the image mode has left `Capture`. A no-op on a VM that deferred nothing
/// (every normal run). Refused (0, nothing run) while still capturing: a
/// capture process must reach P1 with the queue intact.
///
/// A starter that defers again (it runs while the mode is not `Capture`, so
/// it cannot) or that queues a further start is drained too: the loop runs
/// until the queue stays empty.
pub fn run_deferred_starts(shared: &Arc<SharedVm>) -> usize {
    if DeferredStarts::holding() {
        tracing::warn!("run_deferred_starts called while capturing a boot image; refused");
        return 0;
    }
    let mut started = 0;
    loop {
        // Taken out of the lock before running: a starter may queue (or ask
        // for the names of) further starts.
        let batch = std::mem::take(&mut *shared.deferred_starts.pending.lock());
        if batch.is_empty() {
            return started;
        }
        for (name, start) in batch {
            tracing::debug!(name = &*name, "boot image restore: deferred thread start");
            start(shared);
            started += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn outside_a_capture_run_every_start_runs_at_once() {
        let starts = DeferredStarts::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = Arc::clone(&ran);
        assert!(starts.start_or_defer("t", move || {
            r.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        assert_eq!(starts.pending_len(), 0);
        assert!(starts.pending_names().is_empty());
    }
}
