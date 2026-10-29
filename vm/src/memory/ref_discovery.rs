// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reference discovery at allocation, the VM half (gcg g4/rs, opt-in
//! `CRATONVM_REF_DISCOVER_AT_ALLOCATION`).
//!
//! `docs/feature-designs/native-dispatch-architecture.md`,
//! steps 1-2. The processor half (steps 3-4) is g3/rw's:
//! `cratonvm_gc::ReferenceProcessor::note_allocated_reference` keeps an
//! UNBOUND entry, and `runtime::interpreter::bind_allocated_references_pre_gc`
//! binds it from the object's own `referent` / `queue` slots at the next
//! pre-collection pass (a constructor bridge that registered the same object
//! wins; an unconstructed object is requeued).
//!
//! # Why
//!
//! The processor hears of a `Reference` only from the `java/lang/ref`
//! constructor `Bridge` natives. Under
//! `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ref/` the real constructor
//! bytecode runs and nothing is cleared or enqueued (row `r2_gate_shadowed`).
//! HotSpot discovers in the marker; a finalizable object, here, is registered at
//! allocation from a class bit. This registers a `Reference` the same way.
//!
//! # The kind
//!
//! [`reference_kind_in`] walks the superclass chain to the first JDK reference
//! class: `SoftReference`, `WeakReference`, `PhantomReference` (and
//! `jdk/internal/ref/Cleaner`, a phantom that RUNS, checked first because it
//! extends `PhantomReference`). `Reference`, `FinalReference` and `Object`
//! end the walk with no kind: a `FinalReference` keeps the finalizer
//! registration, and a bare `Reference` subclass is not a kind the JDK lets
//! user code make. No `Class` field and no cache: the walk runs only when the
//! switch is on (a per-allocation cost the flip must price; a recipe bit in
//! `jit::alloc_class_cache` is the cheap form, see the report).
//!
//! # Where it is called
//!
//! [`note_allocation`] after the object's fields are initialised, at the sites
//! that register a finalizable object (interpreter `new`, the native
//! constructors through `init_new_instance`, the JIT's `jit_post_alloc_init`,
//! JNI `NewObjectA`). Those calls are cross-lane hunks in
//! `docs/feature-designs/jdk-only-mode.md`. With the switch off
//! every call is one relaxed load.

use crate::classloading::{ClassId, ClassStore};
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_gc::reference::ReferenceType;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// The switch (default off, `runtime_flag_on`). Read once per VM, at
/// construction ([`RefDiscoveryState::from_flags`]); the JIT's new-site flags
/// read it at compile time to keep a reference class on the helper path.
pub(crate) const DISCOVER_AT_ALLOCATION_FLAG: &str = "CRATONVM_REF_DISCOVER_AT_ALLOCATION";

/// gch h2/jr (`gch-h1ak-soft-lru-free-figure-is-young-bounded-not-hotspots-FIXED-20261008`,
/// option B): read once per VM. On, the soft-reference LRU policy is fed
/// HotSpot's free figure ([`soft_lru_free_mb`]) instead of
/// `VmHeap::soft_ref_policy_free_mb`'s (on Generational the young-bounded one).
///
/// gch h4/wr: default ON (`runtime_flag_default_on`), the user's call after
/// vh2-jr D and vh3-jx B met parity (`_hs` rows SAME, the young-bounded `_def`
/// rows DIFF, no OOME on `softcache`). `=0` is the kill switch and restores the
/// young-bounded figure.
///
/// gci i48/zl2: every collector (it was Generational only, "inert on G1 and
/// ZGC"): HotSpot's `LRUMaxHeapPolicy` reads `MaxHeapSize - used_at_last_gc`
/// on Serial, G1 and ZGC, and G1's default figure was its soft capacity (below
/// `-Xmx` until the heap grows), ZGC's the arena. Same default and kill on all
/// three; `=0` gives each collector its `VmHeap::soft_ref_policy_free_mb`.
pub(crate) const SOFT_HOTSPOT_FREE_FLAG: &str = "CRATONVM_SOFTREF_HOTSPOT_FREE";

/// Per-VM state of discovery at allocation. Lives in `HeapRealm`; never a
/// process global.
pub struct RefDiscoveryState {
    /// The switch as this VM was built with it.
    on: AtomicBool,
    /// Allocations registered over the VM's life (the engagement counter).
    noted: AtomicU64,
    /// `java/lang/ref/Reference.referent`'s identity in THIS VM, packed as
    /// `(slot << 32) | (class id + 1)`; `0` until the class is loaded and the
    /// slot resolved. See [`is_referent_field`].
    referent: AtomicU64,
    /// gch h2/jr: [`SOFT_HOTSPOT_FREE_FLAG`] as this VM was built with it.
    /// Kept here because this is the VM's per-VM reference state (no process
    /// global); it has nothing to do with discovery itself.
    soft_hotspot_free: AtomicBool,
    /// gch h2/jr: how many soft-LRU free figures [`soft_lru_free_mb`] answered
    /// with HotSpot's figure (the switch's engagement counter).
    soft_hotspot_free_reads: AtomicU64,
    /// Interpreter round i7 wave 143, lane L6 ([`REF_NOTES_PER_THREAD_FLAG`],
    /// performance, default ON since 2026-10-28, `=0` kills; retired proposal
    /// `i77-L5-proposal-reference-registry-notes-without-the-global-mutex`):
    /// the striped allocation notes [`note_allocation`] takes instead of the
    /// processor's mutex. The VM attaches the same object to its processor
    /// when it builds it (`vm_init`), before any allocation, so every note
    /// made here is one the processor absorbs
    /// (`cratonvm_gc::reference::AllocationNoteStripes`). `None`: the locked
    /// push, as before.
    note_stripes: Option<Arc<cratonvm_gc::reference::AllocationNoteStripes>>,
}

/// [`RefDiscoveryState::note_stripes`]'s switch, read once per VM by
/// [`RefDiscoveryState::from_flags`] (performance, default ON since 2026-10-28:
/// the i77-L5 A/B passed its rule; `=0` is the kill switch).
pub(crate) const REF_NOTES_PER_THREAD_FLAG: &str = "CRATONVM_REF_NOTES_PER_THREAD";

