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

/// `CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP` (perf, default OFF; t1 wave 10,
/// lane L71): the resolution sites of a referencing class with no loader
/// namespace are stamped with the store slot's GLOBAL resolution counter
/// ([`SiteCache::epochs_for_referencing`], [`GLOBAL_RESOLUTION_TAG`]), which a
/// user loader's new initiating row does not move. Latched in a
/// [`cratonvm_types::flags::MemoSlot`] (one relaxed load; a test override
/// reaches it). Read on the stamp paths of the cast / `new` / `anewarray` /
/// catch-row / `ldc` tables only.
#[inline]
pub(crate) fn site_global_resolution_stamp_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        cratonvm_types::flags::MEMO_UNSET => derive_site_global_resolution_stamp(&SLOT),
        state => state == 2,
    }
}

#[cold]
#[inline(never)]
fn derive_site_global_resolution_stamp(slot: &'static cratonvm_types::flags::MemoSlot) -> bool {
    let on = cratonvm_types::flags::runtime_flag_on("CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP");
    slot.publish(if on { 2 } else { 1 });
    on
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
    /// [`Self::in_store`] for a stamp taken in the selection-edge mode
    /// ([`SELECTION_EDGE_TAG`], [`SiteCache::epochs_for_selection`];
    /// interpreter round i6 wave 116, lane L7). Only [`SelectionEpochs`]
    /// answers anything else; every other table never takes such a stamp.
    #[inline(always)]
    fn in_store_selection_edge(epochs: &cratonvm_classloading::StoreEpochs) -> (u64, u64) {
        Self::in_store(epochs)
    }
    /// [`Self::in_store`] for a stamp taken in the global-resolution mode
    /// ([`GLOBAL_RESOLUTION_TAG`], [`SiteCache::epochs_for_referencing`]; t1
    /// wave 10, lane L71): the second word is slot `slot`'s GLOBAL resolution
    /// counter (`constants::resolution_epoch_global_at`), which a new row of
    /// a user loader's initiating memo or record does not move. Only
    /// [`NameEpochs`] answers anything else. The default, [`Self::in_store`],
    /// is conservative: the ordinary counter moves on every bump the global
    /// one moves on, and more.
    #[inline(always)]
    fn in_store_global_resolution(
        epochs: &cratonvm_classloading::StoreEpochs,
        slot: usize,
    ) -> (u64, u64) {
        let _ = slot;
        Self::in_store(epochs)
    }
}

/// Bit 63 of a stamp's second word: the stamp holds a
/// [`cratonvm_classloading::StoreEpochs`] slot's counters, not the process
/// ones, and bits 56..62 name the slot (wave 20, lane L2). The counters stay
/// far below 2^56, so the process stamp never has the bit.
const STORE_TAG: u64 = 1 << 63;
const STORE_SLOT_SHIFT: u32 = 56;
/// The counter bits of a store-tagged stamp's second word: bits 0..54. Bit 55
/// is [`GLOBAL_RESOLUTION_TAG`] (t1 wave 10, lane L71; it was a counter bit
/// before, which no counter reaches: 2^55 bumps).
const STORE_VALUE_MASK: u64 = GLOBAL_RESOLUTION_TAG - 1;

/// Bit 55 of a store-tagged stamp's second word: the counter in bits 0..54 is
/// [`SiteEpochs::in_store_global_resolution`]'s, the slot's GLOBAL resolution
/// counter, which every resolution bump moves except a new row of a user
/// loader's initiating memo or record (`constants::resolution_epoch_global_at`).
/// t1 wave 10, lane L71, behind `CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`
/// ([`SiteCache::epochs_for_referencing`] alone takes such a stamp). Below
/// the slot bits (56..61), so it is free in every other stamp.
const GLOBAL_RESOLUTION_TAG: u64 = 1 << 55;

/// Bit 62 of a store-tagged stamp's second word: the stamp's first word is
/// [`SiteEpochs::in_store_selection_edge`]'s, not [`SiteEpochs::in_store`]'s
/// (interpreter round i6 wave 116, lane L7; item 3 of
/// `i94-L7-proposal-memos-stamped-with-the-definition-or-origin-epoch-retire-on-every-define`).
/// The slot takes bits 56..61 (`STORE_EPOCH_SLOTS` is 64), so the bit is
/// free; a stamp carries its mode, so an entry is always checked against the
/// counters it was stamped with.
const SELECTION_EDGE_TAG: u64 = 1 << 62;

/// The slot bits of a store-tagged stamp's second word.
const STORE_SLOT_BITS: u64 = (cratonvm_classloading::STORE_EPOCH_SLOTS as u64) - 1;

/// The current stamp of the counters `tag_word` (the second word of a stamp)
/// was taken from: the process pair for an untagged word, else that slot's
/// pair (in the stamp's mode) with the same tag. Equal to the stamp exactly
/// when none of its counters has moved.
#[inline(always)]
fn stamp_now<E: SiteEpochs>(tag_word: u64) -> (u64, u64) {
    if tag_word & STORE_TAG == 0 {
        return E::now();
    }
    let slot = ((tag_word >> STORE_SLOT_SHIFT) & STORE_SLOT_BITS) as usize;
    let epochs = cratonvm_classloading::store_epochs_at(slot);
    let (first, resolution) = if tag_word & (SELECTION_EDGE_TAG | GLOBAL_RESOLUTION_TAG) == 0 {
        E::in_store(epochs)
    } else if tag_word & SELECTION_EDGE_TAG != 0 {
        E::in_store_selection_edge(epochs)
    } else {
        E::in_store_global_resolution(epochs, slot)
    };
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

    /// The name generation and the slot's GLOBAL resolution counter (t1 wave
    /// 10, lane L71): see [`SiteCache::epochs_for_referencing`] for which
    /// sites may take such a stamp.
    #[inline(always)]
    fn in_store_global_resolution(
        epochs: &cratonvm_classloading::StoreEpochs,
        slot: usize,
    ) -> (u64, u64) {
        (
            epochs.class_name_generation(),
            super::constants::resolution_epoch_global_at(slot),
        )
    }
}

