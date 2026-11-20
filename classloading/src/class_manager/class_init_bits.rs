#[allow(unused_imports)]
use super::*;

/// Class ids [`ClassInitBits`] covers: `[0, 1 << 20)`, in 128 KiB per VM.
pub(in crate::class_manager) const CLASS_INIT_BITS_CAPACITY: usize = 1 << 20;

/// One class manager's lock-free "class is initialized" facts, shared with its
/// VM (`ClassRealm::class_init_bits`): one bit per dense `ClassId` for the
/// JIT's static-field helpers (`jit::helpers::class_init_memo`), and the
/// **init-reset generation** every per-thread memo of a positive answer checks
/// (`vm_util::is_class_initialized_via_manager`). Ids outside the bitmap
/// answer "not known", and the caller takes the authoritative
/// `ensure_class_initialized_shared` path.
///
/// # Initialization is monotonic, except across a stub upgrade
///
/// A `ClassId` that reads `CLASS_INIT_INITIALIZED` stays there, with one
/// exception: [`ClassManager::upgrade_synthetic_class`] (`--compatible` only)
/// replaces an INITIALIZED synthetic stub with its real class file on the same
/// `ClassId` and resets `init_state` so the real `<clinit>` runs. That upgrade
/// calls [`Self::note_init_reset`] under the class-manager write lock: it
/// clears the class's bit and advances the generation, which retires every
/// memo filled before it (interpreter round i2 wave 61, lane L5;
/// `interpreter-L5-the-class-initialized-memo-survives-a-stub-upgrade-FIXED-20261025.md`,
/// probe `L5W61StubUpgradeInit`). Until then the bitmap and the per-thread memo
/// both kept the stub's answer, so a thread that had touched the stub read the
/// real class's statics without its `<clinit>`.
///
/// Moved here from the VM crate in wave 61 so the upgrade can clear the bit
/// itself; per VM since interpreter round i1 wave 22 (lane L5), when it
/// replaced a process bitmap claimed by the first VM that touched it.
#[derive(Clone)]
pub struct ClassInitBits {
    pub(in crate::class_manager) words: Arc<[AtomicU64]>,
    /// Advanced by every [`Self::note_init_reset`]. Never reset.
    pub(in crate::class_manager) resets: Arc<AtomicU64>,
}

