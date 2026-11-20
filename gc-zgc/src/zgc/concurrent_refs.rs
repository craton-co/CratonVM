// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t22/zc3: the referent protocol of stage 1c (`CRATONVM_ZGC_CONCURRENT_REFS`,
//! **opt-in**; on under the concurrent profile `CRATONVM_ZGC_CONCURRENT=1`,
//! `=0` kills it there), and the census of the VM's reference round. Stage 1c of
//! `docs/feature-designs/zgc-architecture.md`.
//!
//! # The protocol this replaces
//!
//! The VM's pre-collection pass (`weakref_null_referents_pre_gc`, HIB-CV-24)
//! NULLS the referent slot of every active Weak / Phantom `Reference` (and of
//! every Soft one the LRU policy condemns) before the mark, so the mark cannot
//! keep the referent alive through its own `Reference`; the post-collection
//! round (`process_references_after_gc`) writes the survivors back. Between the
//! two a LIVE referent's slot reads `null`, which is why that round could never
//! leave the pause (zt t21/zc2, section 1c of the page).
//!
//! # The protocol under the switch (HotSpot's shape)
//!
//! * **The slot keeps its referent.** The pre-collection pass hands the rows it
//!   would have nulled to the heap instead ([`ZgcRealHeap::set_stw_vm_reference_skip`]),
//!   and every marker of the collection hides their slot 0: the serial marker's
//!   `ref_skip_objs`, the parallel / concurrent engine's skip snapshot
//!   ([`ZgcRealHeap::begin_concurrent_mark_cycle`]) and the concurrent mark end's
//!   kept-soft roots ([`ZgcRealHeap::kept_soft_referent_roots`]).
//! * **The pause clears what the mark did not reach.** After the mark,
//!   finalizer resurrection and the kept-soft remark -- the mark is final -- and
//!   before the sweep or the slide can free or re-issue anything,
//!   [`ZgcRealHeap::clear_dead_vm_referents_stw`] nulls slot 0 of each hidden
//!   `Reference` that is itself live and whose referent is not a marked
//!   registered object. That is the VM round's own death verdict (its survival
//!   predicate on ZGC is the registry after the sweep's prune, i.e. "registered
//!   and marked"), so the round that follows finds every slot as the null pass
//!   left it: null for a dead referent, the referent for a live one (the
//!   restore rewrites the same value, or the moved one after a slide).
//!
//! What it buys: no slot ever reads a transient `null` for a live referent, and
//! no slot ever names an object the sweep is about to free. A mutator released
//! at any point after this pause reads the HotSpot answer from the slot itself
//! (`get()` of an unmarked referent is `null`; of a marked one, the referent),
//! with no read gate. That is the precondition for moving the rest of the
//! round out of the pause; what still blocks that move is written on the page
//! (1c, "what is left").
//!
//! Scope: non-generational ZGC only (a young cycle traces referents by design,
//! zt t7/cc); with generational mode on the switch is refused per cycle and the
//! null pass runs as before (`crf_refused`).
//!
//! # The census (item 1 of zt t22/zc3)
//!
//! The VM door times its pre-collection pass, `process_references_after_gc`
//! and the JNI weak-global sweep, and hands the laps to
//! [`ZgcRealHeap::note_vm_reference_round`]: one `[GC] zgc-vm-refs:` line per
//! collection under `--verbose:gc`, the sum as `cp_vm_refs_pause_us=` on a
//! deferred cycle's `[GC] zgc-concurrent-phases:` line, and the totals on
//! `[GC] zgc-concurrent-refs:` at exit (`CRATONVM_GC_STATS=1`). Recorded with
//! the switch off too: the census is what decides whether moving the round is
//! worth building.
//!
//! # zt t32: the clear after the release (steps 2b and 2d)
//!
//! Under `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR` (opt-in; on under the profile,
//! `=0` kills it there), a collection that parks its sweep (1d) no longer
//! clears the dead referents in its pause. The pause STAGES the hidden rows
//! ([`ZgcRealHeap::clear_dead_vm_referents_stw`]) and, when the sweep is
//! deferred, arms the READ GATE ([`ZgcRealHeap::arm_referent_read_gate`]).
//! The collecting thread runs the clear after the release, first thing in the
//! VM's reference round ([`ZgcRealHeap::run_staged_referent_clear`]); every
//! runner of the parked sweep runs it before the walk too, so no dead referent
//! is freed while a slot still names it. A cycle that does not defer clears in
//! the pause, before its sweep, as before.
//!
//! Between the release and the clear a slot may still name a referent the mark
//! did not reach. HotSpot answers `null` there ("resurrection blocked"), and so
//! does every mutator read of `Reference.referent` here: the VM's read sites
//! ask [`ZgcRealHeap::referent_gate_state`] and, while the gate is armed,
//! [`ZgcRealHeap::referent_read_blocked`] -- the clear's own verdict (a hidden
//! row, itself live, whose referent is not registered and marked). The pause
//! pruned the dead from the registry, the window allocates above the pause's
//! cursor (never on a dead object's bytes), and the mark bits stay the pause's
//! until the parked sweep's install, which follows the disarm. A reader that
//! finds the gate disarmed -- before or after its verdict -- re-reads the slot:
//! the clear's writes happen before the disarm.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashSet;