/// [`DefinitionEpochs`], plus a selection-edge mode for a table whose values
/// are JVMS §5.4.6 SELECTION answers only, read off classes stored by id
/// ([`IfaceSelectSiteCache`]: `(receiver, selected declaring class)` pairs,
/// each verified by `selection::select_or_lenient`). Interpreter round i6 wave
/// 116, lane L7 (item 3 of
/// `i94-L7-proposal-memos-stamped-with-the-definition-or-origin-epoch-retire-on-every-define`).
///
/// A stamp taken in the mode ([`SiteCache::epochs_for_selection`] with
/// `edge`, which the VM's class store answers
/// `ClassStore::select_memo_edge_epoch()`, `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH`)
/// carries `hierarchy_edge_epoch + class_origin_in_place_epoch` as its first
/// word, in place of the definition epoch: the selection memo's own stamp
/// (`selection::select_memo_hierarchy_stamp` / `select_memo_origin_stamp`,
/// whose doc carries the argument that those two counters move whenever a
/// stored class's edges, loader, method table or provenance can change, and a
/// brand-new class can join an existing hierarchy only through a forward
/// edge, which moves the edge counter). Both only count up, so their sum is
/// unchanged exactly when both are. The receiver of every pair is the class
/// of a live object, so it is stored (the memo's "no entry for an id with
/// nothing stored" rule holds). The resolution epoch (a redefinition, an
/// in-place layout rewrite) is kept. Without the mode, exactly
/// [`DefinitionEpochs`].
pub struct SelectionEpochs;

impl SiteEpochs for SelectionEpochs {
    #[inline]
    fn now() -> (u64, u64) {
        DefinitionEpochs::now()
    }

    #[inline(always)]
    fn in_store(epochs: &cratonvm_classloading::StoreEpochs) -> (u64, u64) {
        DefinitionEpochs::in_store(epochs)
    }

    #[inline(always)]
    fn in_store_selection_edge(epochs: &cratonvm_classloading::StoreEpochs) -> (u64, u64) {
        (
            epochs
                .hierarchy_edge_epoch()
                .wrapping_add(epochs.class_origin_in_place_epoch()),
            epochs.resolution_epoch(),
        )
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

    /// [`Self::epochs_for`], in the selection-edge mode when `edge`
    /// ([`SELECTION_EDGE_TAG`]; meaningful for [`SelectionEpochs`] only, every
    /// other `E` answers its ordinary pair under either tag). `edge` is the
    /// VM's `ClassStore::select_memo_edge_epoch()`, read under the guard the
    /// caller's walk runs under. Interpreter round i6 wave 116, lane L7.
    #[inline]
    pub fn epochs_for_selection(shared: &crate::vm::SharedVm, edge: bool) -> (u64, u64) {
        let slot = cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain);
        let mode = if edge { SELECTION_EDGE_TAG } else { 0 };
        stamp_now::<E>(STORE_TAG | mode | ((slot as u64) << STORE_SLOT_SHIFT))
    }

    /// [`Self::epochs_for`] for the site `(class_id, cp_index)` of a table
    /// whose answers are resolutions of the referencing class `class_id`'s
    /// constants (t1 wave 10, lane L71; proposal
    /// `t1-intv-proposal-loader-memo-rows-retire-boot-class-site-caches`).
    /// With `CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP` on and `namespace_free()`
    /// true, the stamp is taken in the global-resolution mode
    /// ([`GLOBAL_RESOLUTION_TAG`]), so a new initiating-memo / record row of a
    /// user loader -- one per class a `URLClassLoader` loads -- no longer
    /// retires the entry. Off, exactly [`Self::epochs_for`].
    ///
    /// `namespace_free` must answer `!opcodes::referencing_class_has_loader_namespace`
    /// for `class_id` (or `true` for a table whose value reads no class name
    /// at all). Why that is enough: such a class's `CONSTANT_Class`
    /// resolution (`constants::resolve_class_loader_aware_unrecorded`) takes
    /// none of the loader arms -- every one of them is guarded by
    /// `has_loader_namespace` or `has_registered_defining_loader`, the same
    /// two facts -- and reduces to the global name -> `ClassId` route
    /// (`load_class_concurrent_for`), the class-entry record (swept by a
    /// redefinition, which moves both counters) and the access check (class
    /// facts). None of those reads an initiating row, the only writes the
    /// global counter skips. The fact is immutable per class
    /// (`referencing_class_has_loader_namespace`'s doc), so the mode a fill
    /// was stamped in stays right for the entry's life.
    ///
    /// Probe first: a key-matching entry that is still VALID carries the
    /// class's mode already (its counters did not move, so neither did the
    /// name generation an unload moves, and its `ClassId` was not reused), and
    /// its current stamp is the answer -- the pre-probe snapshots of the cast
    /// arms then pay no class-manager read on a hit. A stale, absent or
    /// colliding entry asks `namespace_free` (a side-table probe and a
    /// class-manager read, on the miss path that resolves anyway).
    #[inline]
    pub fn epochs_for_referencing<F: FnOnce() -> bool>(
        &self,
        shared: &crate::vm::SharedVm,
        class_id: ClassId,
        cp_index: u16,
        namespace_free: F,
    ) -> (u64, u64) {
        if !site_global_resolution_stamp_enabled() {
            return Self::epochs_for(shared);
        }
        let slot = cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain);
        let base = STORE_TAG | ((slot as u64) << STORE_SLOT_SHIFT);
        if let Some(site) = self
            .slots
            .as_ref()
            .and_then(|slots| slots[self.slot_of(class_id, cp_index)].as_ref())
        {
            let tag = site.epochs.1;
            if site.class_id == class_id
                && site.cp_index == cp_index
                && tag & (STORE_TAG | (STORE_SLOT_BITS << STORE_SLOT_SHIFT)) == base
                && tag & SELECTION_EDGE_TAG == 0
            {
                let now = stamp_now::<E>(tag);
                if site.epochs == now {
                    return now;
                }
            }
        }
        let mode = if namespace_free() {
            GLOBAL_RESOLUTION_TAG
        } else {
            0
        };
        stamp_now::<E>(base | mode)
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