impl ClassInitBits {
    /// An empty memo (nothing known initialized), generation 0.
    pub fn new() -> Self {
        Self {
            // zs s6/rs: allocated zeroed (`CRATONVM_LAZY_ZERO_TABLES`).
            words: cratonvm_types::zeroed_table::zeroed_arc_slice(
                CLASS_INIT_BITS_CAPACITY / 64,
                || AtomicU64::new(0),
            ),
            resets: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether `class_id` is recorded initialized. One `Acquire` load (a
    /// plain load on x86-64), pairing with [`Self::set_as_of`]'s release: a
    /// reader that sees the bit and skips the JVMS §5.5 check must also see
    /// the static values `<clinit>` stored before the class was marked.
    #[inline]
    pub fn is_set(&self, class_id: u32) -> bool {
        let idx = class_id as usize;
        self.words
            .get(idx / 64)
            .is_some_and(|w| w.load(Ordering::Acquire) & (1u64 << (idx % 64)) != 0)
    }

    /// Whether `class_id` has a bit here (ids at or past the capacity, 1 Mi
    /// per VM, have none and are never recorded).
    #[inline]
    pub fn covers(&self, class_id: u32) -> bool {
        (class_id as usize) < self.words.len() * 64
    }

    /// The init-reset generation. One `Acquire` load. A memo of "class C is
    /// initialized" is valid while this still reads the value loaded BEFORE
    /// the read that found C initialized.
    #[inline]
    pub fn reset_generation(&self) -> u64 {
        self.resets.load(Ordering::Acquire)
    }

    /// Record `class_id` initialized (no-op outside the covered range), given
    /// the [`Self::reset_generation`] loaded before the read that found it
    /// initialized. Call only once its initialization has completed. If an
    /// init reset happened since, the bit is cleared again: the set's
    /// read-modify-write either precedes the reset's clear (which removes it)
    /// or reads from it, and then also sees the generation the reset advanced
    /// first.
    #[inline]
    pub fn set_as_of(&self, class_id: u32, generation_before: u64) {
        let idx = class_id as usize;
        if let Some(w) = self.words.get(idx / 64) {
            let bit = 1u64 << (idx % 64);
            w.fetch_or(bit, Ordering::AcqRel);
            if self.resets.load(Ordering::Acquire) != generation_before {
                w.fetch_and(!bit, Ordering::AcqRel);
            }
        }
    }

    /// `class_id`'s initialization was reset (a stub upgrade): advance the
    /// generation, then clear its bit. Cold; under the class-manager write
    /// lock, after `init_state` was reset.
    pub(in crate::class_manager) fn note_init_reset(&self, class_id: u32) {
        self.resets.fetch_add(1, Ordering::Release);
        let idx = class_id as usize;
        if let Some(w) = self.words.get(idx / 64) {
            w.fetch_and(!(1u64 << (idx % 64)), Ordering::Release);
        }
    }
}

impl Default for ClassInitBits {
    fn default() -> Self {
        Self::new()
    }
}

// `SYSTEM_CLASS_ID` / `class_is_java_lang_system` — a process-global latch of
// `java/lang/System`'s `ClassId` for `op_getstatic`'s System.out/err/in
// screen — was deleted in interpreter round i1 wave 5 (lane L4). Every
// definition of `System` overwrote it, so with two VMs in one process each
// screened its `getstatic`s against the id of whichever VM defined `System`
// last. The screen is per-VM now (`ClassRealm::system_class_id`, read by
// `interpreter::opcodes::static_may_be_system`). Do not reintroduce a
// process-global here.

/// True once two distinct `ClassId`s have shared a binary name. Single
/// relaxed load — the fast-path gate for loader-split field retargeting.
#[inline]
pub fn any_duplicate_class_name() -> bool {
    ANY_DUPLICATE_CLASS_NAME.load(Ordering::Relaxed)
}

// The annotation-proxy latch: [`StoreEpochs::annotation_proxy_defined`], set
// the first time a class named `java/lang/annotation/AnnotationProxy` is
// defined in a store (a process static, `ANY_ANNOTATION_PROXY_DEFINED`, until
// interpreter round i1 wave 25, lane L5: one VM minting the class raised it
// for every VM in the process).
//
// # Why a latch, when there is already an epoch-keyed negative
//
// `ClassRealm::is_annotation_proxy_class` gates a correctness decision on
// **every** `invokevirtual`/`invokeinterface` that hits the inline cache: an
// annotation proxy has no bytecode for the `Annotation` contract, so a cached
// virtual target must never serve one. Its own doc says "steady state is one
// relaxed load and one `u32` compare" — but that steady state is only reached
// once the class *exists*. `AnnotationProxy` is a VM-internal synthetic class
// that most programs never mint, so the realm's `annotation_proxy_cid` hint
// stays `u32::MAX` for the life of the process and every virtual invoke falls
// into the cold resolver.
//
// The cold resolver is `#[cold]` — an out-of-line call — and does two atomic
// loads before it can answer, so the advertised "one relaxed load and one
// `u32` compare" is not what the common case pays. Its negative cache is
// additionally keyed on [`class_definition_epoch`], bumped by **every class
// definition**, so while classes are loading the first virtual invoke after
// each definition also takes a `class_manager` read lock and probes the name
// across the builtin loader delegation chain. (That is O(classes defined),
// **not** O(invokes) — the resolver stamps the epoch on its negative. Said
// plainly here because the first draft of this note claimed the latter.)
//
// This latch answers the epoch-independent half of the question — *can* the
// class exist at all — in one relaxed load. It deliberately does **not**
// replace the per-realm `annotation_proxy_cid`: two VMs in one process mint
// the class independently and must not share an id. A realm that has not yet
// minted one simply performs the same lookup it does today, and only once the
// latch is raised.
//
// Never reset, for the same reason as [`ANY_CLASS_REDEFINED`]: a raised latch
// restores the pre-latch behaviour exactly, a lowered one could skip a live
// annotation proxy. Per store slot, like the epochs beside it: two live
// stores whose layout domains share a slot share the latch, which only
// over-approximates (the other VM performs the lookup, as before).

/// Raise [`ANY_DUPLICATE_CLASS_NAME`]. Called from the two places that can
/// make a name resolve to a second `ClassId`: the per-insert hook and the
/// unload path's index rebuild. `name` is the binary name that now has two
/// ids.
#[inline]
pub(in crate::class_manager) fn note_duplicate_class_name(name: &str) {
    // Relaxed store against a relaxed load: the flag only ever goes
    // false -> true, and a reader that misses the transition falls back to
    // the authoritative walk, which is the pre-latch behaviour.
    ANY_DUPLICATE_CLASS_NAME.store(true, Ordering::Relaxed);
    // r9w5 (typecheck5): the byte the JIT's inline `instanceof` final-class
    // miss tests at run time (`jit/src/x64/op_object.rs`). Raised here, on the
    // defining thread, before the duplicate is published — so before any
    // instance of it can reach compiled code. See
    // `cratonvm_types::raise_duplicate_class_name_seen`.
    //
    // Not for an ARRAY class name. The miss only answers for a plain object
    // (the emitted code sends every array header to the helper first), and a
    // plain object's class and superclasses are never named `[...`, so a
    // duplicated array class cannot feed the helper's by-name fallback there.
    // Array duplicates are also the routine kind — the per-loader array path
    // (`synthesize_array_class_for_loader`) mints a second `[Lp/X;` when two
    // loaders' same-named components differ — and letting them raise the byte
    // would turn the miss off in exactly the multi-loader applications that
    // have them.
    if duplicate_name_raises_jit_latch(name) {
        cratonvm_types::raise_duplicate_class_name_seen();
    }
}

/// Whether a second `ClassId` for `name` must raise the JIT's
/// duplicate-class-name byte — every name but an array class's. See
/// [`note_duplicate_class_name`].
#[inline]
pub(in crate::class_manager) fn duplicate_name_raises_jit_latch(name: &str) -> bool {
    !name.starts_with('[')
}

/// Generation of the class-name -> `ClassId` mapping — and, since round i1
/// wave 5, of the superclass / interface edges between stored classes (see
/// [`bump_class_definition_epoch`] for why).
///
/// Bumped by every mutation of `loaded_classes` — `loaded_classes_insert`,
/// `loaded_classes_remove`, and the bulk `retain` in
/// `unload_user_classes_inner` — which together are the only state
/// `get_loaded_class_id`, `get_loaded_class_id_for_requester` and
/// `find_unique_class_by_name` consult. (`user_loaders`, the other input to
/// the custom-loader fallback, is only ever extended in the same block as an
/// insert, so the insert hook covers it.)
///
/// This exists so a cache whose validity condition is *"this name still
/// resolves to this `ClassId`"* can be revalidated with one atomic load
/// instead of taking the `class_manager` read lock.
/// [`any_class_redefined`] does NOT stand in for it: redefinition changes a
/// class's *contents* in place and leaves the name mapping alone, whereas a
/// second loader defining the same name changes the mapping while redefining
/// nothing. Nor do the JIT's `jit_cache_generation` / supersede epoch,
/// which track compiled-code lifetime and never move for a plain class
/// definition.
///
/// Never reset; wraps only after 2^64 definitions.
pub(in crate::class_manager) static CLASS_DEFINITION_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Current [`CLASS_DEFINITION_EPOCH`]. One `Acquire` load — free on x86.
///
/// A reader that observes a *changed* epoch also observes every mapping
/// mutation the bumping writer made before it, because the writer bumps with
/// `Release` while holding the `class_manager` write lock.
#[inline]
pub fn class_definition_epoch() -> u64 {
    CLASS_DEFINITION_EPOCH.load(Ordering::Acquire)
}

/// Record that the class-name -> `ClassId` mapping changed. Called from the
/// three mutation sites while the `class_manager` write lock is held.
///
/// Also called by the `ClassStore` edge mutators (`add` over a forward edge,
/// `set_superclass`, `set_interfaces`, `remove`) through
/// `class::note_hierarchy_edge_change`, AFTER they have emptied the
/// secondary-supers closures: every cache tagged with this epoch memoises
/// subtype verdicts too (the cast sites' positive / negative receivers, the
/// catch-row refusals, the JIT's type-check memos), and a verdict is a function
/// of the superclass / interface edges, not only of the name mapping. Edge
/// changes ride on class definitions, so this adds no measurable bumps.
///
/// `layout_domain` names the store whose mapping or edges changed: its
/// [`StoreEpochs`] slot moves too (interpreter round i1 wave 20, lane L2).
#[inline]
pub(crate) fn bump_class_definition_epoch(layout_domain: u32) {
    store_epochs(layout_domain)
        .definition
        .fetch_add(1, Ordering::Release);
    CLASS_DEFINITION_EPOCH.fetch_add(1, Ordering::Release);
}

/// Advance the store's [`StoreEpochs::hierarchy_edge_epoch`]. Called by
/// `class::note_hierarchy_edge_change` (before it moves the definition epoch)
/// and by the array-class loader re-tag, with the `class_manager` write lock
/// held (or `&mut ClassStore`), after the change itself.
#[inline]
pub(crate) fn bump_hierarchy_edge_epoch(layout_domain: u32) {
    store_epochs(layout_domain)
        .hierarchy_edges
        .fetch_add(1, Ordering::Release);
}

/// Generation of the EXISTING class-name -> `ClassId` answers (i6-L2,
/// proposal `i1-L2-proposal-per-class-resolved-constant-pool`, stage 1a).
///
/// [`CLASS_DEFINITION_EPOCH`] moves on every definition and every hierarchy
/// edge change, which is what subtype-verdict memos need, but a cache that
/// memoises only a RESOLUTION ("name N resolves to id I") cannot be changed by
/// the definition of a brand-new name: every name such an entry holds was
/// already mapped when it was filled. This counter moves only when an existing
/// answer can change:
///
/// * a `loaded_classes` insert whose name (in either separator spelling)
///   already maps to a DIFFERENT id, or that displaces a different id — a
///   second loader defining `p/X` can turn a unique answer ambiguous or
///   re-point a loader-specific lookup;
/// * a `loaded_classes` removal, and the unload path's bulk `retain` (which
///   also covers `ClassId` reuse after unloading).
///
/// In-place layout replacement and the initiating-loader memo are NOT covered
/// here; tables keyed on this counter pair it with the VM's resolution epoch,
/// which covers them. Never reset.
pub(in crate::class_manager) static CLASS_NAME_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Current [`CLASS_NAME_GENERATION`]. One `Acquire` load. Same publication
/// argument as [`class_definition_epoch`]: the writer bumps with `Release`
/// while holding the `class_manager` write lock.
#[inline]
pub fn class_name_generation() -> u64 {
    CLASS_NAME_GENERATION.load(Ordering::Acquire)
}

#[inline]
pub(in crate::class_manager) fn bump_class_name_generation(layout_domain: u32) {
    store_epochs(layout_domain)
        .name_generation
        .fetch_add(1, Ordering::Release);
    CLASS_NAME_GENERATION.fetch_add(1, Ordering::Release);
}

/// Slots in the [`StoreEpochs`] table. A power of two: a store's slot is its
/// layout domain masked.
pub const STORE_EPOCH_SLOTS: usize = 64;

/// One class store's copies of [`class_definition_epoch`] and
/// [`class_name_generation`], plus its VM's resolution epoch (interpreter
/// round i1 wave 20, lane L2,
/// `interpreter-L5-resolution-epoch-is-process-wide`).
///
/// The process counters move on every store's events, so a per-thread site
/// cache tagged with them retired its entries whenever ANY VM in the process
/// defined, unloaded or redefined a class. A cache that can name its VM tags
/// with that VM's slot instead: every bump that moves a process counter for a
/// store also moves the same counter in the store's slot, so the slot pair is
/// exact for the store, and the process counters stay exact (conservative)
/// for every reader that cannot name one.
///
/// Slots are shared modulo [`STORE_EPOCH_SLOTS`]: two live stores whose
/// layout domains collide read each other's bumps. That only costs a
/// re-resolution, the process-wide cost this replaces. The resolution
/// counter lives here, not in the VM crate, so one slot holds all three and
/// a site-cache check touches one cache line.
#[repr(align(64))]
pub struct StoreEpochs {
    pub(in crate::class_manager) definition: AtomicU64,
    pub(in crate::class_manager) name_generation: AtomicU64,
    pub(in crate::class_manager) resolution: AtomicU64,
    /// The global-resolution epoch ([`StoreEpochs::resolution_global_epoch`]).
    pub(in crate::class_manager) resolution_global: AtomicU64,
    /// The hierarchy-edge epoch ([`StoreEpochs::hierarchy_edge_epoch`]).
    pub(in crate::class_manager) hierarchy_edges: AtomicU64,
    /// The in-place provenance epoch
    /// ([`StoreEpochs::class_origin_in_place_epoch`]).
    pub(in crate::class_manager) origin_in_place: AtomicU64,
    /// The annotation-proxy latch (see the note above
    /// [`note_duplicate_class_name`]): raised by `loaded_classes_insert` the
    /// first time this store defines `java/lang/annotation/AnnotationProxy`.
    pub(in crate::class_manager) annotation_proxy_defined: AtomicBool,
}

impl StoreEpochs {
    pub(in crate::class_manager) const fn new() -> Self {
        Self {
            definition: AtomicU64::new(0),
            name_generation: AtomicU64::new(0),
            resolution: AtomicU64::new(0),
            resolution_global: AtomicU64::new(0),
            hierarchy_edges: AtomicU64::new(0),
            origin_in_place: AtomicU64::new(0),
            annotation_proxy_defined: AtomicBool::new(false),
        }
    }

    /// How many times `Class::set_origin` has run (in any store: the writer
    /// cannot name its store, so it moves every slot): every provenance write
    /// except the ones the define paths make on a class not yet stored
    /// (`Class::set_origin_of_unstored_class`). One `Acquire` load
    /// (interpreter round i4 wave 94, lane L7).
    ///
    /// `class_origin_epoch` moves on every class DEFINITION too (the define
    /// paths install the new class's origin through the same counter), so a
    /// memo stamped with it retires on every define anywhere in the process.
    /// A memo keyed by class id whose answer can change only when an EXISTING
    /// class's provenance (and with it, on a stub upgrade, its method table)
    /// changes in place can be stamped with this instead: the selection and
    /// native-callback memos under `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH` /
    /// `CRATONVM_NATIVE_CALLBACK_MEMO_EDGE_EPOCH`.
    #[inline]
    pub fn class_origin_in_place_epoch(&self) -> u64 {
        self.origin_in_place.load(Ordering::Acquire)
    }

    /// This store's hierarchy-edge epoch (interpreter round i4 wave 94, lane
    /// L7; `i92-L7-selection-and-callback-memos-need-a-hierarchy-edge-epoch`).
    /// One `Acquire` load.
    ///
    /// It moves exactly when an EXISTING class's superclass / interface edges,
    /// its defining loader as `can_override` reads it, or the store's §5.4.5
    /// run-time package policy change: every `class::note_hierarchy_edge_change`
    /// (`ClassStore::set_superclass`, `set_interfaces`, `add` over a forward
    /// edge, `remove`, `set_override_packages_by_loader`,
    /// `note_split_boot_packages`) and the array-class loader re-tag in
    /// `ClassManager`. Each of those also moves [`Self::class_definition_epoch`],
    /// which in addition moves on EVERY definition of a new class. A memo
    /// whose answer reads only the stored classes by id (their edges, method
    /// tables, flags and loaders), never a name -> class answer, can be
    /// stamped with this instead: a brand-new class cannot be in an existing
    /// class's hierarchy except through a forward edge, and a forward edge
    /// moves this counter. In-place PROVENANCE changes (a stub promoted to
    /// real bytes, with a new method table) move `class_origin_epoch`, not
    /// this; such a memo pairs the two.
    #[inline]
    pub fn hierarchy_edge_epoch(&self) -> u64 {
        self.hierarchy_edges.load(Ordering::Acquire)
    }

    /// True once this store (or one sharing its slot) has defined
    /// `java/lang/annotation/AnnotationProxy`. One relaxed load: the fast-path
    /// gate for `ClassRealm::is_annotation_proxy_class`. Never reset.
    #[inline]
    pub fn annotation_proxy_defined(&self) -> bool {
        self.annotation_proxy_defined.load(Ordering::Relaxed)
    }

    /// This store's [`class_definition_epoch`]. One `Acquire` load.
    #[inline]
    pub fn class_definition_epoch(&self) -> u64 {
        self.definition.load(Ordering::Acquire)
    }

    /// This store's [`class_name_generation`]. One `Acquire` load.
    #[inline]
    pub fn class_name_generation(&self) -> u64 {
        self.name_generation.load(Ordering::Acquire)
    }

    /// This store's VM's resolution epoch. One `Acquire` load.
    #[inline]
    pub fn resolution_epoch(&self) -> u64 {
        self.resolution.load(Ordering::Acquire)
    }

    /// Advance this slot's resolution epoch AND its global resolution epoch
    /// ([`Self::resolution_global_epoch`]): the safe default, which retires
    /// every site entry whichever counter it was stamped with. The VM's
    /// `bump_resolution_epoch_in` calls it together with the process counter.
    ///
    /// The global counter moves first, so a reader that observes the moved
    /// ordinary counter also observes the moved global one. Both are
    /// `Release` increments of cells in the same cache line.
    #[inline]
    pub fn bump_resolution_epoch(&self) {
        self.resolution_global.fetch_add(1, Ordering::Release);
        self.resolution.fetch_add(1, Ordering::Release);
    }

    /// Advance this slot's resolution epoch only, NOT its global one: for a
    /// NEW row of a user loader's initiating memo or initiating record and
    /// nothing else (t1 wave 10, lane L71;
    /// `CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`). A row is read only by a
    /// resolution from a class with a loader namespace, so the entries of
    /// classes with none, stamped with the global counter, are meant to
    /// survive it. Any other caller would leave a stale global-mode entry
    /// valid: `vm/tests/t1_intv_global_stamp_bump_ratchet.rs` allows exactly
    /// one caller, `bump_resolution_epoch_for_loader_row_in`.
    #[inline]
    pub fn bump_resolution_epoch_loader_row_only(&self) {
        self.resolution.fetch_add(1, Ordering::Release);
    }

    /// This slot's GLOBAL resolution epoch (t1 wave 10, lane L71;
    /// `CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`): it moves with every
    /// resolution bump EXCEPT a new row of a user loader's initiating memo or
    /// initiating record ([`Self::bump_resolution_epoch_loader_row_only`]), so
    /// a site whose referencing class has no loader namespace can be stamped
    /// with it. One `Acquire` load.
    #[inline]
    pub fn resolution_global_epoch(&self) -> u64 {
        self.resolution_global.load(Ordering::Acquire)
    }
}

// A site-cache check reads up to three of a slot's counters (the name
// generation or definition epoch, and the ordinary or global resolution
// epoch): they must stay on ONE cache line (t1 INTV, 2026-11-02).
const _: () = assert!(std::mem::size_of::<StoreEpochs>() == 64);

pub(in crate::class_manager) static STORE_EPOCHS: [StoreEpochs; STORE_EPOCH_SLOTS] =
    [const { StoreEpochs::new() }; STORE_EPOCH_SLOTS];

/// The slot index of the store with `layout_domain`.
#[inline(always)]
pub fn store_epoch_slot(layout_domain: u32) -> usize {
    layout_domain as usize & (STORE_EPOCH_SLOTS - 1)
}

/// The [`StoreEpochs`] of slot `slot` (masked, so any value is in range).
#[inline(always)]
pub fn store_epochs_at(slot: usize) -> &'static StoreEpochs {
    &STORE_EPOCHS[slot & (STORE_EPOCH_SLOTS - 1)]
}

/// The [`StoreEpochs`] of the store with `layout_domain`.
#[inline(always)]
pub fn store_epochs(layout_domain: u32) -> &'static StoreEpochs {
    store_epochs_at(store_epoch_slot(layout_domain))
}

/// Advance every slot's resolution epoch: for a resolution invalidation that
/// cannot name its store. Cold.
pub fn bump_every_store_resolution_epoch() {
    for slot in &STORE_EPOCHS {
        slot.bump_resolution_epoch();
    }
}

/// Advance every slot's [`StoreEpochs::class_origin_in_place_epoch`]: called
/// by `Class::set_origin`, which cannot name its store. Cold (a stub upgrade
/// in production; the define paths do not call it).
pub(crate) fn bump_every_store_origin_in_place() {
    for slot in &STORE_EPOCHS {
        slot.origin_in_place.fetch_add(1, Ordering::Release);
    }
}

/// Whether mapping `name` to `id` can change an answer some lookup already
/// gave: `name`, or its other separator spelling (the lookups probe both),
/// already maps to an id other than `id`. Cold: class-definition path only.
pub(in crate::class_manager) fn name_insert_changes_an_existing_answer(
    name_definitions: &FxHashMap<Arc<str>, FxHashMap<ClassId, u32>>,
    name: &str,
    id: ClassId,
) -> bool {
    let maps_elsewhere = |key: &str| {
        name_definitions
            .get(key)
            .is_some_and(|ids| ids.keys().any(|&other| other != id))
    };
    if maps_elsewhere(name) {
        return true;
    }
    let other_spelling = if name.contains('/') {
        name.replace('/', ".")
    } else if name.contains('.') {
        name.replace('.', "/")
    } else {
        return false;
    };
    name_definitions.contains_key(other_spelling.as_str())
}

// The C1→C2 supersede epoch was a process static here until interpreter
// round i1 wave 19. It is per VM now: `JitRealm::supersede_epoch` in the `vm`
// crate, read by invoke-cache entries through `resolution::SupersedeGate`.

/// Install the JIT-invalidate hook. Called once by the VM during
/// `SharedVm::new`. Idempotent.
pub fn install_jit_invalidate_hook(hook: JitInvalidateHook) {
    if JIT_INVALIDATE_HOOK.set(hook).is_ok() {
        JIT_INVALIDATE_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// Invoke the JIT-invalidate hook for `class_id` of the store whose layout
/// domain is `owner_domain` (see [`JitInvalidateHook`]). Hot path is a single
/// relaxed atomic load + branch when no hook is attached (e.g. during
/// classloading-only unit tests).
#[inline]
pub(in crate::class_manager) fn fire_jit_invalidate_hook(class_id: u32, owner_domain: u32) {
    if !JIT_INVALIDATE_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = JIT_INVALIDATE_HOOK.get() {
        hook(class_id, owner_domain);
    }
}

// ---------------------------------------------------------------------------
// Round 4 audit fix (CRIT) — ResolutionCache invalidation hook
// ---------------------------------------------------------------------------
//
// Mirror of the JIT-invalidate hook above, fired at the same point in
// `redefine_class`. The VM-owned `SharedVm::resolution_cache` caches
// resolved field/method/call-site lookups keyed by (referring-class,
// cp-index). After a JVMTI redefine swaps the bytecode + constant pool
// of `class_id`, those cached entries are stale — the same cp-index in
// the new pool may refer to a different field/method, and entries that
// resolved INTO the redefined class hold pointers (declaring_class_id,
// field_index) that are no longer valid against the new layout.
//
// Previously only `InvokeCache` had a per-class invalidation path
// (via `RedefineGate`); `ResolutionCache` had no gate and no hook, so
// every `getfield`/`getstatic`/`invokestatic`/`invokevirtual` slow path
// returned the cached old resolution after a redefine.

/// Signature of the resolution-cache invalidation hook fired by
/// `redefine_class`.
///
/// Parameters: the `ClassId` (as `u32`) whose cached resolutions must be
/// evicted, and the firing store's `ClassStore::layout_domain()` — the same
/// owner token as [`JitInvalidateHook`]'s, since a `ClassId` names a class
/// only within its own store. The VM-side adapter takes the owning VM's
/// `resolution_cache` write lock and calls `invalidate_class` (see
/// `crate::resolution::ResolutionCache::invalidate_class`), which
/// drops every entry whose key refers to this class AND every entry
/// whose resolved declaring class IS this class.
///
/// The third parameter says what changed (interpreter round i1 wave 24, lane
/// L5); see [`ResolutionInvalidation`].
pub type ResolutionInvalidateHook = fn(u32, u32, ResolutionInvalidation);

/// What a [`ResolutionInvalidateHook`] fire changed about the class, which
/// decides how much of its cached resolution state may survive.
///
/// Until interpreter round i1 wave 24 the hook carried no reason and the
/// adapter dropped EVERY record keyed on the class for all three firing
/// sites. For a class whose constant pool was replaced that is required. For a
/// descendant whose only change is a shifted field layout
/// (`recompute_subclass_layouts`, after an ancestor stub was upgraded) it also
/// dropped the class's recorded resolution FAILURES (JVMS §5.4.3: a failed
/// entry must fail the same way forever; dropped, the next attempt could
/// succeed) and its permanent constants (a `CONSTANT_Dynamic` bootstrap ran a
/// second time and a `CONSTANT_MethodHandle` `ldc` answered a second,
/// non-identical handle, JVMS §5.4.3.6), although nothing in its pool had
/// changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionInvalidation {
    /// The class's constant pool (and bytecode) was replaced: a JVMTI
    /// redefinition, or a synthetic stub upgraded to its real class file.
    /// Every record keyed on the class is stale.
    ConstantPoolReplaced,
    /// Only the class's instance-field layout moved (`first_field_index` /
    /// `num_total_fields`); its constant pool is the same. Only resolutions
    /// that carry a field index are stale.
    FieldLayoutOnly,
}

pub(in crate::class_manager) static RESOLUTION_INVALIDATE_HOOK: OnceLock<ResolutionInvalidateHook> = OnceLock::new();
pub(in crate::class_manager) static RESOLUTION_INVALIDATE_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the resolution-cache invalidate hook. Called once by the VM
/// during `SharedVm::new`. Idempotent.
pub fn install_resolution_invalidate_hook(hook: ResolutionInvalidateHook) {
    if RESOLUTION_INVALIDATE_HOOK.set(hook).is_ok() {
        RESOLUTION_INVALIDATE_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// Invoke the resolution-cache invalidate hook for `class_id` of the store
/// whose layout domain is `owner_domain` (see [`ResolutionInvalidateHook`]).
/// Hot path is a single relaxed atomic load + branch when no hook is attached
/// (e.g. during classloading-only unit tests).
#[inline]
pub(in crate::class_manager) fn fire_resolution_invalidate_hook(class_id: u32, owner_domain: u32, what: ResolutionInvalidation) {
    if !RESOLUTION_INVALIDATE_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = RESOLUTION_INVALIDATE_HOOK.get() {
        hook(class_id, owner_domain, what);
    }
}

// ---------------------------------------------------------------------------
// T10.5 — vtable install hook registry
// ---------------------------------------------------------------------------
//
// After a class is linked (after its methods are parsed and its superclass's
// vtable has already been built) the class loader fires a `VtableInstallHookScoped`
// that hands a pre-built vec of slot descriptors to the VM. The VM's installed
// adapter converts each descriptor into a `crate::runtime::vtable::VtableEntry`
// and stores the whole vec in `shared.classes.vtable_manager` via `install_vtable`.
// The vtable is then queryable by slot in O(1) for the lifetime of the class.
//
// The hook delivers OWNED data (moved `Vec`) so the adapter doesn't need to
// re-walk the class_manager under any lock. This keeps the class-link path
// and the vtable-install path fully decoupled.

/// T10.9.A — dispatch-time snapshot carried inside each `VtableSlotDescriptor`.
///
/// The link-time vtable installer captures everything the interpreter needs
/// to execute a virtual call **without** re-entering `class_manager.read()`.
/// A `None` variant means "no snapshot" (abstract or native method) — the
/// caller falls through to the slower resolution path.
///
/// Fields line up with `cratonvm_reader::CachedBytecodeMethod` so the VM
/// adapter can build that type directly. `code` is stored as `Vec<u8>`
/// because classloading runs before the VM's padded-bytecode helper is
/// reachable; the adapter pads-and-Arc's before handing off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtableMethodSnapshot {
    /// Fully-qualified internal name of the declaring class: a refcount
    /// clone of `Class::name` (zr z11/md, 2026-10-13; it was a fresh
    /// `String` per snapshot, and the VM adapter then copied it again into
    /// the `CachedBytecodeMethod`'s `Arc<str>`).
    pub class_name: Arc<str>,
    /// Source file (from SourceFile attribute), if any. One `Arc<str>` per
    /// class, shared by its snapshots (zr z11/md; was a `String` each).
    pub source_file: Option<Arc<str>>,
    /// Raw bytecode (NOT yet padded for speculative reads).
    ///
    /// A [`ByteView`](cratonvm_reader::ByteView) over the class-file buffer
    /// the method's `Code` attribute already pins (zr z11/md, 2026-10-13).
    /// It was an `Arc<[u8]>` copy (`ByteView::to_arc`) per declared virtual
    /// method, held for the class's lifetime by `vtable_descriptors`; the
    /// only consumer, the VM adapter in `vm/src/runtime/vtable.rs`, reads it
    /// as `&[u8]` and pads its own copy.
    pub code: cratonvm_reader::ByteView,
    /// Exception handler table.
    pub exception_table: Vec<cratonvm_reader::attribute::ExceptionTableEntry>,
    /// Max operand-stack depth.
    pub max_stack: u16,
    /// Max local-variable count.
    pub max_locals: u16,
    /// Parameter slot count (excluding `this`).
    pub num_params: u16,
    /// ACC_SYNCHRONIZED flag.
    pub is_synchronized: bool,
    /// ACC_STATIC flag (always false for vtable slots, kept for parity
    /// with `CachedBytecodeMethod`).
    pub is_static: bool,
    /// ACC_NATIVE flag — when true, `code` is empty and the interpreter
    /// must route through the native-method registry instead.
    pub is_native: bool,
}

/// One slot in a class's vtable, as seen by the class loader. The VM's
/// installed hook converts these into `runtime::vtable::VtableEntry`.
///
/// The absence of an entry (i.e. `None` in the vec) represents a reserved
/// slot that no concrete method fills yet (e.g. an abstract superclass
/// method that the subclass hasn't overridden). Dispatch against such a
/// slot would raise `AbstractMethodError` at runtime.
///
/// zs s2/fp2: a shared handle (8 bytes) to the slot's [`VtableSlotData`].
/// Every class's vector starts as its superclass's, so before this each
/// inherited slot was a 48-byte by-value copy of the declaring class's
/// entry: ~1.1 KB a java.base class, most of it copies
/// (`docs/feature-designs/zgc-architecture.md`).
/// An inherited slot now shares the declaring class's block; an override
/// replaces the slot with a new one (copy on override). The data is
/// immutable once built, so sharing changes no answer; equality is the
/// data's. `CRATONVM_VTABLE_SHARE_SLOTS=0` deep-copies every inherited slot
/// again (the footprint kill arm, [`ClassManager`]'s `vtable_share_slots`).
/// zt t3/fp (zs3rv2 §9): that arm does not unshare everything. A method that
/// overrides several inherited slots stores one handle in each, and the VM's
/// install hook receives the loader's handles. No answer depends on it (the
/// data is immutable); the kill restores the inherited-slot copies only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtableSlotDescriptor(pub(in crate::class_manager) Arc<VtableSlotData>);

impl VtableSlotDescriptor {
    /// Wrap one slot's data in a new shared block.
    #[inline]
    pub fn new(data: VtableSlotData) -> Self {
        Self(Arc::new(data))
    }

    /// The slot's data.
    #[inline]
    pub fn data(&self) -> &VtableSlotData {
        &self.0
    }

    /// `true` when both handles share one block (an inherited slot and its
    /// declaring class's own entry, under the default sharing).
    #[inline]
    pub fn shares_block_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// A handle to a private copy of the data: what every inherited slot was
    /// before zs s2/fp2, and is under `CRATONVM_VTABLE_SHARE_SLOTS=0`.
    #[inline]
    pub fn deep_copy(&self) -> Self {
        Self(Arc::new((*self.0).clone()))
    }
}

impl std::ops::Deref for VtableSlotDescriptor {
    type Target = VtableSlotData;
    #[inline]
    fn deref(&self) -> &VtableSlotData {
        &self.0
    }
}

impl From<VtableSlotData> for VtableSlotDescriptor {
    #[inline]
    fn from(data: VtableSlotData) -> Self {
        Self::new(data)
    }
}

/// The data of one vtable slot ([`VtableSlotDescriptor`] is the shared
/// handle the vectors hold).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtableSlotData {
    /// ClassId (as `u32`) of the class that actually declares the method
    /// occupying this slot. May be the subclass (override) or a
    /// superclass (inherited).
    pub declaring_class_id: u32,
    /// Index into the declaring class's `methods` vec.
    pub method_index: u32,
    /// Method name, stored so the VM side can rebuild a `name_to_slot`
    /// index without re-parsing the class file.
    ///
    /// T10.9.E: `Arc<str>` (was `String`) — the source `ClassFileMethod.name`
    /// is already `Arc<str>` so the producer clones the existing arc
    /// (refcount bump, no allocation) instead of `.to_string()`-ing it.
    /// Saves ~90k throwaway allocations per Spring Boot cold start. The VM
    /// consumer (`vm/src/runtime/vtable.rs`, `install_vtable_descriptors` and
    /// `build_dispatch_snapshot`) takes `Arc::clone(&d.method_name)`, a
    /// refcount bump (zt t2/fp: the T10.9.E follow-up TODO that stood here
    /// was long done).
    pub method_name: Arc<str>,
    /// Method descriptor.
    ///
    /// T10.9.E: `Arc<str>` (was `String`) — same rationale as `method_name`.
    pub descriptor: Arc<str>,
    /// T10.9.A — snapshot of everything the interpreter needs to execute
    /// the method without re-entering the class manager. `None` for
    /// abstract methods (no Code attribute) or when the snapshot couldn't
    /// be built at link time (defensive fallback).
    ///
    /// Behind an `Arc`: every subclass's vtable starts as a clone of its
    /// parent's, and a by-value snapshot made that clone a deep copy of each
    /// inherited method's class-name `String`, source-file `String` and
    /// exception-table `Vec` — per class linked, for every inherited
    /// bytecode method (every class inherits `Object`'s). An inherited slot
    /// now shares its declaring class's snapshot.
    pub dispatch: Option<Arc<VtableMethodSnapshot>>,
}

// ---------------------------------------------------------------------------
// C2 review P1 — VM-scoped vtable hooks
// ---------------------------------------------------------------------------
//
// The original hooks were `fn(u32, Vec<…>)` / `fn(u32, usize)`: captureless
// function pointers with no VM parameter, so the VM's adapter wrote into a
// process-global `VtableManager` (`GLOBAL_VTABLE_MANAGER`) keyed by a bare
// `ClassId`. Because `ClassId`s are allocated per VM, a second VM's vtables
// landed in the first VM's index under colliding ids, and
// `resolve_virtual_slot` could hand a caller in VM B a `CachedBytecodeMethod`
// built from VM A's class — silent wrong virtual dispatch (item V1 in
// `docs/architecture/per-vm-state.md`).
//
// Unlike the redefine / JIT-invalidation hooks, this one cannot be fixed by
// fanning out to every live VM: invalidation is idempotent and safe to
// over-apply, whereas a vtable *install* is a write of authoritative state.
//
// So: a VM-scoped hook family taking a [`ClassHandle`] instead of a `u32`.
// The handle carries the owning VM's identity *and* the class's metadata
// generation, which is what lets the VM side both (a) route the install into
// the right VM's manager and (b) tell whether an install it is holding was
// superseded by a later redefine. Every VM installs it since interpreter
// round i1 wave 19; the legacy `fn(u32, …)` family, which no VM installed
// any more, was deleted in wave 21 (lane L5). See
// `docs/feature-designs/class-resolution-and-caching.md` §4.

/// The VM's vtable-install hook.
///
/// Parameters: `(class_handle, entries)`. The handle names the VM, the class
/// and the metadata generation the descriptor vec was built from;
/// `entries[i]` is the vtable slot at position `i`. The hook MUST NOT
/// re-enter the class manager (the class loader still holds `&mut self` when
/// firing it); the vec is fully self-contained.
pub type VtableInstallHookScoped = fn(ClassHandle, Vec<Option<VtableSlotDescriptor>>);

/// T10.9.A — the VM's vtable-override hook.
///
/// Fired immediately after the install hook for each super-class slot that
/// the newly defined class overrode. The VM's listener
/// (`vm/src/runtime/vtable.rs`, `vtable_override_adapter_scoped`) does
/// nothing by default since interpreter round i1 wave 22: the super's own
/// vtable entry stays correct for receivers of the super's class, and no
/// cached dispatch entry depends on the signal
/// (`OVERRIDE_SIGNAL_CLEARS_SUPER_ENTRY` restores the old invalidation).
///
/// Parameters: `(super_class_handle, slot_index)`. May be fired multiple
/// times per class definition (once per overridden slot).
pub type VtableOverrideHookScoped = fn(ClassHandle, usize);

pub(in crate::class_manager) static VTABLE_INSTALL_HOOK_SCOPED: OnceLock<VtableInstallHookScoped> = OnceLock::new();
pub(in crate::class_manager) static VTABLE_OVERRIDE_HOOK_SCOPED: OnceLock<VtableOverrideHookScoped> = OnceLock::new();
pub(in crate::class_manager) static VTABLE_SCOPED_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the VM-scoped vtable-install hook. Idempotent — only the first
/// install wins. Called by every `SharedVm::new`; the hook routes by the
/// handle's VM, so one process-wide function serves every VM.
pub fn install_vtable_install_hook_scoped(hook: VtableInstallHookScoped) {
    if VTABLE_INSTALL_HOOK_SCOPED.set(hook).is_ok() {
        VTABLE_SCOPED_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// Install the VM-scoped vtable-override hook. Idempotent.
pub fn install_vtable_override_hook_scoped(hook: VtableOverrideHookScoped) {
    let _ = VTABLE_OVERRIDE_HOOK_SCOPED.set(hook);
    VTABLE_SCOPED_HOOK_ACTIVE.store(true, Ordering::Release);
}

/// Fire the vtable-install hook for `class_id`. Hot path is one atomic load
/// and a branch when no hook is attached (unit tests that exercise the class
/// loader in isolation).
///
/// Nothing is installed when `class_id`'s generation counter is exhausted
/// and no handle can be minted: a missing vtable only sends virtual dispatch
/// down the slow path. That is also what happened before wave 21, whose
/// fallback went to a legacy `fn(u32, …)` hook no VM installed any more.
#[inline]
pub(in crate::class_manager) fn fire_vtable_install_hook_for(
    realm: &MetadataRealm,
    class_id: ClassId,
    entries: Vec<Option<VtableSlotDescriptor>>,
) {
    if !VTABLE_SCOPED_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = VTABLE_INSTALL_HOOK_SCOPED.get() {
        if let Some(handle) = realm.mint_class(class_id) {
            hook(handle, entries);
        }
    }
}

/// Fire the vtable-override hook for `super_class_id`. Same rules as
/// [`fire_vtable_install_hook_for`].
#[inline]
pub(in crate::class_manager) fn fire_vtable_override_hook_for(realm: &MetadataRealm, super_class_id: ClassId, slot: usize) {
    if !VTABLE_SCOPED_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = VTABLE_OVERRIDE_HOOK_SCOPED.get() {
        if let Some(handle) = realm.mint_class(super_class_id) {
            hook(handle, slot);
        }
    }
}

// ---------------------------------------------------------------------------
// WP1.5 — built-in class-loader registration hook
// ---------------------------------------------------------------------------

/// Boot-time hook that associates the three built-in class-loader names
/// (`BootLoader`, `ClassLoaders$PlatformClassLoader`,
/// `ClassLoaders$AppClassLoader`) with reserved [`ClassLoaderId`]s.
///
/// Implementation note: `ClassLoader`-side state (the singleton instances
/// themselves) lives in `native-builtins/src/classloader.rs`. This hook
/// only asks the `ClassManager` to acknowledge the names so that classes
/// *defined by* a built-in loader can be looked up by name without
/// consulting a live ClassLoader object. The built-in loaders always use
/// `ClassLoaderId::BOOTSTRAP (0)` internally; the name registration
/// merely increments a diagnostic counter that tests observe.
///
/// Idempotent — safe to call from any number of initialization paths.
pub fn register_builtin_classloaders() {
    crate::builtin_loaders::register_builtin_loader_aliases();
}

/// Return `true` if `internal_name` is the JVM-internal name of one of
/// the three built-in class loaders (BootLoader, PlatformClassLoader,
/// AppClassLoader) or the shared `BuiltinClassLoader` base.
///
/// Public helper so `native-builtins` can check the reflective mirror
/// name of an allocated loader and dispatch special behavior (e.g. the
/// synthetic `getName()` result) without hard-coding the strings.
pub fn is_builtin_classloader_name(internal_name: &str) -> bool {
    crate::builtin_loaders::builtin_loader_kind(internal_name).is_some()
}

/// Options for [`ClassManager::define_class_with_options`] (NEW-8 + WP2.3).
///
/// Callers that do not need any of the non-default behavior should
/// use the unadorned [`ClassManager::define_class`] entry point which
/// uses `DefineClassOptions::default()`.
#[derive(Debug, Clone, Default)]
pub struct DefineClassOptions {
    /// Override the stored class name. If `Some`, the class is
    /// registered under this name instead of the `this_class` entry
    /// from the class file's constant pool. Used by
    /// `Lookup.defineHiddenClass` so two hidden classes produced from
    /// the same source template get distinct, unique names.
    pub override_name: Option<String>,
    /// Mark the class as hidden (JEP 371) atomically with
    /// registration. Hidden classes are not returned by
    /// `find_class_by_name` / `Class.forName` /
    /// `ClassLoader.findLoadedClass`.
    pub hidden: bool,
    /// WP2.3: Suppress the verifier on these bytes. Off by default (we
    /// run the structural + bytecode verifier). Hidden-class /
    /// runtime-generation paths sometimes set this to `true` because
    /// the JDK trusts its own emitter; ByteBuddy + CGLIB also benefit.
    pub skip_verification: bool,
    /// WP2.3: Optional `CodeSource` to attribute to the new class. If
    /// `Some`, this overrides the default classpath-derived
    /// `code_source`. Used by `defineClass(name, bytes, off, len, pd)`
    /// natives so `Class.getProtectionDomain()` later reflects the
    /// caller-supplied PD.
    pub code_source: Option<CodeSource>,
    /// WP2.3: When defining a class that names an existing class, allow
    /// the redefinition to replace the old one in place. Used by
    /// `java.lang.instrument.Instrumentation.redefineClasses` (WP2.4).
    /// When `false` (default) a duplicate define returns
    /// `LinkageError::IncompatibleClassChangeError` per JVMS §5.3.5.
    pub allow_redefine: bool,
    /// WP2.3: Nest-host attribution for hidden classes. When `Some`,
    /// the new class joins the named class's nest (i.e. its
    /// `nest_host` is set to the lookup class's nest host, NOT to its
    /// own name). Mirrors `Lookup.defineHiddenClass(... NESTMATE ...)`.
    ///
    /// W3-2: when this is `None` **and** `hidden` is set, the new class is in
    /// a nest of its own and any `NestHost` attribute in the supplied bytes is
    /// discarded — see `hidden_class_drops_class_file_nest_host`. `None` on
    /// a non-hidden define still means "use the class file's attribute".
    pub nest_host_class_name: Option<String>,
    /// gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 7): the internal
    /// name of the LOOKUP class a NESTMATE hidden class was defined through
    /// (HotSpot's `jvm_lookup_define_class` `lookup_k`), which is not
    /// [`Self::nest_host_class_name`] when the lookup class is itself a nest
    /// member. Recorded in [`ClassManager::hidden_lookup_class_names`] for the
    /// `-Xlog:class+load` `source:`; it changes nothing else. `None` (the
    /// default) records nothing.
    pub lookup_class_name: Option<String>,
    /// BUG-10: Privileged define from a trusted JVM-internal code-generation
    /// path (`sun.misc.Unsafe.defineClass`). On HotSpot, `Unsafe.defineClass`
    /// bypasses `ClassLoader.preDefineClass`'s "Prohibited package name:
    /// java.*" guard, so toolchains (ByteBuddy/CGLIB) can inject a privileged
    /// accessor into a protected platform package. When `true`, the H5
    /// prohibited-package check below is skipped — matching that semantics.
    /// The ordinary `ClassLoader.defineClass` path leaves this `false`.
    pub privileged_define: bool,
    /// Force loader-faithful supertype/interface linking for THIS define,
    /// regardless of the global `CRATONVM_LOADER_AWARE_RESOLUTION` gate
    /// (default off — see `loader_aware_resolution` above, which keeps
    /// every ordinary define byte-identical). A dynamically generated
    /// `$ProxyN` class MUST link against the exact interface `ClassId` its
    /// generator resolved (e.g. the interface a caller passed to
    /// `Proxy.newProxyInstance(loader, interfaces, handler)`) — unlike the
    /// broader Hibernate-enhancement scenario the global gate exists for,
    /// there is no "which copy is more correct" ambiguity here, so this is
    /// unconditionally required for a generated proxy to actually implement
    /// the interface it was built for. Without this, `resolve_supertype`
    /// falls through to the loader-agnostic `load_class(name)` and can bind
    /// to a DIFFERENT, unrelated same-named class (e.g. one already loaded
    /// by the application loader), so the resulting `$ProxyN` type-checks
    /// (`interfaceClass.isInstance(proxy)`) and `Method.invoke` against the
    /// requested interface both fail — see "Residual issue B" in
    /// `mergedannotationstests-proxy-class-identity-reflection-vs-synthesize.md`
    /// (found via annotation-proxy work but is a general `CRATONVM_REAL_PROXY`
    /// bug, reproducible with a plain `Proxy.newProxyInstance` + custom
    /// `ClassLoader`, independent of annotations).
    pub force_loader_faithful_linking: bool,
    /// Exact superclass identity supplied by a runtime define path that has
    /// already performed JVMS initiating-loader resolution.
    pub superclass_id_override: Option<ClassId>,
    /// Exact interface identities, in class-file declaration order.
    pub interface_id_overrides: Option<Vec<ClassId>>,
    /// Explicit provenance for the class being defined, overriding whatever
    /// [`ClassManager::classify_defined_origin`] would infer from the loader
    /// and the class name.
    ///
    /// A code generator knows *why* it is emitting these bytes; name-shape
    /// heuristics only guess. `Lookup.defineHiddenClass` on behalf of
    /// `LambdaMetafactory`, `Proxy.defineClass0` and the reflection accessor
    /// emitters should all set this so the `--dump-class-origins` census
    /// reports the real generator rather than falling back to "some class its
    /// loader defined".
    ///
    /// `None` (the default) keeps inference, so the existing
    /// `..Default::default()` literals across the tree are unaffected.
    pub origin: Option<ClassOrigin>,
    /// The define came from a user-defined loader whatever `loader_id` the
    /// door chose (`DefineClassFull::user_loader_define`): the
    /// `CRATONVM_DEFINE_NO_CLASS_PATH_CODE_SOURCE` step treats it as a
    /// user-loader define even when the loader-aware-resolution kill switch
    /// sent it to the application namespace (interpreter round i4 wave 96,
    /// lane L5).
    pub user_loader_define: bool,
    /// zt t16/df6 (`docs/feature-designs/zgc-architecture.md`
    /// §1, corner 2): the caller makes HotSpot's `ClassLoader.addClass`
    /// upcall after this define and settles the name afterwards
    /// ([`ClassManager::bind_unbound_define`] when the upcall returned,
    /// [`ClassManager::withdraw_unbound_define`] when it threw). Honoured only
    /// for a named (not hidden, not `allow_redefine`) define into a
    /// USER-DEFINED namespace that mints a fresh class; any other define
    /// binds its name at once, as before. `false` (the default) binds at once.
    pub bind_name_after_add_class: bool,
    /// The define is one CratonVM makes for itself where HotSpot's loader would
    /// define nothing (a flat-store answer the VM falls back to after a loader
    /// refused, an eager load of its own), not a define the defining loader
    /// asked for. A built-in loader's define that breaks a JVMS §5.3.4 loader
    /// constraint is refused (`CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL`)
    /// unless this is set; it is then counted and admitted as before.
    /// `false` (the default): the loader's own define. Interpreter round i6
    /// wave 117, lane L5 (`i46-L5-proposal-a-flat-store-define-is-told-from-a-built-in-loaders-own`).
    pub vm_fallback: bool,
}

/// Options for [`ClassManager::redefine_class`] (WP2.4-B).
///
/// This is the JVMTI / JEP 109 in-place class redefinition entry point.
/// `redefine_class` enforces strict structural equivalence (same name,
/// superclass, interfaces, fields, and method declarations) — only
/// method bodies, the constant pool, static initializers, and
/// annotations may change. See the doc comment on
/// [`ClassManager::redefine_class`] for the full constraint list.
#[derive(Debug, Clone, Default)]
pub struct RedefineOptions {
    /// When `true`, skip the strict structural-equivalence check.
    /// Used by `retransformClasses` when an agent has explicitly
    /// registered with `canRetransform = true` and the JVM trusts it.
    /// Default: `false` (strict redefine).
    ///
    /// Even with this flag set, the new bytes MUST share the same
    /// class name as the old class — replacing one class's bytes with
    /// another class's bytes is never legal under JEP 109.
    pub skip_structural_check: bool,
    /// Trace-flag for diagnostics. When `true`, the redefine path
    /// emits a `tracing::debug` line listing the methods whose bodies
    /// changed (and any structural-check rejections). Default: `false`.
    pub log_diff: bool,
    /// When `true`, do NOT overwrite the cached "original" class bytes
    /// with the redefined bytes. Set by `Instrumentation.retransformClasses`:
    /// per JVMTI, each retransformation re-runs the full transformer chain
    /// against the class's ORIGINAL bytes, so the base must be preserved.
    /// Overwriting it (the `redefineClasses` behaviour) makes the next
    /// retransform of the same class weave on top of the already-woven bytes
    /// — e.g. `mockStatic(X)` then `mock(X)` double-instruments X and the
    /// instance mock fails. Default: `false` (explicit redefine updates the
    /// base, matching HotSpot's `cached_class_file` semantics).
    pub preserve_original_bytes: bool,
}

/// What a JVMTI redefinition may not change about a loaded class, snapshotted
/// by `ClassManager::redefinition_shape` for `redefine_class` and
/// `check_redefinition`.
pub(in crate::class_manager) struct RedefinitionShape {
    pub(in crate::class_manager) name: String,
    pub(in crate::class_manager) loader: ClassLoaderId,
    /// Superclass internal name; empty for `java/lang/Object`.
    pub(in crate::class_manager) super_name: String,
    /// Direct interfaces, in declaration order.
    pub(in crate::class_manager) iface_names: Vec<String>,
    /// `(name, descriptor, access flags)` of each declared field.
    pub(in crate::class_manager) field_sigs: Vec<(String, String, u16)>,
    /// `(name, descriptor, access flags)` of each declared method, in the
    /// class's method order.
    pub(in crate::class_manager) method_sigs: Vec<(String, String, u16)>,
    /// The class file's access flags, or `None` when the class's recorded
    /// flags are not a class file's to compare (a compatibility stub, a hidden
    /// class).
    pub(in crate::class_manager) class_flags: Option<u16>,
    /// The class's `NestHost`, `NestMembers`, `Record` and
    /// `PermittedSubclasses` attributes as it was defined with them, or `None`
    /// when they are not a class file's to compare (the same classes as
    /// `class_flags`). Interpreter round i1 wave 45, lane L3.
    pub(in crate::class_manager) nesting: Option<RedefinitionNesting>,
}

/// The four class attributes a redefinition may not change
/// ([`RedefinitionShape::nesting`]; HotSpot's
/// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_ATTRIBUTE_CHANGED`).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::class_manager) struct RedefinitionNesting {
    pub(in crate::class_manager) nest_host: Option<String>,
    pub(in crate::class_manager) nest_members: Vec<String>,
    /// `(name, descriptor)` of each record component, in order.
    pub(in crate::class_manager) record_components: Vec<(String, String)>,
    pub(in crate::class_manager) permitted_subclasses: Vec<String>,
}

impl RedefinitionNesting {
    /// Why `new` changes the attributes `self` has, in HotSpot's terms, or
    /// `None`. Measured on HotSpot 25 (`L3W45RedefineShapes` and its lane's
    /// order experiments): the nest host must name the same class (or be
    /// absent from both); the nest members and the permitted subclasses must
    /// be the same SET (a reordered `permits` list is accepted); the record
    /// components must be the same names and descriptors in the same order
    /// (component annotations are not compared).
    pub(in crate::class_manager) fn changed_by(&self, new: &RedefinitionNesting) -> Option<String> {
        fn same_set(old: &[String], new: &[String]) -> bool {
            old.len() == new.len() && old.iter().all(|name| new.contains(name))
        }
        if self.nest_host != new.nest_host {
            return Some(format!(
                "NestHost changed: was {:?} now {:?}",
                self.nest_host, new.nest_host
            ));
        }
        if !same_set(&self.nest_members, &new.nest_members) {
            return Some(format!(
                "NestMembers changed: was {:?} now {:?}",
                self.nest_members, new.nest_members
            ));
        }
        if self.record_components != new.record_components {
            return Some(format!(
                "Record changed: was {:?} now {:?}",
                self.record_components, new.record_components
            ));
        }
        if !same_set(&self.permitted_subclasses, &new.permitted_subclasses) {
            return Some(format!(
                "PermittedSubclasses changed: was {:?} now {:?}",
                self.permitted_subclasses, new.permitted_subclasses
            ));
        }
        None
    }
}

/// The verifier policy a redefinition's new bodies are held to, by
/// `ClassManager::redefine_class_typed` (Step 5b) and, before anything is
/// installed, by `ClassManager::check_redefinition`.
///
/// A redefine must not be judged by a STRICTER policy than the definition it
/// replaces. `define_class_with_options` defers the Pass-3 *type-state* verdict
/// for any class whose defining loader is `UserDefined` while
/// `loader_aware_resolution()` is on (the default), because the hierarchy
/// adapter cannot preserve both loader identities through every pre-definition
/// edge and produces false areturn/checkcast rejections for otherwise valid
/// forked bytecode. It enforces the hierarchy-independent structural half and
/// moves on.
///
/// The redefine path used to run the FULL `verify_class` unconditionally. So
/// every application class in a Spring / WildFly / H2 / Elasticsearch run — all
/// of them defined by user loaders — was excused from Pass 3 when it loaded and
/// then held to it the moment a JVMTI agent retransformed it. Mockito's inline
/// mock maker retransforms every class it mocks, so the class of false
/// rejection the deferral exists to avoid was reachable there and nowhere
/// else: the class loads, the mock is created, and `retransformClasses0` logs
/// an `UnsupportedClassRedefinitionError` naming a verifier complaint about
/// bytecode the VM already accepted.
pub(in crate::class_manager) fn verify_redefined_class(
    cls: &Class,
    store: &ClassStore,
    hierarchy: &dyn ClassHierarchy,
) -> Result<(), LinkageError> {
    if loader_aware_resolution() && matches!(cls.loader_id, ClassLoaderId::UserDefined(_)) {
        crate::verifier::verify_class_structure(cls, store)
            .and_then(|()| crate::verifier::verify_class_structural_bytecode(cls))
    } else {
        crate::verifier::verify_class(cls, store, hierarchy)
    }
}

/// `live` with `new_class_file`'s method bodies, constant pool and bootstrap
/// methods: what a redefinition installs, for `check_redefinition` to verify
/// without touching the live class (interpreter round i1 wave 28, lane L3).
/// Every other field is the live class's, which a redefinition may not change;
/// the struct literal names each field so a field added to `Class` must be
/// placed here (as `RedefineInvariantSnapshot::from_class` does). The methods
/// stay in the class file's order: verification is per method.
pub(in crate::class_manager) fn redefinition_scratch_class(live: &Class, new_class_file: cratonvm_reader::ClassFile) -> Class {
    let bootstrap_methods = new_class_file
        .attributes
        .iter()
        .find_map(|a| match a.as_decoded() {
            Some(Attribute::BootstrapMethods(bms)) => Some(bms.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Class {
        id: live.id,
        loader_id: live.loader_id,
        name: Arc::clone(&live.name),
        source_file: live.source_file.clone(),
        version: live.version,
        state: live.state,
        initializing_thread: live.initializing_thread,
        constant_pool: new_class_file.constant_pool,
        access_flags: live.access_flags,
        superclass: live.superclass,
        interfaces: live.interfaces.clone(),
        fields: live.fields.clone(),
        methods: new_class_file.methods,
        first_field_index: live.first_field_index,
        num_total_fields: live.num_total_fields,
        bootstrap_methods,
        signature: live.signature.clone(),
        annotations: Vec::new(),
        nest_host: live.nest_host.clone(),
        nest_members: live.nest_members.clone(),
        record_components: live.record_components.clone(),
        permitted_subclasses: live.permitted_subclasses.clone(),
        inner_classes: live.inner_classes.clone(),
        enclosing_method: live.enclosing_method.clone(),
        hidden: live.hidden,
        module_name: live.module_name.clone(),
        origin: live.origin.clone(),
        has_finalizer: live.has_finalizer,
        code_source: live.code_source.clone(),
        array_info: live.array_info.clone(),
        init_state: Arc::new(std::sync::atomic::AtomicU8::new(
            live.init_state.load(Ordering::Acquire),
        )),
        record_object_methods: std::sync::atomic::AtomicU8::new(
            live.record_object_methods.load(Ordering::Relaxed),
        ),
    }
}

/// A hierarchy that answers as `inner` does but keeps the type maps the
/// verifier publishes in its own store, dropped with it: the maps of a
/// scratch class (`redefinition_scratch_class`) must not reach the VM's store,
/// which is first-writer-wins and describes the LIVE bodies.
pub(in crate::class_manager) struct ScratchTypeMaps<'a> {
    pub(in crate::class_manager) inner: &'a dyn ClassHierarchy,
    pub(in crate::class_manager) store: crate::type_maps::TypeMapStore,
}

impl ClassHierarchy for ScratchTypeMaps<'_> {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        self.inner.is_subclass(child, parent)
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.inner.is_direct_superclass(child, parent)
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        self.inner.common_superclass(a, b)
    }

    fn is_interface(&self, name: &str) -> bool {
        self.inner.is_interface(name)
    }

    fn is_resolvable(&self, name: &str) -> bool {
        self.inner.is_resolvable(name)
    }

    fn type_map_store(&self) -> Option<&crate::type_maps::TypeMapStore> {
        Some(&self.store)
    }

    fn invokespecial_target(
        &self,
        current: &str,
        ref_class: &str,
    ) -> Option<crate::vtype::SpecialTarget> {
        self.inner.invokespecial_target(current, ref_class)
    }

    fn protected_receiver_rule(
        &self,
        current: &str,
        ref_class: &str,
        name: &str,
        descriptor: &str,
        is_method: bool,
    ) -> bool {
        self.inner
            .protected_receiver_rule(current, ref_class, name, descriptor, is_method)
    }
}

/// What the real class-path image says about one native registration's target
/// method, decided **without loading or defining anything**.
///
/// This is the discriminator contract §1.5 turns on: a `Bridge` is what an
/// `ACC_NATIVE` method binds to, so `acc_native == false` on a `Bridge` row is a
/// registration nobody adjudicated, and `has_code == true` on one is a native
/// shadowing concrete bytecode.
///
/// Distinct from the native census's `real_declaring_method`, which asks the
/// *loaded* class store the same question and therefore answers only for classes
/// the run happened to touch. See
/// [`ClassManager::adjudicate_natives_against_image`] for why both exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMethodVerdict {
    /// The class path (CDS, then bootstrap, extension, application) yields bytes
    /// for this name. `false` means the image has no such class — which for a
    /// JDK name is exactly the case where `load_class` would have *fabricated* a
    /// synthetic stub.
    pub image_has_class: bool,
    /// Those bytes declare a method with this exact name and descriptor.
    pub declared: bool,
    /// …and it is `ACC_NATIVE`.
    pub acc_native: bool,
    /// …and it is neither `native` nor `abstract`, so JVMS §4.6 says it carries
    /// a `Code` attribute. Read from the access flags rather than from a decoded
    /// attribute, so a lazy-attribute decode state cannot masquerade as a fact
    /// about the class.
    pub has_code: bool,
    /// The SUPERTYPE that declares this method, when the named class does not.
    ///
    /// `declared: false` used to be the whole answer, and reading it as "this
    /// registration targets nothing" is wrong for three quarters of the bucket.
    /// CratonVM's dispatch is receiver-driven: a native registered on
    /// `sun/nio/ch/FileDispatcherImpl.read0` intercepts a receiver of that class
    /// even though `read0` is declared a frame up on `UnixFileDispatcherImpl`.
    /// Nineteen such rows are `ACC_NATIVE` on a supertype — genuine §1.5 bridges
    /// the census used to score as unadjudicated — and roughly 1,600 more
    /// inherit concrete bytecode, i.e. are §1.4 shadows no `has_code` column
    /// could see.
    ///
    /// `None` means either "the named class declares it" (read `declared`) or
    /// "no class in the hierarchy does". The three `inherited_*` flags are false
    /// in both of those cases, so a reader never has to tell them apart to
    /// score a row.
    ///
    /// Resolution order is JVMS §5.4.3.3: the class, then its superclass chain,
    /// then its superinterfaces. That is also CratonVM's dispatch order, which
    /// is what makes the answer a statement about what would actually run.
    pub inherited_from: Option<Arc<str>>,
    /// The inherited declaration is `ACC_NATIVE`. A `Bridge` row with this set
    /// is correctly stated — contract §1.5's target exists, on a supertype.
    pub inherited_acc_native: bool,
    /// The inherited declaration carries `Code`: a §1.4 shadow of INHERITED
    /// bytecode.
    pub inherited_has_code: bool,
    /// The inherited declaration is abstract, so the registration intercepts
    /// every implementor rather than shadowing anything.
    pub inherited_abstract: bool,
}

impl ImageMethodVerdict {
    /// "Some class in the hierarchy declares this triple `ACC_NATIVE`" — the
    /// contract §1.5 question, asked of the whole hierarchy rather than of one
    /// class name. Every gate that scores a `Bridge` row should ask this and not
    /// [`Self::acc_native`]; the difference is the nineteen rows the census used
    /// to miscount in the dangerous direction.
    pub fn acc_native_anywhere(&self) -> bool {
        self.acc_native || self.inherited_acc_native
    }

    /// "Some class in the hierarchy declares this triple with a `Code`
    /// attribute" — the §1.4 shadow question over the whole hierarchy.
    pub fn has_code_anywhere(&self) -> bool {
        self.has_code || self.inherited_has_code
    }

    /// The verdict for a class the image does not have at all.
    pub(in crate::class_manager) fn absent() -> Self {
        Self {
            image_has_class: false,
            declared: false,
            acc_native: false,
            has_code: false,
            inherited_from: None,
            inherited_acc_native: false,
            inherited_has_code: false,
            inherited_abstract: false,
        }
    }
}

/// One image class, reduced to what the native adjudication has to ask of it.
///
/// Parsed once per DISTINCT registered class (the registry has thousands of
/// rows over roughly a thousand classes) and then re-read once per row and once
/// per hierarchy walk that passes through it.
#[derive(Default)]
pub(in crate::class_manager) struct ImageClassShape {
    /// `(name, descriptor) -> (acc_native, has_code)`.
    pub(in crate::class_manager) methods: FxHashMap<(Arc<str>, Arc<str>), (bool, bool)>,
    pub(in crate::class_manager) super_class: Option<Arc<str>>,
    pub(in crate::class_manager) interfaces: Vec<Arc<str>>,
}

impl ImageClassShape {
    pub(in crate::class_manager) fn find(&self, name: &str, descriptor: &str) -> Option<(bool, bool)> {
        self.methods
            .iter()
            .find(|((n, d), _)| &**n == name && &**d == descriptor)
            .map(|(_, flags)| *flags)
    }
}

/// How deep [`ClassManager::adjudicate_natives_against_image`] walks before it
/// gives up on a hierarchy.
///
/// A malformed image could describe a cycle, and this pass runs on a diagnostic
/// path where a hang is far worse than an unresolved row. The deepest chain in
/// JDK 25's `java.base` is under a dozen; 64 is past anything real and still
/// bounded.
pub(in crate::class_manager) const IMAGE_HIERARCHY_MAX_DEPTH: usize = 64;

/// One refused fabrication request, identified by *what* was refused and
/// *who* asked: the class name, and the `file`/`line` of the Rust call site
/// that `#[track_caller]` attributes it to.
///
/// The pair is the identity, not the class alone. The same class is refused
/// from more than one site in a real run — `cratonvm/internal/UnmodifiableMap`
/// is refused once by the boot block and again by `System.getenv`'s native —
/// and those are two different diagnoses. See
/// [`ClassManager::compatibility_refusal_counts`].
pub type RefusalSite = (String, &'static str, u32);

/// Is `class` finalizable, given its superclass's answer `parent_has` (JIT
/// round 21 wave 17, lane finalize-reinit; item 4 of
/// `docs/feature-designs/class-resolution-and-caching.md`)?
///
/// HotSpot 25 `ClassFileParser`: a declared `finalize()V` whose code is a
/// lone `return` (`Method::is_empty_method`: one byte, `_return`) sets
/// `_has_empty_finalizer`, any other one (a longer body, or none: native,
/// abstract) sets `_has_finalizer`; `set_precomputed_flags` then sets
/// `has_finalizer` only when `!_has_empty_finalizer && (_has_finalizer ||
/// super->has_finalizer())`. So an empty override is no finalizer, and it
/// cancels the one the class would inherit. `java/lang/Object`'s own empty
/// `finalize` is not a declaration here ([`Class::declares_finalize`]).
///
/// `empty_is_none == false` (`CRATONVM_FINALIZER_EMPTY_IS_NONE=0`) gives
/// the old answer: any declaration, empty or not, or the parent's.
pub(crate) fn class_has_finalizer_by_hotspot_rule(
    class: &Class,
    parent_has: bool,
    empty_is_none: bool,
) -> bool {
    if !class.declares_finalize() {
        return parent_has;
    }
    if !empty_is_none {
        return true;
    }
    // One emptiness test for both rounds' doors (zt t18/hunt2's
    // `Class::declares_empty_finalize` reads a `Raw` Code body without
    // decoding it; a decoded one compares the same single `return` byte).
    !class.declares_empty_finalize()
}

/// What the link of a class whose define left its type-map walk to it owes
/// (`ClassManager::type_maps_pending`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::class_manager) struct PendingTypeMaps {
    /// The walk records HotSpot's verifier loads (the define-time
    /// `record_verifier_loads`).
    pub(in crate::class_manager) record_loads: bool,
    /// Phase 5 fast boot (lane p5-define): the define also left the class's
    /// structural bytecode scan here (a trusted bootstrap class on the
    /// `define_boot_maps_at_link` route), so the link scans before it walks,
    /// sharing each method's decode, and refuses the link on a scan error.
    pub(in crate::class_manager) structural_scan: bool,
}

/// Manages class loading for the VM.
///
/// Maintains the `ClassStore` (all loaded classes), three built-in class finders
/// (bootstrap, extension, application), and a `(loader_id, name) → ClassId` cache.
/// Loading a class follows the parent delegation model and automatically loads
/// its superclass and interfaces recursively.
pub struct ClassManager {
    /// Storage for all loaded classes (shared across all loaders).
    pub class_store: ClassStore,

    /// Bootstrap class finder — loads from rt.jar / boot classpath.
    pub(in crate::class_manager) bootstrap: BootstrapClassFinder,

    /// Extension class finder — loads from $JAVA_HOME/lib/ext.
    pub(in crate::class_manager) extension: ExtensionClassFinder,

    /// Application class finder — loads from the -classpath.
    pub(in crate::class_manager) application: ApplicationClassFinder,

    /// (loader_id, name) → ClassId cache.
    /// T10.9.B: FxHashMap for internal hot-path lookups (keys are internal
    /// class names, never untrusted user input).
    ///
    /// T10.9.E: key is `(ClassLoaderId, Arc<str>)` (was `(_, String)`). All
    /// the high-volume insert sites already hold an `Arc<str>` (from
    /// `Class.name`), so storing the Arc in the key is a refcount bump
    /// rather than a fresh allocation. Probe sites that only have `&str`
    /// construct a one-off `Arc::from(s)` — no worse than the prior
    /// `name.to_string()`. Eliminates ~60+ String allocations per class
    /// define on the hot load_class / define_class path.
    ///
    /// **Round 4 audit fix (CRIT):** this is now the single authoritative
    /// name→id index. The former `name_to_id: FxHashMap<u64, ClassId>`
    /// shadow map was keyed by the raw FNV-1a digest of the class name,
    /// with no name verification and no `ClassLoaderId` component — so any
    /// FNV-1a collision returned the wrong `ClassId` (silent type
    /// confusion downstream) and two loaders that defined the same name
    /// could not coexist through that map. `get_loaded_class_id` now
    /// walks this `(ClassLoaderId, Arc<str>)`-keyed map directly, which
    /// is collision-free (full name comparison) and loader-aware.
    ///
    /// C34 audit fix (HIGH): `hashbrown::HashMap` (not `std::HashMap`) so
    /// hot lookup paths can probe `(loader_id, &str)` via `raw_entry`
    /// without minting a fresh `Arc<str>` per call. Insert / remove sites
    /// retain the std HashMap API surface — hashbrown's `HashMap` is the
    /// underlying implementation of std's anyway.
    pub(in crate::class_manager) loaded_classes: LoadedClassesMap,

    /// zt t16/df6 (define page §1, corner 2 and D6): the defines whose name
    /// is NOT bound yet (`DefineClassOptions::bind_name_after_add_class`),
    /// each with the `loaded_classes` key it will take. HotSpot's
    /// `SystemDictionary::define_instance_class` makes the
    /// `ClassLoader.addClass` upcall BEFORE `update_dictionary`, and while it
    /// runs the name is held only by the define's placeholder: no lookup
    /// answers the class, a second define of the name meets the placeholder,
    /// and an upcall that throws leaves no class under the name. A class here
    /// is in the store (its id, metadata, mirror and supertypes are real) but
    /// in no name index: not in `loaded_classes`, not in the store's name
    /// index ([`ClassStore::withdraw_name`]). The duplicate-definition check
    /// counts it. [`Self::bind_unbound_define`] binds the name;
    /// [`Self::withdraw_unbound_define`] gives it up for good (the class stays
    /// in the store, nameless, until its loader unloads, as HotSpot's
    /// `InstanceKlass` stays in its loader's `ClassLoaderData`). Empty unless
    /// a define door has an upcall in flight: a handful of rows at most.
    pub(in crate::class_manager) unbound_defines: FxHashMap<ClassId, (ClassLoaderId, Arc<str>)>,

    /// JPMS module registry: descriptors, package map, readability graph.
    pub module_registry: ModuleRegistry,

    /// Raw class file bytes for each loaded class, keyed by internal name.
    /// Populated during define_class() for CDS dump support, JVMTI
    /// `RetransformClasses`, and `getResourceAsStream("X.class")`.
    /// T10.9.B: FxHashMap — keys are the defining ClassId. A binary name is
    /// not sufficient: two user-defined loaders may hold distinct enhanced
    /// copies of the same class at once.
    ///
    /// **Round 4 audit fix (HIGH):** insertions go through
    /// [`Self::insert_class_bytes`], which tracks total bytes against
    /// [`Self::class_bytes_cache_cap`] and evicts the oldest entries
    /// (FIFO) once the cap is hit. Prior behaviour kept every class
    /// file resident forever (~90 MB on a medium Spring app, 15k
    /// classes averaging 6 KB each). The default 16 MiB cap covers
    /// JVMTI agents (re-fetch typically targets recently-defined
    /// classes) without bounding the heap of an idle process.
    pub class_bytes_cache: FxHashMap<ClassId, SharedBytes>,

    /// SBR wave 5 (reflmeta): each class's class-level
    /// `RuntimeVisibleTypeAnnotations` BODY (JVMS 4.7.20), kept at define time
    /// for `Class.getRawTypeAnnotations` (a type parameter's annotations, the
    /// annotated supertypes). `Class` keeps its declaration annotations but
    /// not these, and [`Self::class_bytes_cache`] is FIFO-evicted, so reading
    /// them back from the cached class file lost them once enough later classes
    /// loaded. Only classes that HAVE the attribute have a row; dropped with
    /// the class on unload and replaced on redefinition.
    pub class_type_annotations: FxHashMap<ClassId, SharedBytes>,

    /// gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 7): the lookup
    /// class (internal name) of each NESTMATE hidden class whose define named
    /// one ([`DefineClassOptions::lookup_class_name`]), written under the
    /// define's write guard before the class-load hook can fire, read by the
    /// `-Xlog:class+load` emitter (HotSpot prints the lookup class, not the
    /// nest host). Per manager, dropped with the class on unload.
    pub hidden_lookup_class_names: FxHashMap<ClassId, Arc<str>>,

    /// Classes whose `Record` attribute (JVMS 4.7.30) declares NO component
    /// (`record Empty() {}`). `Class::is_record` reads "has components", so
    /// these read as non-records without this row: `Empty.class.isRecord()`
    /// answered `false` and `getRecordComponents()` `null`, where HotSpot's
    /// `InstanceKlass::is_record` (`_record_components != nullptr`, a
    /// zero-length array here) answers a record with no components
    /// (interpreter round i4 wave 84, lane L3; probe
    /// `L3W84EmptyRecord.java`). A side table rather
    /// than a `Class` field: `Class` has ~90 struct literals. Dropped with the
    /// class on unload; a redefinition may not change the attribute.
    pub empty_record_classes: FxHashSet<ClassId>,

    /// Insertion-order tracker for [`Self::class_bytes_cache`] FIFO
    /// eviction. Deque front = oldest entry. Entries re-inserted
    /// (e.g. redefine) are re-pushed at the back: the FIFO ordering
    /// reflects most-recent-insert, not most-recent-access (a real
    /// LRU would need touch-on-read, which isn't worth the `&mut self`).
    pub(in crate::class_manager) class_bytes_cache_fifo: std::collections::VecDeque<ClassId>,

    /// Running total bytes held by [`Self::class_bytes_cache`].
    pub(in crate::class_manager) class_bytes_cache_size: usize,

    /// Soft cap for the class-bytes cache, in bytes. Default 16 MiB.
    /// Set to `usize::MAX` to disable eviction (legacy keep-forever).
    pub(in crate::class_manager) class_bytes_cache_cap: usize,

    /// `(length, content hash)` of the bytes each class's **current definition**
    /// came from — the JVMTI retransformation base. Never evicted by the FIFO;
    /// dropped only when the class unloads (`unload_user_classes_inner`).
    ///
    /// [`Self::class_bytes_cache`] is FIFO-evicted at a soft cap, so by the time
    /// a JVMTI agent retransforms a class (Mockito's inline mock maker and
    /// JaCoCo both do, late and on demand) the base bytes are usually gone and
    /// `retransformClasses0` falls back to a classpath resource lookup. That
    /// lookup searches bootstrap → extension → application, **not the class's
    /// defining loader**, and Spring Boot's test infrastructure routinely
    /// defines classes through `ModifiedClassPathClassLoader` /
    /// `FilteredClassLoader` / per-test `URLClassLoader`s that resolve a
    /// *different copy* of the same name. Matching `this_class` — the only
    /// check the fallback had — does not catch a different **version** of the
    /// same class: the transformer would then weave version A's method bodies
    /// and `retransform_class` would install them over the live version B.
    ///
    /// 16 bytes per loaded class buys the adjudication the byte cache can no
    /// longer make. See [`Self::class_bytes_match_base`].
    pub(in crate::class_manager) class_bytes_base_digest: FxHashMap<ClassId, (u32, u64)>,

    /// The classes whose [`Self::class_bytes_cache`] entry is a
    /// retransformation base the class path cannot give back: a class file a
    /// load-time transformer changed, or one `redefineClasses` installed
    /// ([`Self::insert_class_bytes_pinned`]). Such an entry is not in
    /// [`Self::class_bytes_cache_fifo`] and not counted in
    /// [`Self::class_bytes_cache_size`], so the FIFO never evicts it; it goes
    /// when the class unloads. Until interpreter round i1 wave 28 (lane L3)
    /// the FIFO evicted these like any other entry, and the class-path
    /// fallback then found no file or a different build, so `retransformClasses`
    /// skipped the class for the rest of the run
    /// (`interpreter-L3-an-evicted-retransformation-base-skips-the-class-FIXED-20260929.md`).
    /// HotSpot keeps such a file for the class's lifetime
    /// (`InstanceKlass::_cached_class_file`).
    pub(in crate::class_manager) class_bytes_pinned: FxHashSet<ClassId>,
    /// Byte-for-byte bases captured before a capable transformer changed them.
    /// Other pinned copies are resources, which HotSpot reconstitutes instead.
    pub(in crate::class_manager) class_bytes_raw_agent_bases: FxHashSet<ClassId>,

    /// Bytes held by the pinned entries of [`Self::class_bytes_cache`].
    pub(in crate::class_manager) class_bytes_pinned_size: usize,

    /// Bound on [`Self::class_bytes_pinned_size`]
    /// ([`DEFAULT_PINNED_CLASS_BASES_CAP`]). A pinned insert that would pass it
    /// goes to the FIFO instead, with one warning per VM.
    pub(in crate::class_manager) class_bytes_pinned_cap: usize,

    /// Whether the warning for a pinned insert over the cap was given.
    pub(in crate::class_manager) class_bytes_pinned_overflowed: bool,

    /// Class files [`Self::release_refetchable_class_bytes`] let go (zr z11/md).
    pub(in crate::class_manager) class_bytes_refetched_released: u64,

    /// Redefinitions [`Self::check_redefinition`] verified, by class: the
    /// fingerprint of the class file it verified and the class's redefinition
    /// generation then. [`Self::redefine_class_typed`] takes the entry and
    /// skips its own verification when both still match, so a checked
    /// redefinition verifies once (interpreter round i1 wave 28, lane L3).
    /// A mutex because the check runs under the class-manager read lock.
    pub(in crate::class_manager) pre_verified_redefinitions: parking_lot::Mutex<FxHashMap<ClassId, ((u32, u64), u32)>>,

    /// `-XX:MaxMetaspaceSize` accounting (gc-common w6-f): the bytes each
    /// live class was CHARGED when it was defined, so unloading subtracts
    /// exactly what defining added whatever happened to the class record in
    /// between. See [`Self::metaspace_charged_bytes`] for what is charged.
    pub(in crate::class_manager) metaspace_charges: FxHashMap<ClassId, u64>,

    /// Sum of [`Self::metaspace_charges`].
    pub(in crate::class_manager) metaspace_charged_bytes: u64,

    /// `-XX:MaxMetaspaceSize`, or `None` (the default: unbounded, as before
    /// the flag was honoured). Per VM — set by the launcher on this VM's
    /// class manager, never a process global.
    pub(in crate::class_manager) max_metaspace_bytes: Option<u64>,

    /// gci i33/ms: the VM's running per-arena sums of its HotSpot metaspace
    /// model, kept for the `-XX:MaxMetaspaceSize` check so a define sums only
    /// the classes defined since the last check (see
    /// [`MetaspaceArenaTally`]). A mutex because the check runs under the
    /// class-manager read lock. Per VM, never a process global.
    pub(in crate::class_manager) metaspace_arena_tally: parking_lot::Mutex<MetaspaceArenaTally>,

    /// CDS-cached class bytes: class name -> raw .class bytes.
    /// Populated from the CDS archive at startup, checked before classpath delegation.
    /// T10.9.B: FxHashMap — keys are internal class names loaded from a trusted
    /// CDS archive produced by this VM.
    pub cds_class_cache: FxHashMap<String, Vec<u8>>,

    /// Guard set for classes currently being loaded by `define_class`.
    /// If `load_class` encounters a class name already in this set, it means
    /// we have a circular class hierarchy (A extends B extends A) which is
    /// forbidden by the JVM spec (§5.3.5).
    /// T10.9.B: FxHashSet — keys are internal class names during loading.
    pub(in crate::class_manager) loading_guard: FxHashSet<String>,

    /// JVMS 5.3.4 loader constraints, recorded by `--real-jdk` member
    /// resolution and checked here at define (see the module doc). Under its
    /// own lock so a resolver holding the class-manager READ guard can record
    /// ([`Self::loader_constraint_table`]); the define path, which holds
    /// `&mut self`, reads it with `get_mut` and takes no lock.
    pub(in crate::class_manager) loader_constraints: parking_lot::Mutex<crate::loader_constraints::LoaderConstraints>,

    /// Names of synthetic-stub classes whose real `.class` is known to be
    /// absent from every current classpath.
    ///
    /// `ensure_synthetic_class` / `load_class` try to *upgrade* an existing
    /// synthetic stub to its real bytecode by re-running the full
    /// (CDS → bootstrap → extension → application → IMPL-JARS) classpath
    /// scan on every call. For stubs that can never resolve to a real class
    /// — chiefly the VM-internal `cratonvm/synthetic/AnonymousObject$N`
    /// allocated for every `HashMap`/`LinkedHashMap` node (and friends) —
    /// that scan re-runs on *every object allocation*. With a large
    /// application classpath (Hibernate + JAXB + dozens of dep JARs) each
    /// allocation becomes O(num_jars × zip-probes), turning reflection-heavy
    /// model building (JAXB `ClassInfoImpl`) into a multi-minute hang
    /// (HIB-DEV-03). Memoizing the absent result makes the second and all
    /// later upgrade attempts O(1).
    ///
    /// Correctness: the only event that can make a previously-absent name
    /// resolvable is a classpath extension, so the set is cleared by
    /// [`Self::extend_application_classpath`] /
    /// [`Self::extend_bootstrap_classpath`]. `defineClass` does not change
    /// classpath findability and is reached only after `get_loaded_class_id`
    /// misses, so it needs no invalidation here. This mirrors the JVM's own
    /// sticky negative class resolution.
    pub(in crate::class_manager) synthetic_upgrade_absent: FxHashSet<String>,

    /// This VM's user-defined loaders' delegation parents (child namespace ->
    /// parent namespace), read by resolution here and written by the
    /// loader-namespace natives through the VM's loader-identity tables,
    /// which hold the same `Arc` ([`Self::user_loader_parents`]). The process
    /// global `USER_LOADER_PARENTS` until interpreter round i5 wave 103, lane
    /// L5 (`i101-L5-remaining-loader-family-process-globals`).
    pub(in crate::class_manager) user_loader_parents: Arc<crate::loaders::UserLoaderParents>,

    /// Internal names of the classes provided by jars and directories appended
    /// at runtime to THIS VM's bootstrap search
    /// ([`Self::extend_bootstrap_classpath`]; read by
    /// [`Self::is_bootstrap_appended_class`]). A process static keyed by
    /// `vm_identity` (`BOOTSTRAP_APPENDED_CLASSES`) until round 14 wave 2
    /// (MISC11-2): the census now lives and dies with the search path it
    /// describes, so no teardown purge is needed.
    pub(in crate::class_manager) bootstrap_appended_classes: FxHashSet<String>,

    /// Whether [`Self::bootstrap_appended_classes`] was ever non-empty, readable
    /// WITHOUT the class-manager lock through [`Self::bootstrap_append_seen`]
    /// (round 14 wave 3, lane fixup, RV3-4 of
    /// `r14w3-review3-wave2-review-findings-FIXED-20260929.md`): a VM that never
    /// appended to its bootstrap search answers the loader's "is this an
    /// appended class?" question with one `Acquire` load instead of a read
    /// lock. Set (`Release`) AFTER the census is extended, under the write
    /// lock, so a reader that sees `true` and then takes the read lock sees
    /// the names. Sticky: the census never shrinks.
    pub(in crate::class_manager) bootstrap_append_seen: Arc<AtomicBool>,

    /// Every package of a class appended to the bootstrap search, with the
    /// FIRST appended entry that holds one of its classes (as the search
    /// order finds them) and the appended class names of the package. The
    /// boot loader's `getSystemPackageNames` / `getSystemPackageLocation`
    /// stand-ins read it ([`Self::boot_append_package_location`],
    /// [`Self::boot_append_loaded_packages`]) instead of walking every loaded
    /// class and parsing a resource URL back into a path; HotSpot records the
    /// location when it defines the package. Filled by
    /// [`Self::extend_bootstrap_classpath`], a snapshot like the class census
    /// beside it. Interpreter round i2 wave 57, lane L5
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    pub(in crate::class_manager) bootstrap_append_packages: FxHashMap<String, (String, Vec<String>)>,

    /// T10.5 — per-class vtable descriptor layout, indexed by ClassId.
    /// Populated by `define_class_with_options` at link time and used by
    /// subclasses of the same class as the "parent vtable" when computing
    /// their own layout. The VM's installed `VtableInstallHookScoped` receives a
    /// clone of the owned vec per class and funnels it into
    /// `shared.classes.vtable_manager.install_vtable(...)`.
    ///
    /// Keying on ClassId (not name) keeps the superclass-lookup O(1) even
    /// for classes loaded by many different classloaders.
    ///
    /// T10.9.E: switched from `HashMap` (SipHash) to `FxHashMap`. ClassId
    /// is a trusted internal `u32`, never user input, so the DoS-resistant
    /// SipHash is pure overhead on every class-link superclass probe.
    pub(in crate::class_manager) vtable_descriptors: FxHashMap<ClassId, Vec<Option<VtableSlotDescriptor>>>,

    /// Classes whose vtable holds one signature in more than one slot (a
    /// method that did not override an inherited package-private one), built
    /// while the store compares run-time packages by loader (both modes;
    /// empty under `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`). A name-keyed vtable lookup cannot
    /// say which of those slots a call site means, so the interpreter's
    /// vtable fast path validates such receivers against §5.4.6 selection
    /// ([`Self::vtable_shadows_a_signature`]). Inherited by subclasses. Ids
    /// are never reused, so an unloaded class's entry only costs a check.
    pub(in crate::class_manager) shadowed_signature_vtables: FxHashSet<ClassId>,

    /// Whether [`Self::vtable_shadows_signature`] narrows the validation to
    /// the one duplicated signature (round 13 wave 5, lane proxy3) instead of
    /// every call on a receiver in [`Self::shadowed_signature_vtables`].
    /// `CRATONVM_OVERRIDE_SHADOW_PER_SIGNATURE=0` keeps the per-class answer.
    pub(in crate::class_manager) shadow_validation_per_signature: bool,

    /// `CRATONVM_VTABLE_SHARE_SLOTS` (zs s2/fp2, default ON, `=0` off), read
    /// once per manager: an inherited vtable slot shares its declaring
    /// class's [`VtableSlotDescriptor`] block. Off, every inherited slot is a
    /// private copy, as before the round (the footprint kill arm; no answer
    /// differs, the data is immutable).
    pub(in crate::class_manager) vtable_share_slots: bool,

    /// Whether [`Self::vtable_fast_path_hidden_receiver`] may answer `true`
    /// (P6-5, round 15 wave 5, lane proxy7). `CRATONVM_VTABLE_HIDDEN_RECEIVER_PACKAGE=0`
    /// answers `false` for every class, as before. Read once per manager.
    pub(in crate::class_manager) vtable_hidden_receiver_package: bool,

    /// WP2.3 — per-class "skip bytecode verification" flag, recorded when
    /// `define_class_with_options` is called with
    /// `DefineClassOptions::skip_verification = true`. Stored as a side
    /// table (rather than a field on `Class`) so that VM-side construction
    /// sites that build `Class` directly do not need to know about the
    /// flag — only callers that route through `define_class_with_options`
    /// (the hidden-class / runtime-bytecode-generation entry points) ever
    /// set it.  The verifier reads this through
    /// `class_skip_bytecode_verification(class_id)`.
    ///
    /// Generators like ByteBuddy, CGLIB, and JDK dynamic Proxy emit
    /// classes that pass JVMS structural rules but trip our bytecode
    /// verifier on synthesised stack frames; the only safe escape hatch
    /// is to honour the caller's request to bypass verification (which
    /// HotSpot likewise does for trusted hidden classes).
    pub(in crate::class_manager) skip_bytecode_verification: FxHashSet<ClassId>,

    /// The names HotSpot's split verifier would load through a class's
    /// defining loader while linking it (its reference assignability checks,
    /// `VerificationType::resolve_and_check_assignability`), recorded by the
    /// define-time verification of a class of a non-bootstrap loader
    /// (`ClassStoreHierarchy::verifier_loads`) and loaded by the VM's link
    /// step, which drops the row once the class is linked
    /// ([`Self::verifier_link_loads`], [`Self::verifier_link_loads_done`]).
    /// Only while the `VerifierLoadsAtLink` alignment is on.
    pub(in crate::class_manager) verifier_link_loads: FxHashMap<ClassId, Box<[crate::vtype::VerifierLinkLoad]>>,

    /// `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK`, read once at construction
    /// (default OFF; interpreter round i3 wave 81, lane L5,
    /// `docs/feature-designs/class-resolution-and-caching.md`):
    /// a user-loader class whose Pass 3 is deferred keeps its structural pass
    /// at define but walks its methods for type maps (and its verifier loads)
    /// when it is LINKED ([`Self::publish_pending_type_maps`]), as HotSpot
    /// verifies at link, instead of under this manager's write lock at
    /// define. See [`Self::set_type_maps_at_link`].
    pub(in crate::class_manager) type_maps_at_link: bool,
    /// `CRATONVM_REAL_BOOT_CLASSES_LATCH` (default ON, `=0` off), read once
    /// at construction: [`Self::has_real_boot_classes`] may latch a `true`.
    pub(in crate::class_manager) real_boot_classes_latch: bool,
    /// [`Self::has_real_boot_classes`] answered `true` once (latched).
    pub(in crate::class_manager) real_boot_classes_latched: std::sync::atomic::AtomicBool,

    /// `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK`, read once at construction (default
    /// OFF; interpreter round i4 wave 90, lane L5): a trusted bootstrap class
    /// (the boot loader, a `java/` `jdk/` `sun/` `com/sun/` name, not hidden,
    /// no `-Xverify:all`) with a method that has code keeps Pass 2 and the
    /// structural bytecode scan at define and leaves the type-map walk to its
    /// link, through the same pending route as [`Self::type_maps_at_link`]
    /// (which must be on too). HotSpot does not verify these classes at all
    /// (`BytecodeVerificationLocal` is off), so dropping the define-time Pass 3
    /// verdict moves no `VerifyError` HotSpot raises; what moves is the walk,
    /// out of this manager's write lock, and a class loaded but never linked
    /// never pays it. See [`Self::set_define_boot_maps_at_link`].
    pub(in crate::class_manager) define_boot_maps_at_link: bool,

    /// When enabled (VM default when `-Xverify:all` is not set), trusted
    /// bootstrap classes keep Pass 2 structural/format checks and skip Pass 3
    /// bytecode verification and eager type-map walks, matching HotSpot's
    /// `-Xverify:remote`.
    pub(in crate::class_manager) skip_boot_verification: bool,

    /// `CRATONVM_DEFINE_NO_CLASS_PATH_CODE_SOURCE`, read once at construction
    /// (default OFF; interpreter round i4 wave 88, lane L5): a define by a
    /// user-defined loader, or a hidden define, that carries no code source
    /// takes the synthesized `file:/runtime-defined/<name>.class` one at once
    /// instead of probing every class-path entry of the three built-in class
    /// paths (two passes each, a `stat` per directory on the second) for a
    /// class file of its name -- a probe that misses for every generated
    /// class and that HotSpot does not make (a define without a protection
    /// domain gets the loader's default domain). See
    /// [`Self::set_define_no_class_path_code_source`].
    pub(in crate::class_manager) define_no_class_path_code_source: bool,

    /// `CRATONVM_DEFINE_STUB_PROBE_BY_NAME`, read once at construction
    /// (default OFF; interpreter round i4 wave 94, lane L5): a define's
    /// stub-upgrade probe (is a compatibility stub of this name already
    /// registered?) first asks the class store's name index whether ANY live
    /// class carries the name, and skips `get_loaded_class_id` when none does.
    /// That lookup probes the three built-in loaders and then EVERY user loader
    /// the VM has seen, so for the fresh name of a generated class it cost one
    /// hash probe per user loader on every define (a Groovy, OSGi or
    /// per-test-loader process has hundreds). A stub is registered under its
    /// own name, so with no live class of the name there is none to upgrade.
    pub(in crate::class_manager) define_stub_probe_by_name: bool,

    /// `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL` (default ON since wave
    /// 134 by user decision, `=0` off, `runtime_flag_default_on`; default OFF
    /// in waves 118-133: the L9 review
    /// `i118-L9-builtin-define-constraint-refusal-has-no-fallback-mark` found
    /// the VM-made defines unmarked; the census on the i46 page decides a flip;
    /// HotSpot parity; interpreter round i6 wave 117, lane L5, page
    /// `i46-L5-proposal-a-flat-store-define-is-told-from-a-built-in-loaders-own`):
    /// a BUILT-IN loader's define (bootstrap, platform, application) that
    /// breaks a recorded loader constraint is refused with HotSpot's
    /// `check_constraints` `LinkageError`, as a user loader's is, unless it is a
    /// VM fallback define ([`DefineClassOptions::vm_fallback`]) or the upgrade
    /// of a compatibility stub of the name. Off: counted and admitted (a silent
    /// type confusion: two classes of one name across a constraint).
    pub(in crate::class_manager) define_builtin_constraint_refusal: bool,

    /// The threads whose current defines are VM fallbacks: the flat-store
    /// global route the VM takes after a user loader was asked and refused
    /// (`interpreter::constants::resolve_class_loader_aware`), set around that
    /// load by [`Self::set_vm_fallback_defines`]. A built-in define on such a
    /// thread is treated as [`DefineClassOptions::vm_fallback`] (counted and
    /// admitted, never refused). Per class manager (no process state); empty
    /// but for the length of one such load. Interpreter round i6 wave 118, lane
    /// L5.
    pub(in crate::class_manager) vm_fallback_threads: std::collections::HashSet<std::thread::ThreadId>,

    /// `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_FALLBACK_PIN` (HotSpot parity,
    /// default ON, `=0` off; inert unless
    /// [`Self::define_builtin_constraint_refusal`] is on). Interpreter round i6
    /// wave 133, lane L5c, page
    /// `i46-L5-proposal-a-flat-store-define-is-told-from-a-built-in-loaders-own`,
    /// hazard 1 of the `i118-L9` review: a built-in loader's define is not
    /// refused when the class the constraint is pinned to is itself a VM
    /// fallback copy ([`Self::vm_fallback_classes`]). HotSpot has no such
    /// copy, so the define it refuses on is the name's only class there, and
    /// it succeeds. Counted and admitted, as before wave 117.
    pub(in crate::class_manager) define_builtin_constraint_fallback_pin: bool,

    /// The classes a BUILT-IN loader defined as VM fallbacks
    /// ([`DefineClassOptions::vm_fallback`], or on a
    /// [`Self::vm_fallback_threads`] thread) while the built-in refusal and
    /// [`Self::define_builtin_constraint_fallback_pin`] are both on; empty
    /// otherwise (`=0` on either switch never writes it). A pin naming one of them is
    /// no pin for a built-in define. `ClassId`s are never reused, so a stale
    /// id of an unloaded class can never name another class. Per class
    /// manager (no process state).
    pub(in crate::class_manager) vm_fallback_classes: FxHashSet<ClassId>,

    /// `CRATONVM_FINALIZER_EMPTY_IS_NONE` (JIT round 21 wave 17, lane
    /// finalize-reinit, default ON, `=0` restores the old answer; item 4 of
    /// `docs/feature-designs/class-resolution-and-caching.md`), latched
    /// per VM: a class whose own `finalize()V` is a lone `return` is not
    /// finalizable, and that empty override also cancels a finalizer it would
    /// inherit (HotSpot `ClassFileParser`: `_has_empty_finalizer`,
    /// `set_precomputed_flags`). See [`class_has_finalizer_by_hotspot_rule`].
    pub(in crate::class_manager) finalizer_empty_is_none: bool,
    /// `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES` (JIT round 21 wave 22,
    /// lane hidden-resid, default ON, `=0` restores the old bits), latched
    /// per VM: a stub upgrade that changes a class's `has_finalizer`
    /// recomputes its already-loaded subclasses'
    /// ([`Self::propagate_finalizer_to_subclasses`]).
    pub(in crate::class_manager) stub_upgrade_finalizer_to_subclasses: bool,
    /// The classes whose `has_finalizer` a stub upgrade moved (the upgraded
    /// class and its recomputed subclasses), each with its new bit, in the
    /// order they changed (parents before children), until the VM takes them
    /// ([`Self::take_stub_upgrade_finalizer_flips`]; JIT round 21 wave 23,
    /// lane finvm). Filled only under
    /// [`Self::stub_upgrade_finalizer_to_subclasses`]. Per class manager, so a
    /// raw write lock that strands a row leaves it for the next release. The
    /// VM's production raw sites take its settling write lock instead
    /// (`ClassRealm::class_manager_write_settling`, JIT round 21 wave 25, lane
    /// rawwrite, `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS`), whose release
    /// settles the rows it queued.
    pub(in crate::class_manager) stub_upgrade_finalizer_flips: Vec<(ClassId, bool)>,

    /// The classes whose type-map walk [`Self::type_maps_at_link`] moved to
    /// their link, each with whether the walk records HotSpot's verifier
    /// loads (the define-time `record_verifier_loads`). Dropped at link
    /// ([`Self::finish_pending_type_maps`]) and on unload.
    pub(in crate::class_manager) type_maps_pending: FxHashMap<ClassId, PendingTypeMaps>,

    /// `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK` (interpreter round i5
    /// wave 102, lane L7): the user-loader classes whose Pass 3 verdict the
    /// define left to their link ([`Self::verify_pending_user_pass3`]).
    /// Dropped once the link passes it ([`Self::finish_pending_user_pass3`])
    /// and on unload; a failed link keeps it, so the next attempt verifies
    /// again, as HotSpot does.
    pub(in crate::class_manager) user_pass3_pending: FxHashSet<ClassId>,

    /// WP2.3 — counter used to mangle hidden-class names when the
    /// caller-supplied `override_name` collides with an already-loaded
    /// class. The counter is monotonic across the whole class manager so
    /// that `Foo/0x1`, `Foo/0x2`, ... never collide even across many
    /// hidden defines of the same template.
    pub(in crate::class_manager) hidden_name_counter: u64,

    /// WP2.4-B — JEP 109 / JVMTI `RedefineClasses` generation counter,
    /// keyed by [`ClassId`].  Bumped by [`ClassManager::redefine_class`]
    /// on each successful redefinition; never decremented.  Caches that
    /// snapshot a method-resolution at lookup time stamp the generation
    /// from `class_redefine_generation(class_id)` and on next hit
    /// recheck it; on mismatch the entry is treated as stale and
    /// re-resolved against the freshly-installed bytecode.
    ///
    /// Stored on the manager (not on `Class`) so existing `Class`
    /// construction sites in tests and bench fixtures don't need to be
    /// touched.  An entry is created lazily — a class that has never
    /// been redefined returns generation 0 from
    /// [`ClassManager::class_redefine_generation`] without allocating.
    /// The `Arc` is intentional: `class_redefine_generation_handle`
    /// returns a clone so a JIT cache or invoke cache can hold onto the
    /// counter and check it later without reborrowing the
    /// [`ClassManager`].
    ///
    /// WP2.4-F1 — wrapped in a `RwLock` so the hot per-thread invoke-cache
    /// populate path can acquire a handle through a `&ClassManager`
    /// borrow (which is what `shared.classes.class_manager.read()` provides) and
    /// share the *same* `Arc<AtomicU32>` that `redefine_class` will
    /// later bump.  Without this, populate-time and redefine-time would
    /// hand out two unrelated counters and the cache would never see a
    /// bump.
    ///
    /// T10.9.E: switched from `HashMap` (SipHash) to `FxHashMap`. ClassId
    /// is a trusted `u32` and this map is consulted on every populate of a
    /// per-thread invoke-cache entry; FxHash is a measurable win.
    pub(in crate::class_manager) redefine_generations: RwLock<FxHashMap<ClassId, Arc<AtomicU32>>>,

    /// Which of this manager's class ids are hidden, readable without the
    /// class-manager lock through [`Self::hidden_class_bits`] (JIT round 21
    /// wave 14, lane hidden-static; see [`HiddenClassBits`]).
    pub(in crate::class_manager) hidden_classes: HiddenClassBits,
    // The recorded class-file names (wave 21's `hidden_class_file_names`)
    // live on the class store since wave 22 (lane hidden-resid), so they die
    // with the class and every store holder reads one answer
    // (`ClassStore::hidden_self_name`).

    /// This manager's lock-free "class is initialized" bitmap and init-reset
    /// generation, shared with the VM ([`Self::class_init_bits`]); cleared and
    /// advanced by [`Self::upgrade_synthetic_class`].
    pub(in crate::class_manager) class_init_bits: ClassInitBits,

    /// Per redefined class, the translations that let a frame still running
    /// a replaced body read its constants in the merged pool
    /// ([`crate::obsolete_code`]). Written only by [`Self::redefine_class`];
    /// read through [`Self::redefinition_history`] (interpreter round i1
    /// wave 19, lane L3).
    pub(in crate::class_manager) redefinition_histories: FxHashMap<ClassId, crate::obsolete_code::RedefinitionHistory>,

    /// This VM's redefinitions on the process clock ([`RedefinitionLog`];
    /// nio21 wave 3 L1). Written by every advance [`Self::redefine_class`]
    /// and the VM's redefinition path make
    /// ([`Self::bump_redefinition_count_of`]); shared with the VM
    /// ([`Self::redefinition_log`]) so its per-thread invoke caches retire
    /// for this VM's redefinitions only.
    pub(in crate::class_manager) redefinition_log: Arc<RedefinitionLog>,

    /// Round 5 audit fix (HIGH): the distinct user-defined `ClassLoaderId`s
    /// observed in [`Self::loaded_classes`]. Used by
    /// [`Self::find_class_by_name`] so the user-loader extension probe is
    /// an O(loaders) lookup rather than an O(entries) scan that rebuilt
    /// the set every call (a Spring app with ~15k loaded classes and a
    /// handful of user loaders previously walked the full map per miss).
    ///
    /// Built incrementally: every insert into [`Self::loaded_classes`]
    /// with a `ClassLoaderId::UserDefined(_)` key inserts into this set
    /// (set insert is idempotent — duplicates are no-ops). Class unloading
    /// rebuilds it from the surviving classes (`unload_user_classes_inner`),
    /// so a loader whose last class unloads leaves it. (This note used to say
    /// the set only grows because unloading was not implemented; corrected
    /// by gc-common w10-f.) Cap is implicit by the number of distinct user
    /// loaders with a live class.
    pub(in crate::class_manager) user_loaders: FxHashSet<ClassLoaderId>,

    /// Secondary index over [`Self::loaded_classes`]: class NAME -> the
    /// distinct `ClassId`s defined under that name by ANY loader, each with
    /// the number of `(loader, name)` map entries that point at it.
    ///
    /// Exists solely so [`Self::find_unique_class_by_name`] can answer
    /// "exactly one class carries this name" in O(1). It used to run
    /// `unique_loaded_class`, a full linear scan of `loaded_classes` with a
    /// string compare per entry, for every call. On any class-heavy workload
    /// that dominated the entire VM: a `perf` profile of a Jython module parse
    /// (the SSE variants of Spring's
    /// `FragmentViewResolutionResultHandlerTests`) put
    /// `find_unique_class_by_name` at **55% of all CPU samples**, with the
    /// `memcmp` it drives another 3.8% -- the parse took 154 s where HotSpot
    /// needed 0.28 s. The JIT's own callers memoize their answers, but
    /// `try_compile_direct_dispatcher` deliberately does NOT cache a miss (a
    /// not-yet-loaded class can load later), so every unresolvable name paid a
    /// fresh whole-map scan.
    ///
    /// Maintained incrementally by `loaded_classes_insert` /
    /// `loaded_classes_remove`, which are the ONLY writers of
    /// `loaded_classes`, and rebuilt wholesale by the (rare) loader-unload
    /// `retain`. The refcount is what makes redefinition and multi-loader
    /// aliasing exact: two loaders may legitimately map the same name to the
    /// same `ClassId`, and that must still read as unique.
    pub(in crate::class_manager) name_definitions: FxHashMap<Arc<str>, FxHashMap<ClassId, u32>>,

    /// Virtual classes (lambda proxies) that are not in the `ClassStore` but can be the
    /// COMPONENT of an array class: `<host>$$Lambda/0x<id>` (internal form) -> the proxy's
    /// `ClassId`. Registered by the VM when a reflective array of a proxy is first named
    /// (SBR5 walker); consulted only by array-class synthesis, never by `load_class`.
    pub(in crate::class_manager) virtual_components: FxHashMap<String, ClassId>,

    /// gci i12/ul F2: user-namespace array classes the VM has not yet given a
    /// defining-loader row (see [`Self::take_pending_user_arrays`]). A mutex so
    /// the root scan drains it under the manager's read lock.
    pub(in crate::class_manager) pending_user_arrays: std::sync::Mutex<Vec<ClassId>>,

    /// Round 5 audit fix (HIGH): per-class initialization state for the
    /// AtomicU8 fast path. Values: 0 = UNINITIALIZED (or any pre-init state),
    /// 1 = IN_PROGRESS, 2 = INITIALIZED. The VM calls
    /// [`Self::class_init_state_handle`] to obtain the `Arc<AtomicU8>` once
    /// and then checks the warm-path state with a single atomic load — no
    /// `RwLock<ClassManager>` round-trip on the steady-state hot path.
    ///
    /// The full class lifecycle in `Class::state` (Loaded → Verifying →
    /// Verified → Preparing → Prepared → Initializing → Initialized) is
    /// authoritative; this AtomicU8 is a cache of "is it INITIALIZED?"
    /// that the slow path keeps in sync via
    /// [`Self::set_class_init_state`].
    ///
    /// Entries are created lazily by `class_init_state_handle` so classes
    /// that are never initialized cost nothing.
    ///
    /// Round 8 (CRIT, audit `round8-classloading-reader.md` §3):
    /// `parking_lot::RwLock` replaces `std::sync::RwLock` here so the
    /// ensure_class_initialized fast path is no longer paying for std's
    /// poisoning-aware `Result`-wrapped lock + futex-style park. The
    /// parking_lot lock is a single CAS on the uncontended fast path,
    /// which matches the steady-state shape of this map (overwhelming
    /// majority of accesses are reads of already-INITIALIZED entries).
    pub(in crate::class_manager) init_states: parking_lot::RwLock<FxHashMap<ClassId, Arc<std::sync::atomic::AtomicU8>>>,

    /// Real-JDK policy for *this* manager (contract §5).
    ///
    /// Deliberately a field and not a process global: two VMs in one process
    /// must be able to run under different policies, and this repository has
    /// already been bitten by process-global native caches leaking across VMs.
    /// Set once at VM init by `vm_init`, before any class is loaded; defaults
    /// to [`CompatibilityMode::Compatible`], which is today's behaviour
    /// byte-for-byte.
    pub(in crate::class_manager) compatibility_mode: CompatibilityMode,

    /// The `--compatible` rows aligned with HotSpot
    /// (`cratonvm_types::hotspot_alignment`), the native registry's set handed
    /// over by `vm_init` with the mode; asked through [`Self::aligned`].
    /// Defaults to the table's defaults (a manager nobody configures).
    /// Interpreter round i1 wave 50, lane L5.
    pub(in crate::class_manager) hotspot_alignments: cratonvm_types::HotSpotAlignments,
    /// Each resolved `--module-path` module's roots on the application class
    /// path, by module name: its `--patch-module` paths first, then the
    /// module root (exploded directory or modular jar). Filled by `vm_init`
    /// ([`Self::set_module_path_roots`]); asked by
    /// [`Self::module_path_resource_url`]. Per VM, never a process global
    /// (AGENTS.md). Interpreter round i4 wave 98, lane L3.
    pub(in crate::class_manager) module_path_roots: FxHashMap<String, Vec<std::path::PathBuf>>,
    /// JDK 17/21 instrument a boot-loader event class against the typed
    /// `EventConfiguration` field; JDK 25 against an `Object`-typed one (SBR5 lane `jfr`).
    pub(in crate::class_manager) jfr_typed_event_configuration: bool,
    /// This VM's `--enable-preview` (`VmConfig::enable_preview`, set by `vm_init`
    /// through [`Self::set_preview_enabled`]): whether a preview class file of
    /// this release (`69.65535`) may be defined. Off by default, as HotSpot.
    /// Per VM (AGENTS.md): a process global in the reader, set only by the
    /// launcher, until interpreter round i5 wave 101, lane L5.
    pub(in crate::class_manager) preview_enabled: bool,
    /// zt t18/hunt2: this VM's `--finalization=` (`VmConfig::finalization_enabled`,
    /// set by `vm_init` through [`Self::set_finalization_enabled`]). HotSpot 25
    /// (JEP 421) gives no class `has_finalizer` while finalization is disabled,
    /// so no instance is ever registered and `finalize()` never runs. On by
    /// default, as HotSpot.
    pub(in crate::class_manager) finalization_enabled: bool,
    /// zt t18/hunt2: `CRATONVM_EMPTY_FINALIZER_NOT_FINALIZABLE` (default ON,
    /// `=0` kill), read once at construction; see
    /// [`Self::computed_has_finalizer`].
    pub(in crate::class_manager) empty_finalizer_not_finalizable: bool,

    /// How often each alignment row changed an answer
    /// (`cratonvm_types::AlignmentCensus`): the class realm's, shared by
    /// `vm_init`; a manager nobody configures counts into its own.
    /// Interpreter round i1 wave 51, lane L5.
    pub(in crate::class_manager) alignment_census: std::sync::Arc<cratonvm_types::AlignmentCensus>,

    /// `-Xverify:all` — verify every class strictly, boot image included.
    ///
    /// A field for the same reason `compatibility_mode` is: verification policy
    /// is per-VM, and two VMs in one process must be able to differ. Defaults
    /// to `false`, which is `-Xverify:remote` — HotSpot's default and today's
    /// behaviour byte-for-byte. See [`Self::set_strict_verification`].
    pub(in crate::class_manager) strict_verification: bool,

    /// `-Xverify:none` / `-noverify` for the define-time verifier (t1 wave 9,
    /// lane I24V, `CRATONVM_NOVERIFY_SKIPS_DEFINE_PASS3`, default ON, `=0`
    /// kill). See [`Self::set_skip_remote_verification`]. Defaults to `false`.
    pub(in crate::class_manager) skip_remote_verification: bool,

    /// `-Xverify:none` / `-noverify` for the define-time format legality
    /// checks (t1 wave 10, lane I24W, `CRATONVM_NOVERIFY_RELAXES_FORMAT_CHECKS`,
    /// default ON, `=0` kill). See [`Self::set_noverify_relaxes_format_checks`].
    /// Defaults to `false`.
    pub(in crate::class_manager) noverify_relaxes_format_checks: bool,

    /// `-Xverify:none` / `-noverify` for the STRUCTURAL `_need_verify` format
    /// checks too (t1 wave 13, lane I24W,
    /// `CRATONVM_NOVERIFY_RELAXES_STRUCTURAL_CHECKS`, default ON, `=0` kill).
    /// See [`Self::set_noverify_relaxes_structural_checks`]. Defaults to
    /// `false`; only takes effect with `noverify_relaxes_format_checks`.
    pub(in crate::class_manager) noverify_relaxes_structural_checks: bool,

    /// Does `-Xverify:all` extend the format checks to the boot and platform
    /// loaders (t1 wave 13, lane I24W, `CRATONVM_VERIFY_ALL_CHECKS_BOOT_FORMAT`,
    /// default ON, `=0` kill)? Defaults to `true`; only read with
    /// `strict_verification`.
    pub(in crate::class_manager) verify_all_checks_boot_format: bool,

    /// Class-fabrication violations recorded this run, in first-observation
    /// order.
    ///
    /// Populated in **both** modes: wave 1 is measurement, not deletion
    /// (contract §10), so a `Compatible` run still produces the work list that
    /// tells an operator what a `--real-jdk` run would have refused.
    pub(in crate::class_manager) origin_violations: Vec<JdkOnlyViolation>,

    /// Dedupe key set for [`Self::origin_violations`].
    ///
    /// A single missing class is requested over and over (constant-pool
    /// resolution, `Class.forName` probes, every allocation of a stub-backed
    /// container). Without this the violation list would be dominated by
    /// repeats of a handful of names and would be useless as a backlog.
    pub(in crate::class_manager) origin_violations_seen: FxHashSet<String>,

    /// `class name -> owner/Class.method(Desc)` of whoever first caused the
    /// class to be fabricated, for `ClassOriginEntry::requested_by`.
    ///
    /// `ClassManager` is called with a bare name and cannot see the frame, so
    /// this map is filled from the *outside*, by
    /// [`Self::attach_origin_requester`], which the VM's class-load choke point
    /// calls with the frame it was resolving for. Only classes that actually
    /// produced a violation get an entry, so the map is bounded by the
    /// violation list, not by the class store.
    ///
    /// Keyed by class name rather than `ClassId` because the violation is
    /// recorded *before* the class exists, and in `JdkOnly` mode no class is
    /// ever created for it.
    pub(in crate::class_manager) origin_requesters: FxHashMap<String, String>,

    /// Optional live sink for class-origin violations, installed per VM.
    ///
    /// `--trace-jdk-only` used to be a poll of [`Self::origin_violations`],
    /// which meant every mid-run fabrication surfaced at shutdown, detached
    /// from the code that caused it. With a sink installed the launcher gets
    /// each violation at the instant it is recorded.
    ///
    /// A field, not a process global: contract §2, and the repo has a history
    /// of process-global native caches leaking between two VMs in one process.
    /// Two VMs in one process therefore see only their own violations.
    ///
    /// Invoked from [`Self::admit_compatibility_class`] while the manager is
    /// mutably borrowed, so the sink must not re-enter the class manager.
    pub(in crate::class_manager) violation_sink: Option<Arc<dyn Fn(&JdkOnlyViolation) + Send + Sync>>,

    /// `(class name, requesting Rust site) -> how many times
    /// [`Self::try_ensure_synthetic_class`] has refused that pair`, and the
    /// dedupe key for the refusal `warn!`.
    ///
    /// # The key is the PAIR, and that is the whole design
    ///
    /// Measured 2026-08-12 under `--real-jdk --explain-jdk-only`: every
    /// boot-time refusal is reported (the boot block warns) and **every
    /// runtime refusal is silent** — `AtomicReferenceFieldUpdater$RustJvmImpl`
    /// (kills `java.sql`), `cratonvm/internal/UnmodifiableMap` (kills
    /// `System.getenv`), `javax/net/ssl/SSLSocketOutputStream` (kills HTTPS),
    /// `java/util/function/Consumer$AndThen`, `java/lang/foreign/DowncallHandle`.
    ///
    /// `UnmodifiableMap` is in **both** sets: refused at boot from
    /// `vm_init::ensure_bootstrap_compat_class`, and refused again later when
    /// `System.getenv()`'s native asks for it. Those are two different events
    /// with two different diagnoses — one says a boot wiring was skipped, the
    /// other says an application call just died — and a class-keyed dedupe
    /// would report the harmless one and swallow the one that matters. Keying
    /// on the pair keeps them apart, and it is the same discrimination the
    /// `requested_by` field exists to make: a refusal without a requester is
    /// half a diagnosis.
    ///
    /// # Why this is not [`Self::origin_violations_seen`]
    ///
    /// That set is inserted into by [`Self::admit_compatibility_class`]
    /// *before* it returns the `Err`, so by the time the refusal is back in
    /// `try_ensure_synthetic_class` the name is always already present and
    /// "first insert" can no longer be read as "first refusal" — it would
    /// suppress **every** warn. It is also class-keyed, which is exactly the
    /// collapse described above, and covers a different population:
    /// `admit_compatibility_class` also runs from `create_synthetic_stub` (the
    /// `load_class` chain) and records in both modes, whereas this counts only
    /// refusals actually handed back to a caller — the set an operator has to
    /// act on, because each one is a native that did not get its receiver.
    ///
    /// # Why a count and not a set
    ///
    /// A refused allocation shape is re-requested on every allocation of
    /// whatever wanted it — thousands of times for a container node class — so
    /// the warn is rate-limited rather than per-call, and the count is what
    /// the limiter reads. See [`Self::warn_fabrication_refused`] for the
    /// schedule. "Refused once at boot" and "refused 40,000 times from a hot
    /// native" are different diagnoses, and the count is the only thing that
    /// separates them.
    ///
    /// # Where the state lives
    ///
    /// A field on the manager — **VM-scoped, never process-global**, for the
    /// same reason [`Self::violation_sink`] is (contract §2). The nearest
    /// in-tree precedent for this rate limiter, `gc::autobox`'s
    /// `observe_primitive_into_reference_field`, uses a process-global
    /// `static SEEN: AtomicU64`; that is the shape this must NOT copy. A global
    /// here lets the first VM in a process consume the whole quota and silence
    /// the second VM's refusals, and the failure is invisible — the second
    /// VM's log is simply missing a line nobody knows to expect. The map is
    /// bounded by the number of distinct `(class, call site)` pairs, a smaller
    /// population than the class store and the same class of bound
    /// [`Self::origin_violations_seen`] already accepts.
    pub(in crate::class_manager) jdk_only_refusals: FxHashMap<RefusalSite, u64>,

    /// C2 review P1 — VM identity + per-class metadata generation for the
    /// generational handles in [`crate::metadata_handle`].
    ///
    /// This manager's class table is indexed by `ClassId`, and a `ClassId` is
    /// meaningless outside the VM that allocated it (`ClassStore::next_id`
    /// returns `self.classes.len()`, and every `SharedVm` owns its own store —
    /// `docs/architecture/per-vm-state.md` §0, Fact 1). The realm is what makes
    /// a long-lived reference to this table checkable: it records *whose* table
    /// this is and *which definition* of each class is current, so a stale
    /// index fails with a named error at the resolution boundary instead of
    /// silently naming another VM's class or a pre-redefine method table.
    ///
    /// Unbound until [`Self::bind_vm_id`] runs; see that method for the
    /// ordering requirement.
    pub(in crate::class_manager) metadata_realm: MetadataRealm,

    /// Class files a `java.lang.instrument` agent has already rewritten, waiting
    /// for the load that asked for them. Keyed by internal class name.
    ///
    /// # Why the bytes arrive from outside instead of a hook in here
    ///
    /// The JVMTI/`java.lang.instrument` contract is that a registered
    /// `ClassFileTransformer` is handed every class file *before* it is parsed,
    /// and the transformer is **Java code**. Every definition path in this file
    /// runs under the VM's L10 class-manager write lock, and running Java under
    /// that lock deadlocks the first time the transformer touches a class.
    ///
    /// So the transform runs one level up, in the VM crate, on a thread that
    /// holds no class-manager lock (`SharedVm::load_class_transformed`): it
    /// looks the bytes up with [`Self::find_class_bytes_delegated`], invokes the
    /// chain, stages the result here, and then calls the ordinary load. The
    /// staged entry is consumed by [`Self::load_class`] at the point the
    /// untransformed bytes would otherwise have been read, so the whole rest of
    /// the load — the per-name lock, the circularity guard, the class-bytes
    /// cache, `ClassLoad`/`ClassPrepare` — is unchanged and sees only the final
    /// bytes.
    ///
    /// Consumed on read (`remove`), so a staged entry can never outlive the load
    /// it was produced for. Empty on every run with no agent installed.
    ///
    /// The third element is the JVMTI retransformation base when it is not the
    /// staged class file itself: the bytes as they were before the first
    /// retransform-capable transformer changed them (interpreter round i1 wave
    /// 26, lane L3). The load records it as the class's cached bytes, which is
    /// what `retransformClasses` hands the transformers.
    pub(in crate::class_manager) pending_transformed_classes:
        FxHashMap<String, (SharedBytes, ClassLoaderId, Option<SharedBytes>)>,

    /// The staged names ([`Self::pending_transformed_classes`]) whose class,
    /// once defined from the staged bytes, cedes its registered natives to
    /// that woven bytecode ([`Self::cede_native_shadows_to_woven_bytecode`]):
    /// the VM stages one when the registry may name the class
    /// ([`Self::stage_native_shadow_cede`]). Interpreter round i1 wave 45,
    /// lane L3.
    pub(in crate::class_manager) pending_native_shadow_cedes: FxHashSet<String>,

    /// zt t20/pl9 (t16 defect D6): the class-path classes this manager defined
    /// for the application loader, and their class-file bytes
    /// ([`Self::note_app_class_path_define`]). HotSpot's application loader
    /// defines each through `defineClass1` / `defineClass2`, which count
    /// `sun.cls.defineAppClasses` / `appClassBytes`; this VM's application
    /// loader (`--real-jdk`'s gated class-path lookup and `--compatible`'s
    /// adopted loader alike) has [`Self::load_class`] define them instead, so
    /// the perf-data sampler adds these to the define natives' counts.
    /// Cumulative; never reset.
    pub(in crate::class_manager) app_class_path_defines: (u64, u64),
    /// zt pl4 item 1: the VM's class-trace figures when its perf-data image
    /// has the class timers (`perf_data_file::add_hotspot_counters` hands
    /// them in, [`Self::set_class_trace`]): the class-path door times its
    /// boot reads (`sun.cls.sysClassLoadTime`), its application-loader
    /// defines (`defineAppClassTime`) and, when the VM initiated the load,
    /// HotSpot's `CLASS_LOAD` event (`appClassLoadTime` / `Count`). `None`:
    /// nothing timed.
    pub(in crate::class_manager) class_trace: Option<std::sync::Arc<cratonvm_types::perf_class_trace::ClassTraceCounters>>,
    /// zt t29/cb (perf-counter page item 1, `sysClassBytes`): the class-file
    /// bytes of every class the boot loader defined from what its search
    /// read -- the runtime image, `-Xbootclasspath/a` / agent-appended and
    /// `--patch-module` entries -- and not from the CDS cache
    /// ([`Self::note_sys_class_path_bytes`]), as HotSpot's boot loader adds
    /// each class file it reads outside its archive to
    /// `sun.cls.sysClassBytes`. Cumulative; never reset.
    pub(in crate::class_manager) sys_class_path_bytes: u64,
    /// zt t29/cb (perf-counter page item 1, `unloadedBytes`): the metadata
    /// model bytes of every class an unload transaction removed
    /// ([`Self::note_unloaded_class_bytes`]; the VM's unload transaction
    /// sizes them before the removal, under this manager's write lock), as
    /// HotSpot's `ClassLoadingService::notify_class_unloaded` adds to
    /// `sun.cls.unloadedBytes`. Cumulative; never reset.
    pub(in crate::class_manager) unloaded_class_bytes: u64,
    pub app_workarounds: Vec<cratonvm_native_api::AppWorkaroundEntry>,
    pub app_stub_layouts: FxHashMap<&'static str, &'static [cratonvm_native_api::SyntheticStubField]>,
}

/// What the class-name index knows about a name, with the three "no single
/// answer" outcomes kept apart.
///
/// See [`ClassManager::classify_loaded_name`] for why the distinction is
/// load-bearing: `Absent` and `Ambiguous` both surface as `None` from every
/// `Option`-returning lookup in this file, and they demand opposite actions
/// from the caller (load it vs. definitely do not load it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameResolution {
    /// No loader has a class registered under this name, and none is hidden
    /// behind it. Loading one is safe.
    Absent,
    /// Exactly one class carries this name. Several loaders may name it (a
    /// delegation alias), which is still one class and still unique.
    Unique(ClassId),
    /// Two or more **distinct** classes carry this name. There is no correct
    /// context-free answer. Re-ask with an initiating loader
    /// ([`ClassManager::find_class_by_name_for_loader`] /
    /// [`ClassManager::find_class_by_name_for_class`]) or fail — treating this
    /// as `Absent` and loading a fresh copy makes it a three-way collision.
    Ambiguous {
        /// How many distinct `ClassId`s carry the name.
        definitions: usize,
    },
}

impl NameResolution {
    /// The class id when the name resolves uniquely, `None` for both `Absent`
    /// and `Ambiguous`. Provided for call sites that genuinely do not care why
    /// there is no answer; anything that would *load* on a miss must match on
    /// the variants instead.
    #[inline]
    pub fn unique(self) -> Option<ClassId> {
        match self {
            NameResolution::Unique(id) => Some(id),
            NameResolution::Absent | NameResolution::Ambiguous { .. } => None,
        }
    }