impl RefDiscoveryState {
    /// The switch as this VM is built: ON wherever the `java/lang/ref`
    /// constructors run as the JDK's own bytecode -- `--jdk-only` (SBR round
    /// 3, lane `ref`) and `--compatible` over a real JDK with the
    /// `JdkReferences` HotSpot-alignment row (nio20, lane L4) -- because the
    /// processor can then only hear of a `Reference` at its allocation;
    /// otherwise the environment switch, read once at VM construction. The
    /// caller decides from the VM's own policy (`VmConfig` and its alignment
    /// set), never from the environment.
    ///
    /// Where the constructors are bytecode the switch is an opt-OUT (`=0` is
    /// the positive control for the lane's gate: with the constructors retired
    /// and discovery off, no reference is ever cleared); otherwise opt-in.
    pub fn from_flags(constructors_are_bytecode: bool) -> Self {
        let state = Self::with(if constructors_are_bytecode {
            cratonvm_types::flags::runtime_flag_default_on(DISCOVER_AT_ALLOCATION_FLAG)
        } else {
            cratonvm_types::flags::runtime_flag_on(DISCOVER_AT_ALLOCATION_FLAG)
        });
        // gch h4/wr: default ON, `=0` kills (see the flag's doc).
        state.set_soft_hotspot_free(cratonvm_types::flags::runtime_flag_default_on(
            SOFT_HOTSPOT_FREE_FLAG,
        ));
        let mut state = state;
        // WARNING i77-ref-notes-flip-20261028: default ON, `=0` restores the
        // locked push.
        if cratonvm_types::flags::runtime_flag_default_on(REF_NOTES_PER_THREAD_FLAG) {
            state.note_stripes = Some(Arc::new(
                cratonvm_gc::reference::AllocationNoteStripes::new(),
            ));
        }
        state
    }

    /// The striped allocation notes this VM was built with
    /// ([`REF_NOTES_PER_THREAD_FLAG`]), for the VM to attach to its processor.
    pub(crate) fn note_stripes(&self) -> Option<&Arc<cratonvm_gc::reference::AllocationNoteStripes>> {
        self.note_stripes.as_ref()
    }

    /// An explicit switch (tests, embedders). The soft-LRU figure switch is
    /// off here, whatever the VM default ([`Self::from_flags`] reads it, default
    /// ON since gch h4/wr); set it with [`Self::set_soft_hotspot_free`].
    pub fn with(on: bool) -> Self {
        Self {
            on: AtomicBool::new(on),
            noted: AtomicU64::new(0),
            referent: AtomicU64::new(0),
            soft_hotspot_free: AtomicBool::new(false),
            soft_hotspot_free_reads: AtomicU64::new(0),
            note_stripes: None,
        }
    }

    /// gch h2/jr: switch [`soft_lru_free_mb`]'s HotSpot figure (tests; the VM
    /// reads [`SOFT_HOTSPOT_FREE_FLAG`] once at construction).
    pub fn set_soft_hotspot_free(&self, on: bool) {
        self.soft_hotspot_free.store(on, Ordering::Relaxed);
    }

    /// gch h2/jr: soft-LRU free figures answered with HotSpot's figure.
    pub fn soft_hotspot_free_reads(&self) -> u64 {
        self.soft_hotspot_free_reads.load(Ordering::Relaxed)
    }

