// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread, lock-free **resolved constant pool** for the interpreter.
//!
//! # What this is for
//!
//! A constant-pool reference is resolved once per `(referencing class, cp
//! index)` pair and never changes after that — JVMS §5.4.3 makes resolution
//! idempotent. CratonVM already memoizes the *answers* in
//! `SharedVm::resolution_cache`, but reaching that map costs an
//! `OrderedPlRwLock` read and a hash probe, and the interpreter's field path
//! additionally **re-derives the field-owning class from its name on every
//! access, cache hit included** — two `String` allocations, two more
//! `class_manager` read acquisitions and a full `resolve_class_loader_aware`.
//!
//! On Tomcat's BCEL annotation scan (~1.2M instance field accesses and ~2.0M
//! bytecodes; see `docs/known-issues/tomcat/`) that revalidation plus the lock
//! traffic around it is a double-digit share of profile. This module is the
//! side table that removes it: a direct-mapped array per thread, tagged with
//! the full key and validated by two global epochs, so a warm site is answered
//! with an array index, two integer compares and two atomic loads.
//!
//! # The validity condition
//!
//! An entry stays correct exactly as long as all three of:
//!
//! 1. **The class-name → `ClassId` mapping is unchanged.**
//!    [`cratonvm_classloading::class_definition_epoch`] is the counter whose
//!    documented purpose is this question — bumped by every `loaded_classes`
//!    insert, remove and bulk retain. It is also bumped by every superclass /
//!    interface EDGE change (`ClassStore::{add over a forward edge,
//!    set_superclass, set_interfaces, remove}`), which is what keeps the cast
//!    sites' positive / negative receiver memos and catch-row refusals — all
//!    subtype verdicts — exact when a hierarchy changes under unchanged ids.
//! 2. **No cached resolution has been invalidated.**
//!    [`super::constants::resolution_epoch`] covers the rest of the input set:
//!    the per-loader initiating-resolution memo (which changes what a loader
//!    answers *without* touching `loaded_classes`), and classloading's
//!    resolution-invalidate hook. Two of that hook's four firing sites —
//!    `upgrade_synthetic_class` and `recompute_subclass_layouts` — move a
//!    class's **field layout in place** while keeping its `ClassId`, its name
//!    and the redefine latch, so this epoch is their only signal.
//! 3. **No class has been redefined in place since the entry was filled.**
//!    A redefinition replaces a class's constant pool under an unchanged
//!    `ClassId`, so every `(class, cp index)` key of it may now name another
//!    member. Until interpreter round i1 wave 17 the tables answered this with
//!    the process-wide `cratonvm_classloading::any_class_redefined` latch,
//!    which disabled every table of every thread for the rest of the process
//!    on the first redefine — the first Mockito inline mock or coverage
//!    retransform cost the whole run its site caches. It is now condition 2
//!    again: every redefinition advances the resolution epoch (the
//!    invalidate hook, and `NativeContextImpl::redefine_class_with` before
//!    the class is replaced and after the shared memo is swept), so an entry
//!    filled before it misses and one filled after it resolved the new
//!    class. [`SITE_CACHES_SURVIVE_REDEFINITION`] is the kill switch back to
//!    the latch.
//!
//! The checks are single atomic loads, on every hit and every fill. Any
//! change wipes the table; the caller then falls through to the authoritative
//! slow path, which re-fills.
//!
//! # Whose counters (interpreter round i1 wave 20, lane L2)
//!
//! All three conditions are events of ONE VM, but the counters above are
//! process-wide, so another VM's class loading retired this VM's entries.
//! A fill that names its VM ([`SiteCache::epochs_for`]) is tagged with the
//! VM's copies instead — the [`cratonvm_classloading::StoreEpochs`] slot of
//! its class store, which moves on exactly the same events of that store —
//! and records the slot in its tag, so the hit reads the same two cells it
//! was filled against. [`SiteCache::epochs_now`] keeps the process counters
//! for a caller that cannot name a VM; they move on every VM's events, so
//! such an entry is conservative, never stale.
//!
//! # Why one generic table
//!
//! The field and method arms have the same key, the same validity condition and
//! the same failure mode — a wrong answer that is *plausible* rather than
//! detectable. Sharing [`SiteCache`] means that argument is implemented and
//! audited once. Only what is stored differs.

use cratonvm_types::ClassId;

/// Kill switch for condition 3 of the module docs (interpreter round i1 wave
/// 17, stage 2a of `i14-L3-proposal-class-scoped-redefinition-invalidation`).
/// `true`: a redefinition retires the entries filled before it through the
/// resolution epoch, and the tables keep serving afterwards. `false`: the
/// historical latch — the first redefinition anywhere in the process turns
/// every table off for good. A `const`, so the unused arm costs nothing.
pub(crate) const SITE_CACHES_SURVIVE_REDEFINITION: bool = true;

/// Whether the tables are latched off by a redefinition: never, unless the
/// kill switch restores the latch.
#[inline(always)]
fn latched_off_by_redefinition() -> bool {
    !SITE_CACHES_SURVIVE_REDEFINITION && cratonvm_classloading::any_class_redefined()
}

/// Default number of direct-mapped slots. A power of two so the index is a mask.
///
/// 1024 was chosen against BCEL's class parser, which touches on the order of a
/// hundred distinct sites in its hot loop and hits **99.9%** there
/// (`hit=1329432 miss=1234` over one annotation scan).
///
/// **That number does not generalise.** A Spring Boot unit-test class measured
/// `hit=184126 miss=166035` — a **53%** hit rate, with nearly every miss
/// causing a fill, i.e. the table thrashing rather than warming. Broad
/// application code touches far more distinct field sites than a narrow hot
/// loop does, so the right size is a workload question, and
/// [`CRATONVM_JIT=field-site-slots`](field_site_slots) exists to answer it with
/// hit-rate data instead of a guess. Hit rate is load-independent, which makes
/// it measurable on a busy shared host where timings are not.
const DEFAULT_SLOTS: usize = 1024;