    /// True when more than one distinct class carries the name.
    #[inline]
    pub fn is_ambiguous(self) -> bool {
        matches!(self, NameResolution::Ambiguous { .. })
    }
}

/// Metadata released when a user-defined class loader is unloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnloadedClass {
    pub id: ClassId,
    pub name: Arc<str>,
    pub loader_id: ClassLoaderId,
}

/// Initialization-state values stored in [`ClassManager::init_states`].
pub const CLASS_INIT_UNINITIALIZED: u8 = 0;
pub const CLASS_INIT_IN_PROGRESS: u8 = 1;
pub const CLASS_INIT_INITIALIZED: u8 = 2;

/// Round 5 audit fix (HIGH): debug-only snapshot of every invariant
/// field on [`Class`] used by [`ClassManager::redefine_class`] to verify
/// that the redefine touched ONLY the documented mutable set:
///
/// * `methods`
/// * `constant_pool`
/// * `bootstrap_methods`
/// * `annotations`
/// * `source_file`
///
/// `from_class` destructures the entire `Class` with `..`-trailing pattern
/// removed so adding a new field is a hard compile error here until the
/// author classifies it as either an invariant (add to this struct + the
/// assertion) or a mutable redefine field (add to the swap block + the
/// documented set above).
///
/// Round 7 audit fix (CRIT #1): the snapshot type *itself* and the
/// destructuring `let Class { … } = c;` in `from_class` are now always
/// compiled (the `#[cfg(debug_assertions)]` gate was previously around
/// both, so release builds silently lost the field-exhaustiveness
/// trip-wire — a future contributor could add a new `Class` field that
/// the redefine path quietly clobbered without anyone noticing until a
/// debug-build test caught it). Only the *runtime* `debug_assert_eq!`
/// checks in [`assert_eq`] remain `#[cfg(debug_assertions)]`-gated; the
/// destructuring pattern is purely compile-time and adds zero runtime
/// cost in either profile.
#[derive(Debug)]
pub(in crate::class_manager) struct RedefineInvariantSnapshot {
    pub(in crate::class_manager) id: ClassId,
    pub(in crate::class_manager) loader_id: ClassLoaderId,
    pub(in crate::class_manager) name: Arc<str>,
    pub(in crate::class_manager) version_major: u16,
    pub(in crate::class_manager) version_minor: u16,
    pub(in crate::class_manager) state: ClassState,
    pub(in crate::class_manager) initializing_thread: Option<u64>,
    pub(in crate::class_manager) access_flags_bits: u16,
    pub(in crate::class_manager) superclass: Option<ClassId>,
    pub(in crate::class_manager) interfaces: Vec<ClassId>,
    pub(in crate::class_manager) field_sigs: Vec<(Arc<str>, Arc<str>, u16)>,
    pub(in crate::class_manager) first_field_index: usize,
    pub(in crate::class_manager) num_total_fields: usize,
    pub(in crate::class_manager) signature: Option<String>,
    pub(in crate::class_manager) nest_host: Option<String>,
    pub(in crate::class_manager) nest_members: Vec<String>,
    pub(in crate::class_manager) record_components_len: usize,
    pub(in crate::class_manager) permitted_subclasses: Vec<String>,
    // `inner_classes` / `enclosing_method` are NOT invariants since JIT round
    // 21 wave 17 (lane innerclasses): HotSpot's `redefine_single_class` swaps
    // the scratch class's `inner_classes` array (which carries the
    // `EnclosingMethod` indices) into the redefined class
    // (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`).
    pub(in crate::class_manager) hidden: bool,
    pub(in crate::class_manager) module_name: Option<String>,
    /// Provenance is an invariant of the class, not of its bytes: JEP 109
    /// swaps method bodies, it does not turn a boot-image class into a
    /// fabricated one (or vice versa). Snapshotting it means a redefine that
    /// silently rewrote `origin` trips the assertion instead of quietly
    /// changing what the `--real-jdk` policy sees.
    ///
    /// This used to be snapshotted twice — once as `origin`, once as the
    /// derived `is_synthetic_stub` bool. The bool is gone (2026-08-06); the
    /// origin is the single source of truth, and comparing it covers both.
    pub(in crate::class_manager) origin: ClassOrigin,
    pub(in crate::class_manager) has_finalizer: bool,
    pub(in crate::class_manager) code_source_present: bool,
    pub(in crate::class_manager) array_info_present: bool,
}