use super::ZgcRealHeap;
use cratonvm_gc_core::collector::GarbageCollector;
use cratonvm_types::{ObjectRef, Value};

/// `CRATONVM_ZGC_CONCURRENT_REFS` (zt t22/zc3, **opt-in**; on under
/// `CRATONVM_ZGC_CONCURRENT=1`, whose `=0` kills it there): the referent
/// slot keeps its referent through the collection (module doc). Read once per
/// heap, at construction ([`ZConcRefsState::new`]).
pub fn zgc_concurrent_refs_enabled() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => super::zgc_concurrent_profile_enabled(),
    }
}

/// `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR` (zt t32, steps 2b / 2d, **opt-in**;
/// on under `CRATONVM_ZGC_CONCURRENT=1`, whose `=0` kills it there): a cycle
/// that parks its sweep clears its dead referents after the release, behind
/// the read gate (module doc). Needs `CRATONVM_ZGC_CONCURRENT_REFS`. Read once
/// per heap, at construction.
pub fn zgc_concurrent_referent_clear_enabled() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => super::zgc_concurrent_profile_enabled(),
    }
}

/// [`ZgcRealHeap::referent_gate_state`]: no gate was ever armed on this heap.
pub const ZGC_REFERENT_GATE_NEVER: u8 = 0;
/// The gate is armed: a referent read asks [`ZgcRealHeap::referent_read_blocked`].
pub const ZGC_REFERENT_GATE_ARMED: u8 = 1;
/// The gate was armed and the clear has run: a referent read re-reads its slot.
pub const ZGC_REFERENT_GATE_DISARMED: u8 = 2;

/// The per-heap state (inside the boxed `ZgcCounters`, so `ZgcRealHeap`'s size
/// ratchet is untouched).
pub struct ZConcRefsState {
    /// `CRATONVM_ZGC_CONCURRENT_REFS` on this heap (construction seed).
    enabled: AtomicBool,
    /// The `Reference` objects whose slot 0 this collection hides instead of
    /// the pre-collection pass nulling it. Set by the VM inside the pause,
    /// taken by [`ZgcRealHeap::clear_dead_vm_referents_stw`] (and cleared by
    /// the VM door after the collection, so no later mark start can read a
    /// stale address).
    stw_skip: Mutex<Vec<usize>>,
    /// This collection took the skip arm (reset by the round's note).
    cycle_skip: AtomicBool,
    /// Rows hidden / slots cleared by this collection (reset by the note).
    cycle_rows: AtomicUsize,
    cycle_cleared: AtomicUsize,
    /// Cumulative engagement.
    skip_cycles: AtomicUsize,
    skip_rows: AtomicUsize,
    stw_cleared: AtomicUsize,
    /// Collections the switch was on for but generational mode refused.
    refused: AtomicUsize,
    /// The census (recorded with the switch off too): rounds noted, and their
    /// microseconds, cumulative.
    rounds: AtomicUsize,
    pre_us: AtomicU64,
    post_us: AtomicU64,
    jni_us: AtomicU64,
    max_round_us: AtomicU64,
    /// The last noted round's pre + post + JNI microseconds (a deferred
    /// cycle's `cp_vm_refs_pause_us=`).
    last_round_us: AtomicU64,
    /// zt t32: `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR` on this heap
    /// (construction seed, and only with `enabled`).
    clear_after_release: AtomicBool,
    /// zt t32: the rows the pause staged instead of clearing (sorted), taken by
    /// [`ZgcRealHeap::run_staged_referent_clear`].
    staged: Mutex<Vec<usize>>,
    /// zt t32: one runner of the staged clear at a time (the round, a sweep
    /// runner's backstop), so the gate never disarms before the clear ends.
    clear_run: Mutex<()>,
    /// zt t32: the read gate (`ZGC_REFERENT_GATE_*`).
    gate: AtomicU8,
    /// zt t32: the armed gate's rows (sorted). Kept after the disarm until the
    /// next arm (inside a pause), so a reader that saw the gate armed still
    /// finds its row.
    gate_rows: parking_lot::RwLock<Vec<usize>>,
    stw_skip_persistent: AtomicBool,
    staged_persistent: AtomicBool,
    gate_persistent: AtomicBool,
    /// zt t32: collections that armed the gate, slots their after-release
    /// clears nulled, and reads the gate answered `null`.
    deferred_clears: AtomicUsize,
    after_release_cleared: AtomicUsize,
    gate_blocked: AtomicUsize,
}