/// Slot count for new tables — `CRATONVM_JIT_FIELD_SITE_SLOTS`, rounded UP to a
/// power of two and clamped to `[64, 65536]`. Read once and cached.
///
/// The clamp is not decoration: the index is a mask, so a non-power-of-two would
/// silently address only part of the table, and an unbounded value would let one
/// env var allocate arbitrary per-thread memory.
fn field_site_slots() -> usize {
    static SLOTS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SLOTS.get_or_init(|| {
        let requested = cratonvm_types::flags::runtime_var("CRATONVM_JIT_FIELD_SITE_SLOTS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_SLOTS)
            .clamp(64, 65536);
        // Round UP to a power of two so the mask covers every allocated slot.
        requested.next_power_of_two().min(65536)
    })
}

struct Site<T> {
    class_id: ClassId,
    cp_index: u16,
    /// The two epochs as of the resolution that produced `value`. Held
    /// **per entry** rather than once for the whole table.
    ///
    /// The obvious design — one epoch pair on the table, wiped when either
    /// moves — is a performance trap, and a measured one. `class_definition_epoch`
    /// advances on *every class definition*, so during VM start-up and any
    /// class-loading burst it moves constantly; a table-wide wipe then walks all
    /// [`SLOTS`] entries on nearly every field access. Six regression vectors
    /// that never reach steady state (~500 ms each, dominated by boot) were
    /// uniformly SLOWER with the cache on, which is what exposed it.
    ///
    /// Per-entry epochs make an epoch change cost nothing: stale entries simply
    /// miss, one at a time, and are replaced in place. The comparison is two
    /// `u64`s already on the same cache line as the tag.
    epochs: (u64, u64),
    value: T,
}

/// The pair of counters that tags a [`SiteCache`] entry (condition 1 and 2
/// of the module docs; condition 3, a redefinition, moves the resolution
/// epoch both pairs carry).
pub trait SiteEpochs {
    /// Snapshot both process-wide counters.
    fn now() -> (u64, u64);
    /// Snapshot both counters of one class store's slot (untagged; see
    /// [`SiteCache::epochs_for`]).
    fn in_store(epochs: &cratonvm_classloading::StoreEpochs) -> (u64, u64);
}

/// Bit 63 of a stamp's second word: the stamp holds a
/// [`cratonvm_classloading::StoreEpochs`] slot's counters, not the process
/// ones, and bits 56..62 name the slot (wave 20, lane L2). The counters stay
/// far below 2^56, so the process stamp never has the bit.
const STORE_TAG: u64 = 1 << 63;
const STORE_SLOT_SHIFT: u32 = 56;
const STORE_VALUE_MASK: u64 = (1 << STORE_SLOT_SHIFT) - 1;

/// The current stamp of the counters `tag_word` (the second word of a stamp)
/// was taken from: the process pair for an untagged word, else that slot's
/// pair with the same tag. Equal to the stamp exactly when none of its
/// counters has moved.
#[inline(always)]
fn stamp_now<E: SiteEpochs>(tag_word: u64) -> (u64, u64) {
    if tag_word & STORE_TAG == 0 {
        return E::now();
    }
    let slot = ((tag_word & !STORE_TAG) >> STORE_SLOT_SHIFT) as usize;
    let (first, resolution) = E::in_store(cratonvm_classloading::store_epochs_at(slot));
    (
        first,
        (tag_word & !STORE_VALUE_MASK) | (resolution & STORE_VALUE_MASK),
    )
}

/// `(class_definition_epoch, resolution_epoch)` — the default, and the only
/// admissible choice for a table that memoises a SUBTYPE VERDICT or any other
/// fact of the hierarchy edges (the field and method arms, the
/// interface-selection memo): the definition epoch is what moves on an edge
/// change. The cast sites' receiver memos carry that epoch per memo instead
/// (`CastSite::memo_epoch`, i7-L2), so their table can use [`NameEpochs`].
pub struct DefinitionEpochs;

impl SiteEpochs for DefinitionEpochs {
    #[inline]
    fn now() -> (u64, u64) {
        (
            cratonvm_classloading::class_definition_epoch(),
            super::constants::resolution_epoch(),
        )
    }

    #[inline(always)]
    fn in_store(epochs: &cratonvm_classloading::StoreEpochs) -> (u64, u64) {
        (epochs.class_definition_epoch(), epochs.resolution_epoch())
    }
}

/// `(class_name_generation, resolution_epoch)` — for a table whose values are
/// RESOLUTION facts only (a name's `ClassId`, its layout width, a constant
/// pool value), with no subtype verdict among them (i6-L2, stage 1a of
/// `i1-L2-proposal-per-class-resolved-constant-pool`).
/// `cratonvm_classloading::class_name_generation` moves only when an EXISTING
/// name answer can change (a second definition of a known name, a removal,
/// an unload), not on the definition of a brand-new name, so such a table
/// keeps its entries through a class-loading burst — and a site whose own
/// resolution loaded its class is filled on that first execution, where the
/// definition epoch moved under it and dropped the fill. In-place layout
/// replacement and the initiating-loader memo move the resolution epoch.
pub struct NameEpochs;

impl SiteEpochs for NameEpochs {
    #[inline]
    fn now() -> (u64, u64) {
        (
            cratonvm_classloading::class_name_generation(),
            super::constants::resolution_epoch(),
        )
    }

    #[inline(always)]
    fn in_store(epochs: &cratonvm_classloading::StoreEpochs) -> (u64, u64) {
        (epochs.class_name_generation(), epochs.resolution_epoch())
    }
}

/// Direct-mapped, epoch-validated per-thread cache keyed on
/// `(referencing class, constant-pool index)`. See the module docs for the
/// validity argument — **read it before adding a `put` call site**, because
/// what makes an entry admissible is a property of the resolution that produced
/// it, not of this type. `E` picks the counters that tag an entry; see
/// [`DefinitionEpochs`] and [`NameEpochs`] for which one a table may use.
pub struct SiteCache<T, E: SiteEpochs = DefinitionEpochs> {
    /// Lazily allocated. `None` in a slot means empty.
    slots: Option<Box<[Option<Site<T>>]>>,
    /// `slots.len() - 1`, captured when the table is allocated so the hot path
    /// masks without re-reading the flag. Zero while unallocated.
    mask: usize,
    _epochs: std::marker::PhantomData<E>,
}

impl<T, E: SiteEpochs> SiteCache<T, E> {
    pub fn new() -> Self {
        Self {
            slots: None,
            mask: 0,
            _epochs: std::marker::PhantomData,
        }
    }

    /// Direct-mapped slot index. Fibonacci-hash the pair and take the HIGH
    /// bits, so that constant-pool indices — which cluster in a narrow range
    /// within any one class — do not alias across classes.
    #[inline]
    fn slot_of(&self, class_id: ClassId, cp_index: u16) -> usize {
        let key = ((class_id.as_u32() as u64) << 16) | cp_index as u64;
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize & self.mask
    }

    /// The two counters a caller must snapshot **before** resolving, and hand
    /// back to [`Self::put`] — the PROCESS-wide pair, which moves on every
    /// VM's events. For a caller that cannot name its VM: only tests do;
    /// the interpreter uses [`Self::epochs_for`].
    #[cfg(test)]
    #[inline]
    pub fn epochs_now() -> (u64, u64) {
        E::now()
    }

    /// [`Self::epochs_now`] for a caller running in `shared`: the pair of
    /// `shared`'s class-store slot, tagged with the slot, so the entry it
    /// fills is retired by this VM's events only (wave 20, lane L2).
    #[inline]
    pub fn epochs_for(shared: &crate::vm::SharedVm) -> (u64, u64) {
        let slot = cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain);
        stamp_now::<E>(STORE_TAG | ((slot as u64) << STORE_SLOT_SHIFT))
    }

    /// Look up a site. Returns a borrow so the caller clones only what it needs.
    ///
    /// Every check here is O(1): one array index, and four integer compares
    /// against the entry's own tag and epochs (plus the redefine latch, when
    /// [`SITE_CACHES_SURVIVE_REDEFINITION`] is off). Nothing scans or clears
    /// the table — see [`Site::epochs`] for why that matters.
    #[inline]
    pub fn get(&mut self, class_id: ClassId, cp_index: u16) -> Option<&T> {
        if latched_off_by_redefinition() {
            return None;
        }
        let idx = self.slot_of(class_id, cp_index);
        let site = self.slots.as_ref()?[idx].as_ref()?;
        // BOTH halves of the key, always. A direct-mapped table with a partial
        // tag check answers one site with another site's value, and every
        // consumer here treats that answer as authoritative.
        if site.class_id != class_id || site.cp_index != cp_index {
            return None;
        }
        // Against the counters the entry was stamped from: its own tag says
        // which (the process pair, or one class store's).
        if site.epochs != stamp_now::<E>(site.epochs.1) {
            return None;
        }
        Some(&site.value)
    }

    /// [`Self::get`], mutably: the same key and validity checks. For a table
    /// whose owner moves a value out of a live entry for the duration of one
    /// call and puts it back afterwards (the `invokedynamic` table lends its
    /// `Arc` out rather than bumping a refcount every thread shares).
    #[inline]
    pub fn get_mut(&mut self, class_id: ClassId, cp_index: u16) -> Option<&mut T> {
        if latched_off_by_redefinition() {
            return None;
        }
        let idx = self.slot_of(class_id, cp_index);
        let site = self.slots.as_mut()?[idx].as_mut()?;
        if site.class_id != class_id || site.cp_index != cp_index {
            return None;
        }
        if site.epochs != stamp_now::<E>(site.epochs.1) {
            return None;
        }
        Some(&mut site.value)
    }

    /// Publish a resolved site.
    ///
    /// `epochs_at_entry` must come from [`Self::epochs_for`] (or
    /// [`Self::epochs_now`]) called **before** the resolution that produced
    /// `value`. If either counter has moved since,
    /// the answer may already describe superseded state, so the insert is
    /// dropped rather than published: this thread would otherwise serve a stale
    /// answer until the epochs next moved. Snapshotting at insert time instead
    /// would swallow exactly that race.
    #[inline]
    pub fn put(&mut self, class_id: ClassId, cp_index: u16, epochs_at_entry: (u64, u64), value: T) {
        if latched_off_by_redefinition() {
            // Latched off for the rest of the process — release the memory too.
            self.slots = None;
            return;
        }
        if epochs_at_entry != stamp_now::<E>(epochs_at_entry.1) {
            return;
        }
        if self.slots.is_none() {
            let n = field_site_slots();
            self.slots = Some((0..n).map(|_| None).collect());
            self.mask = n - 1;
        }
        let idx = self.slot_of(class_id, cp_index);
        let slots = match self.slots.as_mut() {
            Some(s) => s,
            None => return,
        };
        slots[idx] = Some(Site {
            class_id,
            cp_index,
            epochs: epochs_at_entry,
            value,
        });
    }

    /// Drop every entry, keeping the allocation.
    pub fn clear(&mut self) {
        if let Some(slots) = self.slots.as_mut() {
            for slot in slots.iter_mut() {
                *slot = None;
            }
        }
    }

    /// Live entry count — diagnostics only.
    pub fn live(&self) -> usize {
        self.slots
            .as_ref()
            .map(|s| s.iter().filter(|e| e.is_some()).count())
            .unwrap_or(0)
    }
}