impl RedefineInvariantSnapshot {
    pub(in crate::class_manager) fn from_class(c: &Class) -> Self {
        // Destructure with explicit names. The trailing `..` is
        // intentionally omitted on the field list so a new field added
        // to `Class` produces an "unused field" warning (or, with
        // `#![deny(unused)]` in test/CI builds, a hard error) until the
        // author wires it through here.
        //
        // We don't snapshot the explicitly mutable fields
        // (`methods`, `constant_pool`, `bootstrap_methods`,
        // `annotations`, `source_file`) — those are expected to change.
        let Class {
            id,
            loader_id,
            name,
            source_file: _,
            version,
            state,
            initializing_thread,
            constant_pool: _,
            access_flags,
            superclass,
            interfaces,
            fields,
            methods: _,
            first_field_index,
            num_total_fields,
            bootstrap_methods: _,
            signature,
            annotations: _,
            nest_host,
            nest_members,
            record_components,
            permitted_subclasses,
            inner_classes: _,
            enclosing_method: _,
            hidden,
            module_name,
            origin,
            has_finalizer,
            code_source,
            array_info,
            // Round 8 audit fix (CRIT #4): init_state is the per-class
            // initialization AtomicU8 (`UNINITIALIZED → IN_PROGRESS →
            // INITIALIZED`). Mutating during `<clinit>` is normal,
            // expected behavior — NOT a redefine invariant. Excluded
            // from the snapshot.
            init_state: _,
            record_object_methods: _,
        } = c;
        Self {
            id: *id,
            loader_id: *loader_id,
            name: Arc::clone(name),
            version_major: version.major,
            version_minor: version.minor,
            state: *state,
            initializing_thread: *initializing_thread,
            access_flags_bits: access_flags.bits(),
            superclass: *superclass,
            interfaces: interfaces.clone(),
            field_sigs: fields
                .iter()
                .map(|f| {
                    (
                        Arc::clone(&f.name),
                        Arc::clone(&f.descriptor),
                        f.access_flags.bits(),
                    )
                })
                .collect(),
            first_field_index: *first_field_index,
            num_total_fields: *num_total_fields,
            signature: signature.clone(),
            nest_host: nest_host.clone(),
            nest_members: nest_members.clone(),
            record_components_len: record_components.len(),
            permitted_subclasses: permitted_subclasses.clone(),
            hidden: *hidden,
            module_name: module_name.clone(),
            origin: origin.clone(),
            has_finalizer: *has_finalizer,
            code_source_present: code_source.is_some(),
            array_info_present: array_info.is_some(),
        }
    }