/// The `[GC] zgc-concurrent-refs:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZConcRefsReport {
    pub enabled: bool,
    pub skip_cycles: usize,
    pub skip_rows: usize,
    pub stw_cleared: usize,
    pub refused: usize,
    pub rounds: usize,
    pub pre_us: u64,
    pub post_us: u64,
    pub jni_us: u64,
    pub max_round_us: u64,
    /// zt t32: collections that cleared after the release, the slots those
    /// clears nulled, and mutator reads the gate answered `null`.
    pub deferred_clears: usize,
    pub after_release_cleared: usize,
    pub gate_blocked: usize,
}

impl ZConcRefsState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            stw_skip: Mutex::new(Vec::new()),
            cycle_skip: AtomicBool::new(false),
            cycle_rows: AtomicUsize::new(0),
            cycle_cleared: AtomicUsize::new(0),
            skip_cycles: AtomicUsize::new(0),
            skip_rows: AtomicUsize::new(0),
            stw_cleared: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
            pre_us: AtomicU64::new(0),
            post_us: AtomicU64::new(0),
            jni_us: AtomicU64::new(0),
            max_round_us: AtomicU64::new(0),
            last_round_us: AtomicU64::new(0),
            clear_after_release: AtomicBool::new(
                enabled && zgc_concurrent_referent_clear_enabled(),
            ),
            staged: Mutex::new(Vec::new()),
            clear_run: Mutex::new(()),
            gate: AtomicU8::new(ZGC_REFERENT_GATE_NEVER),
            gate_rows: parking_lot::RwLock::new(Vec::new()),
            stw_skip_persistent: AtomicBool::new(false),
            staged_persistent: AtomicBool::new(false),
            gate_persistent: AtomicBool::new(false),
            deferred_clears: AtomicUsize::new(0),
            after_release_cleared: AtomicUsize::new(0),
            gate_blocked: AtomicUsize::new(0),
        }
    }
}