impl<T, E: SiteEpochs> Default for SiteCache<T, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, E: SiteEpochs> std::fmt::Debug for SiteCache<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SiteCache({} slots filled)", self.live())
    }
}

/// What the interpreter's argument-popping helpers actually consume out of a
/// resolved method reference.
///
/// `resolve_method_ref` returns `(class_name, method_name, descriptor,
/// num_params)` — four values, three of them `Arc<str>`, behind a
/// `resolution_cache` read lock and a hash probe. On the inline-cache HIT path
/// `pop_coerced_invoke_args_virtual` / `_static` call it for the last two and
/// discard the rest, so every invoke through a cached native or intrinsic pays
/// a lock, a probe and three `Arc` clone/drop pairs for one string and one
/// integer. That drop glue is a measurable share of the annotation-scan profile
/// in its own right.
#[derive(Clone)]
pub struct MethodSiteInfo {
    pub descriptor: std::sync::Arc<str>,
    pub num_params: u16,
}

/// Everything the interpreter's `new` (0xbb) opcode needs after resolution.
///
/// `Instruction::New` re-derived all of this on EVERY execution: a
/// `class_manager` read plus `get_class_name(cp_index).to_string()` (a fresh
/// `String` per allocation), a full `resolve_class_loader_aware`, a second
/// `class_manager` read for `check_class_access`, an
/// `ensure_class_initialized_shared` (a third), and a fourth for
/// `num_total_fields`. None of it can change for a site that this table admits
/// — see [`ClassSiteCache`] for which sites those are.
#[derive(Clone, Copy)]
pub struct ResolvedNewSite {
    /// The class `new` allocates.
    pub class_id: ClassId,
    /// `Class::num_total_fields` as of the fill. A layout replacement moves it
    /// — and bumps `resolution_epoch`, which is one half of the entry's tag.
    pub num_fields: u32,
}

/// Per-thread resolved `new`-site cache.
///
/// # Which sites are admissible
///
/// Those whose referencing class has **no loader namespace**: no entry in
/// the defining-loader side table, and a class-manager loader id that is not
/// `UserDefined`. For such a class `resolve_class_loader_aware` reduces to the
/// global name → `ClassId` mapping plus the resolution memo, which is exactly
/// what this module's two epochs cover. The field arm draws the same line, and
/// puts its loader-sensitive half behind a separate flag for the same reason.
///
/// Since interpreter round i1 wave 24 (lane L5) also a loader-namespaced
/// class's site whose answer is a JDK-global name or the one its loader has
/// itself recorded (`opcodes::site_fill_admitted`, which states why the two
/// epochs cover every change of that record). This table and the cast table
/// share the rule.
///
/// That property is **immutable per class**, not merely current: a defining
/// loader is fixed when the class is defined, so an app- or bootstrap-defined
/// referencing class can never later acquire one. Checking it at FILL time is
/// therefore a complete guard, and the hit path needs no re-check.
///
/// # What a hit is allowed to skip
///
/// The access check (JVMS §5.4.4) is a function of the (accessor, target) pair
/// alone, and neither class's identity or modifiers change once defined — so a
/// site that passed once passes forever.
///
/// Class initialization is monotonic (JVMS §5.5): an entry is only filled once
/// the class is `Initialized` (`is_class_initialized_via_manager`), not merely
/// after `ensure_class_initialized_shared` returned `Ok` -- which it also does
/// for a class this thread is still initializing, whose `<clinit>` may yet
/// fail -- so a hit cannot be the first touch. This is the one skip worth naming, because
/// `ensure_class_initialized_shared`'s own "fast path" still takes a
/// `class_manager` read lock — the hit would otherwise keep paying a lock for a
/// question already answered. A redefinition moves the resolution epoch
/// (and does not reset a class's initialization state) and class unloading
/// bumps `class_name_generation`, so neither can strand an entry describing
/// an uninitialized class.
///
/// # Tagged with [`NameEpochs`] (i6-L2)
///
/// Every value here is a resolution fact: the id a name resolves to, its
/// `num_total_fields` (moved in place only with the resolution epoch) and a
/// monotonic initialization state. No subtype verdict, so the definition of an
/// unrelated class does not invalidate it.
pub type ClassSiteCache = SiteCache<ResolvedNewSite, NameEpochs>;

/// Per-thread resolved-field sites. Stores the whole `ResolvedField` — it is
/// plain data (ids, an index and four flag bytes), so a hit clones no `Arc`.
pub type FieldSiteCache = SiteCache<cratonvm_classloading::resolution::ResolvedField>;

/// One quickened instance-field site (see `JvmThread::fast_field_sites`).
///
/// Everything the `getfield` / `putfield` fast arms need to touch the field
/// of a receiver whose header matches `(receiver_class_id, num_slots)`
/// without resolving, retargeting or looking up a layout. A receiver whose
/// header does not match falls back to the full handler, which refills.
#[derive(Clone, Copy, Debug)]
pub struct FastFieldSite {
    /// `ObjectHeader::class_id` the layout below was resolved for.
    pub receiver_class_id: cratonvm_types::ClassId,
    /// `ObjectHeader::num_slots()` of that receiver; the compact layout
    /// registry is keyed by `(class_id, field_count)`.
    pub num_slots: u32,
    /// ABSOLUTE byte offset of the field from the object base: the layout's
    /// own displacement for a compact body (it already includes the 8-byte
    /// header), `HEADER_SIZE + field_index * SLOT_SIZE` for a legacy one.
    pub offset: u32,
    /// Storage kind the compact layout assigned to the field, or `None` when
    /// the receiver has a **legacy** body — one 16-byte tagged `Value` cell
    /// per field, which is what `ZgcRealHeap::try_alloc_object` (the TLAB path
    /// the interpreter allocates through) produces. This doubles as the
    /// body-shape discriminant the fast arms re-check against the receiver's
    /// `GC_FLAG_COMPACT`.
    pub storage: Option<cratonvm_types::FieldStorageKind>,
    /// `ResolvedField::field_index`, for the JVMTI watch check and the
    /// slow-path barrier calls that take a slot index.
    pub field_index: u32,
    /// `ResolvedField::desc_byte` — the legacy arm converts the cell's
    /// `Value` by descriptor exactly as `op_getfield` / `op_putfield` do.
    pub desc_byte: u8,
    /// `ResolvedField::is_reference`, for the same reason.
    pub is_reference: bool,
    /// Which quickened `putfield`s may use this site (JVMS §6.5 final-field
    /// rule): [`Self::PUT_ANY`], [`Self::PUT_INIT_ONLY`] or
    /// [`Self::PUT_NEVER`]. The site is keyed by `(class, cp index)`, not by
    /// method, and a `getfield` fills it too, so the slow handler's check
    /// alone would be bypassed by the next store through the same entry.
    pub final_put: u8,
}

impl FastFieldSite {
    /// Not a final field: every `putfield` through the site may take it.
    pub const PUT_ANY: u8 = 0;
    /// A final field of the site's own class: only a store from an `<init>`
    /// frame may take the fast arm (for a class file below version 53 the
    /// other stores are legal too, and simply run the full handler).
    pub const PUT_INIT_ONLY: u8 = 1;
    /// A final field of another class: no store may take the fast arm; the
    /// full handler throws `IllegalAccessError`.
    pub const PUT_NEVER: u8 = 2;
}

pub type FastFieldSiteCache = SiteCache<FastFieldSite>;

/// Per-thread resolved-method sites; see [`MethodSiteInfo`].
pub type MethodSiteCache = SiteCache<MethodSiteInfo>;

