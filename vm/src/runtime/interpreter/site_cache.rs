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
//!    insert, remove and bulk retain.
//! 2. **No cached resolution has been invalidated.**
//!    [`super::constants::resolution_epoch`] covers the rest of the input set:
//!    the per-loader initiating-resolution memo (which changes what a loader
//!    answers *without* touching `loaded_classes`), and classloading's
//!    resolution-invalidate hook. Two of that hook's four firing sites —
//!    `upgrade_synthetic_class` and `recompute_subclass_layouts` — move a
//!    class's **field layout in place** while keeping its `ClassId`, its name
//!    and the redefine latch, so this epoch is their only signal.
//! 3. **No class has been redefined in place.**
//!    `cratonvm_classloading::any_class_redefined` latches on the first
//!    redefine and permanently disables the cache. Redefinition is a
//!    JVMTI/Mockito scenario, not a throughput one, so the conservative latch
//!    is the right trade against carrying a per-class generation here.
//!
//! All three are single atomic loads, checked on every hit and every fill. Any
//! change wipes the table; the caller then falls through to the authoritative
//! slow path, which re-fills.
//!
//! # Why one generic table
//!
//! The field and method arms have the same key, the same validity condition and
//! the same failure mode — a wrong answer that is *plausible* rather than
//! detectable. Sharing [`SiteCache`] means that argument is implemented and
//! audited once. Only what is stored differs.

use cratonvm_types::ClassId;

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

/// Direct-mapped, epoch-validated per-thread cache keyed on
/// `(referencing class, constant-pool index)`. See the module docs for the
/// validity argument — **read it before adding a `put` call site**, because
/// what makes an entry admissible is a property of the resolution that produced
/// it, not of this type.
pub struct SiteCache<T> {
    /// Lazily allocated. `None` in a slot means empty.
    slots: Option<Box<[Option<Site<T>>]>>,
    /// `slots.len() - 1`, captured when the table is allocated so the hot path
    /// masks without re-reading the flag. Zero while unallocated.
    mask: usize,
}