    /// Does this VM register a `Reference` at its allocation?
    #[inline]
    pub fn engaged(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Switch it (tests; only meaningful before the first allocation).
    pub fn set_engaged(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }

    /// Allocations registered over the VM's life.
    pub fn noted(&self) -> u64 {
        self.noted.load(Ordering::Relaxed)
    }
}

/// Is `(declaring, slot)` the field `java.lang.ref.Reference.referent`?
///
/// SBR round 4, lane `satb`. HotSpot's `Reference.get()` is an intrinsic that
/// loads the referent THROUGH an SATB keep-alive (`G1ReferenceGet`): a referent
/// handed to the mutator after a concurrent cycle's snapshot, and stored behind
/// an object the marker already scanned, must still be marked. The VM's marker
/// hides the referent slot, so a plain `getfield` of it has no such protection.
/// Every bytecode read of the field therefore goes through a door that asks this
/// question: `op_getfield` and the cached trivial-getter door (which apply the
/// barrier), the quickened `getfield` site fill (which declines to quicken it),
/// and the JIT's field-site resolvers (which answer a marker tag so the load
/// stays on the checked `jit_getfield` helper, where `jit_getfield_body` applies
/// the barrier; a splice or OSR body is refused).
///
/// Per VM (the realm's `RefDiscoveryState`), never a process global. The
/// common answer is one relaxed load and a compare; a slot other than the
/// recorded one never takes the lock. Until the class is loaded the answer is
/// `false` and a slot-0 probe repeats; no `Reference` is readable before that.
#[inline]
pub(crate) fn is_referent_field(shared: &SharedVm, declaring: ClassId, slot: usize) -> bool {
    if slot > u32::MAX as usize {
        return false;
    }
    let packed = shared.mem.ref_discovery.referent.load(Ordering::Relaxed);
    if packed != 0 {
        return packed == (((slot as u64) << 32) | (u64::from(declaring.as_u32()) + 1));
    }
    if slot != 0 {
        // Not yet resolved: the JDK declares `referent` first, so only slot 0
        // can be the field and the rest never pay the lock.
        return false;
    }
    is_referent_field_cold(shared, declaring, slot)
}

#[cold]
#[inline(never)]
fn is_referent_field_cold(shared: &SharedVm, declaring: ClassId, slot: usize) -> bool {
    resolve_referent_field(shared)
        .is_some_and(|(reference, index)| declaring == reference && slot == index)
}

#[cold]
fn resolve_referent_field(shared: &SharedVm) -> Option<(ClassId, usize)> {
    let cm = shared.classes.class_manager.read();
    let reference = cm.find_bootstrap_class_by_name("java/lang/ref/Reference")?;
    let index = crate::vm::vm_exec::resolve_field_index_in_hierarchy(
        reference,
        "referent",
        cm.class_store(),
    )
    .or_else(|| {
        // The existing synthetic Reference uses anonymous native slots:
        // referent=0, queue=1. Real bytes remain authoritative when present.
        cm.get_class(reference)
            .filter(|class| class.origin.is_compatibility_stub())
            .map(|_| 0)
    })?;
    // gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 14): the
    // keep-alive doors that cannot ask this question per read --
    // `vm_exec::gen_referent_read_keep_alive` and `unsafe_natives`'
    // `slot0_read_keep_alive` -- test `slot == 0` for HotSpot's
    // `is_referent_field(base, offset)`. True for every `Reference` layout
    // (the JDK declares `referent` first, `Object` has no fields); a layout
    // that moved it must fail here, in a debug build, not silently there.
    debug_assert_eq!(
        index, 0,
        "java.lang.ref.Reference.referent must resolve to slot 0 (the keep-alive doors test slot 0)"
    );
    let packed = ((index as u64) << 32) | (u64::from(reference.as_u32()) + 1);
    shared
        .mem
        .ref_discovery
        .referent
        .store(packed, Ordering::Relaxed);
    Some((reference, index))
}

/// HotSpot's unknown-strength referent load: identify the actual receiver,
/// then publish the returned value through the owning heap's mark ingress.
/// Applies to every collector. Ordinary slot-zero fields and reference-array
/// elements must not gain a spurious lifetime extension. The field identity
/// is the same per-VM cached bootstrap identity used by bytecode loads;
/// subclass checks use the published supers cache, including FinalReference.
pub(crate) fn keep_alive_referent_read(
    shared: &SharedVm,
    receiver: ObjectRef,
    slot: usize,
    value: Value,
) {
    let Value::Object(Some(value)) = value else {
        return;
    };
    if !referent_read_is_active(shared) || !is_referent_slot(shared, receiver, slot) {
        return;
    }
    // zt t23/zb7 (dev merge): SATB, or under the ZGC color flip the load
    // barrier's mark step.
    shared.mem.heap.referent_keep_alive(Value::Object(Some(value)));
}

/// JIT fallback for a `getfield` site whose field descriptor was unavailable
/// when the code was compiled. Such a site cannot promise that its raw return
/// word is an oop, so classify the receiver and slot before validating it as a
/// heap object and publishing the keep-alive.
pub(crate) fn keep_alive_referent_read_raw(
    shared: &SharedVm,
    receiver: ObjectRef,
    slot: usize,
    raw_value: usize,
) {
    if raw_value == 0
        || !referent_read_is_active(shared)
        || !is_referent_slot(shared, receiver, slot)
    {
        return;
    }
    if let Some(value) = shared.mem.heap.is_object_address(raw_value) {
        // zt t23/zb7 (dev merge): as above.
        shared.mem.heap.referent_keep_alive(Value::Object(Some(value)));
    }
}

/// zt t32 (steps 2b / 2d of the concurrent referent clear,
/// `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): the resurrection-blocked read of
/// `Reference.referent`. `value` was just read from slot `slot` of `receiver`;
/// `reread` reads that slot again. Between a ZGC collection's release and its
/// after-release referent clear, a slot may still name a referent the mark did
/// not reach, and HotSpot's load answers `null` for it: so does this one,
/// through the heap's armed gate (`ZgcRealHeap::referent_read_blocked`, the
/// clear's own verdict). Once the clear has run the gate is disarmed and the
/// slot is re-read: the clear's writes happen before the disarm, so a value
/// read before the clear is replaced by the slot's answer. A read whose
/// receiver is not a `Reference`, or a primitive / `null` value, passes.
///
/// One `Acquire` load (a plain load on x86) for a non-null value while no gate
/// was ever armed: every run without the switch, every collector but ZGC.
/// Every mutator door that hands a referent to Java calls it: the interpreter
/// `getfield` and its trivial-getter door, the compiled `jit_getfield` helper,
/// JNI `GetObjectField`, the lambda `GetField` arm, and the natives' heap
/// access (`get_field` / `get_field_no_keepalive`: `Reference.get`,
/// `refersTo`, the `Unsafe` getters, reflection and the handles).
#[inline]
pub(crate) fn gate_referent_read(
    shared: &SharedVm,
    receiver: ObjectRef,
    slot: usize,
    value: Value,
    reread: impl FnOnce() -> Value,
) -> Value {
    if !matches!(value, Value::Object(Some(_))) {
        return value;
    }
    let state = shared.mem.heap.zgc_referent_gate_state();
    if state == cratonvm_gc::zgc::ZGC_REFERENT_GATE_NEVER {
        return value;
    }
    gate_referent_read_cold(shared, receiver, slot, value, state, reread)
}

#[cold]
#[inline(never)]
fn gate_referent_read_cold(
    shared: &SharedVm,
    receiver: ObjectRef,
    slot: usize,
    value: Value,
    state: u8,
    reread: impl FnOnce() -> Value,
) -> Value {
    if !is_referent_slot(shared, receiver, slot) {
        return value;
    }
    if state != cratonvm_gc::zgc::ZGC_REFERENT_GATE_ARMED {
        return reread();
    }
    let Value::Object(Some(referent)) = value else {
        return value;
    };
    let blocked = shared
        .mem
        .heap
        .zgc_referent_read_blocked(receiver.as_ptr() as usize, referent.as_ptr() as usize);
    // The verdict reads the pause's registry and mark bits, which the parked
    // sweep's install clears AFTER the disarm. Still armed after the verdict:
    // nothing it read came from the install (on x86 the install's writes
    // cannot be seen before the disarm that precedes them). Disarmed
    // meanwhile, either way: the clear ran, and the slot holds the answer.
    if shared.mem.heap.zgc_referent_gate_state() != cratonvm_gc::zgc::ZGC_REFERENT_GATE_ARMED {
        return reread();
    }
    if blocked {
        Value::Object(None)
    } else {
        value
    }
}

#[inline]
fn referent_read_is_active(shared: &SharedVm) -> bool {
    if !shared.mem.heap.satb_armed() {
        return false;
    }
    #[cfg(feature = "zgc")]
    if let crate::memory::VmHeap::Zgc(heap) = &shared.mem.heap {
        if !heap.mark_active() {
            return false;
        }
    }
    true
}

fn is_referent_slot(shared: &SharedVm, receiver: ObjectRef, slot: usize) -> bool {
    let packed = shared.mem.ref_discovery.referent.load(Ordering::Relaxed);
    let layout = if packed != 0 {
        Some((
            ClassId::new((packed as u32).wrapping_sub(1)),
            (packed >> 32) as usize,
        ))
    } else {
        resolve_referent_field(shared)
    };
    // The compiled getfield helper cannot assume Reference.referent is slot
    // zero: real JDK class layouts assign it the hierarchy-wide field index.
    // Resolve once, then reject every unrelated reference field before any
    // receiver/class lookup. Arrays cannot match the named instance field;
    // check them after the cheap cached-slot comparison.
    if slot != 0 && layout.is_none_or(|(_, index)| slot != index) {
        return false;
    }
    if shared.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return false;
    }
    let class = shared.mem.heap.class_id_of(receiver);
    if let Some((reference, index)) = layout {
        if slot == index && crate::runtime::interpreter::class_is_subtype(shared, class, reference)
        {
            return true;
        }
    }
    // Existing synthetic reference classes have native slot zero but may
    // have no Reference superclass or named field. Use the marker's own
    // reference-kind rule, restricted to a compatibility-stub ancestor.
    slot == 0 && synthetic_reference_receiver(shared, class)
}