/// Per-thread resolved **cast** sites — the target `ClassId` of a `checkcast`
/// or `instanceof`.
///
/// # Why this is not [`ClassSiteCache`]
///
/// It stores less and it would be tempting to share the `new` table, since both
/// map `(referencing class, cp index)` to a resolved class. **They must not
/// share it.** The same `CONSTANT_Class` entry can be referenced by a `new` and
/// by a `checkcast` in one class, so one table would let a `checkcast` fill
/// answer a `new` — and [`ClassSiteCache`]'s hit path deliberately skips the
/// initialization check on the grounds that a fill only happens after
/// `ensure_class_initialized_shared` returned `Ok`. A `checkcast` must NOT
/// initialize its target (JVMS §6.5 `checkcast` performs resolution, not
/// initialization), so a `checkcast` fill cannot carry that guarantee, and a
/// `new` served from one would allocate an uninitialized class.
///
/// Separate tables keep each cache's precondition its own.
///
/// # What a hit is allowed to answer
///
/// The resolution, and — through [`CastSite::positive_receiver`] — the one
/// assignability verdict `is_subclass_of` last returned `true` for at this
/// site. A hit removes the `String` for the class name, the loader-aware
/// `resolve_class_loader_aware`, and one of the two `class_manager` read
/// acquisitions; a hit whose receiver class matches the memo removes the
/// remaining read acquisition and the hierarchy walk as well.
///
/// A hit is taken only when the receiver is not an array (arrays answer through
/// descriptor-based assignability, which needs the name) and only when
/// `is_subclass_of` says yes. A negative `is_subclass_of` falls through to the
/// full path, because the five fail-open fallbacks after it
/// (`loader_aware_name_assignable`, `synthetic_implements`,
/// `proxy_instance_satisfies_target`, …) are name-based. The one exception is
/// [`CastSite::negative_receivers`]: an `instanceof` refusal the full path
/// already produced for this receiver CLASS, stored only when every fallback's
/// answer is a function of the class (see
/// `opcodes::receiver_class_determines_cast_verdict`).
///
/// `anewarray` reads and fills this table too: its component is a
/// `CONSTANT_Class` resolution with no initialization, i.e. the same answer a
/// cast site records for the same (class, cp index).
///
/// Array names are admitted since i11-L2: `checkcast` / `instanceof` resolve
/// their target for every non-null receiver (JVMS §6.5), and an array name
/// records the array class, whose `array_info` names the component its own
/// loader produced. The full path of either opcode reuses a hit's `target`
/// instead of resolving again. The two class receiver memos are non-array;
/// an ARRAY receiver has its own, [`CastSite::positive_array`] (i16-L4).
///
/// # Two tags (i7-L2)
///
/// The entry is tagged with [`NameEpochs`]: `target` is a resolution fact,
/// exactly [`ClassSiteCache`]'s (same admission rule), so the definition of
/// an unrelated class — every class-loading burst — no longer drops it. The
/// four receiver memos are SUBTYPE verdicts, which the definition epoch
/// covers; they carry it themselves, in [`CastSite::memo_epoch`], and a
/// reader sees them only through [`CastSite::observed`] /
/// [`CastSite::observed_at`], which clear them once that epoch has moved. A
/// memo is therefore answered only while the name generation, the resolution
/// epoch AND the definition epoch are all unchanged — a strict subset of the
/// states the old single `DefinitionEpochs` tag answered in.
pub type CastSiteCache = SiteCache<CastSite, NameEpochs>;

/// The definition epoch as a cast-site memo records it: snapshot it BEFORE
/// computing the verdict a memo will hold (see [`CastSite::observed_at`]).
/// The process-wide counter, for tests; the interpreter uses
/// [`cast_memo_epoch_in`].
#[cfg(test)]
#[inline]
pub fn cast_memo_epoch_now() -> u64 {
    cratonvm_classloading::class_definition_epoch()
}

/// [`cast_memo_epoch_now`] for a reader running in `shared`: its class
/// store's definition epoch, tagged with the slot the way a
/// [`SiteCache::epochs_for`] stamp is, so it never equals a process-wide
/// snapshot and a memo proved under one is never read under the other
/// (wave 20, lane L2).
#[inline]
pub fn cast_memo_epoch_in(shared: &crate::vm::SharedVm) -> u64 {
    let slot = cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain);
    let epoch = cratonvm_classloading::store_epochs_at(slot).class_definition_epoch();
    STORE_TAG | ((slot as u64) << STORE_SLOT_SHIFT) | (epoch & STORE_VALUE_MASK)
}

/// One resolved cast site. See [`CastSiteCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CastSite {
    /// The `CONSTANT_Class` the site names, resolved loader-aware (for an
    /// array name, the array class).
    pub target: ClassId,
    /// The last NON-ARRAY receiver class `is_subclass_of(receiver, target)`
    /// proved assignable here — a monomorphic site answers from this with no
    /// `class_manager` lock. Only a class-hierarchy verdict is ever stored:
    /// the instance-dependent admissions (proxies, annotation proxies,
    /// unmodifiable-collection display classes) never write it. Covered by
    /// the entry's epochs exactly as `target` is; `IfaceSelectSiteCache`
    /// documents why that set is the precise one for a subtype relation.
    pub positive_receiver: Option<ClassId>,
    /// The last two NON-ARRAY receiver classes the FULL `instanceof` path
    /// answered `false` for here, newest in slot 0, each stored only when that
    /// answer is a function of the receiver's class alone
    /// (`opcodes::receiver_class_determines_cast_verdict`: not a lambda proxy,
    /// dynamic proxy, annotation proxy or unmodifiable stamp, whose admissions
    /// read the instance). Same epochs as `target`. Two slots (i20-L5) so a
    /// rung of an `instanceof` ladder whose receiver rotates through three
    /// classes answers both refusals from the memo; a fill pushes the new
    /// class into slot 0 and slot 0 into slot 1 ([`Self::with_negative`]).
    /// Written by `instanceof` only; `checkcast`'s refusal throws, and building
    /// the exception dominates it.
    pub negative_receivers: [Option<ClassId>; 2],
    /// The last exception class a CATCH ROW naming this `CONSTANT_Class`
    /// refused (`exception_dispatch::catch_row_verdict`). Kept apart from
    /// [`Self::negative_receivers`] because the two refusals are different
    /// predicates: a catch row matches on `is_subclass_of || by-name`, while
    /// `instanceof`'s full path admits more (proxies, loader-aware names), so
    /// neither refusal implies the other. A throwable is never an array and
    /// the catch verdict reads only the class hierarchy, so the entry's epochs
    /// cover it exactly as they cover `target`.
    pub negative_catch: Option<ClassId>,
    /// The last ARRAY receiver the full `checkcast` / `instanceof` path
    /// admitted here (`typecheck::array_receiver_cast_verdict`), keyed by
    /// what an array header CAN be tested against (i16-L4): see
    /// [`ArrayReceiverKey`]. Kept apart from [`Self::positive_receiver`]
    /// because a reference array's header id is its COMPONENT's, so the two
    /// keys would otherwise collide (`String[]` against a `String` memo).
    pub positive_array: Option<ArrayReceiverKey>,
    /// The `class_definition_epoch` the four memos above were proved under
    /// (i7-L2): a snapshot taken BEFORE their verdicts were computed. The
    /// entry's own tag is [`NameEpochs`], which does not move on an edge
    /// change, so this is what keeps the memos exact. Read the memos only
    /// from a site passed through [`Self::observed`] / [`Self::observed_at`].
    pub memo_epoch: u64,
}

/// What the array verdict of a cast site reads from an ARRAY receiver
/// (i16-L4): the header's element type and its class id (on a reference
/// array the COMPONENT's id, on a primitive array 0). Everything else
/// `typecheck::array_receiver_cast_verdict` reads is the site's target,
/// the VM's mode and the class table, which the entry's tags and
/// [`CastSite::memo_epoch`] cover — so two arrays with one key get one
/// verdict. Only ever compared against an array receiver's key.
pub type ArrayReceiverKey = (cratonvm_types::ArrayElementType, ClassId);

impl CastSite {
    /// A freshly resolved site with no receiver verdict yet.
    pub fn resolved(target: ClassId) -> Self {
        Self {
            target,
            positive_receiver: None,
            negative_receivers: [None; 2],
            negative_catch: None,
            positive_array: None,
            memo_epoch: 0,
        }
    }

    /// Does either negative slot name `receiver`?
    #[inline]
    pub fn refuses(&self, receiver: ClassId) -> bool {
        self.negative_receivers.contains(&Some(receiver))
    }

    /// This site with `receiver` recorded as the newest refusal: it takes
    /// slot 0 and the previous slot 0 moves to slot 1 (the oldest refusal
    /// is dropped). A receiver already in either slot leaves the site as it
    /// is, so a hit never reorders and two alternating refusals never evict
    /// each other.
    #[inline]
    pub fn with_negative(self, receiver: ClassId) -> Self {
        if self.refuses(receiver) {
            return self;
        }
        Self {
            negative_receivers: [Some(receiver), self.negative_receivers[0]],
            ..self
        }
    }

