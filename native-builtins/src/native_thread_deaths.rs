// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j3/rt3 -- the dead-thread queues of the VM-registered native-builtins
//! threads (the XNIO I/O thread, the Vert.x event loop), keyed by the VM that
//! registered each thread
//! (`docs/internal/gc/gcj-j2cm2-native-thread-dead-queues-are-process-wide-FIXED-20261020.md`).
//!
//! A dying registered thread has no `NativeContext`, so it queues its VM
//! `ThreadId` and the next native call that has a context tells the VM
//! (`NativeContext::unregister_native_thread`). The ids are per-VM registry
//! ids (every registry starts at 1) while the queues are per process, so a
//! queue that held bare ids handed VM A's dead id N to whichever VM drained
//! it: VM B then marked ITS live thread N dead and force-released its monitors,
//! and A never heard of the death. Each row now carries the
//! `NativeContext::vm_identity` captured at the spawn, and a drain takes only
//! the drainer VM's rows. In a one-VM process every row is the drainer's, so
//! the drain is the old one.
//!
//! The cap no longer clears the whole queue when it is reached (that dropped
//! every pending death, of every VM): it drops the OLDEST row, which in a
//! process that keeps a destroyed VM's undrained rows is that VM's.
//!
//! `CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM=0` restores the process-wide
//! drain and the clear-on-full cap (and the XNIO spawn's drain is skipped, as
//! nothing drained that queue before). Every backend: the defect is the
//! thread registry's identity, not a collector's.

use std::sync::Mutex;

use cratonvm_native_api::NativeContext;

/// Pending deaths kept per queue, across every VM of the process.
pub(crate) const DEAD_QUEUE_CAP: usize = 4096;

/// `CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM` -- default ON, `=0` kills (see
/// the module doc). Read on a thread's death and on a non-empty drain only.
pub(crate) fn dead_queue_per_vm_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM")
}

/// One subsystem's queue of `(vm_identity, ThreadId)` deaths.
pub(crate) struct DeathQueue {
    rows: Mutex<Vec<(usize, u64)>>,
}

impl DeathQueue {
    pub(crate) const fn new() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
        }
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, Vec<(usize, u64)>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Is nothing queued? (Tests peek without draining.)
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows().is_empty()
    }

    /// Queue the death of `id`, registered by the VM whose identity is `vm`.
    pub(crate) fn push(&self, vm: usize, id: u64) {
        self.push_ruled(vm, id, dead_queue_per_vm_enabled());
    }

    fn push_ruled(&self, vm: usize, id: u64, per_vm: bool) {
        let mut q = self.rows();
        if q.len() >= DEAD_QUEUE_CAP {
            if per_vm {
                q.remove(0);
            } else {
                q.clear();
            }
        }
        q.push((vm, id));
    }

    /// Every pending id, of every VM (the old drain; tests and the kill).
    pub(crate) fn drain_all(&self) -> Vec<u64> {
        std::mem::take(&mut *self.rows())
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    }

    /// The pending ids `vm` registered; the other VMs' rows stay queued.
    pub(crate) fn drain_for(&self, vm: usize) -> Vec<u64> {
        let mut q = self.rows();
        let mut mine = Vec::new();
        q.retain(|&(owner, id)| {
            if owner == vm {
                mine.push(id);
                false
            } else {
                true
            }
        });
        mine
    }

    /// Tell `ctx`'s VM about the deaths it owns. Cheap when nothing is
    /// queued (one lock, no flag read).
    pub(crate) fn flush_into(&self, ctx: &mut dyn NativeContext) {
        if self.rows().is_empty() {
            return;
        }
        let dead = if dead_queue_per_vm_enabled() {
            self.drain_for(ctx.vm_identity())
        } else {
            self.drain_all()
        };
        for id in dead {
            ctx.unregister_native_thread(id);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.rows().len()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeathQueue, DEAD_QUEUE_CAP};

    #[test]
    fn gcj3rt3_a_drain_takes_only_its_own_vms_deaths() {
        let q = DeathQueue::new();
        q.push_ruled(10, 1, true);
        q.push_ruled(20, 1, true);
        q.push_ruled(10, 2, true);
        assert_eq!(q.drain_for(10), vec![1, 2], "VM 10's ids, in order");
        assert_eq!(q.len(), 1, "VM 20's death stays queued for VM 20");
        assert_eq!(q.drain_for(10), Vec::<u64>::new());
        assert_eq!(q.drain_for(20), vec![1]);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn gcj3rt3_a_full_queue_drops_its_oldest_row_not_every_row() {
        let q = DeathQueue::new();
        for id in 0..DEAD_QUEUE_CAP as u64 {
            q.push_ruled(7, id, true);
        }
        q.push_ruled(8, 99, true);
        assert_eq!(q.len(), DEAD_QUEUE_CAP);
        assert_eq!(q.drain_for(8), vec![99]);
        let left = q.drain_all();
        assert_eq!(left.len(), DEAD_QUEUE_CAP - 1);
        assert_eq!(left[0], 1, "only the oldest (id 0) was dropped");

        // The kill: the old clear-on-full.
        for id in 0..DEAD_QUEUE_CAP as u64 {
            q.push_ruled(7, id, false);
        }
        q.push_ruled(8, 99, false);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn gcj3rt3_the_per_vm_queue_is_default_on_and_killed_by_zero() {
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM", None)],
            super::dead_queue_per_vm_enabled,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM", Some("0"))],
            super::dead_queue_per_vm_enabled,
        );
        assert!(on && !off);
    }
}