    /// [`Self::get_mut`] for an owner that adds to a live entry's value rather
    /// than replacing it: the entry must also carry exactly `epochs_at_entry`
    /// -- the counters read before the resolution that produced what is about
    /// to be added -- so an addition never joins values verified under other
    /// epochs than the entry's own.
    #[inline]
    pub fn get_mut_stamped(
        &mut self,
        class_id: ClassId,
        cp_index: u16,
        epochs_at_entry: (u64, u64),
    ) -> Option<&mut T> {
        if latched_off_by_redefinition() {
            return None;
        }
        let idx = self.slot_of(class_id, cp_index);
        let site = self.slots.as_mut()?[idx].as_mut()?;
        if site.class_id != class_id || site.cp_index != cp_index {
            return None;
        }
        if site.epochs != epochs_at_entry || site.epochs != stamp_now::<E>(site.epochs.1) {
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

/// Test support (interpreter round i3 wave 73, lane L7; page
/// `i70-orch-fused-increment-unit-tests-fail-intermittently-in-the-full-suite-20261103`):
/// a [`SiteCache`] whose test-filled entries survive the PROCESS epochs
/// moving under them.
///
/// A unit test that fills a site with [`SiteCache::epochs_now`] and then runs
/// a fused helper that looks the site up races every other test in the
/// crate: any class definition, resolution invalidation or
/// `bump_every_store_resolution_epoch` on another test thread between the
/// fill and the helper's `get` retires the entry, the helper declines as on
/// a cold site, and an assertion that expected a hit fails ("passes alone,
/// fails in the full suite"). `PinnedSites` remembers every entry a test
/// [`pin`](Self::pin)s, and [`run`](Self::run) re-fills them and retries the
/// helper while a decline coincided with a pinned entry going stale.
/// Epochs only grow, so an entry still valid after the call was valid at the
/// call's lookup: a decline returned with every pin valid is the helper's
/// own answer. Every fused helper writes nothing on a decline, which is what
/// makes the retry harmless.
#[cfg(test)]
pub(crate) struct PinnedSites<T: Clone, E: SiteEpochs = DefinitionEpochs> {
    cache: SiteCache<T, E>,
    pins: Vec<(ClassId, u16, T)>,
}

#[cfg(test)]
impl<T: Clone, E: SiteEpochs> PinnedSites<T, E> {
    pub(crate) fn new() -> Self {
        Self {
            cache: SiteCache::new(),
            pins: Vec::new(),
        }
    }

    /// Fill `(class_id, cp)` with `value` at the current epochs (retrying
    /// while they move) and remember it; `false` only if a hundred fills in
    /// a row were retired at once.
    pub(crate) fn pin(&mut self, class_id: ClassId, cp: u16, value: T) -> bool {
        self.pins.retain(|(c, p, _)| !(*c == class_id && *p == cp));
        self.pins.push((class_id, cp, value.clone()));
        self.fill_one(class_id, cp, value)
    }

    fn fill_one(&mut self, class_id: ClassId, cp: u16, value: T) -> bool {
        for _ in 0..100 {
            let epochs = SiteCache::<T, E>::epochs_now();
            self.cache.put(class_id, cp, epochs, value.clone());
            if self.cache.get(class_id, cp).is_some() {
                return true;
            }
        }
        false
    }

    /// Whether every pinned entry is live in the table now.
    fn all_pins_live(&mut self) -> bool {
        for i in 0..self.pins.len() {
            let (c, p) = (self.pins[i].0, self.pins[i].1);
            if self.cache.get(c, p).is_none() {
                return false;
            }
        }
        true
    }

    /// Run `call` over the table; while it declines (`None`) and a pinned
    /// entry went stale meanwhile, re-fill every pin and run it again (at
    /// most a thousand times). A hit, or a decline with every pin live, is
    /// returned as is.
    pub(crate) fn run<R>(
        &mut self,
        mut call: impl FnMut(&mut SiteCache<T, E>) -> Option<R>,
    ) -> Option<R> {
        let mut last = None;
        for _ in 0..1000 {
            last = call(&mut self.cache);
            if last.is_some() || self.all_pins_live() {
                return last;
            }
            let pins = self.pins.clone();
            for (c, p, v) in pins {
                self.fill_one(c, p, v);
            }
        }
        last
    }
}

#[cfg(test)]
impl<T: Clone, E: SiteEpochs> std::ops::Deref for PinnedSites<T, E> {
    type Target = SiteCache<T, E>;
    fn deref(&self) -> &SiteCache<T, E> {
        &self.cache
    }
}

#[cfg(test)]
impl<T: Clone, E: SiteEpochs> std::ops::DerefMut for PinnedSites<T, E> {
    fn deref_mut(&mut self) -> &mut SiteCache<T, E> {
        &mut self.cache
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

/// One quickened instance-field site (see `JvmThread::site_tables.fast_field_sites`).
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
    ///
    /// One more writer since interpreter round i1 wave 52 (lane L7,
    /// `VmConfig::cast_memo_wide`): a LAMBDA PROXY id (`LAMBDA_PROXY_ID_BASE`
    /// and up, registered in `lambda_proxies`) that `lambda_proxy_satisfies`
    /// admitted against `target`. That verdict is a function of the id and
    /// the target: a proxy's `lambda_proxies` entry, serializability and
    /// marker interfaces are written once, when the id is allocated, before
    /// any instance exists, and are never replaced or removed; ids come from a
    /// monotonic counter and are never reused. The interface it names is
    /// found by `load_class_concurrent`, whose answer moves only with a class
    /// definition -- the definition epoch [`Self::memo_epoch`] carries. A
    /// catch row reads this slot too (`catch_row_verdict`); a thrown object is
    /// a `Throwable`, never a lambda proxy, so it never meets such an id.
    pub positive_receiver: Option<ClassId>,
    /// The receiver [`Self::positive_receiver`] held before the newest
    /// positive fill displaced it (interpreter round i2 wave 53, lane L7,
    /// `VmConfig::cast_memo_two_positive`): a bimorphic site -- two receiver
    /// classes, or two lambda proxies, in turn -- answers both from the memo
    /// instead of rewriting one slot on every execution. Same writers, same
    /// verdict, same epochs as [`Self::positive_receiver`]; written only
    /// through [`Self::with_positive`] with the switch on, so with it off it
    /// stays `None` and the hit's second compare never matches.
    pub older_positive_receiver: Option<ClassId>,
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
    /// The last ARRAY receiver the full `instanceof` path REFUSED here
    /// (`typecheck::array_receiver_cast_verdict` said `false`), under the same
    /// key and the same argument as [`Self::positive_array`]: the verdict
    /// reads the key, the target, the mode and the class table, never the
    /// instance (interpreter round i1 wave 52, lane L7,
    /// `VmConfig::cast_memo_wide`). Written by `instanceof` only, as
    /// [`Self::negative_receivers`] is.
    pub negative_array: Option<ArrayReceiverKey>,
    /// The `class_definition_epoch` the memos above were proved under
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
            older_positive_receiver: None,
            negative_receivers: [None; 2],
            negative_catch: None,
            positive_array: None,
            negative_array: None,
            memo_epoch: 0,
        }
    }

    /// Does either positive slot name `receiver`? Slot 0 first: a
    /// monomorphic hit is decided by its first compare.
    #[inline(always)]
    pub fn admits(&self, receiver: ClassId) -> bool {
        self.positive_receiver == Some(receiver) || self.older_positive_receiver == Some(receiver)
    }

    /// This site with `receiver` recorded as the newest positive verdict.
    /// With `two` (`VmConfig::cast_memo_two_positive`) it takes slot 0 and the
    /// previous slot 0 moves to [`Self::older_positive_receiver`], as
    /// [`Self::with_negative`] rotates the refusals; without it, slot 0 is
    /// overwritten and the older slot is left as it is (always `None`, since
    /// only this rotation writes it). A receiver already in either slot
    /// leaves the site as it is, so a hit never reorders and two alternating
    /// receivers never evict each other.
    #[inline]
    pub fn with_positive(self, receiver: ClassId, two: bool) -> Self {
        if self.admits(receiver) {
            return self;
        }
        Self {
            positive_receiver: Some(receiver),
            older_positive_receiver: if two {
                self.positive_receiver
            } else {
                self.older_positive_receiver
            },
            ..self
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
            older_positive_receiver: None,
            negative_receivers: [None; 2],
            negative_catch: None,
            positive_array: None,
            negative_array: None,
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
///
/// # Several pairs per site (SBR5 lane interp)
///
/// The value was ONE pair, so a call site that rotates between two receiver
/// classes -- both selecting a method declared above them, which is every
/// `Collection` / `List` / `Map` call in code that sees an `ArrayList` and a
/// `HashSet` -- missed on every call and paid the `class_manager` read lock and
/// the hierarchy walk each time: 28 000 of the 182 000 door declines of one
/// cold `MethodHandles.countedLoop` run. The value is now a small set of
/// verified pairs ([`IfaceSelectPairs`]), each verified by the same walk under
/// the same epochs (an entry joins the set only while the epochs it was
/// stamped with are the ones its walk started under:
/// `SiteCache::get_mut_stamped`), replaced round-robin when full.
///
/// Since interpreter round i6 wave 116 (lane L7) its stamps are
/// [`SelectionEpochs`]: the definition pair as before, or, while the VM's
/// store runs the selection memo on the edge epoch
/// (`CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH`, default ON since wave 129), the same edge
/// stamp, so a define-per-iteration workload no longer retires every site in
/// front of the memo ([`SiteCache::epochs_for_selection`]).
pub type IfaceSelectSiteCache = SiteCache<IfaceSelectPairs, SelectionEpochs>;

/// Pairs one interface call site remembers.
const IFACE_SELECT_WAYS: usize = 4;

/// The `(receiver class, selected declaring class)` pairs verified at one
/// interface call site: see [`IfaceSelectSiteCache`].
#[derive(Clone, Copy, Debug)]
pub struct IfaceSelectPairs {
    pairs: [(ClassId, ClassId); IFACE_SELECT_WAYS],
    len: u8,
    next: u8,
}

impl IfaceSelectPairs {
    /// A set holding the one pair a first walk verified.
    pub fn single(receiver: ClassId, declaring: ClassId) -> Self {
        let mut pairs = [(ClassId::new(0), ClassId::new(0)); IFACE_SELECT_WAYS];
        pairs[0] = (receiver, declaring);
        Self {
            pairs,
            len: 1,
            next: 0,
        }
    }

    /// Was exactly this pair verified at this site?
    #[inline(always)]
    pub fn contains(&self, receiver: ClassId, declaring: ClassId) -> bool {
        self.pairs[..self.len as usize]
            .iter()
            .any(|&(r, d)| r == receiver && d == declaring)
    }

    /// Remember one more verified pair, evicting the oldest when full.
    pub fn add(&mut self, receiver: ClassId, declaring: ClassId) {
        if self.contains(receiver, declaring) {
            return;
        }
        let at = if (self.len as usize) < IFACE_SELECT_WAYS {
            let at = self.len as usize;
            self.len += 1;
            at
        } else {
            let at = self.next as usize % IFACE_SELECT_WAYS;
            self.next = ((at + 1) % IFACE_SELECT_WAYS) as u8;
            at
        };
        self.pairs[at] = (receiver, declaring);
    }
}

/// The value an `ldc` / `ldc_w` of a `CONSTANT_Integer` / `CONSTANT_Float`, or
/// an `ldc2_w` of a `CONSTANT_Long` / `CONSTANT_Double`, pushes. Typed, so the
/// category-2 push stays tagged (`push_long` / `push_double`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrimitiveConstant {
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    /// A REFERENCE `ldc` result (`CRATONVM_INTERP_LDC_REF_SITES`, default
    /// on, `=0` off; interpreter round i7 wave 143, lane L3): the object the shared
    /// record (`ResolutionCache`, a GC root) holds for the site, and the
    /// `mem.address_epoch()` it was read at. Valid only while the epoch is
    /// unchanged: no collection that moves or frees objects ran since, so the
    /// address still names that object. This table is not a GC root, and
    /// does not need to be one: a stale entry is never read, only refilled.
    Ref {
        obj: cratonvm_types::ObjectRef,
        address_epoch: u64,
    },
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

/// The per-thread site tables the dispatch loop's fused helpers read
/// (`JvmThread::site_tables`; interpreter round i3 wave 83, lane L7, proposal
/// `i70-L7-proposal-one-argument-for-the-fused-helpers-site-tables`): the
/// quickened instance-field sites, the quickened `getstatic` / `putstatic`
/// sites and the numeric `ldc` / `ldc2_w` constants, as one field so a helper
/// takes ONE `&mut` argument reaching all three. The loop cannot pass
/// `&mut JvmThread` (its `frame` borrows the thread's frame stack), but a
/// borrow of this field is disjoint from `frames` exactly as a borrow of one
/// table was. Each table keeps its own type, key and epoch validation; only
/// the path to it changed (`thread.site_tables.fast_field_sites` for
/// `thread.fast_field_sites`).
///
/// `#[repr(C)]`, three 24-byte tables in declaration order, so the offsets
/// `vm/src/runtime/layout_ratchet_tests.rs` freezes are this field's offset
/// plus 0, 24 and 48.
#[repr(C)]
pub struct InterpSiteTables {
    /// See `JvmThread::site_tables`: quickened instance-field sites (the
    /// `getfield` / `putfield` fast arms and the fused field groups).
    pub fast_field_sites: FastFieldSiteCache,
    /// Quickened `getstatic` / `putstatic` sites (admission rule in
    /// `interpreter::field_fast::fill_static_site`).
    pub static_field_sites: FieldSiteCache,
    /// Numeric `ldc` / `ldc_w` / `ldc2_w` constants (see
    /// [`PrimitiveConstantSiteCache`]).
    pub prim_const_sites: PrimitiveConstantSiteCache,
}

impl InterpSiteTables {
    /// Three empty (unallocated) tables.
    pub fn new() -> Self {
        Self {
            fast_field_sites: FastFieldSiteCache::new(),
            static_field_sites: FieldSiteCache::new(),
            prim_const_sites: PrimitiveConstantSiteCache::new(),
        }
    }
}

impl Default for InterpSiteTables {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// A fast door's frame install that kept the retired slot's own handle to
    /// the callee entry (`FrameStack::push_cached_compact_reusing_same`,
    /// proposal i37-L7 item 1, interpreter round i1 wave 47, lane L7): no
    /// reference-count update. Zero with `CRATONVM_FRAME_INSTALL_BORROWED` on
    /// and `door: static hit` climbing means the doors never reach it.
    pub const INSTALL_SAME: usize = 61;
    /// A signature-polymorphic site-cache hit that published the call-site
    /// descriptor as the site's own `Arc<str>` (`poly_call_site::arm_shared`,
    /// proposal i48-L7, interpreter round i1 wave 49, lane L7): no
    /// allocation. Zero with `CRATONVM_POLY_CALL_SITE_SHARED` on and an
    /// `invokeExact` loop running means the hit never arms that way.
    pub const POLY_ARM_SHARED: usize = 62;
    /// A `checkcast` / `instanceof` answered by the RAW cast-site probe
    /// (`opcodes::cast_site_raw_probe`, `VmConfig::cast_hit_raw`, proposal
    /// i50-L7, interpreter round i1 wave 51, lane L7): a subset of
    /// `CAST_HIT` plus `CAST_NEG_HIT`. Zero with `CRATONVM_CAST_HIT_RAW` on
    /// and `cast: hit` climbing means the raw probe never answers.
    pub const CAST_RAW_HIT: usize = 63;
    /// A fill of one of the three memos `VmConfig::cast_memo_wide` adds
    /// (`opcodes::update_cast_memo`: a lambda receiver's admission, a
    /// refused array, a non-array refused by an array target; interpreter
    /// round i1 wave 52, lane L7). The positive control: non-zero with
    /// `CRATONVM_CAST_MEMO_WIDE` on and a lambda cast in a loop, zero with
    /// it off.
    pub const CAST_WIDE_FILL: usize = 64;
    /// A positive cast-site fill that kept the displaced receiver in
    /// `CastSite::older_positive_receiver` (`VmConfig::cast_memo_two_positive`,
    /// interpreter round i2 wave 53, lane L7). Zero with the switch off.
    pub const CAST_TWO_POSITIVE_FILL: usize = 65;
    /// A cast-site hit answered by the OLDER positive slot. The positive
    /// control of `CRATONVM_CAST_MEMO_TWO_POSITIVE`: non-zero on a bimorphic
    /// cast loop with the switch on, zero with it off.
    pub const CAST_OLDER_POSITIVE_HIT: usize = 66;
    /// A `VarHandle` CAS the signature-polymorphic site cache answered with
    /// its instance-field CAS arm (`sig_poly_site::run_route`, MH16-6,
    /// `CRATONVM_VH_INTERP_CAS_FAST`; the weak access modes since
    /// interpreter round i2 wave 53, lane L7). Zero with the switch `=0`.
    pub const VH_CAS_FAST: usize = 67;
    /// A signature-polymorphic site answered by the route cache's probe
    /// AHEAD of the fast door and the cached dispatcher
    /// (`sig_poly_site::try_hit_first`, `VmConfig::sig_poly_site_first`,
    /// interpreter round i2 wave 54, lane L7). The positive control of
    /// `CRATONVM_SIG_POLY_SITE_FIRST`: non-zero on a `VarHandle` loop with the
    /// switch on, zero with it off.
    pub const SIG_POLY_FIRST_HIT: usize = 68;
    /// The early probe ran (the per-thread site filter said "maybe") and
    /// declined: a filter false positive, or a site whose route does not
    /// serve this receiver. Each costs one table probe on top of the old
    /// order; `first_decline` near `first_hit` on a workload is the filter
    /// being too small for it.
    pub const SIG_POLY_FIRST_DECLINE: usize = 69;
    /// A `VarHandle.compareAndExchange` the signature-polymorphic site cache
    /// answered with its instance-field exchange arm
    /// (`sig_poly_site::run_route`, `VmConfig::vh_interp_cae_fast`,
    /// interpreter round i2 wave 54, lane L7). The positive control of
    /// `CRATONVM_VH_INTERP_CAE_FAST`: non-zero on an `int` / `long` exchange
    /// loop with the switch on, zero with it off.
    pub const VH_CAE_FAST: usize = 70;
    /// A primitive-returning `MethodHandle.invokeExact` whose boxed result
    /// the strict return check answered by class id, with no class-manager
    /// lock (`vm_exec::exact_return_wrapper_mismatch`,
    /// `VmConfig::mh_exact_return_by_id`, interpreter round i2 wave 55, lane
    /// L7). The positive control of `CRATONVM_MH_EXACT_RETURN_BY_ID`: non-zero
    /// on an `invokeExact` loop returning a primitive with the switch on, zero
    /// with it off.
    pub const MH_EXACT_RETURN_BY_ID: usize = 71;
    /// A `checkcast` / `instanceof` answered by the IN-PLACE cast-site hit
    /// (`opcodes::cast_site_hit_in_place`, `VmConfig::cast_hit_in_place`,
    /// interpreter round i2 wave 56, lane L7): a subset of `CAST_RAW_HIT`.
    /// The positive control of `CRATONVM_CAST_HIT_IN_PLACE`: non-zero on a
    /// monomorphic cast loop with the switch on, zero with it off.
    pub const CAST_IN_PLACE_HIT: usize = 72;
    /// A signature-polymorphic route-cache hit that ran on a LENT site
    /// (`sig_poly_site::SigPolySiteCache::lend`, `VmConfig::sig_poly_site_lend`,
    /// interpreter round i2 wave 57, lane L7). The positive control of
    /// `CRATONVM_SIG_POLY_SITE_LEND`: non-zero on a `VarHandle` loop with the
    /// switch on, zero with it off.
    pub const SIG_POLY_LENT_HIT: usize = 73;
    /// A `VarHandle` return W6-1's rule refused, or (with
    /// `VmConfig::vh_w6_1_retired`) would have refused
    /// (`vm_exec::unbox_poly_return_checked`, interpreter round i2 wave 58,
    /// lane L4; proposal `i57-L4-proposal-retire-the-w6-1-varhandle-return-text`).
    /// The measurement that decides the rule's retirement: zero on a workload
    /// means the access natives' own checks judged every return.
    pub const VH_W6_1_REFUSAL: usize = 74;
    /// The fused instance-field increment (`field_fast::fused_field_increment`,
    /// interpreter round i3 wave 71, lane L7): a statement-form hit, a
    /// value-form hit (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`), and the
    /// body's declines by reason -- the watch / field-phase gate, a shape the
    /// decoder refuses (or a receiver local past `max_locals`), a null /
    /// non-object receiver or one `field_ptr_for` refuses, a cold site, a
    /// field the group may not store (descriptor, conversion, final rule),
    /// and a read / store / stack-room refusal. The positive control of the
    /// value switch: `value_hit` non-zero on a `last = count++` loop.
    pub const FIELD_INC_HIT: usize = 75;
    pub const FIELD_INC_VALUE_HIT: usize = 76;
    pub const FIELD_INC_DECLINE_GATE: usize = 77;
    pub const FIELD_INC_DECLINE_SHAPE: usize = 78;
    pub const FIELD_INC_DECLINE_RECEIVER: usize = 79;
    pub const FIELD_INC_DECLINE_SITE: usize = 80;
    pub const FIELD_INC_DECLINE_FIELD: usize = 81;
    pub const FIELD_INC_DECLINE_STORE: usize = 82;
    /// The fused array element increment (`field_fast::fused_array_increment_local`
    /// and `_top`, interpreter round i3 wave 72, lane L7): a statement-form
    /// hit, a value-form hit (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_VALUE`),
    /// the hits of either form whose array was on the operand stack
    /// (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP`; a subset of the first
    /// two), and the declines: a shape the decoder refuses (or a full stack
    /// for a value form), and an array / index the quickened `*aload` would
    /// refuse (null, non-array, another element type, out of range). The
    /// positive control of the switch: `hit` non-zero on an `a[i]++` loop.
    pub const ARRAY_INC_HIT: usize = 83;
    pub const ARRAY_INC_VALUE_HIT: usize = 84;
    pub const ARRAY_INC_TOP_HIT: usize = 85;
    pub const ARRAY_INC_DECLINE_SHAPE: usize = 86;
    pub const ARRAY_INC_DECLINE_ARRAY: usize = 87;
    /// A two-field update `this.a = this.b + k` served by
    /// `field_fast::fused_two_field_update`
    /// (`CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`, round i3 wave 72,
    /// lane L7); its declines are counted with the `field-inc` ones.
    pub const FIELD_INC_TWO_FIELDS_HIT: usize = 88;
    /// A fused array element increment entered at its `dup2`
    /// (`field_fast::fused_array_increment_dup2`,
    /// `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2`; a subset of the
    /// `array-inc` hits, round i3 wave 72, lane L7).
    pub const ARRAY_INC_DUP2_HIT: usize = 89;
    /// The fused array element accumulate `s += a[i]`
    /// (`field_fast::fused_array_accumulate`,
    /// `CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE`, round i3 wave 73, lane L7):
    /// a hit, and a decline of any kind (a shape the decoder refuses, an
    /// accumulator / index local that is not an int, an array the quickened
    /// `*aload` would refuse). The positive control of the switch: `hit`
    /// non-zero on a `s += a[i]` loop.
    pub const ARRAY_ACC_HIT: usize = 90;
    pub const ARRAY_ACC_DECLINE: usize = 91;

    const N: usize = 92;

    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static COUNTS: [AtomicU64; N] = [ZERO; N];

    #[inline]
    pub fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FIELD_SITE").is_some()
        })
    }

    /// The off test inline, the count out of line (interpreter round i1
    /// wave 50, lane L7): fat LTO left this whole body a call on every
    /// cast-site hit (three call sites in `op_checkcast`, four in
    /// `op_instanceof`, measured in the w47 and w49 fat-LTO builds), to
    /// return at once in every run without `CRATONVM_DBG_FIELD_SITE`.
    #[inline(always)]
    pub fn bump(index: usize) {
        if on() {
            bump_counted(index);
        }
    }

    /// [`bump`] for a HIT path, with the gate the caller already holds:
    /// `VmConfig::site_census` (the same variable, read once per VM), or the
    /// dispatch loop's per-entry copy of it, so the unarmed test is one
    /// branch on a register or a line the caller has loaded, not a
    /// `OnceLock` read (interpreter round i2 wave 60, lane L7; proposal
    /// `interpreter-L7-proposal-the-process-gates-left-on-the-hit-and-door-paths-FIXED-20261024`).
    /// The misses and fills keep [`bump`]: they are off the hit path.
    #[inline(always)]
    pub fn bump_if(census: bool, index: usize) {
        if census {
            bump_counted(index);
        }
    }

    /// The in-place cast-site hit's counters (wave 56, lane L7) behind ONE
    /// gate test: the copying hit's (`CAST_HIT` or `CAST_NEG_HIT`,
    /// `CAST_RAW_HIT`, `CAST_OLDER_POSITIVE_HIT` for the older slot), so the
    /// census reads the same with the switch on or off, plus
    /// `CAST_IN_PLACE_HIT`. `census` is `VmConfig::site_census` (wave 60).
    #[inline(always)]
    pub fn bump_in_place_cast_hit(census: bool, admitted: bool, older: bool) {
        if census {
            bump_in_place_cast_hit_counted(admitted, older);
        }
    }

    #[cold]
    #[inline(never)]
    fn bump_in_place_cast_hit_counted(admitted: bool, older: bool) {
        bump_counted(if admitted { CAST_HIT } else { CAST_NEG_HIT });
        if older {
            bump_counted(CAST_OLDER_POSITIVE_HIT);
        }
        bump_counted(CAST_RAW_HIT);
        bump_counted(CAST_IN_PLACE_HIT);
    }

    #[cold]
    #[inline(never)]
    fn bump_counted(index: usize) {
        let n = COUNTS[index].fetch_add(1, Ordering::Relaxed) + 1;
        if n % 1_000_000 == 1 {
            report("progress");
        }
    }

    fn report(when: &str) {
        eprintln!(
            "[site-cache] {when} slots={} field: hit={} miss={} fill={} reject_loader={} | method: hit={} miss={} fill={} | new: hit={} miss={} fill={} reject_loader={} | cast: hit={} neg_hit={} array_hit={} miss={} fill={} reject_loader={} unusable={} | ldc: hit={} miss={} fill={} | jit-ldc: hit={} miss={} fill={} | iface-select: hit={} miss={} fill={} trivial={} | fast-field: get hit={} miss={} fill={} put hit={} miss={} fill={} unusable={} | door: static hit={} miss={} special hit={} miss={} virtual_poly hit={} | install: reuse={} emplace={} byvalue={} | arraylength: hit={} miss={} | aaload: hit={} miss_barrier={} miss_wrapper={} miss_shape={} miss_word={} | cached-native: facts_static={} facts_virtual={} resolve={} leaf={} | virtual-promote: handler-bearing={} plain={} | subtype: published={} locked={} | aastore: hit={} miss={} | install-same: {} | poly-arm-shared: {} | cast-raw-hit: {} | cast-wide-fill: {} | cast-two-positive: fill={} older_hit={} | vh-cas-fast: {} | sig-poly-first: hit={} decline={} | vh-cae-fast: {} | mh-exact-return-by-id: {} | cast-in-place-hit: {} | sig-poly-lent-hit: {} | vh-w6-1-refusal: {} | field-inc: hit={} value_hit={} decline gate={} shape={} receiver={} site={} field={} store={} | array-inc: hit={} value_hit={} top_hit={} decline shape={} array={} | field-inc-two-fields: hit={} | array-inc-dup2: hit={} | array-acc: hit={} decline={}",
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
            COUNTS[INSTALL_SAME].load(Ordering::Relaxed),
            COUNTS[POLY_ARM_SHARED].load(Ordering::Relaxed),
            COUNTS[CAST_RAW_HIT].load(Ordering::Relaxed),
            COUNTS[CAST_WIDE_FILL].load(Ordering::Relaxed),
            COUNTS[CAST_TWO_POSITIVE_FILL].load(Ordering::Relaxed),
            COUNTS[CAST_OLDER_POSITIVE_HIT].load(Ordering::Relaxed),
            COUNTS[VH_CAS_FAST].load(Ordering::Relaxed),
            COUNTS[SIG_POLY_FIRST_HIT].load(Ordering::Relaxed),
            COUNTS[SIG_POLY_FIRST_DECLINE].load(Ordering::Relaxed),
            COUNTS[VH_CAE_FAST].load(Ordering::Relaxed),
            COUNTS[MH_EXACT_RETURN_BY_ID].load(Ordering::Relaxed),
            COUNTS[CAST_IN_PLACE_HIT].load(Ordering::Relaxed),
            COUNTS[SIG_POLY_LENT_HIT].load(Ordering::Relaxed),
            COUNTS[VH_W6_1_REFUSAL].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_HIT].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_VALUE_HIT].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_DECLINE_GATE].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_DECLINE_SHAPE].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_DECLINE_RECEIVER].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_DECLINE_SITE].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_DECLINE_FIELD].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_DECLINE_STORE].load(Ordering::Relaxed),
            COUNTS[ARRAY_INC_HIT].load(Ordering::Relaxed),
            COUNTS[ARRAY_INC_VALUE_HIT].load(Ordering::Relaxed),
            COUNTS[ARRAY_INC_TOP_HIT].load(Ordering::Relaxed),
            COUNTS[ARRAY_INC_DECLINE_SHAPE].load(Ordering::Relaxed),
            COUNTS[ARRAY_INC_DECLINE_ARRAY].load(Ordering::Relaxed),
            COUNTS[FIELD_INC_TWO_FIELDS_HIT].load(Ordering::Relaxed),
            COUNTS[ARRAY_INC_DUP2_HIT].load(Ordering::Relaxed),
            COUNTS[ARRAY_ACC_HIT].load(Ordering::Relaxed),
            COUNTS[ARRAY_ACC_DECLINE].load(Ordering::Relaxed),
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