    /// This site as seen at definition epoch `snapshot`: the memos are kept
    /// only if they were proved under exactly that epoch, and `memo_epoch`
    /// becomes `snapshot`, so a memo written from the result (`..site`) is
    /// stamped with the epoch read before ITS verdict. Since the epoch only
    /// grows, a memo stamped with a snapshot that has since moved is simply
    /// dropped by the next reader.
    #[inline]
    pub fn observed_at(self, snapshot: u64) -> Self {
        if self.memo_epoch == snapshot {
            return self;
        }
        Self {
            target: self.target,
            positive_receiver: None,
            negative_receivers: [None; 2],
            negative_catch: None,
            positive_array: None,
            memo_epoch: snapshot,
        }
    }

    /// [`Self::observed_at`] the current definition epoch — for a reader
    /// that computes any verdict it memoises AFTER this call. Process-wide,
    /// for tests; the interpreter uses [`Self::observed_in`].
    #[cfg(test)]
    #[inline]
    pub fn observed(self) -> Self {
        self.observed_at(cast_memo_epoch_now())
    }

    /// [`Self::observed`] at `shared`'s own definition epoch
    /// ([`cast_memo_epoch_in`]).
    #[inline]
    pub fn observed_in(self, shared: &crate::vm::SharedVm) -> Self {
        self.observed_at(cast_memo_epoch_in(shared))
    }
}

/// Per-thread memo for the **interface receiver-selection re-check**.
///
/// # What it removes
///
/// `execute_invokevirtual_cached`'s `VirtualBytecode` arm re-verifies, on every
/// `invokeinterface` that hits the inline cache, that receiver-rooted
/// maximally-specific resolution from the *actual* receiver class still selects
/// the cached method's declaring class. The check exists for a real bug — a
/// parent-interface default that stayed cached after it masked a covariant
/// bridge on a receiver subinterface — but it was being answered with a
/// `class_manager` read lock plus a full `find_method_recursive` hierarchy walk,
/// **per call**.
///
/// `invokeinterface` and `invokevirtual` reach the same dispatcher and differ in
/// exactly this block, which makes the cost directly attributable. Measured
/// (`probes/Dispatch.java`, `--nojit`, min-of-7, arms interleaved):
/// interface-over-virtual was **114 ns** on CratonVM against **3.4 ns** on
/// HotSpot's template interpreter.
///
/// # What is stored, and why the value is a pair
///
/// Key: the call site, `(caller class, cp index)`. Value: the
/// `(receiver class, selected declaring class)` pair the walk *verified*.
///
/// The key alone is not enough. One interface call site can see several
/// concrete receiver classes, and the answer is a property of the receiver, not
/// of the site. Storing the verified pair and comparing both halves on a hit
/// means a site that rotates receivers simply misses and re-walks — today's
/// behaviour, no worse — while a monomorphic site (the overwhelming majority)
/// answers from an array index and two integer compares.
///
/// # Validity
///
/// Exactly [`SiteCache`]'s: `class_definition_epoch` and `resolution_epoch`.
/// That set is not merely sufficient here, it is the precise one — a receiver
/// class's superclass and superinterface chain is fixed at load time, so the
/// walk's answer can only move when a class is redefined in place or when
/// `upgrade_synthetic_class` / `recompute_subclass_layouts` rewrites a class
/// under an unchanged `ClassId` (both move the resolution epoch, which the
/// invalidate hook bumps and which nothing else in the invoke-cache path
/// observes; a redefinition latched the table off instead until interpreter
/// round i1 wave 17). Any other superclass / interface
/// edge change — the deferred interface resolution of a freshly minted stub,
/// boot-time compat wiring, an unload — goes through a `ClassStore` edge
/// mutator, which moves `class_definition_epoch` (round i1 wave 5).
pub type IfaceSelectSiteCache = SiteCache<(ClassId, ClassId)>;

/// The value an `ldc` / `ldc_w` of a `CONSTANT_Integer` / `CONSTANT_Float`, or
/// an `ldc2_w` of a `CONSTANT_Long` / `CONSTANT_Double`, pushes. Typed, so the
/// category-2 push stays tagged (`push_long` / `push_double`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrimitiveConstant {
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
}

/// Per-thread numeric-constant sites — see `constants::execute_ldc` and
/// `constants::execute_ldc2w`.
///
/// The value is a pure function of the referencing class's constant pool,
/// which changes under an unchanged `ClassId` only through redefinition and
/// `upgrade_synthetic_class` (both move the resolution epoch), so
/// [`SiteCache`]'s validity condition over-covers it. What a hit removes is,
/// for `ldc2_w`, the `class_manager` read lock and `get_class` that every
/// execution paid, and for `ldc`, the `resolution_cache` read lock and hash
/// probe of the shared record. Reference results (`String`, `Class`,
/// `MethodType`, ...) are never stored here — a per-thread table is not a GC
/// root — and neither is a `CONSTANT_Dynamic`, whose record is the condy map.
/// A constant-pool index has exactly one tag, so an `ldc` and an `ldc2_w`
/// can never read each other's entry.
///
/// Tagged with [`NameEpochs`] (i6-L2): the value is not even a resolution,
/// and the name generation still moves on the unload that could let a
/// reused `ClassId` alias an entry.
pub type PrimitiveConstantSiteCache = SiteCache<PrimitiveConstant, NameEpochs>;