impl ZgcRealHeap {
    /// zt t22/zc3: may this collection's pre-collection pass hide the referent
    /// slots instead of nulling them? The switch on and generational mode off.
    /// Asked once per collection, inside the pause; a refusal is counted.
    pub fn concurrent_refs_skip_mode(&self) -> bool {
        let st = &self.counters.crefs;
        if !st.enabled.load(Ordering::Relaxed) {
            return false;
        }
        if self.generational_enabled() && self.young_cycle_predicted() {
            st.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// zt t22/zc3: the rows the pre-collection pass did NOT null (each screened
    /// by the VM: a live `java.lang.ref.Reference` with its registered
    /// identity). Their slot 0 is hidden from every marker of the collection
    /// about to run and cleared by [`Self::clear_dead_vm_referents_stw`] when
    /// the referent dies. **Call inside the pause, right before the
    /// collection**; replaces any earlier set.
    pub fn persistent_skip_bits_enabled(&self) -> bool {
        self.counters.skipbits.persistent_enabled()
    }

    pub fn persistent_skip_bits_arc(&self) -> std::sync::Arc<super::ZSkipBits> {
        std::sync::Arc::clone(&self.counters.skipbits)
    }

    pub fn set_stw_vm_reference_skip(&self, addrs: Vec<usize>) {
        let st = &self.counters.crefs;
        let persistent = self.counters.skipbits.persistent_enabled()
            && self.counters.skipbits.persistent_len() > 0;
        let n = addrs.len()
            + if persistent {
                self.counters.skipbits.persistent_len()
            } else {
                0
            };
        st.cycle_skip.store(true, Ordering::Relaxed);
        st.cycle_rows.store(n, Ordering::Relaxed);
        st.skip_cycles.fetch_add(1, Ordering::Relaxed);
        st.skip_rows.fetch_add(n, Ordering::Relaxed);
        st.stw_skip_persistent.store(persistent, Ordering::Relaxed);
        *st.stw_skip.lock() = addrs;
    }

    /// zt t31/zc11: did this collection's pre-collection pass hide its rows
    /// ([`Self::set_stw_vm_reference_skip`]) instead of nulling them? Read in
    /// the pause, before the VM notes the round (which resets it).
    pub(super) fn stw_vm_reference_skip_used_this_cycle(&self) -> bool {
        self.counters.crefs.cycle_skip.load(Ordering::Relaxed)
    }

    /// zt t22/zc3: forget the collection's set (the VM door, after the
    /// collection: a mark start must never see a stale, possibly re-issued
    /// address). The collection itself already took it.
    pub fn clear_stw_vm_reference_skip(&self) {
        self.counters.crefs.stw_skip.lock().clear();
        self.counters
            .crefs
            .stw_skip_persistent
            .store(false, Ordering::Relaxed);
    }

    /// zt t22/zc3: add the collection's hidden rows to a skip set (the serial
    /// marker's `ref_skip_objs`, the engine's snapshot). Empty unless the VM
    /// handed a set in for this collection.
    pub(super) fn extend_with_stw_vm_reference_skip(&self, set: &mut impl Extend<usize>) {
        let rows = self.counters.crefs.stw_skip.lock();
        if !rows.is_empty() {
            set.extend(rows.iter().copied());
        }
    }

    /// zt t22/zc3: the collection's hidden rows as a set (the concurrent mark
    /// end's kept-soft roots ask it per soft row).
    pub(super) fn stw_vm_reference_skip_set(&self) -> FxHashSet<usize> {
        let mut set = FxHashSet::default();
        self.extend_with_stw_vm_reference_skip(&mut set);
        set
    }

    /// zt t22/zc3: the pause's referent clear. For every hidden `Reference`
    /// that is itself live (registered and marked), null slot 0 when it names
    /// anything but a registered, marked object -- the VM round's own death
    /// verdict, taken while the mark is final and before the sweep frees (or
    /// the slide re-issues) the referent's storage. A dead `Reference` is left
    /// to the sweep. Takes the set; one lock and nothing else when it is
    /// empty.
    ///
    /// Called by `collect_garbage` after the kept-soft remark, so a referent
    /// that finalizer resurrection or the remark marked is kept (the VM round
    /// then clears a weak reference to a resurrected object itself, as
    /// before).
    ///
    /// zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): STAGES the rows instead;
    /// the collection then arms the read gate when it parks its sweep
    /// ([`Self::arm_referent_read_gate`]) and runs the clear before its own
    /// sweep when it does not ([`Self::run_staged_referent_clear`]).
    pub(super) fn clear_dead_vm_referents_stw(&self) {
        let st = &self.counters.crefs;
        let mut rows = std::mem::take(&mut *st.stw_skip.lock());
        for shard in &*self.counters.discovered_weak_phantom {
            let v = shard.lock();
            rows.extend(v.iter().map(|&(base, _, _)| base));
        }
        let has_persistent = st.stw_skip_persistent.swap(false, Ordering::Relaxed);
        if rows.is_empty() && !has_persistent {
            return;
        }
        if st.clear_after_release.load(Ordering::Relaxed) {
            if !rows.is_empty() {
                rows.sort_unstable();
                rows.dedup();
                *st.staged.lock() = rows;
            }
            if has_persistent {
                st.staged_persistent.store(true, Ordering::Relaxed);
            }
            return;
        }
        let mut cleared = if rows.is_empty() {
            0
        } else {
            self.clear_dead_referent_rows(&rows)
        };
        if has_persistent {
            cleared += self.clear_dead_persistent_referents();
        }
        st.cycle_cleared.store(cleared, Ordering::Relaxed);
        st.stw_cleared.fetch_add(cleared, Ordering::Relaxed);
    }

    #[inline]
    fn clear_dead_referent_slot_cached(
        &self,
        addr: usize,
        slot0_cache: &mut (u32, usize),
    ) -> bool {
        if addr == 0 || !self.registry.contains(addr) || !self.mark_is_set(addr) {
            return false;
        }
        // SAFETY: `addr` is a registered, marked allocation base, so its header
        // and slots are valid.
        let header = unsafe { &*(addr as *const cratonvm_gc_core::ObjectHeader) };
        if header.kind() != cratonvm_gc_core::ObjectKind::Object {
            return false;
        }
        if !header.is_forwarded()
            && cratonvm_types::is_compact_object(header)
        {
            let cid = header.class_id.as_u32();
            let off = if slot0_cache.0 == cid && slot0_cache.1 != 0 {
                Some(slot0_cache.1)
            } else if let Some((offset, storage)) = super::zgc_compact_field_storage(header, 0) {
                if storage.is_reference() && storage.size_runtime() == 8 {
                    *slot0_cache = (cid, offset);
                    Some(offset)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(off) = off {
                // SAFETY: `off` is the validated 8-byte reference slot 0 offset.
                let r = unsafe { *((addr + off) as *const usize) };
                if r == 0 || (self.registry.contains(r) && self.mark_is_set(r)) {
                    return false;
                }
                // A verdict, not a semantic overwrite: no SATB publication.
                unsafe { *((addr + off) as *mut usize) = 0 };
                return true;
            }
        }
        let reference = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        if let Value::Object(Some(referent)) = self.get_field(reference, 0) {
            let r = referent.as_ptr() as usize;
            if !(self.registry.contains(r) && self.mark_is_set(r)) {
                // A verdict, not a semantic overwrite: no SATB publication.
                self.set_field_no_satb(reference, 0, Value::Object(None));
                return true;
            }
        }
        false
    }

    /// The clear's loop: null slot 0 of each live row whose referent is not a
    /// registered, marked object.
    fn clear_dead_referent_rows(&self, rows: &[usize]) -> usize {
        let mut cleared = 0usize;
        let mut cache = (u32::MAX, 0usize);
        for &addr in rows {
            if self.clear_dead_referent_slot_cached(addr, &mut cache) {
                cleared += 1;
            }
        }
        cleared
    }

    fn clear_dead_persistent_referents(&self) -> usize {
        let Some(bits) = self.counters.skipbits.persistent_bitmap() else {
            return 0;
        };
        let nwords = bits.nwords();
        let plen = self.counters.skipbits.persistent_len();
        if plen < 16_384 || nwords < 64 {
            let mut cleared = 0usize;
            let mut cache = (u32::MAX, 0usize);
            self.counters.skipbits.persistent_for_each(|addr| {
                if self.clear_dead_referent_slot_cached(addr, &mut cache) {
                    cleared += 1;
                }
            });
            return cleared;
        }
        static POOL: std::sync::OnceLock<cratonvm_gc_core::evac_pool::EvacPool> = std::sync::OnceLock::new();
        let pool = POOL.get_or_init(|| cratonvm_gc_core::evac_pool::EvacPool::with_name(0, "zgc-refs"));
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 16);
        let cleared_total = std::sync::atomic::AtomicUsize::new(0);
        pool.run_parallel(workers, |w, n| {
            let chunk = nwords.div_ceil(n);
            let start = w * chunk;
            if start >= nwords {
                return;
            }
            let end = (start + chunk).min(nwords);
            let mut local_cleared = 0usize;
            let mut cache = (u32::MAX, 0usize);
            bits.for_each_base_word_range(start, end, &mut |addr| {
                if self.clear_dead_referent_slot_cached(addr, &mut cache) {
                    local_cleared += 1;
                }
            });
            if local_cleared != 0 {
                cleared_total.fetch_add(local_cleared, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let mut cleared = cleared_total.into_inner();
        let mut cache = (u32::MAX, 0usize);
        bits.for_each_spill(&mut |addr| {
            if self.clear_dead_referent_slot_cached(addr, &mut cache) {
                cleared += 1;
            }
        });
        cleared
    }

    pub(super) fn arm_referent_read_gate(&self) {
        let st = &self.counters.crefs;
        let staged = st.staged.lock();
        let has_persistent = st.staged_persistent.load(Ordering::Relaxed)
            && self.counters.skipbits.persistent_len() > 0;
        if staged.is_empty() && !has_persistent {
            return;
        }
        if !staged.is_empty() {
            *st.gate_rows.write() = staged.clone();
        } else {
            st.gate_rows.write().clear();
        }
        st.gate_persistent.store(has_persistent, Ordering::Release);
        st.deferred_clears.fetch_add(1, Ordering::Relaxed);
        st.gate.store(ZGC_REFERENT_GATE_ARMED, Ordering::Release);
    }

    pub fn run_staged_referent_clear(&self) -> usize {
        let st = &self.counters.crefs;
        let _run = st.clear_run.lock();
        let rows = std::mem::take(&mut *st.staged.lock());
        let has_persistent = st.staged_persistent.swap(false, Ordering::AcqRel);
        let armed = st.gate.load(Ordering::Acquire) == ZGC_REFERENT_GATE_ARMED;
        if rows.is_empty() && !has_persistent && !armed {
            return 0;
        }
        let mut cleared = if rows.is_empty() {
            0
        } else {
            self.clear_dead_referent_rows(&rows)
        };
        if has_persistent {
            cleared += self.clear_dead_persistent_referents();
        }
        st.cycle_cleared.store(cleared, Ordering::Relaxed);
        st.stw_cleared.fetch_add(cleared, Ordering::Relaxed);
        if armed {
            st.after_release_cleared
                .fetch_add(cleared, Ordering::Relaxed);
            st.gate_persistent.store(false, Ordering::Release);
            st.gate.store(ZGC_REFERENT_GATE_DISARMED, Ordering::Release);
        }
        cleared
    }

    /// zt t32: arrange `CRATONVM_ZGC_CONCURRENT_REFS` and
    /// `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR` on this heap. **Test seam**
    /// (production reads both once, at construction).
    #[cfg(test)]
    pub fn dbg_set_concurrent_referent_clear(&self, refs: bool, after_release: bool) {
        let st = &self.counters.crefs;
        st.enabled.store(refs, Ordering::Relaxed);
        st.clear_after_release
            .store(refs && after_release, Ordering::Relaxed);
    }

    /// zt t32: the read gate's state (`ZGC_REFERENT_GATE_*`): one `Acquire`
    /// load, taken BEFORE the reader trusts the slot it read.
    #[inline]
    pub fn referent_gate_state(&self) -> u8 {
        self.counters.crefs.gate.load(Ordering::Acquire)
    }

    pub fn referent_gate_state_ptr(&self) -> usize {
        if self.referent_gate_may_arm() {
            self.counters.crefs.gate.as_ptr() as usize
        } else {
            0
        }
    }

    /// j27 wave 5 (lane REFS): can the read gate ever be armed on this heap?
    /// Only [`Self::arm_referent_read_gate`] arms it, over rows that
    /// [`Self::clear_dead_vm_referents_stw`] stages, which it does only with
    /// `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR` on this heap (a construction
    /// seed). `false` therefore means every referent read of this heap's life
    /// answers what its slot holds, as on Gen and G1 -- the JIT's
    /// compare-only (`refersTo`) referent row asks this. A gate already out
    /// of `NEVER` answers `true` whatever the seed says.
    pub fn referent_gate_may_arm(&self) -> bool {
        let st = &self.counters.crefs;
        st.clear_after_release.load(Ordering::Relaxed)
            || st.gate.load(Ordering::Acquire) != ZGC_REFERENT_GATE_NEVER
    }

    /// zt t32 (step 2b): while the gate is armed, is `referent`, read from slot
    /// 0 of `receiver`, one the staged clear will null -- `receiver` a hidden
    /// row that is itself live, `referent` not a registered, marked object
    /// (the clear's own verdict)? The caller then answers `null`, as HotSpot's
    /// resurrection-blocked load does. Counts the `true` answers.
    pub fn referent_read_blocked(&self, receiver: usize, referent: usize) -> bool {
        let st = &self.counters.crefs;
        let in_rows = (st.gate_persistent.load(Ordering::Acquire)
            && self.counters.skipbits.persistent_contains(receiver))
            || st.gate_rows.read().binary_search(&receiver).is_ok();
        let blocked = in_rows
            && self.registry.contains(receiver)
            && self.mark_is_set(receiver)
            && !(self.registry.contains(referent) && self.mark_is_set(referent));
        if blocked {
            st.gate_blocked.fetch_add(1, Ordering::Relaxed);
        }
        blocked
    }

    /// zt t22/zc3 (the census): the VM door's laps of this collection's
    /// reference round -- the pre-collection pass, `process_references_after_gc`
    /// and the JNI weak-global sweep, in microseconds. Prints
    /// `[GC] zgc-vm-refs:` under `--verbose:gc`; accumulated for the summary.
    pub fn note_vm_reference_round(&self, pre_us: u64, post_us: u64, jni_us: u64) {
        self.note_vm_reference_round_after_release(pre_us, post_us, jni_us, 0);
    }

    /// zt t31/zc11 (1c step 2c): as [`Self::note_vm_reference_round`], for a
    /// round whose body ran after the release (`after_us` of it): the summary
    /// still counts it in `crf_post_us`, but the pause figure a deferred cycle
    /// prints (`cp_vm_refs_pause_us=`) is the in-pause laps only.
    pub fn note_vm_reference_round_after_release(
        &self,
        pre_us: u64,
        post_us: u64,
        jni_us: u64,
        after_us: u64,
    ) {
        let st = &self.counters.crefs;
        let in_pause = pre_us.saturating_add(post_us).saturating_add(jni_us);
        let round = in_pause.saturating_add(after_us);
        st.rounds.fetch_add(1, Ordering::Relaxed);
        st.pre_us.fetch_add(pre_us, Ordering::Relaxed);
        st.post_us.fetch_add(post_us.saturating_add(after_us), Ordering::Relaxed);
        st.jni_us.fetch_add(jni_us, Ordering::Relaxed);
        st.max_round_us.fetch_max(round, Ordering::Relaxed);
        st.last_round_us.store(in_pause, Ordering::Relaxed);
        let skip = st.cycle_skip.swap(false, Ordering::Relaxed);
        let rows = st.cycle_rows.swap(0, Ordering::Relaxed);
        let cleared = st.cycle_cleared.swap(0, Ordering::Relaxed);
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            eprintln!(
                "[GC] zgc-vm-refs: cycle={} vr_pre_us={pre_us} vr_post_us={post_us} \
                 vr_jni_us={jni_us} vr_after_release_us={after_us} vr_mode={} \
                 vr_skip_rows={rows} vr_stw_cleared={cleared}",
                self.gc_count(),
                if skip { "skip" } else { "null" },
            );
        }
    }

    /// zt t22/zc3: the last noted round's pre + post + JNI microseconds.
    pub(super) fn last_vm_reference_round_us(&self) -> u64 {
        self.counters.crefs.last_round_us.load(Ordering::Relaxed)
    }

    /// zt t23/x19 (review of t22/zc3): forget the last noted round, at a
    /// deferral (`concurrent_sweep::defer_sweep_and_close_pause`). The round
    /// of the deferring cycle is noted by the VM door only after the
    /// collection returns, and a runner that installs the sweep before that
    /// note read the PREVIOUS cycle's round into this cycle's
    /// `cp_vm_refs_pause_us=`; it reads `0` now.
    pub(super) fn forget_last_vm_reference_round(&self) {
        self.counters.crefs.last_round_us.store(0, Ordering::Relaxed);
    }

    /// The `[GC] zgc-concurrent-refs:` summary figures.
    pub fn concurrent_refs_report(&self) -> ZConcRefsReport {
        let st = &self.counters.crefs;
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        let u = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ZConcRefsReport {
            enabled: st.enabled.load(Ordering::Relaxed),
            skip_cycles: l(&st.skip_cycles),
            skip_rows: l(&st.skip_rows),
            stw_cleared: l(&st.stw_cleared),
            refused: l(&st.refused),
            rounds: l(&st.rounds),
            pre_us: u(&st.pre_us),
            post_us: u(&st.post_us),
            jni_us: u(&st.jni_us),
            max_round_us: u(&st.max_round_us),
            deferred_clears: l(&st.deferred_clears),
            after_release_cleared: l(&st.after_release_cleared),
            gate_blocked: l(&st.gate_blocked),
        }
    }
}

#[cfg(test)]
mod tests;
