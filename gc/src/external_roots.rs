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
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
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
    /// The COMPLETE set of addresses that own roots from this provider, or
    /// `None` when the provider cannot enumerate them.
    ///
    /// `None` is not "no owners" -- it is "do not assume". A marker may skip
    /// the per-object [`Self::roots_for_owner`] call for an address no
    /// provider names, and a provider that returns `None` while still serving
    /// roots would have those edges silently dropped, so `None` disables that
    /// optimisation for every provider at once. Return `Some` of an EMPTY set
    /// to say "I own nothing", which is a fact and is filterable.
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
    // `gate_stats` is compared too. It was the one callback field this omitted,
    // so a re-registration that changed ONLY the gate reporter took the silent
    // `return` below and kept the first one — leaving `provider_gate_stats`
    // reading a counter that belongs to a gate nobody is running any more, which
    // is precisely the "measured inert" reading that field exists to rule out.
    // The name is the identity; every callback behind it has to match.
    a.scan as usize == b.scan as usize
        && a.owner_addrs as usize == b.owner_addrs as usize
        && a.roots_for_owner as usize == b.roots_for_owner as usize
        && a.roots_for_matching_owners as usize == b.roots_for_matching_owners as usize
        && a.remap as usize == b.remap as usize
        && a.prune as usize == b.prune as usize
        && a.gate_stats.map(|f| f as usize) == b.gate_stats.map(|f| f as usize)
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
    // Publish the immutable read-side view BEFORE the latch, so a reader that
    // observes a non-zero count always observes a table that contains that many
    // entries. `PROVIDER_COUNT` is the cheap "is there anything at all" latch;
    // `PUBLISHED` is what the iteration actually walks.
    publish_locked(&providers);
    PROVIDER_COUNT.store(providers.len(), Ordering::Release);
}

/// The lock-free read-side view of [`PROVIDERS`], republished on every
/// registration.
///
/// # Why not just read the `RwLock`
///
/// [`external_roots_for_owner`] runs **once per marked object**, in every
/// marker in the tree, from every marking thread. It took
/// `PROVIDERS.read_recursive()` to do it. An uncontended `parking_lot` read is
/// a few nanoseconds; the same read taken by N marking threads is one shared
/// cache line taken exclusive N times per object, and the measurement on this
/// exact path is on [`PROVIDER_COUNT`]: **four workers cost +153% pause against
/// zero, monotonic in worker count**, which is a lock rather than a start-up
/// offset. `PROVIDER_COUNT` removed the cost for the empty registry (every
/// `--jdk-only` run and every unit test); this removes it for the non-empty one,
/// which is every real application run.
///
/// A `Box::leak` per registration rather than an `Arc` swap: registration is
/// monotone (nothing unregisters) and happens a handful of times at start-up,
/// three call sites in the whole tree, so the superseded tables are a bounded
/// few hundred bytes and the reader needs no reference count — which is the
/// entire point, since a reference count is the shared-line write this exists
/// to delete.
///
/// Null means "nothing registered yet"; readers treat it as the empty slice.
static PUBLISHED: AtomicPtr<Vec<ExternalRootProvider>> = AtomicPtr::new(std::ptr::null_mut());

