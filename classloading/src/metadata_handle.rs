// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational handles for VM metadata (C2 review, P1).
//!
//! # Why this exists
//!
//! A bare [`ClassId`] is an **index into one VM's mutable class table**, and
//! nothing about the value records which table it indexes or which definition
//! of that class it was taken against. Two facts make that unsafe (see
//! `docs/architecture/per-vm-state.md` §0):
//!
//! * **`ClassId`s are allocated per VM.** `ClassStore::next_id` returns
//!   `self.classes.len()`, and every `SharedVm` owns its own `ClassStore`, so
//!   `ClassId(7)` names a *different class in every VM*. Any long-lived
//!   `ClassId` — in a JIT inline cache, a vtable index, a native side table —
//!   silently aliases across VMs.
//! * **Class metadata is mutable in place.** JVMTI `RedefineClasses` and
//!   mockito-style inline redefinition replace a class's `methods` vec and
//!   constant pool under a live `ClassId`; the synthetic-stub upgrade path
//!   goes further and shifts `first_field_index` for the class *and every
//!   subclass*. A memoized `(class_id, method_index)` or
//!   `(class_id, field_index)` pair therefore names something else afterwards.
//!
//! Both failures are silent today: the index is still in range, the lookup
//! still succeeds, and the caller gets the wrong metadata.
//!
//! # The handle
//!
//! [`ClassHandle`] is `(VmId, generation, ClassId)` — 16 bytes, `Copy`, and
//! usable as a map key. [`MethodHandle`] and [`FieldHandle`] add an index into
//! the class's own table and inherit the class's generation, because the events
//! that renumber those tables are exactly the events that bump it.
//!
//! Resolution is checked and every rejection is *named*:
//! [`StaleMetadata::WrongVm`], [`StaleMetadata::OutOfRange`],
//! [`StaleMetadata::Unloaded`] and [`StaleMetadata::StaleGeneration`] are
//! distinct variants with distinct [`StaleMetadataKind`]s, so a caller can log
//! (and a test can assert) *which* invariant failed rather than seeing a bare
//! `None` that reads the same as "not loaded yet".
//!
//! # Relationship to `ClassId`
//!
//! This is **purely additive**. `ClassId` keeps working everywhere; the handle
//! is the safe wrapper you mint at the point where a reference is about to
//! outlive the borrow that produced it, and check again at the transition
//! boundary where it is used. Migration is per-call-site.
//!
//! # Relationship to `ClassManager::class_redefine_generation`
//!
//! [`ClassManager`](crate::ClassManager) already carries a per-class
//! `redefine_generations` counter, bumped by `redefine_class` and consulted by
//! the per-thread invoke cache. The realm's generation is a **separate**
//! counter deliberately: it is bumped on strictly more events (redefine, class
//! unload, and the synthetic-stub layout upgrade that renumbers fields), so
//! folding the two together would change what the existing invoke-cache gate
//! evicts. They should be unified once every consumer of the redefine counter
//! has migrated to handles; until then the realm counter is the authoritative
//! one for handle validity and the redefine counter is left byte-for-byte as it
//! was.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use cratonvm_native_api::VmId;
use cratonvm_types::ClassId;

/// The realm identity of a [`MetadataRealm`] that has not been bound to a VM.
///
/// VM identities are allocated from `NEXT_VM_IDENTITY`, which starts at 1
/// (`vm/src/vm/vm_init.rs:9`), so zero can never collide with a real VM. A
/// handle minted from an unbound realm carries this identity and is therefore
/// rejected with [`StaleMetadata::WrongVm`] the moment the realm is bound —
/// the fail-closed direction.
pub const UNBOUND_VM: VmId = VmId::from_raw(0);

/// Generation value meaning "this class has been redefined so many times that
/// its generation counter is exhausted".
///
/// Reached only after 2^32 − 1 identity-invalidating events on a single class,
/// which no real workload approaches. It is handled explicitly anyway because
/// both alternatives are unsound: a wrapping counter would make a
/// generation-0 handle valid again, and a saturating counter that is still
/// *accepted* would make every handle minted at the ceiling permanently valid.
/// Instead the ceiling is poison — the realm refuses to mint new handles for
/// the class and rejects every existing one.
pub const EXHAUSTED_GENERATION: u32 = u32::MAX;