/// `CRATONVM_DBG=field-site` — prove the site caches are actually firing before
/// anyone times them.
///
/// This exists because the previous attempt at this optimization gated on a
/// predicate that was unconditionally true, so the lever never fired and its
/// A/B measured a wash — a null result that said nothing about the idea. An
/// inert gate shows up here immediately as `hit=0`, which is a fact about the
/// build rather than about the host's load.
pub mod site_stats {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    pub const FIELD_HIT: usize = 0;
    pub const FIELD_MISS: usize = 1;
    pub const FIELD_FILL: usize = 2;
    pub const FIELD_REJECT_LOADER: usize = 3;
    pub const METHOD_HIT: usize = 4;
    pub const METHOD_MISS: usize = 5;
    pub const METHOD_FILL: usize = 6;
    pub const NEW_HIT: usize = 7;
    pub const NEW_MISS: usize = 8;
    pub const NEW_FILL: usize = 9;
    /// A `new` site refused a fill because its referencing class has a loader
    /// namespace. A workload whose whole `new` traffic lands here is one the
    /// cache cannot help, and saying so is the difference between "measured no
    /// effect" and "never fired".
    pub const NEW_REJECT_LOADER: usize = 10;
    pub const CAST_HIT: usize = 11;
    pub const CAST_MISS: usize = 12;
    pub const CAST_FILL: usize = 13;
    /// A cast site refused a FILL: a loader-namespaced referencing class, whose
    /// answer is not a property of the (class, index) pair alone (array
    /// targets fill since i11-L2). Same reason the `new` counterpart exists —
    /// it separates "measured no effect" from "never fired".
    pub const CAST_REJECT_LOADER: usize = 14;
    /// A cast site HIT whose answer could not be used: `is_subclass_of` said no
    /// (or the receiver was an array), so the full path ran anyway — with the
    /// hit's resolution, since i11-L2, not a second one.
    ///
    /// This is deliberately NOT counted as a loader rejection. It dominates any
    /// workload that asks negative `instanceof` questions — a torture probe
    /// here reports 26,446 of these against 34 fills — and folding the two
    /// together would read as "the cache is being refused for loader reasons"
    /// when what is actually happening is that the cache is answering, and the
    /// answer is `false`. A counter whose name implies the wrong cause is the
    /// same defect as a counter that does not fire.
    pub const CAST_UNUSABLE: usize = 15;
    /// An `ldc` answered from the recorded-resolution store — the whole
    /// instruction, before the class_manager lock.
    pub const LDC_HIT: usize = 16;
    /// An `ldc` that had to resolve. Non-zero with `LDC_HIT` at zero would
    /// mean the store is never being written, which is a different defect
    /// from a cache that is written and never read.
    pub const LDC_MISS: usize = 17;
    /// An `ldc` result written to the store. Every tag `ldc` can push records,
    /// including `Integer`/`Float`: an unrecorded tag would MISS the probe on
    /// every execution and then resolve anyway, so the probe would be pure
    /// added cost for it. `ldc2_w` does not probe or record here: its
    /// long/double constants live in the per-thread
    /// `PrimitiveConstantSiteCache` (which also answers `ldc`'s Integer/Float
    /// sites first; those hits count as `LDC_HIT` too) — see
    /// `constants::execute_ldc2w`.
    pub const LDC_FILL: usize = 18;
    /// The COMPILED `ldc` triple, kept separate from the interpreter's three
    /// above. Two populations, two switches
    /// (`CRATONVM_JIT_NO_LDC_CONST_CACHE` and
    /// `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE`), and the compiled one is the
    /// one that was re-deriving its constant every execution until 2026-08-20
    /// — folding them into one number would make "which route is answering
    /// from the record" unanswerable, which is the whole question.
    ///
    /// `jit_fill` non-zero with `jit_hit`/`jit_miss` at zero is the specific
    /// shape a kill switch that gates only the READ produces; it is why the
    /// compiled switch gates the write too.
    pub const JIT_LDC_HIT: usize = 19;
    pub const JIT_LDC_MISS: usize = 20;
    pub const JIT_LDC_FILL: usize = 21;
    /// The interface receiver-selection re-check, answered from the memo
    /// instead of a `class_manager` read lock plus a `find_method_recursive`
    /// hierarchy walk. See [`super::IfaceSelectSiteCache`].
    ///
    /// A `hit` here is one avoided lock acquisition. `hit` at zero with
    /// `miss` climbing on an interface-heavy workload means the call sites are
    /// polymorphic enough that the memo's monomorphic slot thrashes, which is
    /// a different finding from "the memo is not wired up".
    pub const IFACE_SELECT_HIT: usize = 22;
    pub const IFACE_SELECT_MISS: usize = 23;
    pub const IFACE_SELECT_FILL: usize = 24;
    /// The re-check was skipped outright because the receiver's own class is
    /// the cached method's declaring class, which makes receiver-rooted
    /// selection trivially agree. Counted apart from `IFACE_SELECT_HIT` so
    /// "the memo is carrying the workload" stays distinguishable from "the
    /// workload never needed the memo in the first place".
    pub const IFACE_SELECT_TRIVIAL: usize = 25;
    pub const FAST_GET_HIT: usize = 26;
    pub const FAST_GET_MISS: usize = 27;
    pub const FAST_GET_FILL: usize = 28;
    pub const FAST_PUT_HIT: usize = 29;
    pub const FAST_PUT_MISS: usize = 30;
    pub const FAST_PUT_FILL: usize = 31;
    pub const FAST_FIELD_UNUSABLE: usize = 32;
    pub const DOOR_STATIC_HIT: usize = 33;
    pub const DOOR_STATIC_MISS: usize = 34;
    pub const DOOR_SPECIAL_HIT: usize = 35;
    pub const DOOR_SPECIAL_MISS: usize = 36;
    /// The general dispatchers' frame install, one counter per path. These
    /// answer the question a timing arm cannot: whether the path being timed
    /// is the path being taken. `reuse` should dominate in any warm workload;
    /// `emplace` is the first call at each depth; `byvalue` is the kill switch
    /// and the shapes that reach neither.
    pub const INSTALL_REUSE: usize = 37;
    pub const INSTALL_EMPLACE: usize = 38;
    pub const INSTALL_BYVALUE: usize = 39;

    /// The two array-shaped quickened arms added 2026-09-05. Both answer a
    /// question no clock could: whether the arm ran at all. `arraylength`'s
    /// expected win is around one nanosecond, which is below what this host
    /// resolves, so without these a null A/B cannot be told apart from an arm
    /// that never fired — the failure mode the invoke page records twice.
    pub const ARRLEN_HIT: usize = 40;
    pub const ARRLEN_MISS: usize = 41;
    /// `aaload` served by `field_fast::array_load_ref` against declined to
    /// `VmHeap::get_array_element`. A decline is expected and correct for an
    /// armed load barrier, a process that has boxed, and any element word that
    /// is neither zero nor a plausible heap pointer.
    pub const REFARR_HIT: usize = 42;
    /// A decline the arm could never have served: the load barrier is armed or
    /// the process has boxed (`autobox::wrapper_exists`). Split out from the
    /// other two because it is a PROCESS-WIDE latch, not a property of this
    /// access — folding it in produced a census reading `miss=2000146` with no
    /// way to tell "declined 2 M times for 2 M different reasons" from
    /// "declined because a global says never".
    pub const REFARR_MISS_SCREEN: usize = 43;
    /// The two halves of the screen, separated because the first census said
    /// `miss_screen=2000146` and could not say WHICH process-wide latch was
    /// closed — the same fold-two-causes-into-one-counter defect the split
    /// above was meant to fix, one level down. Both are latches, but they are
    /// armed by completely different things and the fix differs accordingly.
    pub const REFARR_MISS_BARRIER: usize = 46;
    pub const REFARR_MISS_WRAPPER: usize = 47;
    /// A decline on this receiver's shape: not a reference array, or the index
    /// is out of bounds. The general path raises the AIOOBE.
    pub const REFARR_MISS_SHAPE: usize = 44;
    /// The element word is neither zero nor a plausible heap pointer, so the
    /// three-way cold decode owns it.
    pub const REFARR_MISS_WORD: usize = 45;

    /// Cached-native dispatch answered from the call site's own
    /// `DescriptorFacts` (`facts`) against the general helper that re-resolves
    /// the constant pool and allocates two `Vec`s (`resolve`), split by the
    /// two arms that serve it.
    ///
    /// The clock cannot tell "the facts path is no faster" from "the facts
    /// path never ran", and this page has shipped a change that measured
    /// nothing for the second reason twice — the field fast path's first
    /// version (`get hit=0 miss=1801267`) and the `invokespecial` door's
    /// (`special hit=0 miss=997`). Read this before reading a clock.
    pub const NATFACTS_STATIC: usize = 48;
    pub const NATFACTS_VIRTUAL: usize = 49;
    pub const NATFACTS_RESOLVE: usize = 50;
    /// A cached-native dispatch that went through the leaf funnel.
    pub const NATFACTS_LEAF: usize = 51;

    /// A cached-virtual PROMOTION that entered a compiled callee which
    /// DECLARES A LOCAL EXCEPTION TABLE, i.e. the population
    /// `CRATONVM_JIT_VIRTUAL_PROMOTE_HANDLER_CALLEE` admits.
    ///
    /// Without this the switch is unfalsifiable from the outside: a probe can
    /// print the right answers with the gate relaxed and prove nothing,
    /// because a callee that never compiled is never promoted either. This
    /// counter is what separates "the relaxed path is correct" from "the
    /// relaxed path never ran" — the failure mode this page has recorded
    /// twice (`fast-field: get hit=0`, `door: special hit=0`).
    pub const HANDLER_CALLEE_DIRECT: usize = 52;
    /// The same promotion for a callee with no handlers — the control, so the
    /// counter above can be read as a share rather than a bare count.
    pub const PLAIN_CALLEE_DIRECT: usize = 53;
    /// An `instanceof` answered `false` from the site's NEGATIVE receiver memo
    /// (`CastSite::negative_receivers`), counted apart from `CAST_HIT` so the
    /// memo's share of the traffic is readable — `neg_hit` at zero on a
    /// workload whose `unusable` climbs means the receivers are not
    /// class-determined or the sites are polymorphic.
    pub const CAST_NEG_HIT: usize = 54;
    /// The virtual fast door served a receiver from the site's poly entries
    /// (`execute_invokevirtual_fast_door`) instead of declining it as
    /// "site went polymorphic". Zero on a workload whose decline census shows
    /// that reason means the poly entries hold non-bytecode targets.
    pub const DOOR_POLY_HIT: usize = 55;
    /// A `checkcast` / `instanceof` of an ARRAY receiver answered from the
    /// site's array memo (`CastSite::positive_array`, i16-L4). A subset of
    /// `CAST_HIT`, which counts every hit; this one says how many of them
    /// were arrays, where before i16-L4 every array landed in `unusable`.
    pub const CAST_ARRAY_HIT: usize = 56;
    /// `typecheck::class_is_subtype` answered from the lock-free published
    /// supers closure (`PublishedSupers::verdict`), against the questions it
    /// had to take the `class_manager` read lock for (the closure was not
    /// published: first question about the class since its last edge change,
    /// an id past the directory, a full arena, the display switched off).
    /// The measurement that decides whether a further lock-free stage of
    /// `i1-L2-proposal-subtype-check-display` is worth building (i16-L4):
    /// `locked` should be a vanishing share on a warm workload.
    pub const SUBTYPE_PUBLISHED: usize = 57;
    pub const SUBTYPE_LOCKED: usize = 58;
    /// `aastore` served by `field_fast::array_store_ref` against declined to
    /// the dispatch loop's general arm (i1 wave 22, lane L7). A decline is
    /// expected for a null or out-of-range receiver, an armed load barrier, a
    /// non-reference value slot, and an element whose assignability no
    /// lock-free verdict confirms.
    pub const AASTORE_HIT: usize = 59;
    pub const AASTORE_MISS: usize = 60;

