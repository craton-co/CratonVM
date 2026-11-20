// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The bodies a tier-up displaced in ONE [`JitCache`](crate::JitCache)
//! (j27 wave 4, lane GLOBALS; R15B-23 / VH7-3 of
//! `docs/feature-designs/jit-ir-optimizations.md`).
//!
//! A superseded body is in no cache map, yet its raw callers still run it, so
//! the superseded half of `JitCache::invalidate_matching_collecting_with`
//! needs a list of them. That list used to be the process-wide
//! `JIT_ENTRY_OWNERS` table, filtered by the `superseded` flag. With two VMs in
//! one process (embeddings, the vm test harness) a sweep of VM A then matched
//! VM B's superseded bodies by class NAME or per-VM class ID, marked them
//! `retired` without withdrawing VM B's published raw callers, and VM B's own
//! later sweep skipped them (`retired` is filtered out), leaving those callers
//! on the stale body. It also cost time proportional to every VM's code.
//!
//! Here each cache records only what its own `put` / `put_osr` displaced.
//! Weak references: the list never keeps code alive. Dead ones are pruned at
//! every scan and, amortised, at every note, so a cache that never sweeps does
//! not grow without bound.

use std::sync::{Arc, Weak};

use crate::CompiledMethod;

/// Below this many records a note never prunes.
const MIN_PRUNE_MARK: usize = 64;

#[derive(Default)]
struct Records {
    bodies: Vec<Weak<CompiledMethod>>,
    /// Length at which the next note prunes the dead records first.
    prune_mark: usize,
}

impl Records {
    fn prune_dead(&mut self) {
        self.bodies.retain(|w| w.strong_count() > 0);
        self.prune_mark = MIN_PRUNE_MARK.max(self.bodies.len() * 2);
    }
}

/// One cache's superseded bodies. See the module documentation.
#[derive(Default)]
pub(crate) struct SupersededBodies {
    records: parking_lot::Mutex<Records>,
}

impl SupersededBodies {
    /// Record a body this cache's publication just displaced.
    pub(crate) fn note(&self, body: &Arc<CompiledMethod>) {
        let mut records = self.records.lock();
        if records.bodies.len() >= records.prune_mark {
            records.prune_dead();
        }
        records.bodies.push(Arc::downgrade(body));
    }

    /// Every live body of this cache that is superseded and not yet retired.
    ///
    /// Every `Arc` is upgraded under the lock and none is dropped there (a
    /// last reference runs `CompiledMethod::drop`). The rejected ones leave
    /// through the retirement queue, as the process-wide walk released them.
    pub(crate) fn live(&self) -> Vec<Arc<CompiledMethod>> {
        let live: Vec<Arc<CompiledMethod>> = {
            let mut records = self.records.lock();
            records.prune_dead();
            records.bodies.iter().filter_map(Weak::upgrade).collect()
        };
        let (kept, rest): (Vec<_>, Vec<_>) = live.into_iter().partition(|cm| {
            cm.superseded.load(std::sync::atomic::Ordering::Acquire)
                && !cm.retired.load(std::sync::atomic::Ordering::Acquire)
        });
        crate::release_unpublished_pins(rest);
        kept
    }
}

#[cfg(test)]
mod two_vm_tests;