/// Republish [`PUBLISHED`] from the writer-locked `providers`.
///
/// Caller must hold the [`PROVIDERS`] write lock, which is what serialises the
/// leak against a concurrent registration.
fn publish_locked(providers: &[ExternalRootProvider]) {
    let leaked: &'static mut Vec<ExternalRootProvider> = Box::leak(Box::new(providers.to_vec()));
    PUBLISHED.store(leaked as *mut _, Ordering::Release);
    // Bumped inside the same write lock that mutated the table, and AFTER the
    // publish, so a reader that sees generation N also sees the table that
    // produced it. See `GENERATION` and `provider_table_generation`.
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Monotonically increasing version of the provider table.
///
/// *Added for
/// `docs/internal/zgc-round-20260920/handoff-i-arm-root-filter-concurrently.md`,
/// which continues `gap-a-root-filter-is-off-for-the-whole-concurrent-phase.md`.*
///
/// A concurrent marker that has snapshotted the key sets into
/// `mark_roots`'s extra-root filter needs a lock-free way to ask *"has anything
/// been registered since?"* — and the only lock-free signal this module had was
/// [`PROVIDER_COUNT`], which answers "has anything **ever** been registered".
/// Those are different questions and only the second one was askable.
///
/// # What this does NOT cover, and it matters
///
/// **This versions the PROVIDER table, not the OWNER sets.** It is bumped when
/// a subsystem registers a root provider — a handful of times at start-up —
/// and *not* when an already-registered provider gains a new owner, because
/// that happens inside the provider's own module (a native collection
/// registering an overlay) and this registry never sees it.
///
/// So an unchanged generation here is **not** a proof that
/// [`owner_addrs_and_completeness`] would still answer the same way, and the
/// extra-root filter must not treat it as one. Arming that filter during the
/// concurrent phase needs the same counter on each *owner* table —
/// `types/src/{loader,mirror,metadata}_pin.rs` — plus the mark-end
/// difference-set pass the handoff page describes, and only the union of all
/// of them is a proof. What this counter is genuinely sufficient for is the
/// narrower question it is named after: whether the set of providers the
/// snapshot was taken over is still the set of providers.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current provider-table version — see the `GENERATION` static for what it does
/// and, importantly, does not version.
///
/// `0` means no provider has ever been registered.
pub(crate) fn provider_table_generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// [`owner_addrs_and_completeness`] plus the [`provider_table_generation`] the
/// answer was computed at, read **before** the walk.
///
/// # The ordering, which was wrong here until 2026-09-21
///
/// *Corrected by
/// `docs/internal/zgc-round-20260920/handoff-r-wire-the-concurrent-root-filter.md`.*
///
/// This function used to read the generation **after** the walk, with a comment
/// arguing that reading it before would pair a mid-walk registration with a
/// generation that predates it. That is backwards, and it inverted the one
/// property the pair exists to have.
///
/// The claim a caller makes from this pair is: *"the generation is still `g`,
/// therefore the owner set I hold is still complete."* Work it through both
/// ways, remembering that [`snapshot`] loads [`PUBLISHED`] **once** and then
/// iterates that immutable table, so a provider registered mid-walk is
/// certainly absent from the result:
///
/// * **Generation read after the walk.** Provider registers mid-walk →
///   [`publish_locked`] bumps `GENERATION` → this function then reads the
///   *new* value. The caller holds an owner set that is missing that
///   provider's owners, labelled with a generation that already counts it. The
///   revalidation passes, the filter keeps excluding an address that provider
///   owns roots for, and the overlay is swept while its owner survives. **A
///   false proof.**
/// * **Generation read before the walk** (what it does now). The same
///   registration bumps `GENERATION` after the value was read, so the caller's
///   later revalidation sees a *different* generation and discards the
///   snapshot — even though the walk may in fact have included the new
///   provider. The error is in the direction of discarding a snapshot that was
///   still good, which costs one cycle of the slow path.
///
/// The pairing with [`publish_locked`]'s own order matters and is already
/// right: it stores `PUBLISHED` and only *then* bumps `GENERATION`. Reading the
/// generation first and the table second therefore cannot produce a table older
/// than the generation, only newer, which is the harmless direction.
///
/// A caller revalidates by re-reading [`provider_table_generation`] and
/// discarding the snapshot on any change — and must still honour the limitation
/// on the `GENERATION` static: **an unchanged value does not mean the owner
/// sets are unchanged**, only that the set of providers is.
pub fn owner_addrs_and_completeness_with_generation() -> (HashSet<usize>, bool, u64) {
    let generation = provider_table_generation();
    let (owners, complete) = owner_addrs_and_completeness();
    (owners, complete, generation)
}

/// The registered providers, as a borrowed slice, with no lock and no
/// allocation.
///
/// Carries its own length (a `Vec` behind one pointer), so there is no window
/// in which a reader can pair a new count with an old table.
#[inline]
fn published() -> &'static [ExternalRootProvider] {
    let p = PUBLISHED.load(Ordering::Acquire);
    if p.is_null() {
        return &[];
    }
    // SAFETY: `PUBLISHED` only ever holds a pointer produced by `Box::leak` in
    // `publish_locked`, which is `&'static mut` and therefore valid for the rest
    // of the process. Superseded tables are leaked rather than freed, so a
    // pointer read here can never dangle. The referent is never mutated after
    // publication — `publish_locked` builds a fresh `Vec` and swaps the pointer
    // — so handing out a shared reference introduces no aliasing conflict.
    unsafe { &*p }
}

fn snapshot() -> &'static [ExternalRootProvider] {
    published()
}