/// Raw value of [`UNBOUND_VM`], for the atomic that backs the realm.
const UNBOUND_RAW: usize = 0;

// ---------------------------------------------------------------------------
// Failure taxonomy
// ---------------------------------------------------------------------------

/// Which metadata table a handle indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetadataKind {
    /// The VM's class table (`ClassStore`).
    Class,
    /// A class's own `methods` vec.
    Method,
    /// A class's own `fields` vec.
    Field,
}

impl fmt::Display for MetadataKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataKind::Class => write!(f, "class"),
            MetadataKind::Method => write!(f, "method"),
            MetadataKind::Field => write!(f, "field"),
        }
    }
}

/// The *kind* of a [`StaleMetadata`] rejection, without the diagnostic payload.
///
/// Exists so callers (and tests) can compare failure kinds without matching on
/// the ids and counters that vary run to run. The whole point of the taxonomy
/// is that these four are distinguishable from one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StaleMetadataKind {
    /// The handle was minted by a different VM's metadata realm.
    WrongVm,
    /// The index is past the end of the table it names.
    OutOfRange,
    /// The class slot exists but holds a tombstone — the class was unloaded.
    Unloaded,
    /// The slot is live, but its metadata was replaced after the handle was
    /// minted.
    StaleGeneration,
}

impl fmt::Display for StaleMetadataKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StaleMetadataKind::WrongVm => write!(f, "wrong-vm"),
            StaleMetadataKind::OutOfRange => write!(f, "out-of-range"),
            StaleMetadataKind::Unloaded => write!(f, "unloaded"),
            StaleMetadataKind::StaleGeneration => write!(f, "stale-generation"),
        }
    }
}

/// A checked metadata resolution that failed, with enough detail to name the
/// offending handle in a log line or an assertion.
///
/// Deliberately **not** collapsed into `Option::None`: "this handle belongs to
/// another VM" and "this class has not been loaded yet" are different bugs with
/// different fixes, and a bare `None` makes the first look like the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleMetadata {
    /// The handle came from another VM's realm. Under the per-VM `ClassId`
    /// allocation this is the case that used to resolve *successfully* to an
    /// unrelated class.
    WrongVm {
        /// VM identity recorded in the handle.
        handle_vm: VmId,
        /// VM identity of the realm the handle was presented to.
        realm_vm: VmId,
        /// The index the handle carried, for the log line only — it must not
        /// be used to look anything up.
        class: ClassId,
    },
    /// The index is past the end of its table.
    OutOfRange {
        /// Which table.
        kind: MetadataKind,
        /// Owning class (equal to `index` when `kind` is
        /// [`MetadataKind::Class`]).
        class: ClassId,
        /// The out-of-range index.
        index: u32,
        /// The table's current length.
        len: u32,
    },
    /// The class slot is in range but was unloaded; `ClassStore` leaves a
    /// tombstone so the id can never alias a later definition.
    Unloaded {
        /// The unloaded class.
        class: ClassId,
    },
    /// The class is live, but its metadata generation moved on — redefinition,
    /// a synthetic-stub layout upgrade, or unload.
    StaleGeneration {
        /// The class whose generation moved.
        class: ClassId,
        /// Generation recorded in the handle.
        handle_generation: u32,
        /// Generation the realm holds now.
        live_generation: u32,
    },
}

impl StaleMetadata {
    /// The failure kind, stripped of diagnostic payload.
    pub fn kind(&self) -> StaleMetadataKind {
        match self {
            StaleMetadata::WrongVm { .. } => StaleMetadataKind::WrongVm,
            StaleMetadata::OutOfRange { .. } => StaleMetadataKind::OutOfRange,
            StaleMetadata::Unloaded { .. } => StaleMetadataKind::Unloaded,
            StaleMetadata::StaleGeneration { .. } => StaleMetadataKind::StaleGeneration,
        }
    }

    /// The class the rejected handle named. Useful for a dependency-invalidation
    /// sweep: the set of classes that produced `StaleGeneration` is exactly the
    /// set whose compiled code must be retired.
    pub fn class(&self) -> ClassId {
        match self {
            StaleMetadata::WrongVm { class, .. }
            | StaleMetadata::OutOfRange { class, .. }
            | StaleMetadata::Unloaded { class }
            | StaleMetadata::StaleGeneration { class, .. } => *class,
        }
    }
}