    const N: usize = 61;

    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static COUNTS: [AtomicU64; N] = [ZERO; N];

    pub fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FIELD_SITE").is_some()
        })
    }

    #[inline]
    pub fn bump(index: usize) {
        if !on() {
            return;
        }
        let n = COUNTS[index].fetch_add(1, Ordering::Relaxed) + 1;
        if n % 1_000_000 == 1 {
            report("progress");
        }
    }

    fn report(when: &str) {
        eprintln!(
            "[site-cache] {when} slots={} field: hit={} miss={} fill={} reject_loader={} | method: hit={} miss={} fill={} | new: hit={} miss={} fill={} reject_loader={} | cast: hit={} neg_hit={} array_hit={} miss={} fill={} reject_loader={} unusable={} | ldc: hit={} miss={} fill={} | jit-ldc: hit={} miss={} fill={} | iface-select: hit={} miss={} fill={} trivial={} | fast-field: get hit={} miss={} fill={} put hit={} miss={} fill={} unusable={} | door: static hit={} miss={} special hit={} miss={} virtual_poly hit={} | install: reuse={} emplace={} byvalue={} | arraylength: hit={} miss={} | aaload: hit={} miss_barrier={} miss_wrapper={} miss_shape={} miss_word={} | cached-native: facts_static={} facts_virtual={} resolve={} leaf={} | virtual-promote: handler-bearing={} plain={} | subtype: published={} locked={} | aastore: hit={} miss={}",
            super::field_site_slots(),
            COUNTS[FIELD_HIT].load(Ordering::Relaxed),
            COUNTS[FIELD_MISS].load(Ordering::Relaxed),
            COUNTS[FIELD_FILL].load(Ordering::Relaxed),
            COUNTS[FIELD_REJECT_LOADER].load(Ordering::Relaxed),
            COUNTS[METHOD_HIT].load(Ordering::Relaxed),
            COUNTS[METHOD_MISS].load(Ordering::Relaxed),
            COUNTS[METHOD_FILL].load(Ordering::Relaxed),
            COUNTS[NEW_HIT].load(Ordering::Relaxed),
            COUNTS[NEW_MISS].load(Ordering::Relaxed),
            COUNTS[NEW_FILL].load(Ordering::Relaxed),
            COUNTS[NEW_REJECT_LOADER].load(Ordering::Relaxed),
            COUNTS[CAST_HIT].load(Ordering::Relaxed),
            COUNTS[CAST_NEG_HIT].load(Ordering::Relaxed),
            COUNTS[CAST_ARRAY_HIT].load(Ordering::Relaxed),
            COUNTS[CAST_MISS].load(Ordering::Relaxed),
            COUNTS[CAST_FILL].load(Ordering::Relaxed),
            COUNTS[CAST_REJECT_LOADER].load(Ordering::Relaxed),
            COUNTS[CAST_UNUSABLE].load(Ordering::Relaxed),
            COUNTS[LDC_HIT].load(Ordering::Relaxed),
            COUNTS[LDC_MISS].load(Ordering::Relaxed),
            COUNTS[LDC_FILL].load(Ordering::Relaxed),
            COUNTS[JIT_LDC_HIT].load(Ordering::Relaxed),
            COUNTS[JIT_LDC_MISS].load(Ordering::Relaxed),
            COUNTS[JIT_LDC_FILL].load(Ordering::Relaxed),
            COUNTS[IFACE_SELECT_HIT].load(Ordering::Relaxed),
            COUNTS[IFACE_SELECT_MISS].load(Ordering::Relaxed),
            COUNTS[IFACE_SELECT_FILL].load(Ordering::Relaxed),
            COUNTS[IFACE_SELECT_TRIVIAL].load(Ordering::Relaxed),
            COUNTS[FAST_GET_HIT].load(Ordering::Relaxed),
            COUNTS[FAST_GET_MISS].load(Ordering::Relaxed),
            COUNTS[FAST_GET_FILL].load(Ordering::Relaxed),
            COUNTS[FAST_PUT_HIT].load(Ordering::Relaxed),
            COUNTS[FAST_PUT_MISS].load(Ordering::Relaxed),
            COUNTS[FAST_PUT_FILL].load(Ordering::Relaxed),
            COUNTS[FAST_FIELD_UNUSABLE].load(Ordering::Relaxed),
            COUNTS[DOOR_STATIC_HIT].load(Ordering::Relaxed),
            COUNTS[DOOR_STATIC_MISS].load(Ordering::Relaxed),
            COUNTS[DOOR_SPECIAL_HIT].load(Ordering::Relaxed),
            COUNTS[DOOR_SPECIAL_MISS].load(Ordering::Relaxed),
            COUNTS[DOOR_POLY_HIT].load(Ordering::Relaxed),
            COUNTS[INSTALL_REUSE].load(Ordering::Relaxed),
            COUNTS[INSTALL_EMPLACE].load(Ordering::Relaxed),
            COUNTS[INSTALL_BYVALUE].load(Ordering::Relaxed),
            COUNTS[ARRLEN_HIT].load(Ordering::Relaxed),
            COUNTS[ARRLEN_MISS].load(Ordering::Relaxed),
            COUNTS[REFARR_HIT].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_BARRIER].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_WRAPPER].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_SHAPE].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_WORD].load(Ordering::Relaxed),
            COUNTS[NATFACTS_STATIC].load(Ordering::Relaxed),
            COUNTS[NATFACTS_VIRTUAL].load(Ordering::Relaxed),
            COUNTS[NATFACTS_RESOLVE].load(Ordering::Relaxed),
            COUNTS[NATFACTS_LEAF].load(Ordering::Relaxed),
            COUNTS[HANDLER_CALLEE_DIRECT].load(Ordering::Relaxed),
            COUNTS[PLAIN_CALLEE_DIRECT].load(Ordering::Relaxed),
            COUNTS[SUBTYPE_PUBLISHED].load(Ordering::Relaxed),
            COUNTS[SUBTYPE_LOCKED].load(Ordering::Relaxed),
            COUNTS[AASTORE_HIT].load(Ordering::Relaxed),
            COUNTS[AASTORE_MISS].load(Ordering::Relaxed),
        );
    }

    /// Final tally, printed at VM shutdown so a short run still reports.
    pub fn dump() {
        if !on() {
            return;
        }
        report("FINAL");
        // The door counters above say HOW OFTEN a door declined; this says WHY,
        // which is the half that names a repair.
        crate::runtime::interpreter::invoke_fast::dump_decline_reasons();
    }
}

#[cfg(test)]
mod i6_l2_name_epoch_tests {
    use super::{DefinitionEpochs, NameEpochs, SiteCache, SiteEpochs};
    use cratonvm_types::ClassId;

    /// Stage 1a: the definition of a brand-new class name retires a
    /// [`DefinitionEpochs`] entry (the definition epoch always moves) but not
    /// a [`NameEpochs`] entry. The counters are process-wide and other tests
    /// define classes concurrently, so the survival half is asserted only
    /// over a window in which the name generation itself stayed put.
    #[test]
    fn a_new_class_name_retires_definition_tagged_entries_only() {
        let site = (ClassId::new(7), 3u16);
        let mut by_definition: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
        let mut by_name: SiteCache<u32, NameEpochs> = SiteCache::new();
        let definition_before = DefinitionEpochs::now();
        let name_before = NameEpochs::now();
        by_definition.put(site.0, site.1, definition_before, 11);
        by_name.put(site.0, site.1, name_before, 22);

        let mut mgr = cratonvm_classloading::ClassManager::new(&[], &[], &[]);
        mgr.try_ensure_synthetic_class("cratonvm/test/i6l2/FreshName", 0)
            .expect("an absent name is fabricated in the default mode");

        assert_eq!(
            by_definition.get(site.0, site.1),
            None,
            "a definition moves the definition epoch"
        );
        if !cratonvm_classloading::any_class_redefined() && NameEpochs::now() == name_before {
            assert_eq!(by_name.get(site.0, site.1).copied(), Some(22));
        }
    }
}