pub fn scan_external_roots(roots: &mut Vec<ObjectRef>) {
    for provider in snapshot() {
        (provider.scan)(roots);
    }
}

/// `(owners, complete)`: every address any provider names, and whether ALL
/// of them were able to say. See [`ExternalRootProvider::owner_addrs`] -- a
/// single `None` makes the whole index unusable for exclusion.
pub fn owner_addrs_and_completeness() -> (HashSet<usize>, bool) {
    let mut owners = HashSet::new();
    let mut complete = true;
    for provider in snapshot() {
        match (provider.owner_addrs)() {
            Some(set) => owners.extend(set),
            None => complete = false,
        }
    }
    (owners, complete)
}

/// The union of every provider's owner set, or `None` when that union is
/// empty.
///
/// # This function DISCARDS incompleteness — prefer [`owner_addrs_and_completeness`]
///
/// A provider that answers `None` means "do not assume", not "I own nothing"
/// ([`ExternalRootProvider::owner_addrs`]). This function silently drops such a
/// provider from the union and returns a set that reads as authoritative. A
/// caller that uses the result to SKIP the per-object
/// [`external_roots_for_owner`] call — which is the only reason to want an
/// owner index — then drops that provider's edges for every object, which is a
/// premature reclamation of anything held only through an external root.
///
/// `gc/src/zgc/mark_roots.rs` takes [`owner_addrs_and_completeness`] and stores
/// the `complete` half as `overlay_unfilterable`, which is the correct shape.
/// `gen_heap.rs`'s young marker takes this one and gates on
/// `overlay_owners.as_ref().is_some_and(|o| o.contains(&addr))`, so it inherits
/// the hazard.
///
/// It is LATENT, not live: the one production provider
/// (`native-collections`' `gc_overlay_owner_addrs`) deliberately returns
/// `Some` of an empty set and says so in its own comment; the only `|| None`
/// in the tree is a ZGC test provider, and ZGC reads completeness. Kept as a
/// separate entry point rather than fixed here because "return `None` when any
/// provider is incomplete" would make `gen_heap`'s gate skip MORE, not less —
/// the repair belongs at that consumer.
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
    let mut roots = Vec::new();
    external_roots_for_owner_into(owner_addr, owner_class_id, &mut roots);
    roots
}

/// [`external_roots_for_owner`], appending into a caller-owned buffer.
///
/// This is the form a marker should use: it runs once per marked object, so the
/// `Vec` it returns is a heap allocation per object per provider that has
/// anything to say. A marker holding one buffer across the whole cycle
/// allocates once and reuses the capacity, and the provider's own result vector
/// is the only one left — which is why `roots_for_owner` keeps its signature
/// (three registration sites, one of them real, and its empty answer already
/// costs nothing because `Vec::new()` does not allocate).
///
/// No lock: see [`PUBLISHED`]. The latch is checked first so the empty registry
/// — every `--jdk-only` run and every unit test — is one relaxed load.
pub fn external_roots_for_owner_into(
    owner_addr: usize,
    owner_class_id: Option<u32>,
    out: &mut Vec<ObjectRef>,
) {
    if PROVIDER_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    for provider in published() {
        let owned = (provider.roots_for_owner)(owner_addr, owner_class_id);
        if !owned.is_empty() {
            out.extend(owned);
        }
    }
}