impl fmt::Display for StaleMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StaleMetadata::WrongVm {
                handle_vm,
                realm_vm,
                class,
            } => write!(
                f,
                "wrong-vm: handle for class {class} was minted by {handle_vm} \
                 but presented to {realm_vm}"
            ),
            StaleMetadata::OutOfRange {
                kind,
                class,
                index,
                len,
            } => write!(
                f,
                "out-of-range: {kind} index {index} of class {class} is past the \
                 table length {len}"
            ),
            StaleMetadata::Unloaded { class } => {
                write!(f, "unloaded: class {class} was unloaded (tombstoned slot)")
            }
            StaleMetadata::StaleGeneration {
                class,
                handle_generation,
                live_generation,
            } => write!(
                f,
                "stale-generation: class {class} handle is at generation \
                 {handle_generation} but the live metadata is at {live_generation}"
            ),
        }
    }
}

impl std::error::Error for StaleMetadata {}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// A generational handle to one class's metadata.
///
/// `(VmId, generation, ClassId)`. Safe to store for as long as you like; unsafe
/// to *use* without going back through
/// [`ClassManager::resolve_class_handle`](crate::ClassManager::resolve_class_handle)
/// (or [`MetadataRealm::resolve_class`], if you own the table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClassHandle {
    vm: VmId,
    generation: u32,
    index: ClassId,
}

impl ClassHandle {
    /// Construct a handle from its three components.
    ///
    /// Prefer [`MetadataRealm::mint_class`] or
    /// [`ClassManager::class_handle`](crate::ClassManager::class_handle), which
    /// read the current generation for you. This constructor exists for
    /// deserialization (a persisted JIT profile, a JVMTI agent's saved state)
    /// and for tests that need to fabricate a deliberately stale handle.
    pub const fn from_parts(vm: VmId, generation: u32, index: ClassId) -> Self {
        Self {
            vm,
            generation,
            index,
        }
    }

    /// The VM whose metadata realm minted this handle.
    pub const fn vm(self) -> VmId {
        self.vm
    }

    /// The metadata generation this handle was minted against.
    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// The raw `ClassId` **without any validity check**.
    ///
    /// Named `_unchecked` because reading it is exactly the mistake this type
    /// exists to prevent: it is meaningful only in the minting VM, and only at
    /// the generation recorded alongside it. Legitimate uses are diagnostics
    /// (log lines, `Debug` output) and cache keys that are re-verified on hit.
    /// Anything that dereferences metadata must call a `resolve_*` first.
    pub const fn class_id_unchecked(self) -> ClassId {
        self.index
    }
}

impl fmt::Display for ClassHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/c{}@g{}", self.vm, self.index, self.generation)
    }
}

/// A generational handle to one method of one class.
///
/// Carries the owning [`ClassHandle`], so it is invalidated by every event that
/// invalidates the class — which is the right granularity, because
/// `RedefineClasses` replaces the whole `methods` vec and renumbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodHandle {
    class: ClassHandle,
    index: u32,
}

impl MethodHandle {
    /// Construct a method handle from an already-minted class handle.
    pub const fn from_parts(class: ClassHandle, index: u32) -> Self {
        Self { class, index }
    }

    /// The owning class handle.
    pub const fn class(self) -> ClassHandle {
        self.class
    }

    /// Index into the declaring class's `methods` vec, unchecked. See
    /// [`ClassHandle::class_id_unchecked`] for why this is spelled out.
    pub const fn index_unchecked(self) -> u32 {
        self.index
    }
}

impl fmt::Display for MethodHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#m{}", self.class, self.index)
    }
}

/// A generational handle to one field of one class.
///
/// Same generation as the owning class: the synthetic-stub upgrade path
/// rewrites `first_field_index` for a class *and every subclass*, so a field
/// index is only meaningful at the generation it was taken against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldHandle {
    class: ClassHandle,
    index: u32,
}

impl FieldHandle {
    /// Construct a field handle from an already-minted class handle.
    pub const fn from_parts(class: ClassHandle, index: u32) -> Self {
        Self { class, index }
    }

    /// The owning class handle.
    pub const fn class(self) -> ClassHandle {
        self.class
    }