/// Interpreter round i6 wave 116, lane L7: a selection-edge stamp carries its
/// mode and slot, an entry filled with one is read back, and a table whose
/// epochs have no edge mode answers its ordinary first word under either tag.
#[cfg(test)]
mod w116_l7_selection_edge_stamp_tests {
    use super::{
        DefinitionEpochs, SelectionEpochs, SiteCache, SELECTION_EDGE_TAG, STORE_SLOT_BITS,
        STORE_SLOT_SHIFT, STORE_TAG,
    };
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    #[test]
    fn a_selection_edge_stamp_carries_its_mode_and_is_read_back() {
        let shared = SharedVm::new(VmConfig::default());
        let slot = cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain);
        let stamp = SiteCache::<u32, SelectionEpochs>::epochs_for_selection(&shared, true);
        assert_ne!(stamp.1 & STORE_TAG, 0);
        assert_ne!(stamp.1 & SELECTION_EDGE_TAG, 0);
        assert_eq!(((stamp.1 >> STORE_SLOT_SHIFT) & STORE_SLOT_BITS) as usize, slot);
        let plain = SiteCache::<u32, SelectionEpochs>::epochs_for_selection(&shared, false);
        assert_eq!(plain.1 & SELECTION_EDGE_TAG, 0);
        // Filled under the edge stamp, read back (retrying while concurrent
        // tests move the counters under the fill).
        let site = (ClassId::new(0x1160), 3u16);
        let mut table: SiteCache<u32, SelectionEpochs> = SiteCache::new();
        let stuck = (0..1000).any(|_| {
            let stamp = SiteCache::<u32, SelectionEpochs>::epochs_for_selection(&shared, true);
            table.put(site.0, site.1, stamp, 9);
            table.get(site.0, site.1).copied() == Some(9)
        });
        assert!(stuck, "an edge-stamped fill is read back");
        // Without an edge mode of its own, a table answers its ordinary pair.
        let same = (0..1000).any(|_| {
            SiteCache::<u32, DefinitionEpochs>::epochs_for_selection(&shared, true).0
                == SiteCache::<u32, DefinitionEpochs>::epochs_for(&shared).0
        });
        assert!(same);
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

/// SBR5 lane interp: the interface-selection memo remembers several verified
/// pairs per site, answers each exactly, and replaces the oldest when full.
#[cfg(test)]
mod iface_select_pairs_tests {
    use super::IfaceSelectPairs;
    use cratonvm_types::ClassId;