fn synthetic_reference_receiver(shared: &SharedVm, mut class: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let kind = reference_kind_in(cm.class_store(), class).is_some();
    for _ in 0..REFERENCE_KIND_WALK_DEPTH {
        let Some(metadata) = cm.get_class(class) else {
            return false;
        };
        if metadata.origin.is_compatibility_stub()
            && (metadata.name.as_ref() == "java/lang/ref/Reference"
                || (kind && reference_kind_in(cm.class_store(), class).is_some()))
        {
            return true;
        }
        let Some(parent) = metadata.superclass else {
            return false;
        };
        class = parent;
    }
    false
}

/// Is `class_id` a SOFT / WEAK / PHANTOM reference class (or a `Cleaner`)?
/// For compiled code's `jit_getfield` helper, which knows the receiver's class
/// but not the field's declaring class: slot 0 of such a receiver IS
/// `Reference.referent`. Lock-free through the allocation recipe cache when it
/// is published, else the superclass walk under the class manager's read lock.
pub(crate) fn receiver_is_reference(shared: &SharedVm, class_id: ClassId) -> bool {
    if shared.jit.jit_alloc_class_cache.enabled() {
        if let Some(info) = shared.jit.jit_alloc_class_cache.get(class_id.as_u32()) {
            return info.reference_kind != 0;
        }
    }
    let cm = shared.classes.class_manager.read();
    reference_kind_in(&cm.class_store, class_id).is_some()
}

/// What an allocation of a reference class registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocationKind {
    /// Soft, weak or phantom.
    pub(crate) ref_type: ReferenceType,
    /// A `jdk.internal.ref.Cleaner`: a phantom that runs instead of being
    /// enqueued (`ReferenceEntry::runs_cleaner`).
    pub(crate) runs_cleaner: bool,
}

/// One class name's verdict: `Some(Some(kind))` the walk stops with a kind,
/// `Some(None)` the walk stops with none, `None` keep walking.
fn kind_of_name(name: &str) -> Option<Option<AllocationKind>> {
    let kind = |ref_type, runs_cleaner| {
        Some(Some(AllocationKind {
            ref_type,
            runs_cleaner,
        }))
    };
    match name {
        "jdk/internal/ref/Cleaner" => kind(ReferenceType::Phantom, true),
        "java/lang/ref/PhantomReference" => kind(ReferenceType::Phantom, false),
        "java/lang/ref/WeakReference" => kind(ReferenceType::Weak, false),
        "java/lang/ref/SoftReference" => kind(ReferenceType::Soft, false),
        "java/lang/ref/Reference" | "java/lang/ref/FinalReference" | "java/lang/Object" => {
            Some(None)
        }
        _ => None,
    }
}

/// How many classes [`walk_reference_kind`] visits before it gives up: the
/// class store's own hierarchy bound (`MAX_HIERARCHY_DEPTH`,
/// `classloading/src/class.rs`), which every other superclass walk of the store
/// honours.
///
/// gci i46/ha (`gci-i46ha-reference-kind-walk-stops-at-64-classes-FIXED-20261017`):
/// this was 64. A linked chain has no cycle (a circular one is refused at
/// definition, `ClassCircularityError`), so the bound only ever ended a LEGAL
/// chain: an instance of a class 64 or more levels below its JDK reference
/// class got no kind -- never registered at allocation (never cleared or
/// enqueued where the `java/lang/ref` constructors are bytecode, `--jdk-only`),
/// given the JIT's inline-bump `new`, and no referent keep-alive in the JIT's
/// `jit_getfield` (`receiver_is_reference`). HotSpot inherits a class's
/// reference type from its superclass at parse, whatever the depth.
const REFERENCE_KIND_WALK_DEPTH: usize = 1024;

/// The walk of [`reference_kind_in`] over any class lookup (`(name,
/// superclass)` of a class id), so it can be tested without a class store.
/// Bounded: a malformed cycle in the chain ends the walk after
/// [`REFERENCE_KIND_WALK_DEPTH`] steps.
fn walk_reference_kind<'a>(
    start: ClassId,
    lookup: impl Fn(ClassId) -> Option<(&'a str, Option<ClassId>)>,
) -> Option<AllocationKind> {
    let mut cid = Some(start);
    for _ in 0..REFERENCE_KIND_WALK_DEPTH {
        let (name, superclass) = lookup(cid?)?;
        if let Some(verdict) = kind_of_name(name) {
            return verdict;
        }
        cid = superclass;
    }
    None
}

/// The reference kind an instance of `class_id` registers at allocation, if
/// any. `None` for every class outside the `java.lang.ref` reference
/// hierarchy, for an unknown class and for a class whose chain has a hole.
pub(crate) fn reference_kind_in(store: &ClassStore, class_id: ClassId) -> Option<AllocationKind> {
    walk_reference_kind(class_id, |cid| {
        store.get(cid).map(|c| (c.name.as_ref(), c.superclass))
    })
}

/// [`reference_kind_in`] as the one-byte code a [`ClassAllocInfo`] stores:
/// `0` none, `1` soft, `2` weak, `3` phantom, `4` `jdk.internal.ref.Cleaner`.
///
/// [`ClassAllocInfo`]: crate::jit::alloc_class_cache::ClassAllocInfo
pub(crate) fn reference_kind_code_in(store: &ClassStore, class_id: ClassId) -> u8 {
    code_of_kind(reference_kind_in(store, class_id))
}

/// The code of one kind (see [`reference_kind_code_in`]).
fn code_of_kind(kind: Option<AllocationKind>) -> u8 {
    match kind {
        None => 0,
        Some(AllocationKind {
            runs_cleaner: true, ..
        }) => 4,
        Some(AllocationKind {
            ref_type: ReferenceType::Soft,
            ..
        }) => 1,
        Some(AllocationKind {
            ref_type: ReferenceType::Weak,
            ..
        }) => 2,
        Some(_) => 3,
    }
}

/// The inverse of [`reference_kind_code_in`] (`0` and unknown codes: none).
fn kind_from_code(code: u8) -> Option<AllocationKind> {
    let (ref_type, runs_cleaner) = match code {
        1 => (ReferenceType::Soft, false),
        2 => (ReferenceType::Weak, false),
        3 => (ReferenceType::Phantom, false),
        4 => (ReferenceType::Phantom, true),
        _ => return None,
    };
    Some(AllocationKind {
        ref_type,
        runs_cleaner,
    })
}