    /// Index into the declaring class's `fields` vec, unchecked.
    pub const fn index_unchecked(self) -> u32 {
        self.index
    }
}

impl fmt::Display for FieldHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#f{}", self.class, self.index)
    }
}

// ---------------------------------------------------------------------------
// MetadataRealm
// ---------------------------------------------------------------------------

/// The identity and generation bookkeeping for one VM's metadata tables.
///
/// One realm per [`ClassManager`](crate::ClassManager). It answers two
/// questions the class tables themselves cannot:
///
/// 1. **Whose table is this?** — [`MetadataRealm::vm`], compared against every
///    handle presented.
/// 2. **Is this class still the class it was?** — a per-class generation,
///    bumped by [`MetadataRealm::bump_generation`] on every event that changes
///    what a `ClassId` names.
///
/// Both are cheap on the steady-state path: the VM identity is a relaxed atomic
/// load, and the generation map is not even locked until the first bump (see
/// `tracked`).
pub struct MetadataRealm {
    /// `VmId::as_usize` of the owning VM, or [`UNBOUND_RAW`].
    vm: AtomicUsize,
    /// Number of entries in `generations`, so the overwhelmingly common
    /// "nothing was ever redefined or unloaded" case answers generation 0
    /// without taking the lock at all.
    tracked: AtomicUsize,
    /// Per-class metadata generation. Absent means 0 — a class that has never
    /// been redefined, unloaded or re-laid-out costs nothing here.
    generations: RwLock<FxHashMap<ClassId, u32>>,
}

impl MetadataRealm {
    /// An unbound realm. Bind it with [`MetadataRealm::bind_vm`] before any
    /// class is loaded — handles minted while unbound carry [`UNBOUND_VM`] and
    /// are rejected as [`StaleMetadata::WrongVm`] once binding happens.
    pub fn new() -> Self {
        Self {
            vm: AtomicUsize::new(UNBOUND_RAW),
            tracked: AtomicUsize::new(0),
            generations: RwLock::new(FxHashMap::default()),
        }
    }

    /// A realm already bound to `vm`.
    pub fn for_vm(vm: VmId) -> Self {
        Self {
            vm: AtomicUsize::new(vm.as_usize()),
            tracked: AtomicUsize::new(0),
            generations: RwLock::new(FxHashMap::default()),
        }
    }

    /// The VM this realm belongs to, or [`UNBOUND_VM`].
    pub fn vm(&self) -> VmId {
        VmId::from_raw(self.vm.load(Ordering::Acquire))
    }

    /// Whether a VM identity has been installed.
    pub fn is_bound(&self) -> bool {
        self.vm.load(Ordering::Acquire) != UNBOUND_RAW
    }

    /// Bind this realm to `vm`.
    ///
    /// Idempotent for the same identity. Returns `Err(current)` if the realm is
    /// already bound to a *different* VM — that means two VMs are sharing one
    /// `ClassManager`, which is the contamination this whole module exists to
    /// make impossible, so the caller must treat it as a hard error rather than
    /// silently rebinding.
    pub fn bind_vm(&self, vm: VmId) -> Result<(), VmId> {
        let raw = vm.as_usize();
        match self
            .vm
            .compare_exchange(UNBOUND_RAW, raw, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(current) if current == raw => Ok(()),
            Err(current) => Err(VmId::from_raw(current)),
        }
    }

    /// The current metadata generation of `id`. Zero for any class that has
    /// never been through an identity-invalidating event.
    pub fn generation(&self, id: ClassId) -> u32 {
        // Fast path: no class in this VM has ever been redefined, unloaded or
        // re-laid-out, so every generation is 0 and the lock is not worth
        // taking. This is the state of essentially every process.
        if self.tracked.load(Ordering::Acquire) == 0 {
            return 0;
        }
        self.generations.read().get(&id).copied().unwrap_or(0)
    }

    /// Advance `id`'s generation, invalidating every handle already minted for
    /// it. Returns the new generation.
    ///
    /// Call this from every event that changes what the `ClassId` names:
    /// redefinition, class unload, and any layout recompute that renumbers the
    /// class's fields or methods.
    pub fn bump_generation(&self, id: ClassId) -> u32 {
        let mut guard = self.generations.write();
        let slot = guard.entry(id).or_insert(0);
        // Saturating, with `EXHAUSTED_GENERATION` treated as poison by
        // `mint_class` / `resolve_class` — see that constant's doc comment for
        // why neither wrapping nor plain saturation is sound on its own.
        *slot = slot.saturating_add(1);
        let new = *slot;
        let len = guard.len();
        drop(guard);
        self.tracked.store(len, Ordering::Release);
        new
    }