    #[test]
    fn a_site_holds_several_pairs_and_evicts_the_oldest() {
        let c = ClassId::new;
        let mut p = IfaceSelectPairs::single(c(1), c(10));
        assert!(p.contains(c(1), c(10)));
        assert!(!p.contains(c(1), c(11)), "both halves are compared");
        assert!(!p.contains(c(2), c(10)), "both halves are compared");
        p.add(c(2), c(10));
        p.add(c(3), c(10));
        p.add(c(4), c(11));
        for (r, d) in [(1, 10), (2, 10), (3, 10), (4, 11)] {
            assert!(p.contains(c(r), c(d)), "{r} {d}");
        }
        // A repeat adds nothing and evicts nothing.
        p.add(c(2), c(10));
        assert!(p.contains(c(1), c(10)));
        // A fifth pair evicts the oldest (the first), then the next oldest.
        p.add(c(5), c(12));
        assert!(!p.contains(c(1), c(10)));
        assert!(p.contains(c(5), c(12)) && p.contains(c(2), c(10)));
        p.add(c(6), c(12));
        assert!(!p.contains(c(2), c(10)));
        assert!(p.contains(c(3), c(10)) && p.contains(c(4), c(11)) && p.contains(c(6), c(12)));
    }
}

/// t1 wave 10, lane L71 (`CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`): a
/// global-resolution stamp carries its mode and slot, survives a user
/// loader's new initiating row, and is retired by every other resolution
/// bump; an ordinary stamp is retired by both; the switch off is exactly
/// `epochs_for`; a table without a global counter of its own is conservative.
#[cfg(test)]
mod t1w10_l71_global_resolution_stamp_tests {
    use super::{
        DefinitionEpochs, NameEpochs, SiteCache, SiteEpochs, GLOBAL_RESOLUTION_TAG,
        SELECTION_EDGE_TAG, STORE_SLOT_BITS, STORE_SLOT_SHIFT, STORE_TAG,
    };
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_types::ClassId;