/// Register the freshly allocated `obj` of `class_id` with the reference
/// processor as an UNBOUND entry when this VM discovers at allocation and the
/// class is a soft / weak / phantom reference class. Call it after the
/// object's fields are initialised and before any constructor runs, with no
/// lock held (it takes the class manager, L10, then the processor, L7, never
/// both). A no-op (one relaxed load) with the switch off.
///
/// Registering an object a constructor bridge then registers again is
/// harmless: the bind drops the unbound entry (`BindOutcome::AlreadyRegistered`).
pub(crate) fn note_allocation(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    if !shared.mem.ref_discovery.engaged() {
        return;
    }
    // SBR3 `ref`: the per-class kind is baked into the allocation recipe
    // (`ClassAllocInfo::reference_kind`), so the common allocation (a class
    // that is no reference) costs one lock-free recipe read, not a class-manager
    // read lock and a superclass walk. A class with no published recipe takes
    // the walk, as before.
    let cached = if shared.jit.jit_alloc_class_cache.enabled() {
        shared
            .jit
            .jit_alloc_class_cache
            .get(class_id.as_u32())
            .map(|info| info.reference_kind)
    } else {
        None
    };
    let kind = match cached {
        Some(0) => return,
        Some(code) => kind_from_code(code),
        None => {
            let cm = shared.classes.class_manager.read();
            reference_kind_in(&cm.class_store, class_id)
        }
    };
    let Some(kind) = kind else {
        return;
    };
    // Interpreter round i7 wave 143, lane L6 (`CRATONVM_REF_NOTES_PER_THREAD`,
    // default ON): a stripe of this VM's note buffer instead of the
    // processor's mutex; the processor absorbs it before any reader looks.
    if let Some(stripes) = shared.mem.ref_discovery.note_stripes() {
        stripes.note(
            kind.ref_type,
            obj.as_ptr() as usize,
            class_id.as_u32(),
            kind.runs_cleaner,
        );
        shared
            .mem
            .ref_discovery
            .noted
            .fetch_add(1, Ordering::Relaxed);
        return;
    }
    shared.mem.ref_processor.lock().note_allocated_reference(
        kind.ref_type,
        obj.as_ptr() as usize,
        class_id.as_u32(),
        kind.runs_cleaner,
    );
    shared
        .mem
        .ref_discovery
        .noted
        .fetch_add(1, Ordering::Relaxed);
}

/// gch h2/jr: the free-heap megabytes the soft-reference LRU policy is fed
/// (`clear when idle_ms > SoftRefLRUPolicyMSPerMB * free_mb`), for every VM
/// caller of the policy: the post-collection pass
/// (`process_references_after_gc`), the remark pass and the pre-collection
/// soft condemnation (`weakref_null_referents_pre_gc`). Read before the
/// reference-processor lock, as each caller did.
///
/// [`SOFT_HOTSPOT_FREE_FLAG`]`=0`: `VmHeap::soft_ref_policy_free_mb`,
/// unchanged (on Generational the pessimistic `min(young free, old free)`,
/// about a third of HotSpot's figure on a mostly empty heap; on G1 the soft
/// capacity `heap_target_bytes()` less allocated, under `-Xmx` from a small
/// committed heap; on ZGC the arena less allocated).
///
/// [`SOFT_HOTSPOT_FREE_FLAG`] on (the default since gch h4/wr), every
/// collector since gci i48/zl2 (Generational only before): HotSpot 25's
/// `LRUMaxHeapPolicy::setup` input on Serial, G1 and ZGC alike,
/// `MaxHeapSize - used_at_last_gc`
/// ([`hotspot_policy_capacity`]: the `-Xmx` budget itself, NOT `max_capacity()`
/// -- the policy reads the flag, so both survivor spaces count) less the
/// heap's allocated bytes now. "At last GC" is the reference processor's own
/// business: under `CRATONVM_SOFTREF_HOTSPOT_LRU` (default on) a round uses the
/// figure the PREVIOUS round was handed (`soft_free_mb_at_last_gc`), and the
/// STW callers measure after their collection, so the figure a round applies
/// is the previous collection's post-collection one, as HotSpot's. Option B of
/// `docs/feature-designs/jdk-only-mode.md`:
/// a policy switch, because a fragmenting non-moving young generation can make
/// "unused" exceed "allocatable", the reason the default figure is
/// pessimistic. Decided by rows, not here.
///
/// gch h3/jx: h2 fed `VmHeap::heap_capacity` (one semispace plus the old
/// generation), about 15 % under `-Xmx` on a default split, so the window was
/// still narrower than HotSpot's under the switch.
pub(crate) fn soft_lru_free_mb(shared: &SharedVm) -> usize {
    let heap = &shared.mem.heap;
    let state = &shared.mem.ref_discovery;
    if state.soft_hotspot_free.load(Ordering::Relaxed) {
        state
            .soft_hotspot_free_reads
            .fetch_add(1, Ordering::Relaxed);
        hotspot_free_mb(
            hotspot_policy_capacity(shared.config.max_heap_size, heap.heap_capacity()),
            heap.allocated_bytes(),
        )
    } else {
        heap.soft_ref_policy_free_mb()
    }
}

/// gch h3/jx: HotSpot's `MaxHeapSize` for [`soft_lru_free_mb`]: the
/// configured `-Xmx` (`VmConfig::max_heap_size`, after the container
/// ergonomics), or the heap's own capacity when a heap built from explicit
/// arena sizes is larger than that budget (tests, embedders), so the figure
/// never reads less headroom than the heap really has.
fn hotspot_policy_capacity(xmx: usize, heap_capacity: usize) -> usize {
    xmx.max(heap_capacity)
}