    /// Advance the generation of every class in `ids`, taking the write lock
    /// once. Used by the class-unload transaction, which invalidates a whole
    /// loader's worth of classes at a time.
    pub fn bump_generations<I: IntoIterator<Item = ClassId>>(&self, ids: I) {
        let mut guard = self.generations.write();
        for id in ids {
            let slot = guard.entry(id).or_insert(0);
            *slot = slot.saturating_add(1);
        }
        let len = guard.len();
        drop(guard);
        self.tracked.store(len, Ordering::Release);
    }

    /// Number of classes with a non-default generation. Diagnostics and tests.
    pub fn tracked_classes(&self) -> usize {
        self.tracked.load(Ordering::Acquire)
    }

    /// Mint a handle for `id` at its current generation.
    ///
    /// Does **not** check that `id` is live — the realm does not own the class
    /// table. [`ClassManager::class_handle`](crate::ClassManager::class_handle)
    /// is the checked entry point and is what callers should use. Returns
    /// `None` only when the class's generation counter is exhausted.
    pub fn mint_class(&self, id: ClassId) -> Option<ClassHandle> {
        let generation = self.generation(id);
        if generation == EXHAUSTED_GENERATION {
            return None;
        }
        Some(ClassHandle {
            vm: self.vm(),
            generation,
            index: id,
        })
    }

    /// Check `handle` against this realm and the caller's table shape.
    ///
    /// `slot_count` is the number of *slots* in the class table (including
    /// tombstones — `ClassStore::slot_count`, not `ClassStore::len`), and
    /// `slot_is_live` says whether that slot currently holds a class. Both are
    /// supplied by the caller because the realm deliberately does not own the
    /// table; this also makes the check unit-testable without a `ClassManager`.
    ///
    /// The order of the checks is the order of decreasing severity, so the
    /// error a caller sees names the *worst* thing wrong with the handle:
    /// cross-VM first (silent wrong answer), then range, then unload, then
    /// generation.
    pub fn resolve_class(
        &self,
        handle: ClassHandle,
        slot_count: u32,
        slot_is_live: bool,
    ) -> Result<ClassId, StaleMetadata> {
        let realm_vm = self.vm();
        if handle.vm != realm_vm {
            return Err(StaleMetadata::WrongVm {
                handle_vm: handle.vm,
                realm_vm,
                class: handle.index,
            });
        }
        let index = handle.index.as_u32();
        if index >= slot_count {
            return Err(StaleMetadata::OutOfRange {
                kind: MetadataKind::Class,
                class: handle.index,
                index,
                len: slot_count,
            });
        }
        if !slot_is_live {
            return Err(StaleMetadata::Unloaded {
                class: handle.index,
            });
        }
        let live_generation = self.generation(handle.index);
        if live_generation != handle.generation || live_generation == EXHAUSTED_GENERATION {
            return Err(StaleMetadata::StaleGeneration {
                class: handle.index,
                handle_generation: handle.generation,
                live_generation,
            });
        }
        Ok(handle.index)
    }
}

impl Default for MetadataRealm {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MetadataRealm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataRealm")
            .field("vm", &self.vm())
            .field("tracked_classes", &self.tracked_classes())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VM_A: VmId = VmId::from_raw(1);
    const VM_B: VmId = VmId::from_raw(2);

    /// Resolve against a table of `slots` slots whose named slot is live.
    fn resolve_live(
        realm: &MetadataRealm,
        handle: ClassHandle,
        slots: u32,
    ) -> Result<ClassId, StaleMetadata> {
        realm.resolve_class(handle, slots, true)
    }

    #[test]
    fn live_handle_round_trips() {
        let realm = MetadataRealm::for_vm(VM_A);
        let handle = realm.mint_class(ClassId::new(7)).expect("mint");
        assert_eq!(handle.vm(), VM_A);
        assert_eq!(handle.generation(), 0);
        assert_eq!(handle.class_id_unchecked(), ClassId::new(7));
        assert_eq!(resolve_live(&realm, handle, 16), Ok(ClassId::new(7)));
    }

