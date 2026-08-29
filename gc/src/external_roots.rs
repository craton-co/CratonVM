// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Collector-facing registry for roots owned by optional runtime subsystems.
//!
//! The collector must understand root *semantics*, but it must not depend on
//! concrete native implementations. Providers register their scan, relocation,
//! owner-edge and pruning callbacks once during subsystem initialization.

use cratonvm_types::ObjectRef;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;

pub type OwnerPredicate<'a> = dyn Fn(usize) -> bool + 'a;

/// One complete external-root contract.
///
/// Keeping every callback in one value makes scan/remap and conditional-owner
/// coverage reviewable as a unit. A provider name is a stable identity:
/// re-registering the same name and callbacks is idempotent, while attempting
/// to reuse a name for different callbacks is rejected.
#[derive(Clone, Copy)]
pub struct ExternalRootProvider {
    pub name: &'static str,
    pub scan: fn(&mut Vec<ObjectRef>),
    pub owner_addrs: fn() -> Option<HashSet<usize>>,
    /// Roots owned by the object at `owner_addr`, whose CURRENT class id the
    /// caller supplies so the provider can reject a STALE owner entry.
    ///
    /// The owner index is keyed by address, and an address is recycled the
    /// moment its previous tenant is reclaimed. Without an identity check the
    /// marker is handed the DEAD owner's references on behalf of whatever
    /// unrelated object now occupies that address — observed as
    /// `rejecting external-overlay(BFS owner) candidate … not a plausible
    /// object base` on non-headers (ASCII string payload, raw heap pointers,
    /// interior addresses). A class id is GC-invariant (it travels with the
    /// header), which is exactly the discriminator `native-collections`'
    /// mutator-side `widened_obj_key` already applies for recycled identity
    /// hashes; this carries it to the marker, which had no check at all.
    ///
    /// `None` means "class unknown at this call site" and skips the check.
    pub roots_for_owner: fn(usize, Option<u32>) -> Vec<ObjectRef>,
    pub roots_for_matching_owners: fn(&OwnerPredicate<'_>) -> Vec<ObjectRef>,
    pub remap: fn(&cratonvm_types::PointerMap),
    pub prune: fn(&OwnerPredicate<'_>),
    /// `(fast_path_hits, mutex_fallthroughs, disabled)` for a provider that
    /// gates [`Self::roots_for_owner`] before doing real work.
    ///
    /// # Why the provider reports and `gc` does not measure
    ///
    /// `roots_for_owner` runs once per marked object and was **20–29% of mark
    /// samples on both the serial and the one-worker parallel arm** (2026-08-17
    /// `perf record`). A provider that gates it returns an empty `Vec` either way,
    /// so from this side a gate that works and a gate that is inert are
    /// indistinguishable — and the previous attempt at that optimisation was
    /// measured inert, so this is not a hypothetical. Only the provider can count
    /// it, and `gc` cannot call into the provider's crate because the dependency
    /// runs the other way.
    ///
    /// `None` for a provider with no gate.
    pub gate_stats: Option<fn() -> (u64, u64, bool)>,
}

/// Every registered provider's gate counters, for the shutdown report. See
/// [`ExternalRootProvider::gate_stats`].
pub fn provider_gate_stats() -> Vec<(&'static str, u64, u64, bool)> {
    if PROVIDER_COUNT.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    PROVIDERS
        .read_recursive()
        .iter()
        .filter_map(|p| {
            p.gate_stats.map(|f| {
                let (h, m, d) = f();
                (p.name, h, m, d)
            })
        })
        .collect()
}

static PROVIDERS: LazyLock<RwLock<Vec<ExternalRootProvider>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// How many providers are registered, as one relaxed word.
///
/// # Why the per-object paths need it
///
/// [`external_roots_for_owner`] is called **once per marked object** by every
/// marker in the tree, and it went through [`snapshot`], which is
/// `PROVIDERS.read().clone()` — an `RwLock` read plus a heap allocation, per
/// object, for a registry that is empty in every `--jdk-only` run and in every
/// unit test.
///
/// The 2026-08-14 parallel-marking measurement is why that matters: four workers
/// cost **+153% pause** against zero, and the rise is monotonic in worker count,
/// which is a lock rather than a start-up offset. An uncontended `RwLock` read is
/// a few nanoseconds; the same read from eight workers is a shared cache line
/// bouncing between eight cores, once per object — and the allocation is worse.
///
/// Monotone in practice (nothing unregisters) but recomputed from the vector's
/// own length under the write lock anyway, so it cannot drift from it.
static PROVIDER_COUNT: AtomicUsize = AtomicUsize::new(0);

fn same_callbacks(a: ExternalRootProvider, b: ExternalRootProvider) -> bool {
    a.scan as usize == b.scan as usize
        && a.owner_addrs as usize == b.owner_addrs as usize
        && a.roots_for_owner as usize == b.roots_for_owner as usize
        && a.roots_for_matching_owners as usize == b.roots_for_matching_owners as usize
        && a.remap as usize == b.remap as usize
        && a.prune as usize == b.prune as usize
}

/// Register a complete root provider.
///
/// # Panics
///
/// Panics if `name` was already registered with a different callback set. A
/// silent replacement could split a collection across two incompatible root
/// contracts and is therefore a correctness error, not recoverable state.
pub fn register_external_root_provider(provider: ExternalRootProvider) {
    let mut providers = PROVIDERS.write();
    if let Some(existing) = providers.iter().find(|item| item.name == provider.name) {
        assert!(
            same_callbacks(*existing, provider),
            "external GC root provider name reused with different callbacks: {}",
            provider.name
        );
        return;
    }
    providers.push(provider);
    PROVIDER_COUNT.store(providers.len(), Ordering::Relaxed);
}

fn snapshot() -> Vec<ExternalRootProvider> {
    PROVIDERS.read().clone()
}

pub fn scan_external_roots(roots: &mut Vec<ObjectRef>) {
    for provider in snapshot() {
        (provider.scan)(roots);
    }
}

pub fn external_owner_addrs() -> Option<HashSet<usize>> {
    let mut result = HashSet::new();
    for provider in snapshot() {
        if let Some(owners) = (provider.owner_addrs)() {
            result.extend(owners);
        }
    }
    (!result.is_empty()).then_some(result)
}

/// Roots owned by the object at `owner_addr`.
///
/// `owner_class_id` is that object's CURRENT class id, used to reject an owner
/// entry left behind by a previous tenant of the same address — see
/// [`ExternalRootProvider::roots_for_owner`]. Pass `None` only where the class
/// genuinely is not available; the check is skipped then.
pub fn external_roots_for_owner(owner_addr: usize, owner_class_id: Option<u32>) -> Vec<ObjectRef> {
    // THE LATCH FIRST, then iterate IN PLACE -- see [`PROVIDER_COUNT`]. This is
    // the one function here that runs per marked object, so it is the only one
    // that does not go through `snapshot`.
    if PROVIDER_COUNT.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    let mut roots = Vec::new();
    // `read_recursive`, not `read`: a provider callback runs while this guard is
    // held, and `parking_lot`'s plain `read` can deadlock a second read on the
    // same thread when a writer is queued behind it. Nothing registers a provider
    // from inside a root callback today, and this is what makes that a
    // non-question rather than a latent deadlock waiting for the day something
    // does. `Vec::new()` does not allocate until something is pushed, so the
    // no-roots case (the overwhelming majority of objects) is allocation-free.
    for provider in PROVIDERS.read_recursive().iter() {
        roots.extend((provider.roots_for_owner)(owner_addr, owner_class_id));
    }
    roots
}

pub fn external_roots_for_matching_owners(owner_matches: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
    let mut roots = Vec::new();
    for provider in snapshot() {
        roots.extend((provider.roots_for_matching_owners)(owner_matches));
    }
    roots
}

pub fn remap_external_roots(pointer_map: &cratonvm_types::PointerMap) {
    for provider in snapshot() {
        (provider.remap)(pointer_map);
    }
}

pub fn prune_external_roots(is_live: &OwnerPredicate<'_>) {
    for provider in snapshot() {
        (provider.prune)(is_live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OWNER: usize = 0xABCD_0000;
    const ROOT: usize = 0xDCBA_0000;
    static SCANS: AtomicUsize = AtomicUsize::new(0);
    static REMAPS: AtomicUsize = AtomicUsize::new(0);

    fn object(addr: usize) -> ObjectRef {
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }
    fn scan(roots: &mut Vec<ObjectRef>) {
        SCANS.fetch_add(1, Ordering::SeqCst);
        roots.push(object(ROOT));
    }
    fn owners() -> Option<HashSet<usize>> {
        Some(HashSet::from([OWNER]))
    }
    fn roots_for_owner(owner: usize, _class_id: Option<u32>) -> Vec<ObjectRef> {
        (owner == OWNER).then(|| object(ROOT)).into_iter().collect()
    }
    fn matching(predicate: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
        predicate(OWNER).then(|| object(ROOT)).into_iter().collect()
    }
    fn remap(_map: &cratonvm_types::PointerMap) {
        REMAPS.fetch_add(1, Ordering::SeqCst);
    }
    fn prune(_is_live: &OwnerPredicate<'_>) {}

    fn provider() -> ExternalRootProvider {
        ExternalRootProvider {
            name: "gc-test-provider",
            scan,
            owner_addrs: owners,
            roots_for_owner,
            roots_for_matching_owners: matching,
            remap,
            prune,
            gate_stats: None,
        }
    }

    #[test]
    fn complete_provider_is_idempotent_and_fans_out() {
        register_external_root_provider(provider());
        register_external_root_provider(provider());

        SCANS.store(0, Ordering::SeqCst);
        REMAPS.store(0, Ordering::SeqCst);
        let mut roots = Vec::new();
        scan_external_roots(&mut roots);
        remap_external_roots(&cratonvm_types::PointerMap::default());

        assert_eq!(SCANS.load(Ordering::SeqCst), 1);
        assert_eq!(REMAPS.load(Ordering::SeqCst), 1);
        assert!(roots.contains(&object(ROOT)));
        assert!(external_owner_addrs().unwrap().contains(&OWNER));
        assert_eq!(external_roots_for_owner(OWNER, None), vec![object(ROOT)]);
        assert_eq!(
            external_roots_for_matching_owners(&|owner| owner == OWNER),
            vec![object(ROOT)]
        );
    }
}