/// HotSpot's figure of [`soft_lru_free_mb`], pure: whole megabytes of
/// `capacity - used`, rounded down (a sub-megabyte remainder is `0`, maximum
/// pressure, as the default figure rounds).
fn hotspot_free_mb(capacity: usize, used: usize) -> usize {
    capacity.saturating_sub(used) / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gci_jit_referent_screen_accepts_the_cached_nonzero_field_index() {
        use crate::config::{GcAlgorithm, VmConfig};

        #[allow(unused_mut)]
        let mut algorithms = vec![GcAlgorithm::Generational, GcAlgorithm::G1];
        #[cfg(feature = "zgc")]
        algorithms.push(GcAlgorithm::Zgc);
        for algorithm in algorithms {
            let shared = crate::vm::SharedVm::new(VmConfig {
                gc_algorithm: algorithm,
                ..VmConfig::with_host_jdk_default()
            });
            let (reference, weak, object) = {
                let mut cm = shared.classes.class_manager_write();
                // `load_class`, not a loaded-class probe: the host-JDK default is
                // `--compatible`, whose fast boot no longer preloads `Reference`.
                let reference = cm
                    .load_class("java/lang/ref/Reference")
                    .expect("bootstrap Reference");
                let weak = cm
                    .load_class("java/lang/ref/WeakReference")
                    .expect("bootstrap WeakReference");
                let object = cm.load_class("java/lang/Object").expect("bootstrap Object");
                (reference, weak, object)
            };
            let receiver = shared.mem.heap.alloc_object(weak, 4);
            let payload = if matches!(algorithm, GcAlgorithm::Generational) {
                shared
                    .mem
                    .heap
                    .try_alloc_object_old(object, 0)
                    .expect("old payload")
            } else {
                shared.mem.heap.alloc_object(object, 0)
            };
            shared
                .mem
                .heap
                .set_field(receiver, 1, Value::Object(Some(payload)));
            // Simulate the resolved layout produced by a real JDK whose
            // hierarchy-wide Reference field index is nonzero.
            shared.mem.ref_discovery.referent.store(
                (1u64 << 32) | (u64::from(reference.as_u32()) + 1),
                Ordering::Relaxed,
            );
            match &shared.mem.heap {
                crate::memory::VmHeap::Generational(_) => shared.mem.concurrent_satb.activate(),
                crate::memory::VmHeap::G1(heap) => heap.satb_queue().activate(),
                #[cfg(feature = "zgc")]
                crate::memory::VmHeap::Zgc(heap) => heap.set_mark_active(true),
            }
            keep_alive_referent_read_raw(&shared, receiver, 1, payload.as_ptr() as usize);
            let retained = match &shared.mem.heap {
                crate::memory::VmHeap::Generational(_) => {
                    cratonvm_gc::satb::flush_thread_satb_buffer(&shared.mem.concurrent_satb);
                    shared.mem.concurrent_satb.drain()
                }
                crate::memory::VmHeap::G1(heap) => {
                    cratonvm_gc::satb::flush_thread_satb_buffer(heap.satb_queue());
                    heap.satb_queue().drain()
                }
                #[cfg(feature = "zgc")]
                crate::memory::VmHeap::Zgc(heap) => {
                    let mut entries = Vec::new();
                    heap.drain_mark_ingress(&mut entries);
                    entries.into_iter().map(|entry| entry as usize).collect()
                }
            };
            assert!(
                retained.contains(&(payload.as_ptr() as usize)),
                "{algorithm:?}: nonzero cached hierarchy field index"
            );
        }
    }

    /// A fake class chain: id -> (name, superclass).
    fn chain(rows: &[(u32, &'static str, Option<u32>)]) -> Vec<(u32, &'static str, Option<u32>)> {
        rows.to_vec()
    }

    fn kind_in(rows: &[(u32, &'static str, Option<u32>)], start: u32) -> Option<AllocationKind> {
        walk_reference_kind(ClassId::new(start), |cid| {
            rows.iter()
                .find(|r| r.0 == cid.as_u32())
                .map(|r| (r.1, r.2.map(ClassId::new)))
        })
    }

    /// The walk finds the first JDK reference class up the chain; `Cleaner`
    /// wins over its `PhantomReference` superclass; `FinalReference`, a bare
    /// `Reference` and every non-reference class have no kind (the negative
    /// controls), and a hole in the chain answers none.
    #[test]
    fn gcg_g4rs_reference_kind_walks_to_the_first_jdk_reference_class() {
        let rows = chain(&[
            (1, "java/lang/Object", None),
            (2, "java/lang/ref/Reference", Some(1)),
            (3, "java/lang/ref/WeakReference", Some(2)),
            (4, "java/lang/ref/SoftReference", Some(2)),
            (5, "java/lang/ref/PhantomReference", Some(2)),
            (6, "jdk/internal/ref/Cleaner", Some(5)),
            (7, "java/lang/ref/FinalReference", Some(2)),
            (8, "com/example/MyWeak", Some(3)),
            (9, "com/example/MyWeakChild", Some(8)),
            (10, "java/util/WeakHashMap$Entry", Some(3)),
            (11, "java/lang/String", Some(1)),
            (12, "com/example/Orphan", Some(99)),
            (
                13,
                "jdk/internal/ref/CleanerImpl$PhantomCleanableRef",
                Some(5),
            ),
        ]);
        let weak = Some(AllocationKind {
            ref_type: ReferenceType::Weak,
            runs_cleaner: false,
        });
        assert_eq!(kind_in(&rows, 3), weak);
        assert_eq!(kind_in(&rows, 8), weak, "a user subclass inherits the kind");
        assert_eq!(kind_in(&rows, 9), weak);
        assert_eq!(kind_in(&rows, 10), weak);
        assert_eq!(
            kind_in(&rows, 4).map(|k| k.ref_type),
            Some(ReferenceType::Soft)
        );
        assert_eq!(
            kind_in(&rows, 5),
            Some(AllocationKind {
                ref_type: ReferenceType::Phantom,
                runs_cleaner: false,
            })
        );
        assert_eq!(
            kind_in(&rows, 13),
            Some(AllocationKind {
                ref_type: ReferenceType::Phantom,
                runs_cleaner: false,
            }),
            "a JDK 25 cleanable is an ordinary phantom"
        );
        assert_eq!(
            kind_in(&rows, 6),
            Some(AllocationKind {
                ref_type: ReferenceType::Phantom,
                runs_cleaner: true,
            }),
            "Cleaner is checked before its PhantomReference superclass"
        );
        assert_eq!(
            kind_in(&rows, 7),
            None,
            "FinalReference keeps the finalizer registration"
        );
        assert_eq!(kind_in(&rows, 2), None);
        assert_eq!(kind_in(&rows, 11), None);
        assert_eq!(kind_in(&rows, 1), None);
        assert_eq!(kind_in(&rows, 12), None, "a hole in the chain");
        assert_eq!(kind_in(&rows, 42), None, "an unknown class");

        // A malformed cycle ends the walk.
        let cyclic = chain(&[(20, "a/A", Some(21)), (21, "a/B", Some(20))]);
        assert_eq!(kind_in(&cyclic, 20), None);
    }

    /// gci i46/ha: a reference subclass deeper than the old 64-step bound still
    /// has its kind (HotSpot inherits the reference type whatever the depth),
    /// up to the class store's own hierarchy bound; past that bound the walk
    /// still ends, with no kind.
    #[test]
    fn ha46_reference_kind_walk_reaches_a_deep_subclass() {
        let names: Vec<String> = (0..1100).map(|i| format!("com/example/Deep{i}")).collect();
        // Ids 1..=3: Object, Reference, WeakReference; 100 + i: Deep{i}, whose
        // superclass is Deep{i - 1}, and Deep0's is WeakReference.
        let mut rows: Vec<(u32, &str, Option<u32>)> = vec![
            (1, "java/lang/Object", None),
            (2, "java/lang/ref/Reference", Some(1)),
            (3, "java/lang/ref/WeakReference", Some(2)),
        ];
        for (i, name) in names.iter().enumerate() {
            let id = 100 + i as u32;
            rows.push((id, name.as_str(), Some(if i == 0 { 3 } else { id - 1 })));
        }
        let kind_at = |start: u32| {
            walk_reference_kind(ClassId::new(start), |cid| {
                rows.iter()
                    .find(|r| r.0 == cid.as_u32())
                    .map(|r| (r.1, r.2.map(ClassId::new)))
            })
        };
        let weak = Some(AllocationKind {
            ref_type: ReferenceType::Weak,
            runs_cleaner: false,
        });
        // Deep{i} is i + 1 levels below WeakReference.
        assert_eq!(kind_at(100 + 62), weak, "inside the old bound");
        assert_eq!(
            kind_at(100 + 63),
            weak,
            "64 levels below: the old bound answered no kind here"
        );
        assert_eq!(kind_at(100 + 199), weak, "200 levels below");
        assert_eq!(
            kind_at(100 + (REFERENCE_KIND_WALK_DEPTH as u32 - 2)),
            weak,
            "the deepest chain the bound admits"
        );
        assert_eq!(
            kind_at(100 + (REFERENCE_KIND_WALK_DEPTH as u32 - 1)),
            None,
            "past the class store's hierarchy bound the walk ends with no kind"
        );
    }

    /// SBR3 `ref`: the kind code a `ClassAllocInfo` bakes round-trips through
    /// `kind_from_code`, `0` is no kind, and an unknown code is no kind.
    #[test]
    fn sbr3_ref_kind_code_round_trips() {
        assert_eq!(code_of_kind(None), 0);
        assert!(kind_from_code(0).is_none());
        assert!(kind_from_code(200).is_none());
        for code in 1u8..=4 {
            let kind = kind_from_code(code);
            assert!(kind.is_some(), "code {code}");
            assert_eq!(code_of_kind(kind), code);
        }
        assert_eq!(
            kind_from_code(4),
            Some(AllocationKind {
                ref_type: ReferenceType::Phantom,
                runs_cleaner: true,
            })
        );
    }

    /// gch h2/jr: HotSpot's figure is whole megabytes of `capacity - used`,
    /// rounded down, and never negative.
    #[test]
    fn gch_jr_hotspot_free_mb_is_capacity_minus_used_rounded_down() {
        const MB: usize = 1024 * 1024;
        assert_eq!(hotspot_free_mb(64 * MB, 0), 64);
        assert_eq!(hotspot_free_mb(64 * MB, 3 * MB + 1), 60, "rounded down");
        assert_eq!(
            hotspot_free_mb(64 * MB, 64 * MB - 1),
            0,
            "sub-megabyte is 0"
        );
        assert_eq!(hotspot_free_mb(MB, 2 * MB), 0, "saturates");
    }

    /// gch h3/jx: HotSpot's policy capacity is `MaxHeapSize` (the `-Xmx`
    /// budget, both survivor spaces included), floored at the heap's own
    /// capacity for a heap built larger than its budget.
    #[test]
    fn gch_jx_soft_lru_capacity_is_max_heap_size_not_one_semispace_plus_old() {
        const MB: usize = 1024 * 1024;
        // -Xmx64m, a 21/2 MiB semispace pair and 43 MiB old: 53.5 MiB "capacity".
        let semi_plus_old = 10 * MB + MB / 2 + 43 * MB;
        assert_eq!(hotspot_policy_capacity(64 * MB, semi_plus_old), 64 * MB);
        assert_eq!(
            hotspot_free_mb(hotspot_policy_capacity(64 * MB, semi_plus_old), 4 * MB),
            60
        );
        assert_eq!(
            hotspot_policy_capacity(16 * MB, 32 * MB),
            32 * MB,
            "explicit arenas"
        );
    }

    /// gch h2/jr: `soft_lru_free_mb` is the default figure with the switch off
    /// (the positive control: byte-for-byte the old input) and HotSpot's figure
    /// with it on (counted). gci i48/zl2: on every collector -- Generational,
    /// G1 and ZGC (G1 used to answer its own figure whatever the switch said).
    #[test]
    fn gci_i48_zl2_soft_lru_free_mb_follows_the_switch_on_every_collector() {
        use crate::config::{GcAlgorithm, VmConfig};
        for algorithm in [GcAlgorithm::Generational, GcAlgorithm::G1, GcAlgorithm::Zgc] {
            let shared = crate::vm::SharedVm::new(VmConfig {
                gc_algorithm: algorithm,
                ..VmConfig::default()
            });
            let state = &shared.mem.ref_discovery;
            let heap = &shared.mem.heap;
            state.set_soft_hotspot_free(false);
            let off = soft_lru_free_mb(&shared);
            assert_eq!(
                off,
                heap.soft_ref_policy_free_mb(),
                "{algorithm:?} off: the old input"
            );
            assert_eq!(state.soft_hotspot_free_reads(), 0, "{algorithm:?}");
            state.set_soft_hotspot_free(true);
            let on = soft_lru_free_mb(&shared);
            assert_eq!(
                on,
                hotspot_free_mb(
                    hotspot_policy_capacity(shared.config.max_heap_size, heap.heap_capacity()),
                    heap.allocated_bytes()
                ),
                "{algorithm:?}"
            );
            assert_eq!(state.soft_hotspot_free_reads(), 1, "{algorithm:?}: engaged");
            // gch h3/jx: the capacity is `-Xmx` (MaxHeapSize), never below the
            // heap's own capacity, never below `-Xmx`.
            assert!(on >= hotspot_free_mb(heap.heap_capacity(), heap.allocated_bytes()));
            assert!(
                on >= hotspot_free_mb(shared.config.max_heap_size, heap.allocated_bytes()),
                "{algorithm:?}: MaxHeapSize - used, as LRUMaxHeapPolicy::setup"
            );
            if algorithm == GcAlgorithm::G1 {
                // G1's own figure is its soft capacity less allocated, never
                // above `-Xmx` less allocated: HotSpot's is the wider window.
                assert!(on >= off, "G1: {on} MB < the soft-capacity figure {off} MB");
            }
        }
    }

    /// gch h4/wr: `CRATONVM_SOFTREF_HOTSPOT_FREE` is default ON. Unset (and
    /// any word but an off word) builds a VM state with HotSpot's figure;
    /// `=0` (the kill switch) builds the young-bounded one. Both
    /// constructor arms (`constructors_are_bytecode`) read it the same way, and
    /// the explicit `with` stays off.
    #[test]
    fn gch_wr_soft_hotspot_free_is_default_on_and_zero_kills_it() {
        for bytecode in [false, true] {
            for (value, expect) in [(None, true), (Some("1"), true), (Some("0"), false)] {
                let state = cratonvm_types::flags::with_thread_overrides(
                    &[(SOFT_HOTSPOT_FREE_FLAG, value)],
                    || RefDiscoveryState::from_flags(bytecode),
                );
                assert_eq!(
                    state.soft_hotspot_free.load(Ordering::Relaxed),
                    expect,
                    "value={value:?} bytecode={bytecode}",
                );
            }
        }
        assert!(!RefDiscoveryState::with(true)
            .soft_hotspot_free
            .load(Ordering::Relaxed));
    }

    /// i77-L5 retirement (2026-10-28): `CRATONVM_REF_NOTES_PER_THREAD` is
    /// default ON. Unset (and any word but an off word) builds the striped
    /// notes; `=0` (the kill switch) the locked push. Both constructor arms
    /// read it the same way, and the explicit `with` stays on the locked push.
    #[test]
    fn i77_ref_notes_per_thread_is_default_on_and_zero_kills_it() {
        for bytecode in [false, true] {
            for (value, expect) in [(None, true), (Some("1"), true), (Some("0"), false)] {
                let state = cratonvm_types::flags::with_thread_overrides(
                    &[(REF_NOTES_PER_THREAD_FLAG, value)],
                    || RefDiscoveryState::from_flags(bytecode),
                );
                assert_eq!(
                    state.note_stripes().is_some(),
                    expect,
                    "value={value:?} bytecode={bytecode}",
                );
            }
        }
        assert!(RefDiscoveryState::with(true).note_stripes().is_none());
    }

    /// `note_allocation` registers nothing with the switch off (the positive
    /// control of the test below) and nothing for a class it cannot resolve.
    #[test]
    fn gcg_g4rs_note_allocation_is_inert_when_off_or_unresolved() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let obj = shared.mem.heap.alloc_object(ClassId::new(1), 2);
        shared.mem.ref_discovery.set_engaged(false);
        note_allocation(&shared, obj, ClassId::new(1));
        assert_eq!(shared.mem.ref_processor.lock().allocated_unbound_len(), 0);
        assert_eq!(shared.mem.ref_discovery.noted(), 0);

        shared.mem.ref_discovery.set_engaged(true);
        // A class id no class occupies: no kind, nothing registered.
        note_allocation(&shared, obj, ClassId::new(u32::MAX - 7));
        assert_eq!(shared.mem.ref_processor.lock().allocated_unbound_len(), 0);
        assert_eq!(shared.mem.ref_discovery.noted(), 0);
    }

    /// With the switch on, an allocation of a weak-reference class leaves one
    /// unbound entry in the processor, carrying the object's class id; a
    /// non-reference class leaves none.
    #[test]
    fn gcg_g4rs_note_allocation_registers_a_reference_class_when_engaged() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (weak_cid, string_cid) = {
            let cm = shared.classes.class_manager.read();
            (
                cm.find_bootstrap_class_by_name("java/lang/ref/WeakReference"),
                cm.find_bootstrap_class_by_name("java/lang/String"),
            )
        };
        let Some(weak_cid) = weak_cid else {
            // A class image without `java.lang.ref` loaded at construction:
            // the walk itself is covered by the pure test above.
            return;
        };
        shared.mem.ref_discovery.set_engaged(true);
        let obj = shared.mem.heap.alloc_object(weak_cid, 4);
        note_allocation(&shared, obj, weak_cid);
        if let Some(string_cid) = string_cid {
            let s = shared.mem.heap.alloc_object(string_cid, 4);
            note_allocation(&shared, s, string_cid);
        }
        let mut rp = shared.mem.ref_processor.lock();
        assert_eq!(rp.allocated_unbound_len(), 1);
        let pending = rp.take_allocated_unbound();
        assert_eq!(pending[0].class_id, weak_cid.as_u32());
        assert_eq!(pending[0].row.reference_obj, obj.as_ptr() as usize);
        assert_eq!(pending[0].row.ref_type, ReferenceType::Weak);
        assert_eq!(
            pending[0].row.referent, 0,
            "unbound until the pre-collection pass"
        );
        drop(rp);
        assert_eq!(shared.mem.ref_discovery.noted(), 1);
    }

    /// JIT round 21 wave 8, lane reflect (`CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES`,
    /// page `r21-reflective-new-skips-finalizer-and-reference-registration-20261019`):
    /// a constructor-less native allocation (`Unsafe.allocateInstance`, which
    /// every constructor `MethodHandle` and JDK 25 `Constructor.newInstance`
    /// allocates through) of a weak-reference class leaves one unbound entry,
    /// as the interpreter's `new` does; with the switch off (the kill switch)
    /// it leaves none, which was the defect.
    #[test]
    fn r21_no_constructor_allocation_notes_a_reference_class() {
        for notes in [true, false] {
            let config = crate::config::VmConfig {
                native_alloc_notes_references: notes,
                ..crate::config::VmConfig::default()
            };
            let shared = crate::vm::SharedVm::new(config);
            let weak_cid = shared
                .classes
                .class_manager
                .read()
                .find_bootstrap_class_by_name("java/lang/ref/WeakReference");
            let Some(weak_cid) = weak_cid else {
                // A class image without `java.lang.ref` loaded at construction.
                return;
            };
            shared.mem.ref_discovery.set_engaged(true);
            let obj = shared.mem.heap.alloc_object(weak_cid, 4);
            crate::runtime::interpreter::init_no_constructor_instance(&shared, obj, weak_cid);
            let mut rp = shared.mem.ref_processor.lock();
            if notes {
                assert_eq!(rp.allocated_unbound_len(), 1);
                let pending = rp.take_allocated_unbound();
                assert_eq!(pending[0].row.reference_obj, obj.as_ptr() as usize);
                assert_eq!(pending[0].row.ref_type, ReferenceType::Weak);
            } else {
                assert_eq!(rp.allocated_unbound_len(), 0, "kill switch: not noted");
            }
        }
    }
}