    // Round 7 audit fix (CRIT #1): only the runtime walk is debug-only;
    // the struct + `from_class` are always compiled so the destructuring
    // trip-wire fires in release builds. `dead_code` is allowed because
    // release callers never invoke this method (the `debug_assert_eq!`
    // bodies compile to no-ops, so the function is effectively unused
    // in release, but we keep the symbol for ABI / future debug runs).
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub(in crate::class_manager) fn assert_eq(&self, after: &Self, class_name: &str) {
        // One assert per invariant so the failure message tells the
        // future-author exactly which field they mutated.
        debug_assert_eq!(
            self.id, after.id,
            "redefine_class mutated Class::id on {class_name}"
        );
        debug_assert_eq!(
            self.loader_id, after.loader_id,
            "redefine_class mutated Class::loader_id on {class_name}"
        );
        debug_assert!(
            Arc::ptr_eq(&self.name, &after.name) || *self.name == *after.name,
            "redefine_class mutated Class::name on {class_name}"
        );
        debug_assert_eq!(
            self.version_major, after.version_major,
            "redefine_class mutated Class::version.major on {class_name}"
        );
        debug_assert_eq!(
            self.version_minor, after.version_minor,
            "redefine_class mutated Class::version.minor on {class_name}"
        );
        debug_assert_eq!(
            self.state, after.state,
            "redefine_class mutated Class::state on {class_name}"
        );
        debug_assert_eq!(
            self.initializing_thread, after.initializing_thread,
            "redefine_class mutated Class::initializing_thread on {class_name}"
        );
        debug_assert_eq!(
            self.access_flags_bits, after.access_flags_bits,
            "redefine_class mutated Class::access_flags on {class_name}"
        );
        debug_assert_eq!(
            self.superclass, after.superclass,
            "redefine_class mutated Class::superclass on {class_name}"
        );
        debug_assert_eq!(
            self.interfaces, after.interfaces,
            "redefine_class mutated Class::interfaces on {class_name}"
        );
        debug_assert_eq!(
            self.field_sigs, after.field_sigs,
            "redefine_class mutated Class::fields shape on {class_name}"
        );
        debug_assert_eq!(
            self.first_field_index, after.first_field_index,
            "redefine_class mutated Class::first_field_index on {class_name}"
        );
        debug_assert_eq!(
            self.num_total_fields, after.num_total_fields,
            "redefine_class mutated Class::num_total_fields on {class_name}"
        );
        debug_assert_eq!(
            self.signature, after.signature,
            "redefine_class mutated Class::signature on {class_name}"
        );
        debug_assert_eq!(
            self.nest_host, after.nest_host,
            "redefine_class mutated Class::nest_host on {class_name}"
        );
        debug_assert_eq!(
            self.nest_members, after.nest_members,
            "redefine_class mutated Class::nest_members on {class_name}"
        );
        debug_assert_eq!(
            self.record_components_len, after.record_components_len,
            "redefine_class mutated Class::record_components on {class_name}"
        );
        debug_assert_eq!(
            self.permitted_subclasses, after.permitted_subclasses,
            "redefine_class mutated Class::permitted_subclasses on {class_name}"
        );
        debug_assert_eq!(
            self.hidden, after.hidden,
            "redefine_class mutated Class::hidden on {class_name}"
        );
        debug_assert_eq!(
            self.module_name, after.module_name,
            "redefine_class mutated Class::module_name on {class_name}"
        );
        debug_assert_eq!(
            self.origin, after.origin,
            "redefine_class mutated Class::origin on {class_name}"
        );
        debug_assert_eq!(
            self.has_finalizer, after.has_finalizer,
            "redefine_class mutated Class::has_finalizer on {class_name}"
        );
        debug_assert_eq!(
            self.code_source_present, after.code_source_present,
            "redefine_class mutated Class::code_source on {class_name}"
        );
        debug_assert_eq!(
            self.array_info_present, after.array_info_present,
            "redefine_class mutated Class::array_info on {class_name}"
        );
    }
}