    #[test]
    fn handle_from_vm_a_does_not_resolve_in_vm_b() {
        let realm_a = MetadataRealm::for_vm(VM_A);
        let realm_b = MetadataRealm::for_vm(VM_B);

        // Both VMs have a class at the same numeric id — the whole point of
        // Fact 1 in `docs/architecture/per-vm-state.md`.
        let handle_a = realm_a.mint_class(ClassId::new(7)).expect("mint");
        let handle_b = realm_b.mint_class(ClassId::new(7)).expect("mint");
        assert_ne!(
            handle_a, handle_b,
            "same ClassId in two VMs must not compare equal"
        );

        // In its own realm each resolves.
        assert!(resolve_live(&realm_a, handle_a, 16).is_ok());
        assert!(resolve_live(&realm_b, handle_b, 16).is_ok());

        // Crossed, both fail, and they fail by *name* rather than by returning
        // the other VM's class.
        let err =
            resolve_live(&realm_b, handle_a, 16).expect_err("VM A handle must not resolve in VM B");
        assert_eq!(err.kind(), StaleMetadataKind::WrongVm);
        assert_eq!(
            err,
            StaleMetadata::WrongVm {
                handle_vm: VM_A,
                realm_vm: VM_B,
                class: ClassId::new(7),
            }
        );
        assert_eq!(
            resolve_live(&realm_a, handle_b, 16).map_err(|e| e.kind()),
            Err(StaleMetadataKind::WrongVm)
        );
    }

    #[test]
    fn handle_stale_by_one_generation_is_rejected_and_says_so() {
        let realm = MetadataRealm::for_vm(VM_A);
        let id = ClassId::new(3);
        let handle = realm.mint_class(id).expect("mint");

        assert_eq!(realm.bump_generation(id), 1);

        let err = resolve_live(&realm, handle, 8).expect_err("stale handle must be rejected");
        assert_eq!(err.kind(), StaleMetadataKind::StaleGeneration);
        assert_eq!(
            err,
            StaleMetadata::StaleGeneration {
                class: id,
                handle_generation: 0,
                live_generation: 1,
            }
        );
        // The message names the class and both generations — this is what a
        // dependency-invalidation sweep logs.
        let rendered = err.to_string();
        assert!(rendered.contains("stale-generation"), "{rendered}");
        assert!(rendered.contains("generation 0"), "{rendered}");

        // A freshly minted handle at the new generation works again.
        let fresh = realm.mint_class(id).expect("re-mint");
        assert_eq!(fresh.generation(), 1);
        assert_eq!(resolve_live(&realm, fresh, 8), Ok(id));
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        let realm = MetadataRealm::for_vm(VM_A);
        let handle = realm.mint_class(ClassId::new(9)).expect("mint");
        let err =
            resolve_live(&realm, handle, 9).expect_err("index 9 with 9 slots is past the end");
        assert_eq!(err.kind(), StaleMetadataKind::OutOfRange);
        assert_eq!(
            err,
            StaleMetadata::OutOfRange {
                kind: MetadataKind::Class,
                class: ClassId::new(9),
                index: 9,
                len: 9,
            }
        );
        // An empty table rejects everything, including id 0.
        assert_eq!(
            realm
                .resolve_class(realm.mint_class(ClassId::new(0)).unwrap(), 0, true)
                .map_err(|e| e.kind()),
            Err(StaleMetadataKind::OutOfRange)
        );
    }

    #[test]
    fn tombstoned_slot_reports_unloaded_not_out_of_range() {
        let realm = MetadataRealm::for_vm(VM_A);
        let id = ClassId::new(2);
        let handle = realm.mint_class(id).expect("mint");
        let err = realm
            .resolve_class(handle, 8, false)
            .expect_err("a tombstoned slot must not resolve");
        assert_eq!(err.kind(), StaleMetadataKind::Unloaded);
        assert_eq!(err, StaleMetadata::Unloaded { class: id });
    }

