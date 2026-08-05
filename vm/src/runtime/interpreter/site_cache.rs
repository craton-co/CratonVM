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

/// Number of direct-mapped slots. A power of two so the index is a mask.
///
/// BCEL's class parser — the workload this was written for — touches on the
/// order of a hundred distinct sites in its hot loop, so 1024 slots make
/// conflict misses vanishingly rare. The table is allocated lazily, so a thread
/// that never reaches the relevant opcode pays nothing at all.
const SLOTS: usize = 1024;

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
}

impl<T> SiteCache<T> {
    pub fn new() -> Self {
        Self { slots: None }
    }

    /// Direct-mapped slot index. Fibonacci-hash the pair and take the HIGH
    /// bits, so that constant-pool indices — which cluster in a narrow range
    /// within any one class — do not alias across classes.
    #[inline]
    fn slot_of(class_id: ClassId, cp_index: u16) -> usize {
        let key = ((class_id.as_u32() as u64) << 16) | cp_index as u64;
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize & (SLOTS - 1)
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
        let idx = Self::slot_of(class_id, cp_index);
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
        let idx = Self::slot_of(class_id, cp_index);
        let slots = self
            .slots
            .get_or_insert_with(|| (0..SLOTS).map(|_| None).collect());
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

/// Per-thread resolved-field sites. Stores the whole `ResolvedField` — it is
/// plain data (ids, an index and four flag bytes), so a hit clones no `Arc`.
pub type FieldSiteCache = SiteCache<cratonvm_classloading::resolution::ResolvedField>;

/// Per-thread resolved-method sites; see [`MethodSiteInfo`].
pub type MethodSiteCache = SiteCache<MethodSiteInfo>;

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

    const N: usize = 7;

    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static COUNTS: [AtomicU64; N] = [ZERO; N];

    pub(super) fn on() -> bool {
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
            "[site-cache] {when} field: hit={} miss={} fill={} reject_loader={} | method: hit={} miss={} fill={}",
            COUNTS[FIELD_HIT].load(Ordering::Relaxed),
            COUNTS[FIELD_MISS].load(Ordering::Relaxed),
            COUNTS[FIELD_FILL].load(Ordering::Relaxed),
            COUNTS[FIELD_REJECT_LOADER].load(Ordering::Relaxed),
            COUNTS[METHOD_HIT].load(Ordering::Relaxed),
            COUNTS[METHOD_MISS].load(Ordering::Relaxed),
            COUNTS[METHOD_FILL].load(Ordering::Relaxed),
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