    const KEY: &str = "CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP";

    fn with_switch<R>(on: bool, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some(if on { "1" } else { "0" }))], f)
    }

    fn slot_of(shared: &SharedVm) -> usize {
        cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain)
    }

    /// Fill `site` with `value`, stamped by `epochs_for_referencing` with
    /// `namespace_free`, retrying while concurrent tests move the counters;
    /// the stamp that stuck.
    fn fill<E: SiteEpochs>(
        table: &mut SiteCache<u32, E>,
        shared: &SharedVm,
        site: (ClassId, u16),
        namespace_free: bool,
        value: u32,
    ) -> Option<(u64, u64)> {
        for _ in 0..1000 {
            let stamp = table.epochs_for_referencing(shared, site.0, site.1, || namespace_free);
            table.put(site.0, site.1, stamp, value);
            if table.get(site.0, site.1).copied() == Some(value) {
                return Some(stamp);
            }
        }
        None
    }

    #[test]
    fn the_stamp_carries_its_mode_and_slot() {
        let shared = SharedVm::new(VmConfig::default());
        with_switch(true, || {
            let table: SiteCache<u32, NameEpochs> = SiteCache::new();
            let site = (ClassId::new(0x7101), 2u16);
            let global = table.epochs_for_referencing(&shared, site.0, site.1, || true);
            assert_ne!(global.1 & STORE_TAG, 0);
            assert_ne!(global.1 & GLOBAL_RESOLUTION_TAG, 0);
            assert_eq!(global.1 & SELECTION_EDGE_TAG, 0);
            assert_eq!(
                ((global.1 >> STORE_SLOT_SHIFT) & STORE_SLOT_BITS) as usize,
                slot_of(&shared)
            );
            let ordinary = table.epochs_for_referencing(&shared, site.0, site.1, || false);
            assert_eq!(ordinary.1 & GLOBAL_RESOLUTION_TAG, 0);
            assert_ne!(ordinary.1 & STORE_TAG, 0);
        });
    }

    #[test]
    fn switch_off_is_exactly_epochs_for_and_never_asks() {
        let shared = SharedVm::new(VmConfig::default());
        with_switch(false, || {
            let table: SiteCache<u32, NameEpochs> = SiteCache::new();
            let same = (0..1000).any(|_| {
                let stamp = table.epochs_for_referencing(&shared, ClassId::new(0x7102), 3, || {
                    panic!("the switch off asks nothing")
                });
                stamp == SiteCache::<u32, NameEpochs>::epochs_for(&shared)
            });
            assert!(same);
        });
    }

    #[test]
    fn a_loader_row_retires_ordinary_entries_only() {
        let shared = SharedVm::new(VmConfig::default());
        with_switch(true, || {
            let mut table: SiteCache<u32, NameEpochs> = SiteCache::new();
            let global_site = (ClassId::new(0x7103), 4u16);
            let ordinary_site = (ClassId::new(0x7104), 5u16);
            let global = fill(&mut table, &shared, global_site, true, 1).expect("a fill sticks");
            fill(&mut table, &shared, ordinary_site, false, 2).expect("a fill sticks");

            let slot = slot_of(&shared);
            let global_before = crate::runtime::interpreter::resolution_epoch_global_at(slot);
            let resolution_before = crate::runtime::interpreter::resolution_epoch_in(&shared);
            crate::runtime::interpreter::bump_resolution_epoch_for_loader_row_in(&shared);
            // The VM's own counter (every other reader's) moved; the ordinary
            // entry is retired, always.
            assert!(crate::runtime::interpreter::resolution_epoch_in(&shared) > resolution_before);
            assert_eq!(table.get(ordinary_site.0, ordinary_site.1), None);
            // The global entry survives -- asserted while no concurrent test
            // moved this slot's global counter or name generation.
            let hit = table.get(global_site.0, global_site.1).copied();
            if crate::runtime::interpreter::resolution_epoch_global_at(slot) == global_before
                && table.epochs_for_referencing(&shared, global_site.0, global_site.1, || true)
                    == global
            {
                assert_eq!(hit, Some(1), "a loader row retired a global-mode entry");
            }

            // Every other bump moves the global counter: the entry goes.
            crate::runtime::interpreter::bump_resolution_epoch_in(&shared);
            assert!(crate::runtime::interpreter::resolution_epoch_global_at(slot) > global_before);
            assert_eq!(table.get(global_site.0, global_site.1), None);
        });
    }

    #[test]
    fn a_valid_entry_answers_its_own_mode_without_asking() {
        let shared = SharedVm::new(VmConfig::default());
        with_switch(true, || {
            let mut table: SiteCache<u32, NameEpochs> = SiteCache::new();
            let site = (ClassId::new(0x7105), 6u16);
            fill(&mut table, &shared, site, true, 3).expect("a fill sticks");
            // While the entry is valid the probe answers its stamp; once a
            // concurrent bump retires it, the predicate is asked (and then
            // answers the ordinary mode here).
            let reused = (0..1000).any(|_| {
                let mut asked = false;
                let stamp = table.epochs_for_referencing(&shared, site.0, site.1, || {
                    asked = true;
                    false
                });
                !asked && stamp.1 & GLOBAL_RESOLUTION_TAG != 0
            });
            assert!(reused, "a valid global entry answers its own mode");
        });
    }

    #[test]
    fn a_table_without_a_global_counter_is_conservative() {
        let shared = SharedVm::new(VmConfig::default());
        with_switch(true, || {
            let mut table: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
            let site = (ClassId::new(0x7106), 7u16);
            fill(&mut table, &shared, site, true, 4).expect("a fill sticks");
            crate::runtime::interpreter::bump_resolution_epoch_for_loader_row_in(&shared);
            assert_eq!(
                table.get(site.0, site.1),
                None,
                "DefinitionEpochs reads the ordinary counter in either mode"
            );
        });
    }
}