    #[test]
    fn the_four_failure_kinds_are_distinguishable() {
        let realm_a = MetadataRealm::for_vm(VM_A);
        let realm_b = MetadataRealm::for_vm(VM_B);
        let id = ClassId::new(1);

        let foreign = realm_b.mint_class(id).expect("mint");
        let live = realm_a.mint_class(id).expect("mint");
        realm_a.bump_generation(id);

        let wrong_vm = realm_a.resolve_class(foreign, 8, true).unwrap_err();
        let out_of_range = realm_a
            .resolve_class(realm_a.mint_class(ClassId::new(99)).unwrap(), 8, true)
            .unwrap_err();
        let unloaded = realm_a
            .resolve_class(realm_a.mint_class(id).unwrap(), 8, false)
            .unwrap_err();
        let stale = realm_a.resolve_class(live, 8, true).unwrap_err();

        let kinds = [
            wrong_vm.kind(),
            out_of_range.kind(),
            unloaded.kind(),
            stale.kind(),
        ];
        assert_eq!(
            kinds,
            [
                StaleMetadataKind::WrongVm,
                StaleMetadataKind::OutOfRange,
                StaleMetadataKind::Unloaded,
                StaleMetadataKind::StaleGeneration,
            ]
        );
        // Pairwise distinct — the property the acceptance criterion asks for.
        for i in 0..kinds.len() {
            for j in (i + 1)..kinds.len() {
                assert_ne!(kinds[i], kinds[j], "failure kinds {i} and {j} collide");
            }
        }
        // And the rendered messages are distinct too, so a log line is enough
        // to tell them apart without a debugger.
        let rendered = [
            wrong_vm.to_string(),
            out_of_range.to_string(),
            unloaded.to_string(),
            stale.to_string(),
        ];
        for i in 0..rendered.len() {
            for j in (i + 1)..rendered.len() {
                assert_ne!(rendered[i], rendered[j]);
            }
        }
    }

    #[test]
    fn unbound_realm_binds_once_and_rejects_a_second_vm() {
        let realm = MetadataRealm::new();
        assert!(!realm.is_bound());
        assert_eq!(realm.vm(), UNBOUND_VM);

        assert_eq!(realm.bind_vm(VM_A), Ok(()));
        assert!(realm.is_bound());
        assert_eq!(realm.vm(), VM_A);
        // Idempotent for the same VM.
        assert_eq!(realm.bind_vm(VM_A), Ok(()));
        // A second, different VM is a hard error, not a silent rebind.
        assert_eq!(realm.bind_vm(VM_B), Err(VM_A));
        assert_eq!(realm.vm(), VM_A);
    }

    #[test]
    fn handles_minted_before_binding_fail_closed_after_it() {
        let realm = MetadataRealm::new();
        let early = realm.mint_class(ClassId::new(4)).expect("mint");
        assert_eq!(early.vm(), UNBOUND_VM);
        // Valid while the realm is still unbound...
        assert!(resolve_live(&realm, early, 8).is_ok());
        // ...and rejected the moment it acquires an identity. Fail-closed is
        // the only safe direction: the alternative is a handle that outlives
        // the anonymity it was minted under.
        realm.bind_vm(VM_A).expect("bind");
        assert_eq!(
            resolve_live(&realm, early, 8).map_err(|e| e.kind()),
            Err(StaleMetadataKind::WrongVm)
        );
    }

    #[test]
    fn generation_map_is_untouched_until_the_first_bump() {
        let realm = MetadataRealm::for_vm(VM_A);
        assert_eq!(realm.tracked_classes(), 0);
        for i in 0..64u32 {
            assert_eq!(realm.generation(ClassId::new(i)), 0);
        }
        assert_eq!(
            realm.tracked_classes(),
            0,
            "reads must not populate the map"
        );

        realm.bump_generation(ClassId::new(5));
        assert_eq!(realm.tracked_classes(), 1);
        assert_eq!(realm.generation(ClassId::new(5)), 1);
        assert_eq!(realm.generation(ClassId::new(6)), 0);
    }