/// Interpreter round i1 wave 17, lane L5 (stage 2a of
/// `i14-L3-proposal-class-scoped-redefinition-invalidation`): a redefinition
/// retires the entries filled before it through the resolution epoch, and the
/// tables keep serving afterwards instead of latching off for the process.
#[cfg(test)]
mod i17_l5_redefinition_tests {
    use super::{DefinitionEpochs, NameEpochs, SiteCache, SITE_CACHES_SURVIVE_REDEFINITION};
    use cratonvm_types::ClassId;

    /// Fill `site` with `value`, retrying while concurrent tests move the
    /// process-wide epochs under the fill; whether a fill was ever served.
    fn fill<E: super::SiteEpochs>(
        table: &mut SiteCache<u32, E>,
        site: (ClassId, u16),
        value: u32,
    ) -> bool {
        for _ in 0..1000 {
            let epochs = SiteCache::<u32, E>::epochs_now();
            table.put(site.0, site.1, epochs, value);
            if table.get(site.0, site.1).copied() == Some(value) {
                return true;
            }
        }
        false
    }

    #[test]
    fn a_redefinition_retires_older_entries_and_the_tables_keep_serving() {
        assert!(SITE_CACHES_SURVIVE_REDEFINITION);
        let site = (ClassId::new(0x1757), 9u16);
        let mut by_name: SiteCache<u32, NameEpochs> = SiteCache::new();
        let mut by_definition: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
        assert!(fill(&mut by_name, site, 1));
        assert!(fill(&mut by_definition, site, 1));

        // What `redefine_class_with` does around the class swap (and the
        // resolution invalidate hook in between).
        crate::runtime::interpreter::bump_resolution_epoch();
        assert_eq!(
            by_name.get(site.0, site.1),
            None,
            "filled before the redefinition"
        );
        assert_eq!(
            by_definition.get(site.0, site.1),
            None,
            "filled before the redefinition"
        );

        // Filled after it: served, whatever the process-wide
        // `any_class_redefined` latch says (other tests may have raised it).
        assert!(
            fill(&mut by_name, site, 2),
            "the table keeps serving after a redefinition"
        );
        assert!(fill(&mut by_definition, site, 2));
    }
}

/// Interpreter round i1 wave 20, lane L2
/// (`interpreter-L5-resolution-epoch-is-process-wide`): an entry filled with
/// [`SiteCache::epochs_for`] is retired by its own VM's events only, and an
/// entry filled with the process pair still by every VM's.
#[cfg(test)]
mod w20_l2_per_vm_epoch_tests {
    use super::{DefinitionEpochs, NameEpochs, SiteCache, SiteEpochs, STORE_TAG};
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    fn slot_of(shared: &SharedVm) -> usize {
        cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain)
    }

    /// Fill `site` with `value` under `shared`'s counters, retrying while
    /// concurrent tests move them under the fill; the stamp that stuck.
    fn fill<E: SiteEpochs>(
        table: &mut SiteCache<u32, E>,
        shared: &SharedVm,
        site: (ClassId, u16),
        value: u32,
    ) -> Option<(u64, u64)> {
        for _ in 0..1000 {
            let stamp = SiteCache::<u32, E>::epochs_for(shared);
            table.put(site.0, site.1, stamp, value);
            if table.get(site.0, site.1).copied() == Some(value) {
                return Some(stamp);
            }
        }
        None
    }

    #[test]
    fn a_vm_stamp_carries_its_slot() {
        let shared = SharedVm::new(VmConfig::default());
        let stamp = SiteCache::<u32, NameEpochs>::epochs_for(&shared);
        assert_ne!(stamp.1 & STORE_TAG, 0);
        assert_eq!(
            ((stamp.1 & !STORE_TAG) >> super::STORE_SLOT_SHIFT) as usize,
            slot_of(&shared)
        );
        // The process stamp is untagged.
        assert_eq!(SiteCache::<u32, NameEpochs>::epochs_now().1 & STORE_TAG, 0);
    }

    #[test]
    fn another_vms_resolution_bump_leaves_this_vms_entries() {
        let a = SharedVm::new(VmConfig::default());
        let b = SharedVm::new(VmConfig::default());
        let site = (ClassId::new(0x2020), 4u16);
        let mut by_name: SiteCache<u32, NameEpochs> = SiteCache::new();
        let mut by_definition: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
        let name_stamp = fill(&mut by_name, &b, site, 7).expect("a fill sticks");
        let definition_stamp = fill(&mut by_definition, &b, site, 8).expect("a fill sticks");

        crate::runtime::interpreter::bump_resolution_epoch_in(&a);
        // Slots are shared modulo the table size, and a store another test
        // builds concurrently may share B's: the survival claim is asserted
        // only while B's own counters stayed put.
        let name_hit = by_name.get(site.0, site.1).copied();
        let definition_hit = by_definition.get(site.0, site.1).copied();
        if slot_of(&a) != slot_of(&b) {
            if SiteCache::<u32, NameEpochs>::epochs_for(&b) == name_stamp {
                assert_eq!(name_hit, Some(7), "A's bump retired a B entry");
            }
            if SiteCache::<u32, DefinitionEpochs>::epochs_for(&b) == definition_stamp {
                assert_eq!(definition_hit, Some(8), "A's bump retired a B entry");
            }
        }

        // B's own bump retires both, always.
        crate::runtime::interpreter::bump_resolution_epoch_in(&b);
        assert_eq!(by_name.get(site.0, site.1), None);
        assert_eq!(by_definition.get(site.0, site.1), None);
    }

    #[test]
    fn a_class_definition_retires_only_its_own_vms_definition_entries() {
        let a = SharedVm::new(VmConfig::default());
        let b = SharedVm::new(VmConfig::default());
        let site = (ClassId::new(0x2021), 5u16);
        let mut for_a: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
        let mut for_b: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
        fill(&mut for_a, &a, site, 1).expect("a fill sticks");
        let b_stamp = fill(&mut for_b, &b, site, 2).expect("a fill sticks");

        a.classes.class_manager_write().register_class_name(
            cratonvm_types::ClassLoaderId::Application,
            "w20l2/DefinedInA",
            ClassId::new(900_301),
        );
        assert_eq!(
            for_a.get(site.0, site.1),
            None,
            "A's definition moves A's slot"
        );
        let b_hit = for_b.get(site.0, site.1).copied();
        if slot_of(&a) != slot_of(&b)
            && SiteCache::<u32, DefinitionEpochs>::epochs_for(&b) == b_stamp
        {
            assert_eq!(b_hit, Some(2), "A's definition retired a B entry");
        }
    }

    /// The cast-site memo epoch of a VM is tagged (never equal to a process
    /// snapshot) and moves with that VM's class definitions.
    #[test]
    fn a_vm_cast_memo_epoch_is_tagged_and_moves_with_its_store() {
        let a = SharedVm::new(VmConfig::default());
        let before = super::cast_memo_epoch_in(&a);
        assert_ne!(before & STORE_TAG, 0);
        assert_ne!(before, super::cast_memo_epoch_now());
        a.classes.class_manager_write().register_class_name(
            cratonvm_types::ClassLoaderId::Application,
            "w20l2/MemoEpochProbe",
            ClassId::new(900_302),
        );
        assert_ne!(super::cast_memo_epoch_in(&a), before);
    }

    /// An entry stamped with the process pair is retired by any VM's bump:
    /// the fallback stays conservative.
    #[test]
    fn a_process_stamped_entry_is_retired_by_any_vms_bump() {
        let a = SharedVm::new(VmConfig::default());
        let site = (ClassId::new(0x2022), 6u16);
        let mut table: SiteCache<u32, NameEpochs> = SiteCache::new();
        let mut filled = false;
        for _ in 0..1000 {
            table.put(
                site.0,
                site.1,
                SiteCache::<u32, NameEpochs>::epochs_now(),
                3,
            );
            if table.get(site.0, site.1).copied() == Some(3) {
                filled = true;
                break;
            }
        }
        assert!(filled);
        crate::runtime::interpreter::bump_resolution_epoch_in(&a);
        assert_eq!(table.get(site.0, site.1), None);
    }
}