impl<T> SiteCache<T> {
    pub fn new() -> Self {
        Self {
            slots: None,
            mask: 0,
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
    /// back to [`Self::put`].
    #[inline]
    pub fn epochs_now() -> (u64, u64) {
        (
            cratonvm_classloading::class_definition_epoch(),
            super::constants::resolution_epoch(),
        )
    }

    /// Look up a site. Returns a borrow so the caller clones only what it needs.
    ///
    /// Every check here is O(1): the redefine latch, one array index, and four
    /// integer compares against the entry's own tag and epochs. Nothing scans
    /// or clears the table — see [`Site::epochs`] for why that matters.
    #[inline]
    pub fn get(&mut self, class_id: ClassId, cp_index: u16) -> Option<&T> {
        if cratonvm_classloading::any_class_redefined() {
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
        if site.epochs != Self::epochs_now() {
            return None;
        }
        Some(&site.value)
    }

    /// Publish a resolved site.
    ///
    /// `epochs_at_entry` must come from [`Self::epochs_now`] called **before**
    /// the resolution that produced `value`. If either counter has moved since,
    /// the answer may already describe superseded state, so the insert is
    /// dropped rather than published: this thread would otherwise serve a stale
    /// answer until the epochs next moved. Snapshotting at insert time instead
    /// would swallow exactly that race.
    #[inline]
    pub fn put(&mut self, class_id: ClassId, cp_index: u16, epochs_at_entry: (u64, u64), value: T) {
        if cratonvm_classloading::any_class_redefined() {
            // Latched off for the rest of the process — release the memory too.
            self.slots = None;
            return;
        }
        if epochs_at_entry != Self::epochs_now() {
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

impl<T> Default for SiteCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> std::fmt::Debug for SiteCache<T> {
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
/// Only those whose referencing class has **no loader namespace**: no entry in
/// the defining-loader side table, and a class-manager loader id that is not
/// `UserDefined`. For such a class `resolve_class_loader_aware` reduces to the
/// global name → `ClassId` mapping plus the resolution memo, which is exactly
/// what this module's two epochs cover. The field arm draws the same line, and
/// puts its loader-sensitive half behind a separate flag for the same reason.
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
/// Class initialization is monotonic (JVMS §5.5): an entry is only filled after
/// `ensure_class_initialized_shared` has returned `Ok`, so a hit cannot be the
/// first touch. This is the one skip worth naming, because
/// `ensure_class_initialized_shared`'s own "fast path" still takes a
/// `class_manager` read lock — the hit would otherwise keep paying a lock for a
/// question already answered. A redefinition latches the whole table off
/// (`any_class_redefined`) and class unloading bumps `class_definition_epoch`,
/// so neither can strand an entry describing an uninitialized class.
pub type ClassSiteCache = SiteCache<ResolvedNewSite>;

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
    /// Byte offset of the field from the end of the object header: the
    /// layout's own for a compact body, `field_index * SLOT_SIZE` for a
    /// legacy one.
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
/// Only the resolution. The assignability test still runs on every execution —
/// a hit removes the `String` for the class name, the loader-aware
/// `resolve_class_loader_aware`, and one of the two `class_manager` read
/// acquisitions, and nothing else.
///
/// A hit is taken only when the receiver is not an array (arrays answer through
/// descriptor-based assignability, which needs the name) and only when
/// `is_subclass_of` says yes. A negative `is_subclass_of` falls through to the
/// full path, because the five fail-open fallbacks after it
/// (`loader_aware_name_assignable`, `synthetic_implements`,
/// `proxy_instance_satisfies_target`, …) are name-based. So the cache
/// accelerates the assignable case and leaves every refusal exactly as it was.
pub type CastSiteCache = SiteCache<ClassId>;

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
/// Exactly [`SiteCache`]'s: `class_definition_epoch`, `resolution_epoch`, and
/// the `any_class_redefined` latch. That set is not merely sufficient here, it
/// is the precise one — a receiver class's superclass and superinterface chain
/// is fixed at load time, so the walk's answer can only move when a class is
/// redefined in place (the latch) or when `upgrade_synthetic_class` /
/// `recompute_subclass_layouts` rewrites a class under an unchanged `ClassId`
/// (the resolution epoch, which the invalidate hook bumps and which nothing
/// else in the invoke-cache path observes).
pub type IfaceSelectSiteCache = SiteCache<(ClassId, ClassId)>;

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
    /// A cast site refused a FILL: a loader-namespaced referencing class or an
    /// array target, whose answer is not a property of the (class, index) pair
    /// alone. Same reason the `new` counterpart exists — it separates "measured
    /// no effect" from "never fired".
    pub const CAST_REJECT_LOADER: usize = 14;
    /// A cast site HIT whose answer could not be used: `is_subclass_of` said no
    /// (or the receiver was an array), so the full name-based path ran anyway.
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
    /// added cost for it. `ldc2_w` does not probe or record — see
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
    /// A decline on this receiver's shape: not a reference array, or the index
    /// is out of bounds. The general path raises the AIOOBE.
    pub const REFARR_MISS_SHAPE: usize = 44;
    /// The element word is neither zero nor a plausible heap pointer, so the
    /// three-way cold decode owns it.
    pub const REFARR_MISS_WORD: usize = 45;

    const N: usize = 46;

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
            "[site-cache] {when} slots={} field: hit={} miss={} fill={} reject_loader={} | method: hit={} miss={} fill={} | new: hit={} miss={} fill={} reject_loader={} | cast: hit={} miss={} fill={} reject_loader={} unusable={} | ldc: hit={} miss={} fill={} | jit-ldc: hit={} miss={} fill={} | iface-select: hit={} miss={} fill={} trivial={} | fast-field: get hit={} miss={} fill={} put hit={} miss={} fill={} unusable={} | door: static hit={} miss={} special hit={} miss={} | install: reuse={} emplace={} byvalue={} | arraylength: hit={} miss={} | aaload: hit={} miss_screen={} miss_shape={} miss_word={}",
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
            COUNTS[INSTALL_REUSE].load(Ordering::Relaxed),
            COUNTS[INSTALL_EMPLACE].load(Ordering::Relaxed),
            COUNTS[INSTALL_BYVALUE].load(Ordering::Relaxed),
            COUNTS[ARRLEN_HIT].load(Ordering::Relaxed),
            COUNTS[ARRLEN_MISS].load(Ordering::Relaxed),
            COUNTS[REFARR_HIT].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_SCREEN].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_SHAPE].load(Ordering::Relaxed),
            COUNTS[REFARR_MISS_WORD].load(Ordering::Relaxed),
        );
    }

    /// Final tally, printed at VM shutdown so a short run still reports.
    pub fn dump() {
        if !on() {
            return;
        }
        report("FINAL");
    }
}