    #[test]
    fn bulk_bump_invalidates_every_named_class() {
        let realm = MetadataRealm::for_vm(VM_A);
        let ids: Vec<ClassId> = (0..4).map(ClassId::new).collect();
        let handles: Vec<ClassHandle> = ids
            .iter()
            .map(|id| realm.mint_class(*id).unwrap())
            .collect();

        realm.bump_generations(ids.iter().copied());

        for handle in &handles {
            assert_eq!(
                resolve_live(&realm, *handle, 8).map_err(|e| e.kind()),
                Err(StaleMetadataKind::StaleGeneration),
                "{handle} should have been invalidated"
            );
        }
        // A class outside the bumped set is unaffected.
        let untouched = realm.mint_class(ClassId::new(4)).unwrap();
        assert!(resolve_live(&realm, untouched, 8).is_ok());
    }

    #[test]
    fn an_exhausted_generation_is_poison_in_both_directions() {
        let realm = MetadataRealm::for_vm(VM_A);
        let id = ClassId::new(0);
        let handle = realm.mint_class(id).expect("mint");

        // Drive the counter to the ceiling directly — 2^32 real bumps is not a
        // test we can run.
        realm
            .generations
            .write()
            .insert(id, EXHAUSTED_GENERATION - 1);
        realm.tracked.store(1, Ordering::Release);
        assert_eq!(realm.bump_generation(id), EXHAUSTED_GENERATION);
        // Saturates rather than wrapping back to 0, which would revalidate the
        // generation-0 handle minted above.
        assert_eq!(realm.bump_generation(id), EXHAUSTED_GENERATION);

        assert_eq!(realm.mint_class(id), None, "no new handle at the ceiling");
        assert_eq!(
            resolve_live(&realm, handle, 8).map_err(|e| e.kind()),
            Err(StaleMetadataKind::StaleGeneration)
        );
        // And a fabricated handle *at* the ceiling is rejected too, so the
        // saturation cannot be used to mint a permanently-valid handle.
        let forged = ClassHandle::from_parts(VM_A, EXHAUSTED_GENERATION, id);
        assert_eq!(
            resolve_live(&realm, forged, 8).map_err(|e| e.kind()),
            Err(StaleMetadataKind::StaleGeneration)
        );
    }

    #[test]
    fn method_and_field_handles_carry_the_class_generation() {
        let realm = MetadataRealm::for_vm(VM_A);
        let id = ClassId::new(11);
        let class = realm.mint_class(id).expect("mint");
        let method = MethodHandle::from_parts(class, 3);
        let field = FieldHandle::from_parts(class, 1);

        assert_eq!(method.class(), class);
        assert_eq!(method.index_unchecked(), 3);
        assert_eq!(field.class(), class);
        assert_eq!(field.index_unchecked(), 1);

        realm.bump_generation(id);
        // Both are invalidated through the class handle they embed — the
        // redefine that bumped the generation is exactly what renumbers the
        // methods vec.
        assert_eq!(
            resolve_live(&realm, method.class(), 16).map_err(|e| e.kind()),
            Err(StaleMetadataKind::StaleGeneration)
        );
        assert_eq!(
            resolve_live(&realm, field.class(), 16).map_err(|e| e.kind()),
            Err(StaleMetadataKind::StaleGeneration)
        );
    }

    #[test]
    fn handles_are_usable_as_map_keys_without_aliasing_across_vms() {
        use std::collections::HashMap;

        let realm_a = MetadataRealm::for_vm(VM_A);
        let realm_b = MetadataRealm::for_vm(VM_B);
        let mut cache: HashMap<ClassHandle, &'static str> = HashMap::new();

        cache.insert(realm_a.mint_class(ClassId::new(7)).unwrap(), "A's class 7");
        cache.insert(realm_b.mint_class(ClassId::new(7)).unwrap(), "B's class 7");

        assert_eq!(cache.len(), 2, "the two VMs' class 7 must be distinct keys");
        assert_eq!(
            cache.get(&realm_a.mint_class(ClassId::new(7)).unwrap()),
            Some(&"A's class 7")
        );
    }

    #[test]
    fn display_renders_all_three_components() {
        let realm = MetadataRealm::for_vm(VM_A);
        let class = realm.mint_class(ClassId::new(7)).unwrap();
        assert_eq!(class.to_string(), "vm#0x1/c7@g0");
        let method = MethodHandle::from_parts(class, 2);
        let field = FieldHandle::from_parts(class, 2);
        assert_eq!(method.to_string(), "vm#0x1/c7@g0#m2");
        assert_eq!(field.to_string(), "vm#0x1/c7@g0#f2");
    }
}