thread_local! {
    /// Per-thread scratch for [`with_external_roots_for_owner`].
    ///
    /// One buffer per marking thread for the whole cycle, so the per-object
    /// path allocates once ever instead of once per owning object. `RefCell`
    /// rather than a bare `Cell`/`UnsafeCell` so reentrancy is *detected*
    /// rather than assumed away — see the fallback in the function below.
    static OWNER_SCRATCH: std::cell::RefCell<Vec<ObjectRef>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Call `visit` with the roots owned by the object at `owner_addr`.
///
/// The allocation-free form of [`external_roots_for_owner`], and the one every
/// per-object marker should use. The slice handed to `visit` borrows a
/// thread-local buffer that lives for the whole cycle, so a marker that walks
/// ten million objects performs at most one allocation between them instead of
/// one per object that owns anything.
///
/// # Reentrancy
///
/// If `visit` (or a provider callback) re-enters this function on the same
/// thread, the scratch is already borrowed and this falls back to a fresh
/// `Vec` for the inner call. Nothing in the tree does that today; the point is
/// that if something starts to, it gets a correct answer rather than a panic
/// or an aliased buffer.
pub fn with_external_roots_for_owner<R>(
    owner_addr: usize,
    owner_class_id: Option<u32>,
    visit: impl FnOnce(&[ObjectRef]) -> R,
) -> R {
    if PROVIDER_COUNT.load(Ordering::Relaxed) == 0 {
        return visit(&[]);
    }
    let borrowed = OWNER_SCRATCH.with(|cell| cell.try_borrow_mut().is_ok());
    if !borrowed {
        let mut fresh = Vec::new();
        external_roots_for_owner_into(owner_addr, owner_class_id, &mut fresh);
        return visit(&fresh);
    }
    OWNER_SCRATCH.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        external_roots_for_owner_into(owner_addr, owner_class_id, &mut buf);
        visit(&buf)
    })
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

    /// Set only by [`the_paired_generation_is_read_before_the_walk`], on its own
    /// thread, so a test running concurrently in this binary cannot consume the
    /// one nested registration this fixture performs.
    thread_local! {
        static NEST_ON_NEXT_WALK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    const NESTED_OWNER: usize = 0xABCE_0000;

    fn nested_owners() -> Option<HashSet<usize>> {
        Some(HashSet::from([NESTED_OWNER]))
    }
    fn nested_roots_for_owner(_owner: usize, _class_id: Option<u32>) -> Vec<ObjectRef> {
        Vec::new()
    }
    fn nested_matching(_predicate: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
        Vec::new()
    }
    fn nested_scan(_roots: &mut Vec<ObjectRef>) {}
    /// A no-op remap. Deliberately NOT the counting `remap` above: these two
    /// fixture providers stay registered for the life of the process, and
    /// reusing it would make `complete_provider_is_idempotent_and_fans_out`'s
    /// `REMAPS == 1` depend on test order.
    fn nested_remap(_map: &cratonvm_types::PointerMap) {}

    /// A provider that registers ANOTHER provider from inside its own
    /// `owner_addrs` callback — i.e. exactly the mid-walk registration the
    /// generation pairing has to survive. `snapshot()` loaded `PUBLISHED` before
    /// the walk began, so the nested provider is certainly absent from the
    /// owner set this walk returns.
    fn nesting_owners() -> Option<HashSet<usize>> {
        let armed = NEST_ON_NEXT_WALK.with(|c| c.replace(false));
        if armed {
            register_external_root_provider(ExternalRootProvider {
                name: "gc-test-provider-nested",
                scan: nested_scan,
                owner_addrs: nested_owners,
                roots_for_owner: nested_roots_for_owner,
                roots_for_matching_owners: nested_matching,
                remap: nested_remap,
                prune,
                gate_stats: None,
            });
        }
        Some(HashSet::new())
    }

    /// **The generation must be read BEFORE the walk, or the pair is a false
    /// proof.**
    ///
    /// The caller's claim is "the generation is still `g`, so my owner set is
    /// still complete". A provider registered mid-walk is missing from the set
    /// this call returns — `snapshot()` iterates one immutable, already-loaded
    /// table. If the generation were read afterwards it would already count
    /// that provider, the caller's revalidation would pass, and the extra-root
    /// filter would keep excluding an address the new provider owns roots for:
    /// an overlay swept while its owner is live.
    ///
    /// Read before, the pair invalidates itself, which is the safe direction.
    #[test]
    fn the_paired_generation_is_read_before_the_walk() {
        register_external_root_provider(ExternalRootProvider {
            name: "gc-test-provider-nesting",
            scan: nested_scan,
            owner_addrs: nesting_owners,
            roots_for_owner: nested_roots_for_owner,
            roots_for_matching_owners: nested_matching,
            remap: nested_remap,
            prune,
            gate_stats: None,
        });

        NEST_ON_NEXT_WALK.with(|c| c.set(true));
        let (owners, _complete, paired) = owner_addrs_and_completeness_with_generation();
        NEST_ON_NEXT_WALK.with(|c| c.set(false));
        let after = provider_table_generation();

        assert!(
            !owners.contains(&NESTED_OWNER),
            "the walk iterates the table it loaded first, so a provider \
             registered during it cannot be in the result -- if this fires the \
             fixture is no longer testing the hazard"
        );
        assert_ne!(
            paired, after,
            "the generation was read AFTER the walk: an owner set missing the \
             nested provider is now labelled with a generation that counts it, \
             and a caller revalidating against it holds a false proof"
        );
        assert!(paired < after);
    }

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
