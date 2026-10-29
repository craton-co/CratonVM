//! The bodies a tier-up displaced in ONE [`JitCache`](crate::JitCache)
//! (j27 wave 4, lane GLOBALS; R15B-23 / VH7-3 of
//! `docs/known-issues/jit/j27-r18-backlog-live-rows-20261028.md`).
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
mod two_vm_tests {
    use super::*;
    use crate::{ExecutableBuffer, JitCache};
    use std::sync::atomic::Ordering::SeqCst;

    const DEP: &str = "J27Globals/Dep";

    fn body() -> Option<CompiledMethod> {
        let mut buf = ExecutableBuffer::new(64)?;
        buf.emit(&[0xC3]); // ret
        Some(CompiledMethod::new(buf))
    }

    fn inlining_dep() -> Option<CompiledMethod> {
        let mut cm = body()?;
        cm.inlined_methods
            .push((DEP.to_string(), "value".to_string(), "()I".to_string()));
        Some(cm)
    }

    /// One VM's cache: a body inlining `DEP`, displaced by a tier-up, and a
    /// published raw caller of the displaced body. Returns the displaced body.
    fn vm_with_superseded_dependent(
        cache: &JitCache,
        cid: cratonvm_types::ClassId,
    ) -> Option<Arc<CompiledMethod>> {
        cache.put("J27G".into(), "dep".into(), "()V".into(), cid, inlining_dep()?);
        let old = cache.get("J27G", "dep", "()V", cid)?;
        cache.put("J27G".into(), "dep".into(), "()V".into(), cid, body()?);
        let mut caller = body()?;
        caller._direct_callee_entries = vec![old.entry_ptr() as usize];
        cache.put("J27G".into(), "caller".into(), "()V".into(), cid, caller);
        // Both must hold, or every test below would pass vacuously.
        assert!(
            cache.get("J27G", "caller", "()V", cid).is_some(),
            "the raw caller of the displaced body is published"
        );
        assert!(old.superseded.load(SeqCst), "the tier-up superseded it");
        Some(old)
    }

    fn sweeping_cache() -> JitCache {
        let mut cache = JitCache::new();
        cache.invalidation_retires_superseded = true;
        cache
    }

    /// The scan of one cache lists its own superseded body, never another
    /// cache's.
    #[test]
    fn a_cache_lists_only_its_own_superseded_bodies() {
        let (a, b) = (sweeping_cache(), sweeping_cache());
        let cid = cratonvm_types::ClassId::new(0x2701);
        let (Some(a_old), Some(b_old)) = (
            vm_with_superseded_dependent(&a, cid),
            vm_with_superseded_dependent(&b, cid),
        ) else {
            return;
        };
        let listed = a.superseded_bodies.live();
        assert!(listed.iter().any(|cm| Arc::ptr_eq(cm, &a_old)));
        assert!(!listed.iter().any(|cm| Arc::ptr_eq(cm, &b_old)));
    }

    /// VM A's class-change sweep leaves VM B's superseded body and its raw
    /// caller alone; VM B's own sweep still retires both. Before lane GLOBALS
    /// A's sweep marked B's body `retired` and B's sweep then skipped it,
    /// leaving B's caller published on the stale body.
    #[test]
    fn one_vms_sweep_leaves_the_other_vms_superseded_body() {
        let (a, b) = (sweeping_cache(), sweeping_cache());
        let cid = cratonvm_types::ClassId::new(0x2702);
        let (Some(a_old), Some(b_old)) = (
            vm_with_superseded_dependent(&a, cid),
            vm_with_superseded_dependent(&b, cid),
        ) else {
            return;
        };
        a.invalidate_for_class_change(DEP);
        assert!(a_old.retired.load(SeqCst), "A retires its own body");
        assert!(a.get("J27G", "caller", "()V", cid).is_none());
        assert!(!b_old.retired.load(SeqCst), "A's sweep left B's body alone");
        assert!(b.get("J27G", "caller", "()V", cid).is_some());

        b.invalidate_for_class_change(DEP);
        assert!(b_old.retired.load(SeqCst), "B's own sweep retires it");
        assert!(
            b.get("J27G", "caller", "()V", cid).is_none(),
            "and withdraws B's raw caller"
        );
    }

    /// Dead records are pruned: the list holds no reference that keeps a
    /// body alive, and forgets one that was dropped.
    #[test]
    fn a_dropped_body_leaves_the_list() {
        let list = SupersededBodies::default();
        let Some(cm) = body() else {
            return;
        };
        let cm = Arc::new(cm);
        cm.superseded.store(true, SeqCst);
        list.note(&cm);
        assert_eq!(list.live().len(), 1);
        drop(cm);
        assert!(list.live().is_empty());
        assert!(list.records.lock().bodies.is_empty());
    }
}
