#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// The `young_from` guard, and the trigger predicate's lock-free read
// ---------------------------------------------------------------------------

/// A `young_from` mutex guard that republishes the GC-trigger triple on drop.
///
/// Derefs to [`Arena`], so every existing use reads unchanged. The point is the
/// `Drop`: it is the one place a mutation of that arena can be observed leaving
/// the lock, which is what makes
/// [`GenerationalHeap::needs_gc_with_jit_allocation_frame`]'s lock-free read
/// complete by construction rather than by having enumerated the writers. See
/// [`GenerationalHeap::young_from_pub_used`].
pub struct YoungFromGuard<'a> {
    pub guard: parking_lot::MutexGuard<'a, Arena>,
    pub used: &'a AtomicUsize,
    pub free: &'a AtomicUsize,
    pub capacity: &'a AtomicUsize,
}

impl std::ops::Deref for YoungFromGuard<'_> {
    type Target = Arena;
    #[inline]
    fn deref(&self) -> &Arena {
        &self.guard
    }
}

impl std::ops::DerefMut for YoungFromGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Arena {
        &mut self.guard
    }
}

impl Drop for YoungFromGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        // Three relaxed stores while still holding the lock, so the triple a
        // reader sees was consistent at SOME unlock. Relaxed is the right
        // ordering and not a shortcut: the consumer is a heuristic whose wrong
        // answer costs a collection decided one allocation early or late, and
        // whose backstop (allocation failure -> collect -> retry) is what makes
        // the collector correct. Nothing is published THROUGH these values.
        self.used.store(self.guard.used(), Ordering::Relaxed);
        self.free
            .store(self.guard.free_list_bytes(), Ordering::Relaxed);
        self.capacity
            .store(self.guard.capacity(), Ordering::Relaxed);
        // gcg g4/jn (2026-10-02): the fourth published word (the free-block
        // upper bound) went with `CRATONVM_GC_REFILL_GATE_LOCKFREE`, deleted
        // after triage tg1 found no gain.
    }
}

/// `CRATONVM_GC_TRIGGER_LOCKFREE=0` -- kill switch. With it set, the trigger
/// predicate takes `young_from.lock()` for its three numbers exactly as it did,
/// so the change is one binary apart rather than one build apart.
pub fn trigger_lockfree_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `CRATONVM_DBG_GC_TRIGGER_VERIFY=1` -- the coverage oracle for the published
/// triple.
///
/// Reads the published values, then takes the lock and compares them with the
/// arena. Under the lock the two MUST be equal: every writer holds this mutex,
/// and [`YoungFromGuard::drop`] republishes before releasing it, so a reader
/// holding the lock observes the state of the last unlock -- which is exactly
/// what was published. A divergence is therefore not a race, it is a mutation
/// that reached the arena without going through the guard, which is the only
/// way this design can be wrong.
pub fn trigger_verify_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

/// gci i11/jd: `CRATONVM_DBG_IHASH_WITNESS=1` -- the identity-hash witness for
/// the `--compatible` 4-thread javac S2 page
/// (`gci-i4orch-compatible-concurrent-javac-reads-a-module-info-class-as-source-FIXED-20261017.md`):
/// `Enum.hashCode()` caches `System.identityHashCode(this)` in a racy field,
/// so one constant answering two different hashes makes a per-thread
/// `HashMap<StandardLocation, _>` miss. Opt-in.
///
/// gci i12/sx: read ONCE PER HEAP, at construction, into
/// `GenerationalHeap::ihash_witness` (no process `OnceLock`); off, the cost in
/// `identity_hash_code` is one field load. A heap built under a test's
/// `with_thread_overrides` carries that test's answer.
pub fn gci_i11jd_ihash_witness_enabled() -> bool {
    false
}

/// The witness predicate, pure (unit-tested). Re-read after the answer was
/// chosen:
/// * `forwarded` -- the object's header is FORWARDED while a mutator hashed
///   it, i.e. a collection was copying it concurrently: an install into the
///   from-space copy is lost and the next call mints a different hash;
/// * `installed-differs` -- the header now holds a hash other than the one
///   this call returns (a lost CAS answered by a stale value, or the header
///   replaced under the call);
/// * `displaced-none` -- a locked compact instance whose monitor had no hash
///   (the resolver answered 0, which `Enum.hashCode` would re-ask next time).
pub fn gci_i11jd_ihash_verdict(
    returned: i32,
    installed_now: i32,
    forwarded: bool,
    displaced: bool,
) -> Option<&'static str> {
    if forwarded {
        return Some("forwarded");
    }
    if displaced {
        return (returned == 0).then_some("displaced-none");
    }
    if installed_now != 0 && installed_now != returned {
        return Some("installed-differs");
    }
    None
}

#[cold]
#[inline(never)]
pub fn gci_i11jd_ihash_witness(
    obj_ref: ObjectRef,
    header: &ObjectHeader,
    returned: i32,
    displaced: bool,
) {
    let forwarded = header.is_forwarded();
    let installed_now = if forwarded {
        0
    } else {
        header.installed_identity_hash()
    };
    if let Some(kind) = gci_i11jd_ihash_verdict(returned, installed_now, forwarded, displaced) {
        let class = cratonvm_gc_core::collector::class_name_for_diagnostics(header.class_id.as_u32());
        let thread = std::thread::current();
        eprintln!(
            "[IHASH-WITNESS] {kind} obj={:#x} class={class} returned={returned} installed_now={installed_now} thread={}",
            obj_ref.as_ptr() as usize,
            thread.name().unwrap_or("?"),
        );
    }
}


/// Checks made by the verifier, and the ones that found a divergence.
///
/// Printed as a PAIR for the reason every engagement counter here is: a zero
/// divergence count is equally consistent with "the publish is complete" and
/// with "the verifier never ran", and only the check count tells them apart.
pub static TRIGGER_VERIFY_CHECKS: AtomicUsize = AtomicUsize::new(0);
pub static TRIGGER_VERIFY_DIVERGED: AtomicUsize = AtomicUsize::new(0);

/// `(checks, divergences)` -- see [`TRIGGER_VERIFY_CHECKS`].
pub fn gc_trigger_verify_counts() -> (usize, usize) {
    (
        TRIGGER_VERIFY_CHECKS.load(Ordering::Relaxed),
        TRIGGER_VERIFY_DIVERGED.load(Ordering::Relaxed),
    )
}
// Compact reference-field layout (CRATONVM_COMPACT_REF_FIELDS). Reference
// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of each young-generation semi-space.
///
/// Bumped from 16 MB to 64 MB to reduce minor-GC frequency under
/// allocation-heavy workloads (the RealWorldBench "GC stress" phase
/// was 26× slower than HotSpot because it triggered full GC after
/// every ~16 MB of allocation). HotSpot defaults to ~25% of max heap
/// for the young gen; 64 MB for a 256 MB max heap is the same ratio.
pub const DEFAULT_YOUNG_SEMI_SIZE: usize = 64 * 1024 * 1024;

/// Size of the old generation (128 MB).
pub const DEFAULT_OLD_GEN_SIZE: usize = 128 * 1024 * 1024;

/// Number of minor GC survivals before an object is promoted to old gen.
///
/// gen r4w6/young6: the FIXED promotion age. With adaptive tenuring engaged
/// (`CRATONVM_GC_ADAPTIVE_TENURING`, default ON since gcf vf3, or a `-XX:`
/// tenuring flag) the per-cycle value comes from
/// [`GenerationalHeap::effective_promotion_age`] instead; every tenuring site
/// reads that, never this constant directly. With the default flipped, this
/// is the `CRATONVM_GC_ADAPTIVE_TENURING=0` arm.
pub const PROMOTION_AGE: u8 = 3;

/// `CRATONVM_GC_ADAPTIVE_TENURING` as the young collector reads it: default ON
/// since gcf vf3 (`=0` restores the fixed [`PROMOTION_AGE`]).
///
/// gcg g1/y: read exactly, in tests too. The `cfg(test)` default that read an
/// UNSET variable as OFF (gcf f5/y) is gone: this crate's unit tests that
/// assume the fixed-age schedule now pin it per heap
/// ([`GenerationalHeap::with_fixed_promotion_age`]); the rest run the
/// production default.
pub fn adaptive_tenuring_flag() -> bool {
    gc_flags().gc_adaptive_tenuring
}

/// The generational tenuring decision — the one copy of it.
///
/// `gc_age` is the age the object carries *now*, before this cycle's survival
/// is counted, so the `+ 1` asks "will surviving this collection take it to
/// `promotion_age`?". `force_promote_all` short-circuits the age entirely: the
/// moving path sets it after a cycle that observed high survival pressure, and
/// the non-moving sweep passes its `defrag_escalate` in the same slot.
///
/// # Why this is a function and not three expressions
///
/// It used to be three expressions, and they drifted. The parallel evacuator
/// ([`crate::gen_evac::ParEvac::evacuate`]) has always saturated; the serial
/// [`GenerationalHeap::forward_object_impl`] and the non-moving sweep's
/// selective-promotion pass each read a bare `gc_age() + 1`. `gc_age` is a
/// `u8` and every *increment* in this file saturates, so 255 is reachable —
/// an object that keeps surviving while old gen is full parks there — and
/// `255 + 1` is an arithmetic overflow: a panic inside the collector in a
/// debug build, and in release a wrap to 0 that makes the oldest survivor in
/// the nursery read as the youngest and never tenure again. The 2026-09-20 GC
/// round fixed both, and this extraction is what stops a fourth copy from
/// appearing.
///
/// The boundary is unreachable from any constructible heap fixture in
/// reasonable time — `the_parallel_and_serial_evacuators_agree` compares the
/// two forwarding maps but cannot get an object to age 255 — so
/// `should_tenure_is_exhaustively_total` tests this predicate directly over
/// every `(gc_age, force_promote_all, promotion_age)` triple that matters.
/// That is the only form of the test that can see it.
#[inline]
pub fn should_tenure(gc_age: u8, force_promote_all: bool, promotion_age: u8) -> bool {
    force_promote_all || gc_age.saturating_add(1) >= promotion_age
}

/// The per-object diagnostic gates of the SERIAL evacuator
/// ([`GenerationalHeap::forward_object_impl`]), read ONCE per moving cycle
/// instead of once per copied object (gen r4w2/youngpolicy, 2026-09-23).
///
/// Every field is a latched flag (`GcFlags` snapshot or a `OnceLock`), so
/// within one stop-the-world cycle a per-object re-read can only return the
/// same answer; what it cost was about eight `flags()` / `OnceLock` loads and
/// branches per copied object on a path whose useful work for the median young
/// object is a `memcpy` of about as many words. See
/// `docs/feature-designs/generative-gc-architecture.md`,
/// item 1. `Default` is "every diagnostic off", which is what a production run
/// reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyDiag {
    /// `CRATONVM_DBG_FWDGUARD` ([`fwdguard_enabled`]).
    pub fwdguard: bool,
    /// `CRATONVM_FWD_RESOLVE_STRICT` ([`fwd_resolve_strict`]).
    pub fwd_resolve_strict: bool,
    /// `CRATONVM_DBG_CELLCORRUPT` ([`cell_corrupt_diag_enabled`]).
    pub cell_corrupt: bool,
    /// `CRATONVM_DBG_WATCH_CELL` ([`cratonvm_gc_core::heap::cell_watch_addr`]); `0` = off.
    pub cell_watch: usize,
    /// `CRATONVM_DBG_OBJ_WATCH` ([`obj_watch_pattern`]).
    pub obj_watch: bool,
    /// `CRATONVM_DBG_GCWRITE` ([`gcw_enabled`]).
    pub gcwrite: bool,
    /// `CRATONVM_DBG_DESCTRACE` ([`desc_trace_enabled`]).
    pub desc_trace: bool,
    /// `CRATONVM_DBG=gcpause` as [`copy_tally_arm_enabled`] latches it: count
    /// the re-encounter / copy arms (gen r4w3/young2 — this was the last
    /// per-object `OnceLock` read on the serial copy, the residual the
    /// serial-evacuator page left after item 1).
    pub tally_arm: bool,
}

impl CopyDiag {
    /// The gates as they stand now. Called once per moving cycle, before the
    /// first forward.
    pub fn current() -> Self {
        Self {
            fwdguard: fwdguard_enabled(),
            fwd_resolve_strict: fwd_resolve_strict(),
            cell_corrupt: cell_corrupt_diag_enabled(),
            cell_watch: cratonvm_gc_core::heap::cell_watch_addr(),
            obj_watch: obj_watch_pattern().is_some(),
            gcwrite: gcw_enabled(),
            desc_trace: desc_trace_enabled(),
            tally_arm: copy_tally_arm_enabled(),
        }
    }
}

/// Everything the serial evacuator's per-object decision needs that is fixed
/// for the whole cycle: the tenuring override and the diagnostic gates.
/// Passed by value (it is a handful of words) to every
/// [`GenerationalHeap::forward_object`] call in `collect_garbage_inner`.
#[derive(Clone, Copy, Debug)]
pub struct CopyPolicy {
    /// Promote-on-pressure: tenure every survivor regardless of age. See
    /// [`should_tenure`].
    pub force_promote_all: bool,
    /// `should_tenure`'s `promotion_age` for this cycle (gen r4w6/young6):
    /// [`PROMOTION_AGE`] unless adaptive tenuring is engaged, in which case
    /// it is the heap's adaptive threshold + 1. See
    /// [`GenerationalHeap::effective_promotion_age`].
    pub promotion_age: u8,
    pub diag: CopyDiag,
}

impl Default for CopyPolicy {
    /// The historical policy: no pressure arm, [`PROMOTION_AGE`], every
    /// diagnostic off. Written out (it was `#[derive(Default)]`) because a
    /// derived `promotion_age` of 0 would mean "tenure everything".
    fn default() -> Self {
        Self {
            force_promote_all: false,
            promotion_age: PROMOTION_AGE,
            diag: CopyDiag::default(),
        }
    }
}

/// The serial promotion buffer's first carve (gen r4w4/young4). The three
/// sizes mirror `gen_evac`'s `OLD_PLAB_MIN` / `OLD_PLAB_MAX` /
/// `OLD_PLAB_DIRECT_MIN`, which were measured there; the serial path has no
/// reason to differ (it is the same old-gen allocator, one worker).
pub const SERIAL_OLD_PLAB_MIN: usize = 16 * 1024;
/// The serial promotion buffer's ceiling; each refill doubles up to this.
pub const SERIAL_OLD_PLAB_MAX: usize = 256 * 1024;
/// A promoted object at least this large takes its own old-gen block, so one
/// large array cannot strand most of a buffer.
pub const SERIAL_OLD_PLAB_DIRECT_MIN: usize = 32 * 1024;

/// The SERIAL moving young cycle's promotion state: the promoted-object scan
/// worklist and a promotion buffer (PLAB) carved from old gen.
///
/// gen r4w4/young4 (2026-09-24), item 3 of
/// `docs/feature-designs/generative-gc-architecture.md`.
/// Promotion on the serial path was one `OldGen::alloc_unzeroed` per object:
/// a bucket walk, a best-fit scan, the remainder re-pushed and the sorted
/// free-list cache invalidated, per promoted object. The parallel evacuator
/// replaced exactly that with per-worker buffers in gen-gc-five item 4
/// (`gen_evac::ParEvac::promote_alloc`); this is the same shape for the one
/// serial worker: carve with [`OldGen::alloc_unzeroed_buffer`], bump objects
/// out of it, and hand the never-written tail back with
/// [`OldGen::release_unused_tail`] through [`Self::retire_plab`].
///
/// # The walkability contract, and where it is discharged
///
/// `alloc_unzeroed` hands out a block no old-gen walker may reach before it
/// holds objects end to end; a buffer's tail holds garbage until it is
/// released. The serial copy reaches no old-gen walk while a buffer is live:
/// the dirty-card scan (and the opt-in full rset scan) collect their roots
/// BEFORE the first forward, and the promoted-object scan visits promoted
/// objects by ADDRESS from this worklist, never linearly. The first old-gen
/// walk after the copy is the `loader_pin`-from-old-instances rescue, so
/// `collect_garbage_inner` retires the buffer right after `cheney_drain`, and
/// again after Phase 2.5b (finalizer resurrection may carve a second one),
/// before the `CRATONVM_DBG` verifier walk and Phase 5. A buffer the drain
/// consumed exactly has no tail to return.
///
/// The worklist half is the old `Vec<*mut u8>` behind two forwarding methods,
/// so every `promoted_worklist.push/.pop()` call site reads unchanged.
pub struct PromotionQueue {
    /// Promoted objects awaiting their scan (the CRIT-P2 worklist).
    pub worklist: Vec<*mut u8>,
    /// Next free address in the current buffer, or 0 when none is held.
    pub plab_cursor: usize,
    /// One past the last byte of the current buffer.
    pub plab_end: usize,
    /// Size of the next buffer: 0 = [`SERIAL_OLD_PLAB_MIN`], then doubling to
    /// [`SERIAL_OLD_PLAB_MAX`] while the cycle keeps promoting.
    pub next_plab: usize,
    /// Buffers carved this cycle (diagnostics and tests).
    pub carves: u64,
    /// The pinned in-place young copy's state (gen r4w5/pinned5), `Some` only
    /// on a cycle that runs it. Carried here because this is the one piece of
    /// serial-evacuator state every `forward_object` call site already passes
    /// by `&mut`, so the pinned cycle needs no new parameter at any of them.
    pub in_place: Option<Box<InPlaceEvac>>,
    /// gen r4w6/young6: this cycle's AGE TABLE — bytes of survivors that
    /// stayed young, by their NEW age (HotSpot's `AgeTable`). Filled only
    /// when [`Self::age_census`] is set (adaptive tenuring engaged, or
    /// `-XX:+PrintTenuringDistribution`), by the serial copy in
    /// `forward_object_impl` and, on a parallel cycle, by the driver's shard
    /// merge. Carried here for the reason `in_place` is: every serial forward
    /// already takes this struct by `&mut`.
    pub ages: [u64; tenuring::AGE_TABLE_LEN],
    /// Fill [`Self::ages`] this cycle. Off by default, so the default copy
    /// path pays one predictable branch per young survivor and nothing else.
    pub age_census: bool,
    /// gcd d2/h: the card table's base when its reference-array marks are
    /// element-precise, `None` otherwise; read once per cycle. The promoted
    /// scans hand it to [`HolderCardDefer`]. Carried here for the reason
    /// `in_place` is.
    pub element_card_base: Option<usize>,
    /// gcd d3/m: the heap's refusal ledger, `Some` on a production moving
    /// cycle (an `Arc` clone taken once per cycle). The serial evacuator's
    /// refusals land there instead of in a process static. Carried here for
    /// the reason `in_place` is. `None` (unit tests driving the evacuator
    /// directly) records nothing.
    pub refusal_ledger: Option<Arc<ForwardRefusalLedger>>,
    /// gci i1/yg: how much a survivor that stays young ages on this cycle --
    /// 1, unless the HotSpot age clock (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`)
    /// set the cycle's step ([`GenerationalHeap::age_clock_step`]); the
    /// serial copy tenures with [`clock_promotion_age`] of it. Carried here
    /// for the reason `in_place` is.
    pub age_step: u8,
    /// gcki-pin-age (`CRATONVM_GEN_PINNED_COPY_AGES_IN_PLACE`, default ON):
    /// `Some(tenure_age)` when a pinned in-place cycle's pinned and kept
    /// survivors age where they stay; `tenure_age` is the `promotion_age`
    /// [`should_tenure`] asks this cycle (0 under a forced promotion). Read
    /// once per cycle. `None`: they keep their age.
    pub in_place_tenure_age: Option<u8>,
    /// gci i10/lp (`CRATONVM_GEN_YOUNG_LOADER_PIN_SNAPSHOT`, default ON): this
    /// cycle's `loader_pin` rows for young from-space, taken once
    /// (`gen_evac::young_loader_pin_snapshot`); [`young_loader_pin`] answers
    /// from it instead of the registry's read lock per scanned object. `None`
    /// (switch off, pinning off, empty registry): the per-object lookup.
    /// Carried here for the reason `in_place` is.
    pub loader_pins: Option<FxHashMap<u32, usize>>,
    /// gcj yg3-residuals R3 (`CRATONVM_GEN_SERIAL_YOUNG_GEOMETRY`): to-space
    /// bytes past which the serial copy tenures a survivor instead of copying
    /// it young (HotSpot Serial's survivor space); `usize::MAX`, no cap.
    /// Carried here for the reason `in_place` is.
    pub to_space_cap: usize,
    /// Smallest old-gen promotion size refused this cycle (0 = none yet).
    pub old_failed_min: usize,
}

impl PromotionQueue {
    pub fn new() -> Self {
        Self {
            worklist: Vec::new(),
            plab_cursor: 0,
            plab_end: 0,
            next_plab: 0,
            carves: 0,
            in_place: None,
            ages: [0; tenuring::AGE_TABLE_LEN],
            age_census: false,
            element_card_base: None,
            refusal_ledger: None,
            age_step: 1, // gci i1/yg
            in_place_tenure_age: None, // gcki-pin-age
            loader_pins: None, // gci i10/lp
            to_space_cap: usize::MAX, // gcj yg3-residuals R3
            old_failed_min: 0,
        }
    }

    /// Count one survivor that stayed young, at its NEW age (gen
    /// r4w6/young6). The age is clamped into the table; it cannot exceed
    /// `MAX_GC_AGE` today (`set_gc_age` saturates there), so the clamp is a
    /// bound check, not a policy.
    #[inline]
    pub fn note_survivor_age(&mut self, age: u8, bytes: usize) {
        let i = usize::from(age).min(tenuring::AGE_TABLE_LEN - 1);
        self.ages[i] = self.ages[i].saturating_add(bytes as u64);
    }

    /// The next from-space object the pinned in-place copy must scan in
    /// place: a pinned object, a copy it made into a free span, or a survivor
    /// it could place nowhere. `None` on every other cycle.
    #[inline]
    pub fn pop_in_place(&mut self) -> Option<*mut u8> {
        self.in_place.as_deref_mut()?.scan.pop()
    }

    /// Queue a promoted object for its scan.
    #[inline]
    pub fn push(&mut self, p: *mut u8) {
        self.worklist.push(p);
    }

    /// The next promoted object to scan.
    #[inline]
    pub fn pop(&mut self) -> Option<*mut u8> {
        self.worklist.pop()
    }

    /// Bump `size` bytes out of the current buffer, refusing an allocation
    /// that would leave exactly 8 bytes: an 8-byte remainder can neither go
    /// back to the old-gen free list (its minimum block is `HEADER_SIZE`) nor
    /// carry an `int[]` filler. The tail is therefore always 0 or at least
    /// `HEADER_SIZE`, which is `release_unused_tail`'s contract. The same rule
    /// as `gen_evac::ParEvac::old_lab_alloc`.
    #[inline]
    pub fn bump(&mut self, size: usize) -> Option<*mut u8> {
        if self.plab_cursor == 0 {
            return None;
        }
        let after = self.plab_cursor.checked_add(size)?;
        if after > self.plab_end || self.plab_end - after == 8 {
            return None;
        }
        let p = self.plab_cursor;
        self.plab_cursor = after;
        Some(p as *mut u8)
    }

    /// Old-gen destination for a promoted object of `size` bytes (8-aligned,
    /// at least `HEADER_SIZE`): the current buffer, a fresh buffer, or — when
    /// old gen cannot spare a buffer — the object's own block. `None` means old
    /// gen is full and the caller falls back to to-space, exactly as before.
    pub fn alloc(&mut self, old_gen: &mut OldGen, size: usize) -> Option<*mut u8> {
        if self.old_failed_min != 0 && size >= self.old_failed_min {
            return None;
        }
        if size >= SERIAL_OLD_PLAB_DIRECT_MIN {
            let res = old_gen.alloc_unzeroed(size, 8);
            if res.is_none() {
                self.old_failed_min = if self.old_failed_min == 0 {
                    size
                } else {
                    self.old_failed_min.min(size)
                };
            }
            return res;
        }
        if let Some(p) = self.bump(size) {
            return Some(p);
        }
        self.retire_plab(old_gen);
        let want = if self.next_plab == 0 {
            SERIAL_OLD_PLAB_MIN
        } else {
            self.next_plab
        };
        let mut plab = want.max(size);
        if plab - size == 8 {
            // The first bump must not leave the 8-byte tail `bump` refuses.
            plab += 8;
        }
        match old_gen.alloc_unzeroed_buffer(plab, 8) {
            Some(base) => {
                let base = base as usize;
                self.carves += 1;
                self.next_plab = want.saturating_mul(2).min(SERIAL_OLD_PLAB_MAX);
                self.plab_cursor = base;
                self.plab_end = base + plab;
                self.bump(size)
            }
            // No buffer-sized block: the object on its own, or nothing.
            None => {
                let res = old_gen.alloc_unzeroed(size, 8);
                if res.is_none() {
                    self.old_failed_min = if self.old_failed_min == 0 {
                        size
                    } else {
                        self.old_failed_min.min(size)
                    };
                }
                res
            }
        }
    }

    /// Return the never-written tail of the current buffer to old gen's free
    /// list, and hold no buffer. Idempotent; a no-op when no buffer is held or
    /// it was consumed exactly.
    pub fn retire_plab(&mut self, old_gen: &mut OldGen) {
        let (cur, end) = (self.plab_cursor, self.plab_end);
        self.plab_cursor = 0;
        self.plab_end = 0;
        if end > cur && cur != 0 {
            let tail = end - cur;
            debug_assert!(
                tail >= HEADER_SIZE && tail % 8 == 0,
                "a serial promotion buffer tail must be a free-list-sized block (tail={tail})"
            );
            // SAFETY: `[cur, end)` is the unused remainder of a block carved
            // from THIS old gen by `alloc_unzeroed_buffer` in `alloc`; `bump`
            // only ever hands out bytes below `cur`, so nothing was written
            // there, and the tail is 0 or >= HEADER_SIZE by `bump`'s rule.
            unsafe { old_gen.release_unused_tail(cur as *mut u8, tail) };
        }
    }
}

// ---------------------------------------------------------------------------
// gen r4w5/pinned5 (2026-09-24): the pinned in-place young copy.
//
// `CRATONVM_GEN_PINNED_YOUNG_COPY`. A young cycle that the conservative-JIT-
// root term (`unrewritable_conservative_jit_roots`) would send to the
// non-moving sweep copies instead, and leaves in place every object on a 4 KiB
// page that one of the pause's unrewritable conservative words lands in
// (`gc_quiescence::pause_young_pin_words`, interior and derived words
// included). Design, option C of
// `docs/feature-designs/generative-gc-architecture.md`.
// As built — and where it departs from that page, and why —
// `docs/feature-designs/generative-gc-architecture.md`.
//
// The one structural decision: the pinned pages stay in FROM-space, and
// from-space stays the allocation space. Every other young survivor is copied
// into bytes of from-space that were FREE when the cycle began (its free
// blocks and its un-bumped tail) — never into a byte a live object occupies —
// or promoted, and the semi-spaces are NOT swapped. So between cycles every
// live young object is in from-space, exactly as after a non-moving sweep, and
// no consumer that assumes that (the non-moving sweep, the major collector's
// young walks, the concurrent marker's young roots, the card scan, the
// dangling-reference instruments, the evacuated-space wipe and give-back) has
// to learn about pinned spans. And because a destination is a byte that was
// free at the start, `pointer_map`'s keys and values stay disjoint (pinned
// objects map to themselves), which `remap_external_roots`'s double
// application relies on.
//
// gen r4w6/pinstale6 (2026-09-24): the pin words are the pause's ledger, which
// now also holds every REGISTER word the cross-thread scan read from a peer
// (`gc_quiescence::record_peer_reg` → `YoungPinLedger::note_peer_reg_word`),
// plus — added at the plan, once the object-start bitmap exists — every word
// a blocked peer's native stack holds into from-space that is not an object
// start (`gc_quiescence::peer_stack_slot_values_into`). Those two are the
// populations of the `[peer-reg-stale]` pairing that no channel rewrites: the
// blocked-wake fold rewrites a blocked peer's stack BASES and nothing else.
// With them pinned, the pairing's `stale_live=` is zero on a pinned cycle by
// construction. See `docs/feature-designs/generative-gc-architecture.md`.
//
// gcd d3/m (2026-09-27): two more populations join the ledger, both read by
// the helper-window pass from OUTSIDE a blocked peer, as whole BANDS (register
// file plus every word of `[rsp, top)` in a young region, raw, bases
// included), each crediting its thread's JIT depth
// (`gc_quiescence::YoungPinLedger::note_peer_band`):
//
// * `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER` (opt-in): every helper window
//   the pass pinned whole, so a pause whose only unrewritable peers are such
//   windows can run this copy instead of the sweep (the take-over arm of
//   `collect_garbage_inner_with_pins`; `pinned_copy_takes_takeover_cycle`);
// * `CRATONVM_XT_BLOCKED_MONITOR_PROOF` (opt-in) with this copy or option B
//   on: every blocked `monitorenter` peer credited by proof, whose Rust frames
//   and registers the proof does not cover.
// ---------------------------------------------------------------------------

/// `log2` of the pin page: a conservative word pins the 4 KiB page it lands in.
pub const PIN_PAGE_SHIFT: u32 = 12;

/// Bytes of one pin page.
pub const PIN_PAGE: usize = 1 << PIN_PAGE_SHIFT;

/// A word also pins every page within this many bytes BELOW it, so a derived
/// pointer one past the end of its object still pins the object's page when
/// the two straddle a page line.
pub const PIN_WORD_SLACK: usize = 64;

/// A word also pins every page within this many bytes ABOVE it: a
/// base-minus-offset derived pointer names an object up to the ledger's low
/// slack above it (gen r4w6, review6 finding M). With the old symmetric 64, an
/// object 16 bytes into page p named only by `obj - 200` on page p-1 was not
/// pinned, was copied, and the frame's word then pointed into reused bytes.
pub const PIN_WORD_SLACK_ABOVE: usize = cratonvm_gc_core::gc_quiescence::YOUNG_PIN_LOW_SLACK;

/// A free span smaller than this is not offered to the in-place copy as a
/// destination: it can hold only a sliver of a survivor and costs a commit
/// check and a slot in the span list.
pub const IN_PLACE_MIN_DEST_SPAN: usize = 256;

/// How many destination spans past the first usable one an in-place
/// allocation searches before it gives up on the free spans (the tail span is
/// always tried as well). Bounds the per-object cost on a fragmented list.
pub const IN_PLACE_DEST_SEARCH: usize = 32;

/// gen r4w6 (orchestrator) -- young collections with no old-gen collection
/// after which the periodic shrink cycle opens a concurrent cycle
/// (`GenerationalHeap::shrink_cycle_due`).
pub const SHRINK_CYCLE_PERIOD: u64 = 16;

/// gen r4w6 (orchestrator) -- an old generation holding less than this is not
/// worth a periodic cycle: the resize could return at most this much.
pub const SHRINK_CYCLE_MIN_USED: usize = 4 * 1024 * 1024;

/// gen r4w6 (orchestrator) -- is the periodic shrink cycle due after
/// `young_since` young collections without an old-gen collection, with `used`
/// bytes in old gen? See `GenerationalHeap::shrink_cycle_due`.
pub fn periodic_shrink_cycle_due(young_since: u64, used: usize) -> bool {
    young_since >= SHRINK_CYCLE_PERIOD && used >= SHRINK_CYCLE_MIN_USED
}

/// gen r5w3/oldgen7 — the largest capacity the old generation may grow to
/// under `CRATONVM_GC_OLD_BORROW_YOUNG`: HotSpot Serial's `MaxHeapSize -
/// MaxNewSize` with its default `NewRatio = 2` (young = 1/3 of the heap), i.e.
/// `2/3 · -Xmx`, rounded down to a granule so the reservation is exact.
/// `GenerationalHeap::cede_young_budget_to_old` keeps the young trigger at or
/// below `(Xmx - old capacity) / 2`, which at this maximum is `Xmx / 6` per
/// semi-space: HotSpot's `MaxNewSize` split across the copy pair.
pub fn old_borrow_max(total: usize) -> usize {
    let two_thirds = ((total as u128) * 2 / 3) as usize;
    two_thirds / cratonvm_gc_core::reservation::GRANULE * cratonvm_gc_core::reservation::GRANULE
}

/// JIT r20 wave 18, lane gcI (`gengc-jr20-javac-footprint-wide-references-RETIRED-UNFIXED-20261018`):
/// the old generation's RESERVATION under `CRATONVM_GC_OLD_BORROW_YOUNG` --
/// `-Xmx` less a young pair of `-Xmx / 16` per semi-space. [`old_borrow_max`]
/// stays the cap of the `MinHeapFreeRatio` slack (`OldGen::set_growth_soft_max`);
/// past it the generation grows only by what a refused request needs.
///
/// Why: HotSpot Serial's full collection leaves live objects in eden when the
/// old generation is full, so all of `-Xmx` but one survivor can hold live
/// data. This collector's young pair is a copy reserve, so at the
/// `2/3 · -Xmx` cap it threw `OutOfMemoryError` with a third of the heap
/// unusable for live data: javac over the 772-file gate set needed 256m where
/// HotSpot (equally wide references) passes at 192m (gm-I5, w17q:
/// `cvm_wide_192m` OOME at `old_used=131546248 old_capacity=134217728`).
/// `GenerationalHeap::cede_young_budget_to_old` keeps the young trigger at
/// `(Xmx - old) / 2`, at least `-Xmx / 16` here.
pub fn old_borrow_hard_max(total: usize) -> usize {
    let young_floor = total / 16;
    let hard = total.saturating_sub(young_floor.saturating_mul(2));
    (hard / cratonvm_gc_core::reservation::GRANULE * cratonvm_gc_core::reservation::GRANULE).max(old_borrow_max(total))
}

/// gen r5w3/oldgen7 — HotSpot Serial's `SurvivorRatio` default: eden is eight
/// survivors, so ONE survivor space is a tenth of the young generation. The
/// copy reserve the usable-first `-Xms` split commits up front
/// ([`xms_usable_first_split`]).
pub const XMS_SURVIVOR_DIVISOR: usize = 10;

/// gen r5w3/oldgen7 — how `CRATONVM_GEN_XMS_USABLE_FIRST` divides an `-Xms`
/// of `xms` bytes: `(from-space, to-space)` prefix requests; the old
/// generation commits what the pair does not
/// (`GenerationalHeap::commit_initial_heap_usable_first`).
///
/// HotSpot Serial's model of an initial heap: the YOUNG generation's share of
/// `-Xms` is the young pair's share of the reservation (`(from + to) / total`,
/// which is `1/2` at this collector's default split and the planned split
/// under `-Xmn` / `NewRatio`); of it, one survivor (a tenth) is the copy
/// reserve and the rest is usable. So the to-space is committed only for the
/// survivor volume the first evacuation can need, and the from-space gets the
/// rest of the young share. Each request is clamped to its arena's capacity;
/// what a clamp leaves over goes to the old generation, which is where the
/// even split sent it too.
///
/// `-Xms64m` over `with_capacity(512m)` (128m semis, 256m old): young share
/// 32m, to-space 3.2m, from-space 28.8m, old 32m — `totalMemory()` (one
/// semi-space's commit plus the old generation's, granule-rounded) about
/// 62-64m against HotSpot Serial's ~62m. The even split puts 32m in each
/// semi-space and nothing in the old generation, so `totalMemory()` reads
/// 32m.
pub fn xms_usable_first_split(
    xms: usize,
    from_cap: usize,
    to_cap: usize,
    old_cap: usize,
) -> (usize, usize) {
    let pair = from_cap.saturating_add(to_cap);
    let total = pair.saturating_add(old_cap);
    if xms == 0 || total == 0 {
        return (0, 0);
    }
    // u128: `xms * pair` overflows `usize` on a 32-bit host for any real heap,
    // and on 64 bits for a heap above 4 EiB, neither of which is worth a
    // panic path.
    let young_share = ((xms as u128) * (pair as u128) / (total as u128)) as usize;
    let survivor = young_share / XMS_SURVIVOR_DIVISOR;
    let to = survivor.min(to_cap);
    let from = young_share.saturating_sub(survivor).min(from_cap);
    (from, to)
}

/// Slots of [`GenerationalHeap::pinned_young_census`].
pub const PINNED_YOUNG_CENSUS_LEN: usize = 11;

/// The pin pages `words` name inside from-space `[base, base + used)`, as page
/// indices relative to `base`, ascending and de-duplicated. A word equal to
/// `base + used` (one past the last object) counts; anything else outside the
/// span does not — a word into the idle semi-space names nothing live.
///
/// gen r4w6/pinstale6: except a word in the ledger's LOW SLACK below `base`
/// (`gc_quiescence::YOUNG_PIN_LOW_SLACK`). The ledger admits
/// `[base - slack, base)` on purpose — "a base-minus-offset derived pointer can
/// sit just below the first object of a region" — and this function used to
/// drop exactly those words, leaving the first object of from-space unpinned
/// under the derived pointer the slack exists for. Such a word pins page 0.
pub fn pinned_young_pages(words: &[usize], base: usize, used: usize) -> Vec<usize> {
    let mut pages = Vec::new();
    if used == 0 {
        return pages;
    }
    let Some(end) = base.checked_add(used) else {
        return pages;
    };
    let low = base.saturating_sub(cratonvm_gc_core::gc_quiescence::YOUNG_PIN_LOW_SLACK);
    for &w in words {
        if w < low || w > end {
            continue;
        }
        let lo = w.saturating_sub(PIN_WORD_SLACK).max(base) - base;
        // `max(base)`: a low-slack word's `w + slack` can still be below
        // `base`; it names page 0.
        let hi = w
            .saturating_add(PIN_WORD_SLACK_ABOVE)
            .max(base)
            .min(end - 1)
            - base;
        for p in (lo >> PIN_PAGE_SHIFT)..=(hi >> PIN_PAGE_SHIFT) {
            pages.push(p);
        }
    }
    pages.sort_unstable();
    pages.dedup();
    pages
}

/// Do `pages` pinned pages exceed 1/8 of a from-space of `used` bytes? Past
/// that bound the cycle diverts to the non-moving sweep, as it did before the
/// pinned copy existed: the census that sized the design
/// (`GenR4W4JitWarmDivertProbe`: 2.1 young pins per cycle on average, 9 at
/// most) sits orders of magnitude below it.
pub fn pinned_pages_over_bound(pages: usize, used: usize) -> bool {
    let from_pages = used.div_ceil(PIN_PAGE);
    pages.saturating_mul(8) > from_pages
}

/// Whether a moving cycle's pinned plan also takes this pause's blocked-peer
/// native-stack words that lie in from-space and are NOT object starts
/// (`gc_quiescence::peer_stack_slot_values_into`; the blocked-wake fold
/// rewrites exact bases only, so an interior word must pin its object).
///
/// * `pinned_copy_words`: term 4 fired alone and the pinned copy
///   (`CRATONVM_GEN_PINNED_YOUNG_COPY`) took its ledger words (gen
///   r4w6/pinstale6).
/// * `term4_ledger_cleared`: option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`)
///   stood term 4 down on a complete, empty ledger (gcd d1/e, closing gen
///   r5w4/pin8). That ledger holds deposits and peer REGISTER words, never a
///   blocked peer's stack words, so "empty" said nothing about them.
/// * `forced`: the unit-test entry supplies its own words and never mixes in
///   the capture buffer.
///
/// gch h3/yc: the h2/yg `every_cycle` input (`CRATONVM_GEN_PIN_BLOCKED_PEER_WORDS`,
/// `gcd-d1e-proposal-every-moving-cycle-pins-blocked-peer-interior-words`) is
/// deleted: vh2-yg block D found no default relocating cycle whose capture
/// held such a word, and the pinned copy already pins them where they occur.
/// The census (`blocked_peer_interior_census`) stays.
///
/// With both flags off neither input can be true, so the default path is
/// unchanged.
#[inline]
pub fn plan_takes_blocked_peer_words(
    pinned_copy_words: bool,
    term4_ledger_cleared: bool,
    forced: bool,
) -> bool {
    !forced && (pinned_copy_words || term4_ledger_cleared)
}

/// gcd d3/m: may the pinned in-place copy take a young cycle whose coverage
/// term fired (`divert_for_incomplete_moving_coverage`) -- the take-over arm
/// of `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`? Pure; the caller has already
/// checked that term 3 fired and both flags are on.
///
/// Only when term 3 fired through the take-over licence ALONE:
///
/// * moving young was requested and nothing forced the non-moving roots;
/// * no other coverage obligation is open (`other_coverage_incomplete`: the
///   per-cycle coverage proof and the unregistered-frame flag -- a helper
///   window the pass pinned whole and discharged marks neither);
/// * no other divert term holds (`other_divert_terms`, built by
///   [`takeover_other_divert_terms`]: promotion-OOM risk, an explicit full
///   GC, a GPU veto; gci i11/tk: no longer `CRATONVM_OLDGEN_COMPACT`, whose
///   Phase 5 slide [`phase5_major_no_compact`] already vetoes on every
///   ledger-pinned cycle);
/// * the verdict is one the pinned copy can honour
///   (`gc_quiescence::takeover_verdict_admits_pinned_copy`: nothing frozen,
///   nothing unread, every window pinned from a whole band with derived
///   pointers resolved).
///
/// The ledger read that follows (`gc_quiescence::pause_young_pin_words`)
/// still fails unless every window's band is in the ledger.
pub fn pinned_copy_takes_takeover_cycle(
    moving_young_requested: bool,
    force_non_moving_jit_roots: bool,
    other_coverage_incomplete: bool,
    other_divert_terms: bool,
    verdict: cratonvm_gc_core::gc_quiescence::TakeoverVerdict,
) -> bool {
    moving_young_requested
        && !force_non_moving_jit_roots
        && !other_coverage_incomplete
        && !other_divert_terms
        && cratonvm_gc_core::gc_quiescence::takeover_verdict_admits_pinned_copy(verdict)
}

/// gcj j1/yg item 1: the coverage reasons the pinned copy's UNPROVEN-FRAME
/// arm accepts (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`), as a mask
/// over `gc_quiescence::incomplete_reason`. Each names a frame whose oops no
/// precise channel rewrites but whose words the young pin ledger already
/// holds, so pinning them is the whole obligation:
///
/// * `UNREGISTERED_JIT_FRAME`: a compiled frame with no `JitEntryGuard`. It is
///   not on the chain, so no deposit classifies it: its band lies in the
///   depositing thread's layout-free sweep (`[lo, stack top)` whenever the
///   ledger licenses moves), which notes every young word;
/// * `UNPUBLISHED_FRAME_OOP`: a classified compiled frame's band holds a young
///   word no oop map names. A deposit excuses only map-named slots, so the
///   word is noted;
/// * `CROSS_THREAD_JIT_PEER`: a peer's JIT depth this thread's scan cannot
///   verify. A peer whose own proof fails deposits nothing, and the ledger
///   read (`gc_quiescence::pause_young_pin_words`) then fails on the depth it
///   does not account for: the arm declines;
/// * `XT_TAKEOVER` / `XT_HELPER_WINDOW`: admitted only under the take-over
///   verdict's pinned licence (every window read whole into the ledger).
///
/// Not accepted: every reason that says a map or a frame bound could not be
/// trusted (`NO_PRECISE_MAP`, `MISSING_EXACT_RBP`, the frame-map reasons,
/// `FOREIGN_INNERMOST_RBP`, the band-bound reasons, the oracle's refutation),
/// `OSR_SHADOW`, `JIT_RELOCATION_UNSUPPORTED`.
pub const PINNED_COPY_UNPROVEN_REASONS: usize = (1
    << cratonvm_gc_core::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME)
    | (1 << cratonvm_gc_core::gc_quiescence::incomplete_reason::UNPUBLISHED_FRAME_OOP)
    | (1 << cratonvm_gc_core::gc_quiescence::incomplete_reason::CROSS_THREAD_JIT_PEER)
    | (1 << cratonvm_gc_core::gc_quiescence::incomplete_reason::XT_TAKEOVER)
    | (1 << cratonvm_gc_core::gc_quiescence::incomplete_reason::XT_HELPER_WINDOW);

/// gcj j1/yg item 1: may the pinned in-place copy take a young cycle whose
/// coverage term fired only for [`PINNED_COPY_UNPROVEN_REASONS`] -- the
/// UNPROVEN-FRAME arm? Pure; the caller has checked that term 3 fired, the
/// pinned copy's flag and this arm's.
///
/// Only when moving young was requested, nothing forced the non-moving roots
/// (`force_non_moving_jit_roots`: a side-table or OSR promise), no other
/// divert term holds, at least one reason was recorded and every recorded one
/// is accepted, and -- when the take-over licence forbids an unpinnable move
/// -- the take-over verdict admits the pinned copy. The ledger read that
/// follows still fails (the cycle diverts as before) unless every JIT entry
/// in the process is accounted for by a complete deposit.
pub fn pinned_copy_takes_unproven_cycle(
    moving_young_requested: bool,
    force_non_moving_jit_roots: bool,
    reason_mask: usize,
    other_divert_terms: bool,
    takeover_forbids: bool,
    takeover_admits_pinned: bool,
) -> bool {
    moving_young_requested
        && !force_non_moving_jit_roots
        && !other_divert_terms
        && reason_mask != 0
        && reason_mask & !PINNED_COPY_UNPROVEN_REASONS == 0
        && (!takeover_forbids || takeover_admits_pinned)
}

/// `CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES` -- gcj j1/yg item 1: the
/// pinned copy's unproven-frame arm ([`pinned_copy_takes_unproven_cycle`]).
/// Read per cycle that reaches the arm (term 3 fired). Default ON since gcj
/// j1yg-close (2026-10-27), `=0` kills: with the 2048-word deposit cap it is
/// what lets a multi-threaded JIT-warm program (4-thread javac) copy young on
/// the cycles an unregistered or unpublished compiled frame used to sweep.
pub fn pinned_copy_takes_unproven_frames_enabled() -> bool {
    true
}

/// gci i11/tk (`gci-i8rv-default-flip-review-findings-RETIRED-RULING-20261018` item 7): the
/// take-over arm's `other_divert_terms` -- the divert terms besides term 3
/// that keep a take-over cycle off the pinned copy: promotion-OOM risk, an
/// explicit full GC, a GPU veto. Pure.
///
/// `CRATONVM_OLDGEN_COMPACT` is deliberately NOT a term any more. gcd d3/m
/// added it because the moving cycle's Phase 5 major could slide old gen
/// under the helper windows' unmapped words (the young pin ledger holds young
/// words only). gcd d4/n then closed that hazard at its source: a
/// ledger-pinned cycle -- term 4's arm and this arm alike -- hands
/// [`phase5_major_no_compact`]'s veto to `major_gc_finalizing_with`, which
/// keeps that major in place whatever asked for the slide (`OLDGEN_COMPACT`
/// or a pending fragmentation request; [`moving_major_compaction_plan`]).
/// The compaction is a property of the major, which runs after the young
/// phase and is already vetoed on exactly these cycles, so the compaction
/// intent has nothing left to divert. Term 4's arm never carried the term.
/// With `OLDGEN_COMPACT` default ON (the user's flip of 2026-10-13) it made
/// the opt-in arm decline every cycle under `ptkod_other_divert`.
pub fn takeover_other_divert_terms(
    honor_promotion_oom_risk: bool,
    explicit_full_gc: bool,
    gpu_relocation_forbidden: bool,
) -> bool {
    honor_promotion_oom_risk || explicit_full_gc || gpu_relocation_forbidden
}

/// gcd d4/n, factored by gci i11/tk: the COMPACTION-ONLY veto of a moving
/// young cycle's Phase 5 major (`major_gc_finalizing_with`'s `no_compact`).
/// Pure.
///
/// * `ledger_pinned_copy`: the pause's young pin ledger licensed this cycle
///   (the pinned copy, term 4's arm or the take-over arm; production only:
///   `pinned_young_words.is_some() && forced_pin_words.is_none()`). The
///   ledger holds young words only, so nothing keeps an old object an
///   unrewritable word names in place. Unconditional: no switch lifts it,
///   which is what lets [`takeover_other_divert_terms`] leave
///   `CRATONVM_OLDGEN_COMPACT` out.
/// * `compiled_frames_live && jit_guard()`: compiled frames were live this
///   pause and `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD` (default ON) holds.
///   `jit_guard` is asked last, so a pause without compiled frames, or a
///   ledger-pinned one, pays no flag lookup (as the inline expression did).
pub fn phase5_major_no_compact(
    ledger_pinned_copy: bool,
    compiled_frames_live: bool,
    jit_guard: impl FnOnce() -> bool,
) -> bool {
    ledger_pinned_copy || (compiled_frames_live && jit_guard())
}

/// gcd d5/s: the first condition of [`pinned_copy_takes_takeover_cycle`] that
/// fails, as a [`takeover_decline`] slot; `None` exactly when that function
/// answers `true`. The verdict's conditions are tested first (they are what
/// the take-over arm exists for), then the cycle's other obligations. Pure.
pub fn pinned_takeover_decline_reason(
    moving_young_requested: bool,
    force_non_moving_jit_roots: bool,
    other_coverage_incomplete: bool,
    other_divert_terms: bool,
    verdict: cratonvm_gc_core::gc_quiescence::TakeoverVerdict,
) -> Option<usize> {
    use takeover_decline as d;
    let slot = if verdict.frozen == 0 && verdict.helper_windows == 0 && verdict.unreadable == 0 {
        d::NO_TAKEOVER_PEER
    } else if verdict.frozen > 0 {
        d::FROZEN
    } else if verdict.unreadable > 0 {
        d::UNREAD
    } else if !verdict.pins_complete {
        d::WINDOW_REFUSED
    } else if !verdict.derived_pointers_resolved {
        d::DERIVED_UNRESOLVED
    } else if other_coverage_incomplete {
        d::OTHER_COVERAGE
    } else if other_divert_terms || force_non_moving_jit_roots || !moving_young_requested {
        d::OTHER_DIVERT
    } else {
        return None;
    };
    Some(slot)
}

/// What a pinned in-place cycle leaves where it is.
#[derive(Debug, Default)]
pub struct PinnedYoungPlan {
    /// `(base, total_size)` of every object that overlaps a pinned page,
    /// ascending and disjoint. Each is a ROOT of the cycle (kept alive, not
    /// only kept in place): a derived word may be the only reference a
    /// compiled frame holds to it -- unless [`Self::roots`] is `Some`, which
    /// then names the roots and leaves the rest pinned-if-reached.
    pub objects: Vec<(usize, usize)>,
    /// The pinned pages, as in [`pinned_young_pages`].
    pub pages: Vec<usize>,
    /// gcf f5/y (`CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`, opt-in): when `Some`,
    /// only these bases (objects a word lies in, or ends at) are ROOTS of the
    /// cycle; the other objects of [`Self::objects`] stay pinned in place if
    /// something reaches them and are reclaimed if nothing does. `None`: every
    /// planned object is a root. gcj j2/pn2: `Some` is NOT opt-in-only -- the
    /// default-ON watched and finalizable filters
    /// (`CRATONVM_GEN_PINNED_WATCHED_UNNAMED_NOT_ROOTS`,
    /// `pinned_finalizable_unnamed_not_roots_enabled`) set it too, so the drop
    /// mode runs on the default path; `_NAMED_ROOTS` is the other opt-in.
    pub roots: Option<Vec<usize>>,
    /// gcj j2/pn2: `true` when [`Self::roots`] was set by
    /// `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS` / `_NAMED_ROOTS` (the
    /// `[GC] pinned_exact_roots:` `pxr_*` fields count only those cycles);
    /// `false` when only the default-ON watched / finalizable filters set it
    /// (counted as `pfd_*`).
    pub exact_rule: bool,
    /// gcj j2/pn2 (`CRATONVM_GEN_PINNED_COPY_OBJECT_WINDOWS`): the from-space
    /// OFFSET spans `(off, len)` the copy destinations must avoid -- the pin
    /// words' windows plus the planned objects' extents, ascending and
    /// disjoint. `None`: the whole pinned pages (the default).
    pub reserved: Option<Vec<(usize, usize)>>,
}

/// `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS` -- default on (default on), read per
/// pinned cycle. gcf f5/y; see [`pinned_plan_exact_roots`].
pub fn pinned_copy_exact_roots_enabled() -> bool {
    true
}





/// `CRATONVM_GEN_PINNED_WATCHED_START_LAYOUT_FREE_ROOTED` --
/// gcj-j1pn-pinned-cycle-review-findings item 1 (the twin session's wider
/// form), OPT-IN (`runtime_flag_on`) INSIDE the start-word rule above:
/// a pin word from a layout-free band, a register image or a blocked-peer
/// band (`gc_quiescence::pause_young_pin_words_tagged`) counts as rooted, as a
/// card walk-gap word does ([`pinned_watched_start_rooted_set`]). The i35/wy
/// argument ("the marking side already treats the word as no reference")
/// was measured on, and is about, compiled-band slots; a register or a Rust
/// frame's word is exactly the unrewritable VM-frame holder the pinned plan
/// exists for, and a watched object is not always weakly held (a `Reference`
/// or a queue is a strong object that is also watched). Read per pinned cycle.
///
/// Why opt-in: it roots the layout-free STACK words too, and those are where
/// the stale words the i35/wy rule discounts live. Merged default ON, it
/// failed `WeakClearYoungProbe` 4/4 (`inline=LIVE`, HotSpot clears; 4/4 clear
/// with it off), so the default is the register-only set
/// ([`pinned_watched_register_words_rooted_enabled`]).
pub fn pinned_watched_start_layout_free_rooted_enabled() -> bool {
    false
}

/// How a pin word names a plan object (gci i34/wy census): the word lies in
/// it or one past its end (0), it is a one-past-the-end word at the next
/// object's start and the object is an array (1), or the object starts within
/// [`PIN_WORD_SLACK_ABOVE`] above the word (2).
pub const PIN_NAMES_INSIDE: u8 = 0;
pub const PIN_NAMES_ARRAY_END: u8 = 1;
pub const PIN_NAMES_SLACK: u8 = 2;

/// gci i30/wy, i34/wy: every `(object base, naming word, how)` the pin words
/// name among `objects` (a pinned plan's, ascending, disjoint), unsorted and
/// possibly repeated. `slack_arrays_only`: the base-minus-offset reach names
/// only ARRAYS (a biased element cursor, `p = base + HEADER - k*scale`, the
/// only shape `YOUNG_PIN_LOW_SLACK` and gcg g1/y were built for) and applies to
/// every word, start or not; otherwise (the i30 rule, which i31/fz keeps) it
/// names every object, from words that are not exactly an object start.
pub fn pinned_plan_word_names(
    words: &[usize],
    objects: &[(usize, usize)],
    is_array: &dyn Fn(usize) -> bool,
    slack_arrays_only: bool,
) -> Vec<(usize, usize, u8)> {
    let mut named: Vec<(usize, usize, u8)> = Vec::new();
    for &w in words {
        let i = objects.partition_point(|&(b, _)| b <= w);
        let at_start = i > 0 && objects[i - 1].0 == w;
        if i > 0 {
            let (b, s) = objects[i - 1];
            // Inside `b`, or (when `w` is not a start) one past its end.
            if w <= b + s {
                named.push((b, w, PIN_NAMES_INSIDE));
            }
        }
        if at_start && i > 1 {
            let (b, s) = objects[i - 2];
            if b + s == w && is_array(b) {
                named.push((b, w, PIN_NAMES_ARRAY_END));
            }
        }
        if slack_arrays_only || !at_start {
            let reach = w.saturating_add(PIN_WORD_SLACK_ABOVE);
            named.extend(
                objects[i..]
                    .iter()
                    .take_while(|&&(b, _)| b <= reach)
                    .filter(|&&(b, _)| !slack_arrays_only || is_array(b))
                    .map(|&(b, _)| (b, w, PIN_NAMES_SLACK)),
            );
        }
    }
    named
}

/// gci i30/wy (`gci-i26wy-generational-young-pauses-keep-both-weak-referents-at-256m-FIXED-20261016`):
/// the seeds of a pinned in-place cycle, minus every WATCHED object (a
/// reference processor's referent, `Reference` or queue, a JNI weak global's
/// referent, a weak String-table entry: `gc_quiescence::watched_referents_snapshot`)
/// that no pin word names. `None` when nothing is dropped (the plan's own
/// seeds stand, byte for byte).
///
/// Why. The page rule makes EVERY object on a pinned 4 KiB page a root of the
/// cycle. A compiled `main` holds its two `WeakReference`s in its frame, so
/// their page is pinned every cycle, and the referents the interpreter
/// allocated right after each `WeakReference` (`new WeakReference<>(new
/// Object())` lays out `[WR][Object][WR][Object]`) were seeded as roots: the
/// pre-collection pass had nulled the referent slots, and the page rule rooted
/// the referents anyway, so `WeakClearYoungProbe` never saw a clear (i26a
/// `wy26_d_watch`: the probe's four objects keep their addresses through all
/// ten pauses, `pinned=10` in the census). HotSpot clears both at the first
/// young collection. Since `CRATONVM_GEN_PINNED_YOUNG_COPY` went default ON
/// (gci i14a, `WARNING gci-user-flip-20261013`) this is every JIT-warm young
/// pause of the default path.
///
/// A dropped object stays a member of the plan: if anything reaches it it is
/// kept in place exactly as before (the in-place arms test the pinned set, not
/// the seeds -- `arm_in_place_young_cycle`), and only if nothing does is it
/// reclaimed, the gcf f5/y arm. So the drop is restricted to watched objects
/// that no word can be naming (a referent is weakly held; a `Reference` or a
/// queue is not, which is why a dropped one must be reached through a root --
/// gcj-j1pn-pinned-cycle-review-findings item 1):
///
/// * a word that lies in an object, or ends one (one past its end), names it
///   -- except that a word EXACTLY at an object start names the object ending
///   there only when that object is an ARRAY (`is_array`): a one-past-the-end
///   cursor is an element-iteration artefact, and a frame's word equal to the
///   next object's base is that object's own base pointer;
/// * a word that is NOT exactly an object start also names every object
///   starting within [`PIN_WORD_SLACK_ABOVE`] above it (gcg g1/y's
///   base-minus-offset reach, kept whole: that rule closed a use-after-free);
///   a word exactly at a start is that object's base, not a derived pointer.
///
/// `objects` is a pinned plan's (ascending, disjoint); `seeds` the seeds the
/// plan would use (every object, or the exact-roots subset). The i30 rule
/// (slack names every object from a non-start word); i31/fz's finalizable arm
/// uses it as is. The watched arm uses
/// [`pinned_plan_seeds_without_unnamed_watched_by`] with the i34 narrowing.
pub fn pinned_plan_seeds_without_unnamed_watched(
    words: &[usize],
    objects: &[(usize, usize)],
    seeds: &[usize],
    is_watched: impl Fn(usize) -> bool,
    is_array: impl Fn(usize) -> bool,
) -> Option<Vec<usize>> {
    pinned_plan_seeds_without_unnamed_watched_by(words, objects, seeds, is_watched, is_array, false)
}

/// [`pinned_plan_seeds_without_unnamed_watched`] with the slack rule chosen
/// (`slack_arrays_only`, see [`pinned_plan_word_names`]).
///
/// gci i34/wy: why the narrowing. A base-minus-offset derived pointer is an
/// ARRAY element cursor biased below the array's base; no compiled access to a
/// plain instance forms `obj - k`. The i30 rule let any interior word of the
/// probe's `WeakReference` (a field address, `WR + 16`) name every object up
/// to 256 bytes above it -- the referent 0x70 above, the second
/// `WeakReference` and its referent -- which is reading (a) of Batch 4's FAIL 1.
pub fn pinned_plan_seeds_without_unnamed_watched_by(
    words: &[usize],
    objects: &[(usize, usize)],
    seeds: &[usize],
    is_watched: impl Fn(usize) -> bool,
    is_array: impl Fn(usize) -> bool,
    slack_arrays_only: bool,
) -> Option<Vec<usize>> {
    pinned_plan_seeds_without_unnamed_watched_rooted(
        words,
        objects,
        seeds,
        is_watched,
        is_array,
        slack_arrays_only,
        None,
    )
}

/// Does the `(object, word, how)` naming entry count for liveness? With
/// `start_word_needs_root` (gci i35/wy) a word EXACTLY at the object's start
/// counts only if the root vector holds that object (`in_root(b)`); every
/// other naming (interior, one-past-the-end, slack) counts as before.
#[inline]
pub fn pin_name_counts(
    (b, w, how): (usize, usize, u8),
    start_word_needs_root: Option<&dyn Fn(usize) -> bool>,
) -> bool {
    match start_word_needs_root {
        Some(in_root) if how == PIN_NAMES_INSIDE && w == b => in_root(b),
        _ => true,
    }
}

/// gci i39/rv: the bases whose START word counts under
/// `CRATONVM_GEN_PINNED_WATCHED_START_NEEDS_ROOT` ([`pin_name_counts`]'s
/// `in_root`): the pause's root vector (`root_addrs`) PLUS the dirty-card
/// scan's walk-gap young values (`gap_words`, [`CardScanGapRoots::young_words`]).
/// gcj j1/pn item 1: the caller also passes, in `gap_words`, the ledger's
/// layout-free / register / peer-band words
/// ([`pinned_watched_start_layout_free_rooted_enabled`]).
///
/// A gap word is an old-gen heap word behind a header no walk can size; the
/// card scan cannot trace it, so the plan's pin words are its only root
/// ([`CardScanGapRoots`]: "keep the young object it names alive"). A reference
/// value is an object START, so before this union every gap word naming a
/// watched object failed the i35/wy root test: the object stopped being a seed
/// and, with nothing else reaching it, was reclaimed under a heap word that
/// may be a live reference (a dangling old-to-young slot) -- on the default
/// path since the i35 WARNING flip of
/// `CRATONVM_GEN_PINNED_WATCHED_UNNAMED_NOT_ROOTS`. The i35/wy argument (a
/// stale STACK word above the outermost compiled entry is no reference to a
/// relocating cycle) never covered heap words. Pure, for the tests.
pub fn pinned_watched_start_rooted_set(
    root_addrs: impl IntoIterator<Item = usize>,
    gap_words: &[usize],
) -> FxHashSet<usize> {
    let mut set: FxHashSet<usize> = root_addrs.into_iter().collect();
    set.extend(gap_words.iter().copied());
    set
}

/// `CRATONVM_GEN_PINNED_WATCHED_REGISTER_WORDS_ROOTED` --
/// gcj-j1pn-pinned-cycle-review-findings item 1, default ON (`=0` kills:
/// register words are tested as every other ledger word, the i39 rule). Inside
/// `CRATONVM_GEN_PINNED_WATCHED_START_NEEDS_ROOT`; see
/// [`pinned_watched_start_rooted_words`].
pub fn pinned_watched_register_words_rooted_enabled() -> bool {
    true
}

/// gcj-j1pn-pinned-cycle-review-findings item 1: the words besides the root
/// vector whose START word counts under the i35/wy rule -- the card walk-gap
/// values (gci i39/rv) plus, under
/// [`pinned_watched_register_words_rooted_enabled`], the pause ledger's
/// REGISTER words (`gc_quiescence::pause_young_pin_register_words`: each
/// deposit's register image, the peers' captured register files).
///
/// Why registers. The i35/wy argument -- the marking side already treats the
/// word as no reference -- was made for, and measured on, a compiled-band
/// STACK slot no oop map names (i39/rv correction 1: `wy35_p_entry`, P-BAND).
/// A register at a park is the innermost frame's live value, not a stale slot
/// left behind: no measurement ever showed one naming an object that is dead,
/// and if a root channel ever misses one, dropping a watched object it names
/// (a `Reference`, a queue -- not weakly held -- as well as a referent) frees
/// it under a live register. Counting it rooted costs at most an unclear weak
/// reference for one cycle. The layout-free STACK words (Rust frames, frames
/// with no layout) stay under the rule: a raw reference across a safepoint
/// there breaks the VM's own pinning discipline, and they are where the stale
/// words i35 targeted live.
pub fn pinned_watched_start_rooted_words(gap_words: &[usize]) -> Vec<usize> {
    let mut words = gap_words.to_vec();
    if pinned_watched_register_words_rooted_enabled() {
        cratonvm_gc_core::gc_quiescence::pause_young_pin_register_words(&mut words);
    }
    words
}

/// [`pinned_plan_seeds_without_unnamed_watched_by`] plus gci i35/wy's root
/// test for start words (`start_word_needs_root`, see [`pin_name_counts`]).
///
/// gci i35/wy: why. i34b `wy34_d_census_on` (Batch 6c, D2-A): the probe's
/// `inline` referent is named on 10/10 pauses by a pin word equal to its own
/// base (`how=inside`) that is NOT in the pause's root vector
/// (`in_roots=false`). No root channel -- precise maps, shadow slots, the band
/// scan, the interpreter frames, the snapshots -- holds it, so the moving
/// cycle already treats that word as no reference: an object off a pinned page
/// that only such a word names is collected by every relocating cycle. The pin
/// ledger records it anyway because a ledger that licenses moves spends no
/// deadness claim and sweeps the Rust frames above the outermost compiled
/// entry (`conservative_roots::young_pin_sweep_hi`), which hold stale words
/// the interpreter left; that is a RELOCATION guard (the word cannot be
/// rewritten), not a liveness proof. So for a watched object the plan keeps the
/// pin (it stays in place) but lets only a rooted object's start word make it
/// a seed. An interior or derived word still names it whatever the roots say.
///
/// gci i39/rv: two corrections. (1) The producer is NOT the Rust frames above
/// the entry: vi35-wy WY3-P read P-BAND (`wy35_p_entry`, the sweep stopped at
/// the outermost compiled entry, still names R `how=inside`), so the word is a
/// compiled-band slot the marking scan does not root and the ledger records
/// with no deadness excuse; the rule stands on that (the marking side already
/// treats the word as no reference). (2) The argument is about STACK words
/// only. The plan's words also hold the card scan's walk-gap values, heap
/// words nothing else roots; the caller counts those as rooted
/// ([`pinned_watched_start_rooted_set`]).
pub fn pinned_plan_seeds_without_unnamed_watched_rooted(
    words: &[usize],
    objects: &[(usize, usize)],
    seeds: &[usize],
    is_watched: impl Fn(usize) -> bool,
    is_array: impl Fn(usize) -> bool,
    slack_arrays_only: bool,
    start_word_needs_root: Option<&dyn Fn(usize) -> bool>,
) -> Option<Vec<usize>> {
    if !seeds.iter().any(|&b| is_watched(b)) {
        return None;
    }
    let mut named: Vec<usize> = pinned_plan_word_names(words, objects, &is_array, slack_arrays_only)
        .into_iter()
        .filter(|&n| pin_name_counts(n, start_word_needs_root))
        .map(|(b, _, _)| b)
        .collect();
    named.sort_unstable();
    named.dedup();
    let kept: Vec<usize> = seeds
        .iter()
        .copied()
        .filter(|&b| !is_watched(b) || named.binary_search(&b).is_ok())
        .collect();
    (kept.len() != seeds.len()).then_some(kept)
}

/// gci i34/wy census (`CRATONVM_DBG_WATCHREF`, one pinned cycle): for every
/// WATCHED plan object, one line naming its fate at the plan --
/// `named word=0x.. how=inside|array-end|slack` (still a seed) or `dropped`
/// (not a seed), and for a dropped one whether `roots` holds it exactly
/// (`in_roots=`). The post-collection `[watchref] weak KEEP` / clear lines then
/// say whether a dropped referent was reached anyway (reading (b)) or never
/// dropped (reading (a)). Capped at 256 lines a cycle.
#[allow(clippy::too_many_arguments)] // a diagnostic over the plan's own inputs
pub fn pinned_plan_watched_census(
    words: &[usize],
    objects: &[(usize, usize)],
    seeds: &[usize],
    is_watched: &dyn Fn(usize) -> bool,
    is_array: &dyn Fn(usize) -> bool,
    slack_arrays_only: bool,
    roots: &[ObjectRef],
    start_word_needs_root: bool,
    gap_words: &[usize],
) {
    let names = pinned_plan_word_names(words, objects, is_array, slack_arrays_only);
    let root_set: FxHashSet<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    // gci i39/rv: the plan's own rule (roots + card walk-gap words); `in_roots=`
    // below still prints the root vector alone.
    let counted_set =
        pinned_watched_start_rooted_set(roots.iter().map(|r| r.as_ptr() as usize), gap_words);
    let in_root = |b: usize| counted_set.contains(&b);
    let need: Option<&dyn Fn(usize) -> bool> = if start_word_needs_root {
        Some(&in_root)
    } else {
        None
    };
    let mut printed = 0usize;
    for &(b, s) in objects {
        if !is_watched(b) || printed >= 256 {
            continue;
        }
        printed += 1;
        let seed = seeds.contains(&b);
        let counted = names
            .iter()
            .copied()
            .find(|&n| n.0 == b && pin_name_counts(n, need));
        // gci i35/wy: a start word the root vector does not hold, named but
        // not counted (the i34b `inline` shape).
        if counted.is_none() {
            if let Some(&(_, w, _)) = names.iter().find(|&&(o, _, _)| o == b) {
                eprintln!(
                    "[watchref] pinned watched @0x{b:x} size={s} dropped unrooted-start word=0x{w:x} seed={seed} in_roots={}",
                    root_set.contains(&b)
                );
                continue;
            }
        }
        match counted {
            Some((_, w, how)) => {
                let how = match how {
                    PIN_NAMES_INSIDE => "inside",
                    PIN_NAMES_ARRAY_END => "array-end",
                    _ => "slack",
                };
                eprintln!(
                    "[watchref] pinned watched @0x{b:x} size={s} named word=0x{w:x} how={how} seed={seed} in_roots={}",
                    root_set.contains(&b)
                );
            }
            None => eprintln!(
                "[watchref] pinned watched @0x{b:x} size={s} dropped seed={seed} in_roots={}",
                root_set.contains(&b)
            ),
        }
    }
}


/// `CRATONVM_GEN_PINNED_FINALIZABLE_UNNAMED_NOT_ROOTS` -- gci i31/fz, DEFAULT
/// ON since the i35 WARNING flip (`runtime_flag_default_on`, `=0` kills). It
/// was opt-in between gci i35/fz and that flip (vi31-fz block S failed by its
/// letter -- `fz31_s_wcy` and `fz31_s_jor*` DIFF with no switch-off twin --
/// until vi35-fz's twins cleared it). Off, a registered finalizable on a
/// pinned page is a seed, the pre-i31 plan. Read per pinned cycle. See
/// [`pinned_plan_finalizable_members`].
pub fn pinned_finalizable_unnamed_not_roots_enabled() -> bool {
    // WARNING gci-user-flip-20261016 (fz): default ON since vi35-fz S (off/on twins
    // 6/6 equal) made it flip-eligible on i35a (`=0` kills); before that opt-in.
    true
}



/// `CRATONVM_DBG_GEN_FIN_CHILD_CENSUS` -- gci i40/gn diagnostic, default OFF
/// (`runtime_flag_on`). Read once per moving young cycle. On a cycle that
/// resurrects a dead young finalizable, one `[gen-fin-child]` line per young
/// reference slot of each resurrected finalizable (at most
/// [`FIN_CHILD_CENSUS_MAX_LINES`] a cycle), naming who already held the child
/// when Phase 2.5 began ([`fin_child_holder_label`]): nobody (the HotSpot
/// case: the child is finalizer-reachable only), a pinned plan object (a
/// pin-word SEED, or reached from one), the pause's root vector, or another
/// strong path (a heap slot, a card, an overlay). Decides whether a kept weak
/// referent is a reference-processing order defect or a root / pin word.
pub fn fin_child_census_enabled() -> bool {
    false
}

/// Cap on `[gen-fin-child]` lines a cycle ([`fin_child_census_enabled`]).
pub const FIN_CHILD_CENSUS_MAX_LINES: usize = 32;

/// gci i40/gn: the PRE-collection keys of the moving young cycle's depth-2
/// resurrection set -- every `pointer_map` entry whose destination is
/// to-space at or above the Phase 2.5 mark (`to_lo..to_hi`), an object the
/// resurrection drain promoted (`promoted`), or (gci i40/gn, the pinned
/// in-place cycle) an object it queued for an in-place scan (`in_place`: a
/// from-space span copy, a kept survivor, or a pinned non-seed, the last two
/// identity entries). Pure, for the tests. Order is the map's.
pub fn resurrection_closure_keys(
    pointer_map: &cratonvm_types::PointerMap,
    to_lo: usize,
    to_hi: usize,
    promoted: &FxHashSet<usize>,
    in_place: &FxHashSet<usize>,
) -> Vec<usize> {
    pointer_map
        .iter()
        .filter(|&(_, &to)| {
            (to >= to_lo && to < to_hi) || promoted.contains(&to) || in_place.contains(&to)
        })
        .map(|(&from, _)| from)
        .collect()
}

/// gci i40/gn census label ([`fin_child_census_enabled`]) for a young child
/// of a resurrected finalizable, from what the cycle knew when Phase 2.5
/// began: `reached` its `pointer_map` destination (`None`: no strong path had
/// reached it), `pinned` whether it is a pinned plan object, `seed` whether it
/// was one of the plan's seeds, `in_roots` whether the root vector holds its
/// destination. Pure, for the tests.
pub fn fin_child_holder_label(
    reached: Option<usize>,
    pinned: bool,
    seed: bool,
    in_roots: bool,
) -> &'static str {
    match reached {
        None => "finalizer-only",
        Some(_) if in_roots => "root-vector",
        Some(_) if seed => "pin-seed",
        Some(_) if pinned => "pinned-reached",
        Some(_) => "strong-path",
    }
}

/// gci i40/gn: print the `[gen-fin-child]` census ([`fin_child_census_enabled`])
/// at the start of a moving young cycle's Phase 2.5, BEFORE any dead
/// finalizable is forwarded: so `pointer_map` is exactly the strong closure's
/// answer. For every candidate the strong closure did not reach (a young
/// finalizable Phase 2.5 is about to resurrect), one line per non-null
/// reference slot, read from its untouched pre-collection body. `seeds` is the
/// pinned plan's seed list (empty off a pinned cycle), `roots` the pause's
/// root vector (post-forward addresses, so it is matched against the child's
/// destination). Diagnostic only: reads, never writes.
pub fn fin_child_census_lines(
    finalizer_addrs: &[usize],
    young_from: &Arena,
    old_gen: &OldGen,
    pointer_map: &cratonvm_types::PointerMap,
    in_place: Option<&InPlaceEvac>,
    seeds: &[usize],
    roots: &[ObjectRef],
) {
    let root_set: FxHashSet<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    // The class name of the object at `addr` (a whole object: the untouched
    // pre-collection body, or a forward's destination), `#<id>` when the
    // class-info hook does not know it.
    let class_of = |addr: usize| -> String {
        // SAFETY: every caller passes an object base this cycle has not
        // overwritten (see the call sites); the class id is a plain header read.
        let cid = unsafe { (*(addr as *const ObjectHeader)).class_id.as_u32() };
        cratonvm_gc_core::gc::resolve_class_info(cid).map_or_else(|| format!("#{cid}"), |(n, _)| n)
    };
    let mut printed = 0usize;
    for &fin in finalizer_addrs {
        if pointer_map.contains_key(&fin) || !young_from.contains(fin as *const u8) {
            continue;
        }
        let fin_ptr = fin as *mut u8;
        // SAFETY: `fin` is a registered finalizable's address inside young
        // from-space that no forward of this cycle has touched (it is not in
        // `pointer_map`), so its header and body are the pre-collection bytes;
        // the callback only records slot values and returns `None` (no write).
        let header = unsafe { &*(fin_ptr as *const ObjectHeader) };
        let fin_class = class_of(fin);
        let mut children: Vec<usize> = Vec::new();
        // SAFETY: as above -- a whole, untouched pre-collection object; the
        // callback writes nothing (`None` for every slot).
        unsafe {
            forward_ref_slots(fin_ptr, header, |p| {
                children.push(p as usize);
                None
            });
        }
        for child in children {
            if printed >= FIN_CHILD_CENSUS_MAX_LINES {
                return;
            }
            printed += 1;
            if !young_from.contains(child as *const u8) {
                if old_gen.contains(child as *const u8) {
                    eprintln!(
                        "[gen-fin-child] fin=0x{fin:x} fin_class={fin_class} child=0x{child:x} child_class={} holder=old-gen",
                        class_of(child),
                    );
                } else {
                    eprintln!(
                        "[gen-fin-child] fin=0x{fin:x} fin_class={fin_class} child=0x{child:x} holder=outside-young"
                    );
                }
                continue;
            }
            let reached = pointer_map.get(&child).copied();
            let pinned = in_place.is_some_and(|ip| ip.covers_pinned(child));
            let seed = seeds.contains(&child);
            let in_roots = reached.is_some_and(|to| root_set.contains(&to));
            // A reached child's class is read at its destination (the
            // from-space body may carry a forwarding word); an unreached one's
            // body is untouched.
            let child_class = match reached {
                Some(to) if to != 0 => class_of(to),
                _ => class_of(child),
            };
            eprintln!(
                "[gen-fin-child] fin=0x{fin:x} fin_class={fin_class} child=0x{child:x} child_class={child_class} holder={} to=0x{:x} pinned={pinned} seed={seed} in_roots={in_roots} in_place_cycle={}",
                fin_child_holder_label(reached, pinned, seed, in_roots),
                reached.unwrap_or(0),
                in_place.is_some(),
            );
        }
    }
}

/// gci i47w/ec47 census label ([`fin_child_census_enabled`]) for a child of a
/// dead young finalizable the NON-MOVING young sweep
/// (`sweep_young_non_moving`) resurrects, from what its strong closure had
/// marked when the finalizer-resurrection phase began: `in_young` / `in_old`
/// where the child lives, `marked` whether the strong closure (roots, cards,
/// the conservative JIT-frame words the root gatherer appended, late passes)
/// had already side-marked it, `in_roots` whether the pause's root vector
/// names its base exactly. Pure, for the tests.
///
/// * `finalizer-only` -- nothing strong held it: the drain records it in the
///   depth-2 set and the reference processor clears its weak reference at
///   this pause (HotSpot's answer).
/// * `root-vector` -- a root word names it (on a JIT-active sweep that is
///   usually a conservative compiled-frame word): kept strongly, its weak
///   reference survives the pause.
/// * `strong-path` -- marked through something else (a heap slot, a card, an
///   interior root word, another dead finalizable of the same pause).
/// * `old-gen` / `outside-young` -- not this young sweep's to judge: a young
///   pause keeps an old referent (HotSpot's young collection does not
///   discover it either).
pub fn non_moving_fin_child_holder_label(
    in_young: bool,
    in_old: bool,
    marked: bool,
    in_roots: bool,
) -> &'static str {
    if !in_young {
        return if in_old { "old-gen" } else { "outside-young" };
    }
    match (marked, in_roots) {
        (false, _) => "finalizer-only",
        (true, true) => "root-vector",
        (true, false) => "strong-path",
    }
}

/// gci i47w/ec47: the `[gen-fin-child] path=non-moving` census lines
/// ([`fin_child_census_enabled`]) for the dead young finalizables
/// `sweep_young_non_moving` just claimed (`dead`, object bases, in place and
/// untouched: the sweep side-marks, it never writes a header), computed
/// BEFORE the resurrection drain marks their subgraphs. One line per non-null
/// reference slot, at most [`FIN_CHILD_CENSUS_MAX_LINES`]. Diagnostic only:
/// reads, never writes. Returned rather than printed so the tests read them.
///
/// Why (`gci-i38ec-...-20261016`, vi47w-verdicts-vi40 C): on the i47w build no
/// `aged` / `agedhelper` row printed a moving-cycle `[gen-fin-child]` line, so
/// the pause that judged `Fin` was not the moving Phase 2.5; this names, on the
/// non-moving door, who held the child and in which generation it lived.
pub fn non_moving_fin_child_census_lines(
    dead: &[usize],
    in_young: &dyn Fn(usize) -> bool,
    in_old: &dyn Fn(usize) -> bool,
    marked: &dyn Fn(usize) -> bool,
    roots: &[ObjectRef],
) -> Vec<String> {
    let root_set: FxHashSet<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    let class_of = |addr: usize| -> String {
        // SAFETY: every caller passes an object base this pause has not
        // overwritten (a claimed young finalizable, or a young / old base read
        // out of its slot); the class id is a plain header read.
        let cid = unsafe { (*(addr as *const ObjectHeader)).class_id.as_u32() };
        cratonvm_gc_core::gc::resolve_class_info(cid).map_or_else(|| format!("#{cid}"), |(n, _)| n)
    };
    let mut lines = Vec::new();
    for &fin in dead {
        let fin_ptr = fin as *mut u8;
        // SAFETY: `fin` is a young finalizable base the non-moving sweep just
        // side-marked in place; its header and body are the pre-collection
        // bytes, and the callback writes nothing (`None` for every slot).
        let header = unsafe { &*(fin_ptr as *const ObjectHeader) };
        let fin_class = class_of(fin);
        let mut children: Vec<usize> = Vec::new();
        // SAFETY: as above.
        unsafe {
            forward_ref_slots(fin_ptr, header, |p| {
                children.push(p as usize);
                None
            });
        }
        for child in children {
            if lines.len() >= FIN_CHILD_CENSUS_MAX_LINES {
                return lines;
            }
            let young = in_young(child);
            let old = !young && in_old(child);
            let is_marked = young && marked(child);
            let in_roots = root_set.contains(&child);
            let child_class = if young || old {
                class_of(child)
            } else {
                String::from("?")
            };
            lines.push(format!(
                "[gen-fin-child] path=non-moving fin=0x{fin:x} fin_class={fin_class} child=0x{child:x} child_class={child_class} holder={} marked={is_marked} in_roots={in_roots}",
                non_moving_fin_child_holder_label(young, old, is_marked, in_roots),
            ));
        }
    }
    lines
}

/// gci i31/fz (`gci-i26ed2-serial-eden-initial-halves-the-survivor-target-and-tenures-the-first-cohort-FIXED-20261016`,
/// reopened by vi26-ed2 F2a / S3): the bases of `objects` (a pinned plan's,
/// ascending, disjoint) that are registered finalizable candidates of this
/// collection (`candidates`, the door's `finalizer_addrs`: in any order,
/// duplicates allowed). Ascending, de-duplicated.
///
/// Why. The page rule makes every object on a pinned 4 KiB page a seed, and a
/// seed forwards to itself and enters `pointer_map` as an identity
/// (`forward_object`'s pinned arm), which Phase 2.5 reads as "already
/// reachable": a DEAD finalizable on a page a compiled frame's stale word pins
/// is therefore neither reclaimed nor resurrected, and its `finalize()` never
/// runs while the word persists. `OldGenFinalizeProbe -Xmx2g` under the
/// ergonomic initial heap (`CRATONVM_GC_HOTSPOT_INITIAL_HEAP=1` then; the
/// default on every collector since gci i48, 2026-10-17; eden 273 / 486 MiB) runs its first young cycle with the 64 `Fin`s still where they
/// were allocated, so the page around the word keeps six of them for the rest
/// of the run: `finalized=58/64` on every run with every tenuring threshold 15
/// (vi26-ed2 S1), the 58 others resurrected and finalized normally. HotSpot is
/// precise and finalizes all 64.
///
/// The members found here are dropped from the plan's seeds unless a pin word
/// names them (the same "names" rule as i30/wy's watched objects,
/// [`pinned_plan_seeds_without_unnamed_watched`]). A dropped member stays in
/// the plan: reached by anything, it is kept in place exactly as before;
/// unreached, Phase 2.5 forwards it (to itself, as a pinned object), reports
/// it as a dead finalizable and drains its closure -- so no dropped member is
/// ever reclaimed by the cycle that dropped it, and the switch cannot free an
/// object the page rule kept.
pub fn pinned_plan_finalizable_members(objects: &[(usize, usize)], candidates: &[usize]) -> Vec<usize> {
    if objects.is_empty() || candidates.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&a| {
            let i = objects.partition_point(|&(b, _)| b <= a);
            i > 0 && objects[i - 1].0 == a
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}


/// gcf f5/y: the objects of `objects` (a pinned plan's, ascending, disjoint)
/// that a pin word actually names: the one the word lies in, plus the one
/// ENDING exactly at the word (a one-past-the-end derived pointer). The
/// non-moving sweep roots a conservative word the same way (its containing
/// object); the plan's page rule roots every object on the word's page --
/// up to a page of dead neighbours and everything they reach, kept and
/// copied (or promoted on overflow) every cycle the word persists.
///
/// gcg g1/y: plus every object STARTING within [`PIN_WORD_SLACK_ABOVE`]
/// bytes above the word, `(w, w + slack]`. That is the base-minus-offset
/// derived pointer the ledger's low slack and the page rule's upward reach
/// exist for (gen r4w6 review6 finding M: an object named only by `obj - 200`
/// sitting in its predecessor or in a gap). The f5/y rule rooted only the
/// object such a word lies in, so the object it actually names was a non-seed
/// and -- unreached by anything precise -- reclaimed (zeroed and free-listed)
/// while the frame still held the derived word: a use-after-free, not a
/// retention. Still a bounded over-retention (at most `slack` bytes of
/// objects per word), not a page.
pub fn pinned_plan_exact_roots(words: &[usize], objects: &[(usize, usize)]) -> Vec<usize> {
    let mut roots: Vec<usize> = Vec::new();
    for &w in words {
        let i = objects.partition_point(|&(b, _)| b <= w);
        // gcg g1/y: the base-minus-offset reach (`objects[i..]` start above `w`).
        let reach = w.saturating_add(PIN_WORD_SLACK_ABOVE);
        roots.extend(
            objects[i..]
                .iter()
                .take_while(|&&(b, _)| b <= reach)
                .map(|&(b, _)| b),
        );
        if i > 0 {
            // The object the word lies in (`w < b + s`) or ends at
            // (`w == b + s`, one past the end). gci i1/pn: one test; the two
            // branches pushed the same base (gch h1/yp review note 8).
            let (b, s) = objects[i - 1];
            if w <= b + s {
                roots.push(b);
            }
        }
        // `w` exactly at a start: the object ending there is [i - 2] when
        // [i - 1] starts at `w`.
        if i > 1 && objects[i - 1].0 == w {
            let (b, s) = objects[i - 2];
            if b + s == w {
                roots.push(b);
            }
        }
    }
    roots.sort_unstable();
    roots.dedup();
    roots
}



/// `CRATONVM_GEN_PINNED_COPY_NAMED_ROOTS` -- gcj j1/pn; default ON since gcj
/// j1yg-pause (2026-10-27), `=0` kills. Read once per pinned cycle that has
/// plan objects. See [`pinned_plan_named_roots`]. On 4-thread JIT-warm javac
/// the page rule's dead neighbours roughly doubled the copied objects and
/// tripled the promoted bytes of every pinned cycle.
/// gcj j1yg-pause round 3: `CRATONVM_GEN_PINNED_COPY_ROOT_SLICE_NAMES`
/// (default ON, `=0` kills; on 4-thread JIT-warm javac the plan's seeds fell
/// from ~26 K to ~9.6 K a run and overflow promotions to ~0). Under the named rule, a pinned cycle's seeds come from the pin
/// words its root slice also holds, plus every card walk-gap word and every
/// blocked-peer interior word the plan took -- the rule the in-place P-C
/// major already applies (`CRATONVM_GC_MOVING_MAJOR_IN_PLACE_NAMED_SEEDS`):
/// the ledger is a move veto everywhere else, and the non-moving sweep, which
/// these cycles ran before the pinned copy took them, never marks from it. A
/// ledger-only word (a stale value in a Rust frame between JIT entries, the
/// pause path's own frames) still pins its page; the object it names stays
/// only if something reaches it.
pub fn pinned_copy_root_slice_names_enabled() -> bool {
    true
}

/// gcj j1yg-pause round 3: the words [`pinned_copy_root_slice_names_enabled`]
/// lets name roots -- the pin words the root slice also holds
/// ([`pc_in_place_root_pin_words`]), then every heap walk-gap word and every
/// blocked-peer interior word, ascending and deduplicated. Pure.
pub fn pinned_copy_root_slice_words(
    pin_words: &[usize],
    root_words: impl Iterator<Item = usize>,
    gap_words: &[usize],
    peer_words: &[usize],
) -> Vec<usize> {
    let mut out = pc_in_place_root_pin_words(pin_words, root_words);
    out.extend_from_slice(gap_words);
    out.extend_from_slice(peer_words);
    out.sort_unstable();
    out.dedup();
    out
}

pub fn pinned_copy_named_roots_enabled() -> bool {
    true
}



/// gcj j1/pn (`gci-i30wy-proposal-pinned-page-rule-roots-only-what-a-word-can-name`):
/// the seeds of a pinned in-place cycle under
/// `CRATONVM_GEN_PINNED_COPY_NAMED_ROOTS` -- the objects a pin word NAMES by
/// the watched arm's rule ([`pinned_plan_word_names`] with
/// `slack_arrays_only`, the i34/wy narrowing that is the default for watched
/// objects since the i35 WARNING flip), applied to every plan object:
///
/// * the object a word lies in, or ends one past (a non-start word);
/// * at a word EXACTLY at an object start: that object, plus the object
///   ending there only when it is an ARRAY (an element cursor run off its
///   end);
/// * every ARRAY starting within [`PIN_WORD_SLACK_ABOVE`] above a word (a
///   biased element cursor `base + HEADER - k * scale`).
///
/// A subset of [`pinned_plan_exact_roots`] (which also names a predecessor of
/// any kind at a start word and every object within the slack): pinning (keep
/// in place) is unchanged, a dropped object is pinned-if-reached and
/// reclaimed when nothing reaches it (`InPlaceEvac::drop_unvisited_pinned`).
/// The risk is the i30wy page's: a compiled frame holding `obj - k` for a
/// plain INSTANCE `obj`, which no access the JIT emits forms. Pure; returns
/// the bases ascending and de-duplicated.
pub fn pinned_plan_named_roots(
    words: &[usize],
    objects: &[(usize, usize)],
    is_array: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    let mut roots: Vec<usize> = pinned_plan_word_names(words, objects, is_array, true)
        .into_iter()
        .map(|(b, _, _)| b)
        .collect();
    roots.sort_unstable();
    roots.dedup();
    roots
}



/// `CRATONVM_GEN_PINNED_COPY_OBJECT_WINDOWS` -- gcj j2/pn2, default on (default
/// OFF), read once per pinned plan. The pinned in-place cycle reserves each
/// pin word's WINDOW (`[w - PIN_WORD_SLACK, w + PIN_WORD_SLACK_ABOVE]`) and
/// the objects overlapping it, not the window's whole 4 KiB pages: the other
/// objects of a pinned page are ordinary from-space objects (copied when
/// reached, freed otherwise) and the page's free bytes outside the windows
/// serve as copy destinations
/// (`gcj-j1pn-proposal-pin-object-windows-not-pages-20261019`). The pages
/// are still computed: the 1/8 bound and the census count them.
pub fn pinned_copy_object_windows_enabled() -> bool {
    true
}

/// gcj j2/pn2: the absolute `[lo, hi)` windows `words` reserve inside
/// from-space `[base, base + used)`, merged, ascending and disjoint: the bytes
/// [`pinned_young_pages`] widens to whole pages. The same screen: a word in the
/// ledger's low slack below `base` reserves from `base`, a word above
/// `base + used` reserves nothing, `base + used` itself counts.
pub fn pinned_young_word_windows(words: &[usize], base: usize, used: usize) -> Vec<(usize, usize)> {
    let mut windows: Vec<(usize, usize)> = Vec::new();
    if used == 0 {
        return windows;
    }
    let Some(end) = base.checked_add(used) else {
        return windows;
    };
    let low = base.saturating_sub(cratonvm_gc_core::gc_quiescence::YOUNG_PIN_LOW_SLACK);
    for &w in words {
        if w < low || w > end {
            continue;
        }
        // gcj j2/pn2 (j3a `pnj2_ow_*`: a copy landed at `...b9` and the
        // forward install panicked): a pin word can be a byte cursor, so the
        // window is widened to 8-byte bounds -- the destination spans it is
        // subtracted from must stay 8-aligned, as whole pages kept them.
        let lo = w.saturating_sub(PIN_WORD_SLACK).max(base) & !7;
        // Inclusive top as in `pinned_young_pages`, then one past it, rounded
        // up to the next 8-byte bound and capped at `end`.
        let hi = (w
            .saturating_add(PIN_WORD_SLACK_ABOVE)
            .max(base)
            .min(end - 1)
            + 8)
            & !7;
        let hi = hi.min(end.next_multiple_of(8));
        if lo < hi {
            windows.push((lo, hi));
        }
    }
    windows.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(windows.len());
    for (lo, hi) in windows {
        match merged.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// gcj j2/pn2: `spans` (absolute `[lo, hi)`, ascending) as from-space OFFSETS
/// `(off, len)` relative to `base`, merged, ascending and disjoint -- the
/// shape [`subtract_spans`] takes.
pub fn absolute_spans_as_offsets(spans: &[(usize, usize)], base: usize) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = spans
        .iter()
        .filter(|&&(lo, hi)| hi > lo && lo >= base)
        .map(|&(lo, hi)| (lo - base, hi - base))
        .collect();
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(sorted.len());
    for (lo, hi) in sorted {
        match out.last_mut() {
            Some(last) if lo <= last.0 + last.1 => {
                let end = (last.0 + last.1).max(hi);
                last.1 = end - last.0;
            }
            _ => out.push((lo, hi - lo)),
        }
    }
    out
}

/// Resolve `words` to the objects a pinned in-place cycle must leave where
/// they are: every object that OVERLAPS a pinned page — the one straddling
/// into it from below plus every start inside it — read from the exact
/// object-start bitmap the moving cycle's pre-GC walk just built.
///
/// gcj j2/pn2 (`CRATONVM_GEN_PINNED_COPY_OBJECT_WINDOWS`, opt-in): the
/// objects overlapping a word's WINDOW instead, and the plan's
/// [`PinnedYoungPlan::reserved`] spans (windows plus those objects' extents)
/// replace the pages as what the destinations must avoid.
pub fn build_pinned_young_plan(
    words: &[usize],
    base: usize,
    used: usize,
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
) -> PinnedYoungPlan {
    build_pinned_young_plan_mode(
        words,
        base,
        used,
        starts,
        pinned_copy_object_windows_enabled(),
    )
}

/// [`build_pinned_young_plan`] with the window mode given (unit tests pass it).
pub fn build_pinned_young_plan_mode(
    words: &[usize],
    base: usize,
    used: usize,
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
    object_windows: bool,
) -> PinnedYoungPlan {
    let pages = pinned_young_pages(words, base, used);
    let end = base + used;
    // The absolute `[lo, hi)` spans whose overlapping objects are planned:
    // the pinned pages, or (window mode) the words' windows.
    let windows: Vec<(usize, usize)> = if object_windows {
        pinned_young_word_windows(words, base, used)
    } else {
        Vec::new()
    };
    let spans: Vec<(usize, usize)> = if object_windows {
        windows.clone()
    } else {
        pages
            .iter()
            .map(|&p| {
                let ps = base + (p << PIN_PAGE_SHIFT);
                (ps, (ps + PIN_PAGE).min(end))
            })
            .collect()
    };
    let mut objects: Vec<(usize, usize)> = Vec::new();
    for &(ps, pe) in &spans {
        let first = starts
            .nearest_start_at_or_below(ps)
            .or_else(|| starts.next_start_at_or_above(ps));
        let Some(mut cur) = first else {
            continue;
        };
        while cur < pe {
            // SAFETY: `cur` is an object start the exact pre-GC walk recorded
            // in `starts`, inside `[base, base + used)`, so a whole, mapped,
            // 8-aligned header lies there and nothing writes it before the
            // copy phase starts.
            let size = gen_object_total_size(unsafe { &*(cur as *const ObjectHeader) });
            if size < HEADER_SIZE || cur.checked_add(size).is_none_or(|e| e > end) {
                // gcg g6/xt (`gcg-g5xt-pinned-plan-stops-at-an-unsizable-
                // header-...`): this `break`ed, so every object of the page
                // AFTER the bad start was left out of the plan and the in-place
                // copy moved it under the pinned word. The start bitmap still
                // knows the next start, so resume there: the rest of the page
                // is planned. The bad start itself cannot be planned (it has
                // no extent) and is reported; nothing could copy it either.
                tracing::warn!(
                    start = cur,
                    size,
                    page = ps,
                    "pinned young plan: a recorded object start has no usable size; \
                     the page walk resumes at the next recorded start"
                );
                match starts.next_start_at_or_above(cur + 8) {
                    Some(n) => {
                        cur = n;
                        continue;
                    }
                    None => break,
                }
            }
            if cur + size > ps {
                objects.push((cur, size));
            }
            match starts.next_start_at_or_above(cur + 8) {
                Some(n) => cur = n,
                None => break,
            }
        }
    }
    objects.sort_unstable();
    objects.dedup();
    // gcj j2/pn2: in window mode the destinations avoid the windows and the
    // planned objects' whole extents (an object can reach past its window).
    let reserved = object_windows.then(|| {
        let mut abs = windows;
        abs.extend(objects.iter().map(|&(b, s)| (b, b + s)));
        absolute_spans_as_offsets(&abs, base)
    });
    PinnedYoungPlan {
        objects,
        pages,
        roots: None,
        exact_rule: false,
        reserved,
    }
}

/// gcj-j1pn-pinned-cycle-review-findings item 7: the part of the un-bumped
/// tail, `[young_used, end)`, that a pinned in-place cycle must commit besides
/// its destinations, or `None`. `dest` is the cycle's destination spans as
/// `(offset, len)`; `end` is the end of the highest one above `young_used`.
///
/// Why. A pinned page (or window) reaching past `young_used` is cut out of the
/// tail window, so the first tail destination starts above `young_used`, and
/// only destinations are committed (`commit_for_relocation`). The rebuild then
/// puts the new cursor above the copies there and free-lists the gap below
/// them -- bytes inside the cursor that the lazy backing may never have
/// mapped, where a header probe or a walk below the cursor would fault. One
/// span, so the gap and any between tail destinations are covered together.
pub fn in_place_tail_gap(dest: &[(usize, usize)], young_used: usize) -> Option<(usize, usize)> {
    let tail = dest.iter().filter(|&&(off, _)| off >= young_used);
    let (covered, end) = tail.fold((0usize, young_used), |(c, e), &(off, len)| {
        (c + len, e.max(off + len))
    });
    (end - young_used > covered).then_some((young_used, end - young_used))
}

/// `a` minus `b`, where both are `(offset, len)` lists sorted by offset and
/// pairwise disjoint. The result has the same shape.
pub fn subtract_spans(a: &[(usize, usize)], b: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0usize;
    for &(off, len) in a {
        let end = off + len;
        let mut lo = off;
        while j < b.len() && b[j].0 + b[j].1 <= lo {
            j += 1;
        }
        let mut k = j;
        while lo < end {
            match b.get(k) {
                Some(&(bo, bl)) if bo < end => {
                    if bo > lo {
                        out.push((lo, bo - lo));
                    }
                    lo = lo.max(bo + bl);
                    k += 1;
                }
                _ => {
                    out.push((lo, end - lo));
                    break;
                }
            }
        }
    }
    out
}

/// t1 wave 8, lane YGZERO (`CRATONVM_GEN_YOUNG_ZERO_ON_RECLAIM`): the
/// known-zero claim a pinned in-place rebuild may install -- the bytes of its
/// new free list `free` that lie inside the dead spans `zeroed` it memset in
/// this pause, i.e. the intersection of the two lists (both ascending and disjoint, `(offset,
/// len)`), with touching pieces merged as `Arena::set_known_zero_free_spans`
/// wants. Deliberately NOT the whole free list: the blocks that were already
/// free when the cycle began held this cycle's copies (the in-place
/// destinations) and the old claim is not re-proved here, so their remainder
/// keeps its hand-out memset -- unless the caller passes it as `carried`.
///
/// t1 wave 11, lane GCB: `carried` (ascending, disjoint; may be empty) is the
/// old known-zero set's bytes that were still free when the cycle began
/// (`Arena::known_zero_within(initial_free)`) minus every destination's
/// WRITTEN extent `[start, cursor)` -- the bytes no copy, PLAB gap filler or
/// CAS-loser filler of this pause touched. They join the claim where they are
/// still on the new free list. A sub-header sliver the rebuild stamps with a
/// filler is never on `free`, so it is never claimed. Pure.
pub fn zero_on_reclaim_claim(
    free: &[(usize, usize)],
    zeroed: &[(usize, usize)],
    carried: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let proved = cratonvm_gc_core::arena::union_known_zero_spans(zeroed.to_vec(), carried.to_vec());
    let outside = subtract_spans(free, &proved);
    cratonvm_gc_core::arena::union_known_zero_spans(subtract_spans(free, &outside), Vec::new())
}

/// t1 wave 11, lane GCB (`CRATONVM_GEN_YOUNG_ZERO_ON_RECLAIM`): the `carried`
/// input of [`zero_on_reclaim_claim`] -- `prior` (the old set's bytes inside the
/// cycle's initial free blocks, offsets, ascending and disjoint) minus each
/// destination span's written extent. `dest` is the cycle's
/// `(start, cursor, end)` list (absolute, ascending, disjoint) and `base` the
/// from-space base. Conservative: a cursor past its end subtracts up to the
/// cursor. Pure.
pub fn zero_on_reclaim_carried(
    prior: &[(usize, usize)],
    dest: &[(usize, usize, usize)],
    base: usize,
) -> Vec<(usize, usize)> {
    if prior.is_empty() {
        return Vec::new();
    }
    let written: Vec<(usize, usize)> = dest
        .iter()
        .filter(|&&(s, c, _)| c > s)
        .map(|&(s, c, _)| (s - base, c - s))
        .collect();
    subtract_spans(prior, &written)
}


/// gen r5w4/pin8 — the word every body word of a quarantine stamp holds (see
/// [`poison_quarantined_span`]). Chosen to be useless as anything a stale read
/// could act on: non-canonical as a 64-bit address (a dereference faults),
/// odd (no object is at an odd address), and each 32-bit half `0xdeadfa11`
/// decodes as a narrow oop far beyond any heap this VM reserves. Grep a crash
/// report's registers for `deadfa11`.
pub const QUARANTINE_POISON: u64 = 0xDEAD_FA11_DEAD_FA11;

/// gen r5w4/pin8 — which of `vacated` (`(offset, size)`, the old copies of
/// survivors a pinned in-place cycle moved, in any order, possibly repeated)
/// the rebuild may quarantine: sorted, de-duplicated, pairwise disjoint, each
/// 8-aligned, at least a header long, and wholly inside ONE span of `dead`
/// (sorted, disjoint: the bytes the rebuild would otherwise zero). Anything
/// else is dropped and stays on the ordinary path. Pure.
pub fn quarantine_spans(
    mut vacated: Vec<(usize, usize)>,
    dead: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    vacated.sort_unstable();
    vacated.dedup();
    let mut out = Vec::with_capacity(vacated.len());
    let mut prev_end = 0usize;
    let mut j = 0usize;
    for (off, len) in vacated {
        if len < HEADER_SIZE || len % 8 != 0 || off % 8 != 0 || off < prev_end {
            continue;
        }
        let Some(end) = off.checked_add(len) else {
            continue;
        };
        while j < dead.len() && dead[j].0 + dead[j].1 <= off {
            j += 1;
        }
        let inside = dead
            .get(j)
            .is_some_and(|&(d, dl)| d <= off && end <= d + dl);
        if inside {
            out.push((off, len));
            prev_end = end;
        }
    }
    out
}

/// gen r5w4/pin8 — stamp `[addr, addr + len)` as a quarantine filler: the
/// reference-free `int[]` every linear young walk already strides
/// ([`crate::gen_evac::write_filler`]), whose body words all read
/// [`QUARANTINE_POISON`] instead of zero.
///
/// # Safety
/// `addr` must be 8-aligned, `len` a multiple of 8 and at least
/// `HEADER_SIZE`, and the span writable, dead and owned by the caller (the
/// pause).
pub unsafe fn poison_quarantined_span(addr: usize, len: usize) {
    // SAFETY: forwarded from this function's contract.
    unsafe { crate::gen_evac::write_filler(addr, len) };
    // gci i41/ev + i41/rv (`gci-i41ev-quarantine-poison-overwrites-a-tiled-filler-header-FIXED-20261016`):
    // poison each filler chunk's BODY only. A span above ~8 GiB is tiled with
    // several `int[]`s (`gen_evac::filler_chunk_len`); the later chunks'
    // headers must survive or the next linear walk desyncs at the second.
    for (body, chunk_end) in quarantine_poison_bodies(addr, len) {
        let mut w = body;
        while w + 8 <= chunk_end {
            // SAFETY: an 8-aligned word of one filler chunk's body, inside
            // the span the contract hands this function.
            unsafe { (w as *mut u64).write(QUARANTINE_POISON) };
            w += 8;
        }
    }
}

/// gci i41/rv: the `[body start, chunk end)` ranges [`poison_quarantined_span`]
/// poisons in `[addr, addr + len)` -- one per `int[]` chunk of
/// [`crate::gen_evac::write_filler`]'s tiling (the same
/// `filler_chunk_len` walk), each starting past its chunk's header. One range
/// (`addr + HEADER_SIZE .. addr + len`, the pre-i41 loop) for every span below
/// ~8 GiB. Pure: computes addresses, touches no memory.
pub fn quarantine_poison_bodies(addr: usize, len: usize) -> Vec<(usize, usize)> {
    let end = addr + len;
    let mut out = Vec::new();
    let mut chunk_at = addr;
    while chunk_at < end {
        let chunk = crate::gen_evac::filler_chunk_len(end - chunk_at);
        if chunk == 0 {
            break; // never: `filler_chunk_len` answers at least `HEADER_SIZE`
        }
        let chunk_end = chunk_at + chunk;
        out.push((chunk_at + HEADER_SIZE, chunk_end));
        chunk_at = chunk_end;
    }
    out
}

/// gen r5w4/pin8 — is the object at `addr` (whose header is `header`) a
/// quarantine stamp, i.e. the poisoned old copy of a survivor a pinned
/// in-place cycle moved? Only a stamp whose body holds at least one word can
/// say so; a 16-byte object's stamp has no body and reads as a plain filler.
pub fn is_quarantine_stamp(addr: usize, header: &ObjectHeader) -> bool {
    if header.class_id.as_u32() != cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32()
        || header.kind() != ObjectKind::Array
        || header.array_length() < 2
    {
        return false;
    }
    // SAFETY: an `int[]` of at least two elements has at least 8 body bytes
    // after its header, inside the object the caller holds a reference to.
    unsafe { ((addr + HEADER_SIZE) as *const u64).read() == QUARANTINE_POISON }
}

/// The parts of `[0, extent)` no span of `live` covers. `live` is sorted by
/// offset and pairwise disjoint.
/// gcj pcp: the parallel card seed's claim, in card roots: a chunk is a few
/// hundred microseconds of forwarding, so the claims stay cheap and the tail
/// stays short.
pub const CARD_SEED_CHUNK: usize = 2048;

/// gcj pcp: below this many survivors [`sorted_live_offsets`] sorts on the
/// calling thread (a dispatch would cost more than it saves).
pub const PAR_LIVE_SORT_MIN: usize = 1 << 16;

/// gcj pcp: `live` -- distinct `(address, size)` pairs -- in ascending order,
/// as offsets from `base`, without the pairs whose address is in
/// `dead_sorted` (ascending). The answer of a sort, a retain and a map, in
/// that order. With `pool` and `threads > 1` the pairs are sorted in
/// `threads` contiguous slices on the pool, then merged; `live` is left
/// sorted by slice.
pub fn sorted_live_offsets(
    live: &mut [(usize, usize)],
    base: usize,
    dead_sorted: &[usize],
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
) -> Vec<(usize, usize)> {
    let pool = pool.filter(|_| threads > 1 && live.len() >= PAR_LIVE_SORT_MIN);
    let Some(pool) = pool else {
        // Natural runs: the stable sort merges them (see the caller).
        live.sort();
        let mut out = Vec::with_capacity(live.len());
        merge_live_runs(&[&*live], base, dead_sorted, &mut out);
        return out;
    };
    let per = live.len().div_ceil(threads);
    let slices: Vec<std::sync::Mutex<&mut [(usize, usize)]>> =
        live.chunks_mut(per).map(std::sync::Mutex::new).collect();
    let next = AtomicUsize::new(0);
    let worker = || loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        let Some(slice) = slices.get(i) else {
            return;
        };
        slice.lock().unwrap_or_else(|e| e.into_inner()).sort();
    };
    run_chunk_workers_elastic(pool, threads.min(slices.len()), &worker);
    let runs: Vec<&[(usize, usize)]> = slices
        .into_iter()
        .map(|m| &*m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect();
    // The merge, in parallel too: the address span cut into ranges, each
    // range merged from every run's share of it into its own list, the lists
    // joined in range order.
    let lo = runs.iter().filter_map(|r| r.first()).map(|p| p.0).min().unwrap_or(0);
    let hi = runs.iter().filter_map(|r| r.last()).map(|p| p.0).max().unwrap_or(0) + 1;
    let ranges = threads * 4;
    let bound = |k: usize| lo + ((hi - lo) as u128 * k as u128 / ranges as u128) as usize;
    let parts: Vec<std::sync::Mutex<Vec<(usize, usize)>>> =
        (0..ranges).map(|_| std::sync::Mutex::new(Vec::new())).collect();
    let next = AtomicUsize::new(0);
    let worker = || loop {
        let k = next.fetch_add(1, Ordering::Relaxed);
        if k >= ranges {
            return;
        }
        let (a, b) = (bound(k), bound(k + 1));
        let sub: Vec<&[(usize, usize)]> = runs
            .iter()
            .map(|r| &r[r.partition_point(|p| p.0 < a)..r.partition_point(|p| p.0 < b)])
            .collect();
        let dead = &dead_sorted[dead_sorted.partition_point(|&d| d < a)..];
        let mut out = parts[k].lock().unwrap_or_else(|e| e.into_inner());
        out.reserve(sub.iter().map(|r| r.len()).sum());
        merge_live_runs(&sub, base, dead, &mut out);
    };
    run_chunk_workers_elastic(pool, threads.min(ranges), &worker);
    let parts: Vec<Vec<(usize, usize)>> = parts
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect();
    let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for p in &parts {
        out.extend_from_slice(p);
    }
    out
}

/// gcj pcp: merge ascending `runs` of distinct `(address, size)` pairs into
/// `out` as offsets from `base`, without the addresses in `dead_sorted`
/// (ascending). A tournament over the run heads updates only the winning
/// run's path: log2(k) comparisons per survivor, rather than scanning all k
/// heads. Empty runs never enter the tournament.
pub fn merge_live_runs(
    runs: &[&[(usize, usize)]],
    base: usize,
    dead_sorted: &[usize],
    out: &mut Vec<(usize, usize)>,
) {
    let mut dead = dead_sorted.iter().peekable();
    let mut emit = |(a, sz): (usize, usize)| {
        while dead.next_if(|&&d| d < a).is_some() {}
        if dead.peek() != Some(&&a) {
            out.push((a - base, sz));
        }
    };
    let runs: Vec<_> = runs.iter().copied().filter(|r| !r.is_empty()).collect();
    if runs.len() <= 1 {
        if let Some(run) = runs.first() {
            for &pair in *run {
                emit(pair);
            }
        }
        return;
    }
    let mut heads = vec![0usize; runs.len()];
    let leaves = runs.len().next_power_of_two();
    let mut tree = vec![usize::MAX; leaves * 2];
    for r in 0..runs.len() {
        tree[leaves + r] = r;
    }
    let winner = |a: usize, b: usize, heads: &[usize]| {
        if a == usize::MAX {
            b
        } else if b == usize::MAX || runs[a][heads[a]] <= runs[b][heads[b]] {
            a
        } else {
            b
        }
    };
    for node in (1..leaves).rev() {
        tree[node] = winner(tree[node * 2], tree[node * 2 + 1], &heads);
    }
    while tree[1] != usize::MAX {
        let r = tree[1];
        emit(runs[r][heads[r]]);
        heads[r] += 1;
        let mut node = leaves + r;
        if heads[r] == runs[r].len() {
            tree[node] = usize::MAX;
        }
        node /= 2;
        while node != 0 {
            tree[node] = winner(tree[node * 2], tree[node * 2 + 1], &heads);
            node /= 2;
        }
    }
}

pub fn complement_spans(live: &[(usize, usize)], extent: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(live.len() / 4 + 1);
    let mut cur = 0usize;
    for &(off, len) in live {
        if off >= extent {
            break;
        }
        if off > cur {
            out.push((cur, off - cur));
        }
        cur = cur.max(off + len);
    }
    if cur < extent {
        out.push((cur, extent - cur));
    }
    out
}

/// gen r4w6/pinstale6: which population a word of the `[peer-reg-stale]`
/// pairing came from — `gc_quiescence::record_peer_reg`'s register index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerWordSource {
    /// A general-purpose register. Nothing rewrites a peer's register file;
    /// only a pin keeps such a word naming its object.
    Register,
    /// A take-over (frozen, in compiled code) peer's stack. The blocked-wake
    /// fold cannot route it: the peer has no wake hook.
    TakeoverStack,
    /// A helper-window (blocked) peer's native stack. The blocked-wake fold
    /// rewrites its object BASES at wake, by exact `pointer_map` lookup.
    HelperStack,
}

impl PeerWordSource {
    pub fn of(reg: u8) -> Self {
        match reg {
            cratonvm_gc_core::gc_quiescence::PEER_REG_STACK_TAKEOVER => Self::TakeoverStack,
            cratonvm_gc_core::gc_quiescence::PEER_REG_STACK_HELPER => Self::HelperStack,
            _ => Self::Register,
        }
    }
}

/// gen r4w6/pinstale6: what this cycle did to the object a peer word names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerWordFate {
    /// Outside from-space, or naming nothing this cycle relocated: an
    /// identity-forwarded (pinned or kept) object, a dead one, free space.
    Untouched,
    /// The exact base of an object this cycle relocated.
    RelocatedBase,
    /// Inside an object this cycle relocated: past its base, up to and
    /// including one past its end (a loop cursor that ran off it).
    RelocatedInterior,
}

/// Classify one peer word against this cycle's relocation.
///
/// * `from` — from-space `[lo, hi)` as walked;
/// * `relocated_to(a)` — `pointer_map`'s entry for `a`;
/// * `is_start` / `start_at_or_below` — the cycle's exact object-start bitmap;
/// * `size_at(new)` — the size of the relocated copy at `new` (its header is
///   whole; the from-space one is a forwarding word by now).
pub fn peer_word_fate(
    val: usize,
    from: (usize, usize),
    relocated_to: impl Fn(usize) -> Option<usize>,
    is_start: impl Fn(usize) -> bool,
    start_at_or_below: impl Fn(usize) -> Option<usize>,
    size_at: impl Fn(usize) -> usize,
) -> PeerWordFate {
    if let Some(new) = relocated_to(val) {
        return if new == val {
            PeerWordFate::Untouched
        } else {
            PeerWordFate::RelocatedBase
        };
    }
    if val < from.0 || val >= from.1 || is_start(val) {
        return PeerWordFate::Untouched;
    }
    let Some(s) = start_at_or_below(val) else {
        return PeerWordFate::Untouched;
    };
    match relocated_to(s) {
        Some(new) if new != s && val - s <= size_at(new) => PeerWordFate::RelocatedInterior,
        _ => PeerWordFate::Untouched,
    }
}

/// gen r4w6/pinstale6: does a peer word of `source`, whose object met `fate`,
/// leave the peer holding an address that no channel rewrote and no pin kept?
///
/// Counted as LIVE whenever the collector cannot prove it dead. That is every
/// such register word: the only registers captured on a cycle that relocates
/// belong to blocked peers with no compiled frame (a frozen peer or a helper
/// window fails the take-over verdict, hence the ledger and the coverage
/// proof), i.e. to VM Rust frames, and a thread suspended in a blocked region
/// is not at a call boundary, so no calling-convention rule makes even a
/// caller-saved register dead.
pub fn peer_word_stale_live(
    source: PeerWordSource,
    fate: PeerWordFate,
    helper_stack_remap_on: bool,
) -> bool {
    match fate {
        PeerWordFate::Untouched => false,
        PeerWordFate::RelocatedInterior => true,
        PeerWordFate::RelocatedBase => match source {
            PeerWordSource::HelperStack => !helper_stack_remap_on,
            PeerWordSource::Register | PeerWordSource::TakeoverStack => true,
        },
    }
}

/// The serial evacuator's state for one pinned in-place young cycle, carried
/// in [`PromotionQueue::in_place`].
pub struct InPlaceEvac {
    /// `(base, size)` of every pinned object, ascending and disjoint.
    pub pinned: Vec<(usize, usize)>,
    /// The side bit: set once `forward_object_impl` has identity-forwarded a
    /// pinned object and queued it for its in-place scan.
    pub visited: Vec<bool>,
    /// `[first pinned base, last pinned end)`, one range compare that rejects
    /// almost every address before the binary search.
    pub pinned_lo: usize,
    pub pinned_hi: usize,
    /// Survivors left where they were because neither a free span nor old gen
    /// could take them (the in-place twin of the Cheney path's abort).
    pub kept: FxHashSet<usize>,
    /// Destination spans `(start, cursor, end)`, absolute, ascending: the
    /// from-space bytes that were FREE when the cycle began, minus the pinned
    /// pages.
    pub dest: Vec<(usize, usize, usize)>,
    /// First destination span that may still serve an allocation.
    pub dest_idx: usize,
    /// Objects awaiting their in-place scan.
    pub scan: Vec<*mut u8>,
    /// Every object that stays in from-space after the cycle, `(addr, size)`:
    /// pinned objects, copies into free spans, kept survivors.
    pub live: Vec<(usize, usize)>,
    /// The free list at the start of the cycle, as offsets: bytes that already
    /// read as zero and need no wipe.
    pub initial_free: Vec<(usize, usize)>,
    /// From-space base and its bump cursor at the start of the cycle.
    pub base: usize,
    pub used_at_start: usize,
    /// Census: pinned pages, copies into free spans, young survivors promoted
    /// because the free spans ran out, kept survivors.
    pub pages: usize,
    pub copies: u64,
    pub overflow_promotions: u64,
    pub kept_count: u64,
    /// gen r5w4/pin8: `CRATONVM_DBG_STALE_OBJREF`, read when the cycle is
    /// armed. On, every span this cycle VACATES is recorded in `vacated` and
    /// quarantined by the rebuild instead of zeroed and free-listed (see
    /// [`GenerationalHeap::finish_in_place_young_cycle`]).
    pub quarantine: bool,
    /// `(old address, size)` of every survivor this cycle moved out of
    /// from-space's old bytes -- into a destination span or into old gen.
    /// Filled only when `quarantine` is on.
    pub vacated: Vec<(usize, usize)>,
    /// gcf f5/y: the plan named its seeds (`PinnedYoungPlan::roots` is
    /// `Some`: `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`, `_NAMED_ROOTS`, or the
    /// default-ON watched / finalizable filters -- gcj j2/pn2 corrected "exact
    /// roots only"), so a pinned object nothing reached by the end of the
    /// drain is DEAD and the rebuild reclaims it (`finish_in_place_young_cycle`,
    /// except what [`Self::pinned_droppable`] keeps). `false`: every pinned
    /// object was a root and stays.
    pub drop_unvisited_pinned: bool,
    /// gcg g1/y: how many seeds the exact-roots plan named (0 without the
    /// switch), for [`GenerationalHeap::pinned_exact_roots_census_line`].
    pub exact_seeds: u64,
    /// gcj j1/pn: the plan's seeds (`PinnedYoungPlan::roots`), ascending;
    /// empty when every pinned object is a seed (`roots == None`).
    pub seeds: Vec<usize>,
    /// gcj j1/pn (`CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED`, default ON, `=0`
    /// kills): a SEED is never reclaimed by [`Self::drop_unvisited_pinned`],
    /// visited or not. A seed is unvisited when its forward exits before the
    /// pinned arm (the tag and suspect-header screens of
    /// `forward_object_impl`, `ParEvac::evacuate`'s suspect screen; a lost
    /// self-forward in `claim_in_place` was one until gcj j2/x1's retry
    /// superseded that arm at the j2 merge); before the drop mode existed those
    /// exits only skipped its scan, and with it they freed an object a pin
    /// word names. Read once per armed cycle.
    pub protect_seeds: bool,
    /// gcj j2/pn2: [`PinnedYoungPlan::exact_rule`] of the armed plan, for the
    /// census split of [`GenerationalHeap::note_pinned_young_cycle`].
    pub exact_rule: bool,
    /// gcj j2/pn2 (`CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT`, default ON,
    /// `=0` kills): a pinned object whose forward was refused before the
    /// pinned arm is marked [`Self::refused`] (kept, not scanned), so the drop
    /// mode never reclaims an object something reached. Read once per armed
    /// cycle.
    pub keep_refused: bool,
    /// gcj j2/pn2: pinned objects [`Self::note_refused_forward`] marked
    /// (census and tests), each once.
    pub refused_kept: u64,
    /// gcj j3/x2: per pinned object, "a forward of it was refused" -- its own
    /// bit, not `visited`. `visited` is also the serial pinned arm's "already
    /// scanned" bit, so a parallel-only refusal (`bad-forwarding-target`) of
    /// a pinned base that a later serial forward reaches (the loader rescue,
    /// the finalizer resurrection) made that forward skip the scan and the
    /// identity pair the `=0` arm gives it.
    pub refused: Vec<bool>,
}

impl InPlaceEvac {
    pub fn new(
        plan: PinnedYoungPlan,
        dest: Vec<(usize, usize)>,
        initial_free: Vec<(usize, usize)>,
        base: usize,
        used_at_start: usize,
    ) -> Self {
        let pinned_lo = plan.objects.first().map_or(0, |&(b, _)| b);
        let pinned_hi = plan.objects.iter().map(|&(b, s)| b + s).max().unwrap_or(0);
        let live = plan.objects.clone();
        let mut seeds: Vec<usize> = plan.roots.clone().unwrap_or_default();
        seeds.sort_unstable();
        seeds.dedup();
        Self {
            drop_unvisited_pinned: plan.roots.is_some(),
            exact_seeds: plan.roots.as_ref().map_or(0, |r| r.len() as u64),
            protect_seeds: plan.roots.is_some(),
            exact_rule: plan.exact_rule,
            keep_refused: plan.roots.is_some(),
            refused_kept: 0,
            seeds,
            quarantine: cratonvm_gc_core::stale_objref_debug::enabled(),
            vacated: Vec::new(),
            visited: vec![false; plan.objects.len()],
            refused: vec![false; plan.objects.len()],
            pinned: plan.objects,
            pinned_lo,
            pinned_hi,
            kept: FxHashSet::default(),
            dest: dest.into_iter().map(|(s, e)| (s, s, e)).collect(),
            dest_idx: 0,
            scan: Vec::new(),
            live,
            initial_free,
            base,
            used_at_start,
            pages: plan.pages.len(),
            copies: 0,
            overflow_promotions: 0,
            kept_count: 0,
        }
    }

    /// gcj j1/pn: does the rebuild reclaim the `i`th pinned object? Only under
    /// [`Self::drop_unvisited_pinned`], only when nothing reached it, and --
    /// with [`Self::protect_seeds`] -- never a seed.
    #[inline]
    pub fn pinned_droppable(&self, i: usize) -> bool {
        self.drop_unvisited_pinned
            && !self.visited[i]
            && !self.refused[i]
            && !(self.protect_seeds && self.seeds.binary_search(&self.pinned[i].0).is_ok())
    }

    /// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 5): the
    /// pinned objects only [`Self::protect_seeds`] keeps -- seeds the drain
    /// never visited and no refusal marked. The census of
    /// `CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED` (`psk_kept_sum`); 0 without
    /// the drop mode or under the kill.
    pub fn protected_seeds_kept(&self) -> u64 {
        if !(self.drop_unvisited_pinned && self.protect_seeds) {
            return 0;
        }
        (0..self.pinned.len())
            .filter(|&i| {
                !self.visited[i]
                    && !self.refused[i]
                    && self.seeds.binary_search(&self.pinned[i].0).is_ok()
            })
            .count() as u64
    }

    /// gcj j2/pn2: something reached `addr` and its forward was REFUSED
    /// before the pinned arm (a tag or suspect-header screen, a parallel
    /// worker's refusal). If `addr` is a pinned object's base it now counts as
    /// reached -- `visited`, without a scan (its header failed a screen) and
    /// without a `pointer_map` pair (the refusal ledger already says "left
    /// unmoved") -- so [`Self::pinned_droppable`] keeps it: before the drop
    /// mode such an object stayed where it was, and the fail direction stays
    /// over-retention. Inert under the kill (`keep_refused == false`).
    pub fn note_refused_forward(&mut self, addr: usize) {
        if !self.keep_refused {
            return;
        }
        if let Some(i) = self.pinned_index(addr) {
            // gcj j3/x2: its own bit; `visited` stays the scan's.
            if !self.refused[i] {
                self.refused[i] = true;
                self.refused_kept += 1;
            }
        }
    }

    /// Index of the pinned object whose BASE is `addr`.
    #[inline]
    pub fn pinned_index(&self, addr: usize) -> Option<usize> {
        if addr < self.pinned_lo || addr >= self.pinned_hi {
            return None;
        }
        self.pinned.binary_search_by_key(&addr, |&(b, _)| b).ok()
    }

    /// Does `addr` lie inside a pinned object (base or interior)?
    pub fn covers_pinned(&self, addr: usize) -> bool {
        if addr < self.pinned_lo || addr >= self.pinned_hi {
            return false;
        }
        let i = self.pinned.partition_point(|&(b, _)| b <= addr);
        i > 0 && addr < self.pinned[i - 1].0 + self.pinned[i - 1].1
    }

    /// Carve `size` bytes (8-aligned) out of the destination spans.
    pub fn dest_alloc(&mut self, size: usize) -> Option<usize> {
        let n = self.dest.len();
        let mut i = self.dest_idx;
        let stop = n.min(self.dest_idx.saturating_add(IN_PLACE_DEST_SEARCH));
        while i < stop {
            let (_, cur, end) = self.dest[i];
            if end - cur >= size {
                self.dest[i].1 = cur + size;
                return Some(cur);
            }
            if i == self.dest_idx && end - cur < IN_PLACE_MIN_DEST_SPAN {
                // Exhausted for every practical request: never scan it again.
                self.dest_idx += 1;
            }
            i += 1;
        }
        // The tail span is the one large span; try it even past the window.
        if stop < n {
            let (_, cur, end) = self.dest[n - 1];
            if end - cur >= size {
                self.dest[n - 1].1 = cur + size;
                return Some(cur);
            }
        }
        None
    }

    /// Is `addr` a byte this cycle already wrote a copy into? A slot re-read
    /// after it was rewritten names such a byte, and it is not a from-space
    /// object start (the walk found free space there).
    pub fn dest_written(&self, addr: usize) -> bool {
        let i = self.dest.partition_point(|&(s, _, _)| s <= addr);
        i > 0 && addr < self.dest[i - 1].1
    }

    /// Bytes of from-space that stay live after the cycle.
    pub fn live_bytes(&self) -> usize {
        self.live.iter().map(|&(_, s)| s).sum()
    }

    /// The highest offset this cycle wrote a copy at (or the starting cursor).
    pub fn high_water(&self) -> usize {
        self.dest
            .iter()
            .map(|&(_, cur, _)| cur - self.base)
            .max()
            .unwrap_or(0)
            .max(self.used_at_start)
    }
}

/// GC threshold: trigger minor GC when young from-space usage exceeds this %.
pub const YOUNG_GC_THRESHOLD_PERCENT: usize = 50;

/// `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` -- let the moving young cycle proceed
/// even when a conservatively-discovered JIT root lies in young-from. The
/// pre-2026-09-06 behaviour, kept as a one-binary A/B for the diversion this
/// flag names; see the term it gates in `collect_garbage_inner`.
///
/// gci i2/fr (`gci-i1pn-presence-only-flag-reads-...`): `runtime_flag_on`, so
/// `=0` / `=false` / `=off` keep the divert (presence used to drop it). The
/// twin reader `vm/src/memory/roots.rs` `dbg_no_peer_pin_divert` parses the
/// same way (step 14w's weak-mode promise needs both to answer alike).
///
/// gci i12/sx: both twins read the flag snapshot per call (once per young
/// cycle here, once per root collection there), with no process `OnceLock`:
/// the two can no longer latch different answers in one test process, and a
/// per-thread override reaches both.
pub fn gen_no_peer_pin_divert() -> bool {
    false
}

/// t1 lane D — `CRATONVM_GC_WHOLE_HEAP_CARDS` (default OFF, opt-in `=1`;
/// `gcj-j2jt2-proposal-a-card-table-over-the-whole-heap`): build the
/// Generational card table over the HULL of the old generation's reservation
/// and both young semispaces (`CardTable::with_hull`), so the JIT's inline
/// post barrier becomes `idx = mark - hull_lo; cmp idx, hull_len; jae slow;
/// mov byte [(idx >> 9) + hull_map], DIRTY` -- no young / null / old -> old
/// filter, no conditional mark.
///
/// The three reservations are independent OS allocations here (not one
/// contiguous reservation as in HotSpot), and a young semispace can MOVE when
/// it grows (`Arena::grow`) or is swapped for a quarantined arena; that is why
/// the compiled barrier keeps one unsigned hull compare: a holder outside the
/// hull takes the slow edge (`write_barrier`, which ignores any holder outside
/// the old range). A hull more than [`WHOLE_HEAP_CARDS_MAX_HULL_FACTOR`] times
/// the reserved bytes (scattered reservations) is refused and the historical
/// table is kept; so is any construction with the switch off. Read once per
/// heap.
pub fn whole_heap_card_table(old_gen: &OldGen, young: [&Arena; 2]) -> Option<CardTable> {
    let old_base = old_gen.base_ptr() as usize;
    let old_size = old_gen.growth_max();
    let mut lo = old_base;
    let mut hi = old_base.checked_add(old_size)?;
    let mut reserved = old_size;
    for a in young {
        let base = a.base_ptr() as usize;
        // t1 wave 8 lane CARDTBL: the RESERVATION, so a semispace built with
        // headroom (`gen_whole_heap_young_headroom_enabled`) stays inside the
        // hull when it grows in place. Equal to the capacity otherwise.
        let cap = a.in_place_capacity();
        if cap == 0 {
            continue;
        }
        lo = lo.min(base);
        hi = hi.max(base.checked_add(cap)?);
        reserved = reserved.saturating_add(cap);
    }
    let table = CardTable::with_hull(
        old_base,
        old_size,
        cratonvm_gc_core::card_table::CardTableOptions::from_flags(),
        (lo, hi),
        reserved,
        WHOLE_HEAP_CARDS_MAX_HULL_FACTOR,
    );
    if gc_flags().verify_rset || cratonvm_gc_core::gc_flags().dbg_youngstate {
        eprintln!(
            "[whole-heap-cards] hull=[0x{lo:x}, 0x{hi:x}) reserved={reserved} engaged={}",
            table.is_some()
        );
    }
    table
}

/// t1 lane D: the most a whole-heap card map's hull may exceed the bytes the
/// generations reserve before [`whole_heap_card_table`] refuses it (the map
/// is `hull / 512` bytes, mostly untouched pages).
pub const WHOLE_HEAP_CARDS_MAX_HULL_FACTOR: usize = 4;

/// t1 wave 8 lane CARDTBL — `CRATONVM_GC_WHOLE_HEAP_YOUNG_HEADROOM` (default
/// ON, `=0` kills; inert unless the opt-in `CRATONVM_GC_WHOLE_HEAP_CARDS=1` is
/// on; `t1-R-proposal-whole-heap-hull-covers-young-growth`): build each young
/// semispace with its address space reserved up to the young maximum
/// (`Arena::with_headroom`), so `Arena::grow` stays in place and the hull
/// [`whole_heap_card_table`] bakes into compiled code keeps covering young
/// after it grows. Without it every run with no fixed `-Xmn` left the hull at
/// the first young growth (`Arena::new` reserves the initial size only and a
/// growth past it re-reserves anywhere), and every young-holder store then
/// took the compiled mark's miss edge. Sound either way: the hull compare
/// sends any holder outside the hull to the miss edge. A refused headroom
/// reservation keeps the historical arena. Read once per heap.
pub fn gen_whole_heap_young_headroom_enabled() -> bool {
    true
}

/// gcd d9/c: the pure rule of [`GenerationalHeap::young_starved_behind_blocked_drain`].
/// `used`/`free_list`/`capacity` are young from-space's (the bump cursor, the
/// reclaimed free-list bytes, the arena size); the free room is
/// `capacity - (used - free_list)`, the tail plus the reclaimed holes, i.e.
/// the complement of the live occupancy `needs_gc` triggers on. Starved:
/// the last cycle could not drain AND that room is under `min_free`.
#[inline]
pub fn young_starved_behind_blocked_drain_rule(
    drain_blocked: bool,
    used: usize,
    free_list: usize,
    capacity: usize,
    min_free: usize,
) -> bool {
    drain_blocked && capacity.saturating_sub(used.saturating_sub(free_list)) < min_free
}

/// The moving young collection's trigger, as a percentage of from-space
/// capacity. `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<n>` overrides
/// [`YOUNG_GC_THRESHOLD_PERCENT`]; clamped to 1..=95.
///
/// # Why this is a knob and not just a constant (gc-genpause F6)
///
/// The 50 % is documented as "so the to-space can hold all survivors" -- but
/// to-space has the SAME capacity as from-space (`with_capacity` splits the
/// budget into two equal semis, and `Phase 6`'s expansion grows both), and
/// promotion drains survivors to old gen on top of that. The copying
/// collector's actual constraint is `survivors <= to-space capacity`, which
/// permits a trigger far above 50 %. At `-Xmx1g` the 50 % rule collects after
/// 128 MB of allocation out of a 1 GB heap.
///
/// Halving the collection count halves every per-collection cost -- which,
/// after gc-genpause F1/F2/F3, is most of what a minor pause still is. But
/// raising it also raises the peak survivor volume a single to-space must
/// absorb, and a cycle that cannot fit them diverts to the non-moving sweep
/// and spills to old gen. Which effect wins is a property of the workload's
/// survival rate, so this ships as a measurable knob at its historical default
/// rather than as a new default nobody has swept.
///
/// (This block used to sit above `gen_no_peer_pin_divert`, which had been
/// inserted between the doc comment and the function it documents — so
/// `rustdoc` attributed the whole gc-genpause F6 argument to an unrelated
/// one-line flag reader and left the knob itself undocumented.)
///
/// gcg h1/ob (`gcg-g4yg-young-sizing-and-xlog-review-findings` item 7): read
/// per HEAP, at construction (`GenerationalHeap::young_trigger_pct`), not
/// cached per process -- a process-global `OnceLock` gave a second VM in the
/// process (embedding) and a test under `with_thread_overrides` the first
/// reader's value. Uncached: called once per heap construction (and by tests).
pub fn young_trigger_percent() -> usize {
    None::<std::ffi::OsString>
        .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
        .map(|n| n.clamp(1, 95))
        .unwrap_or(YOUNG_GC_THRESHOLD_PERCENT)
}

/// gcf f2/y: the EDEN equivalent adaptive tenuring sizes HotSpot's survivor
/// target from (`tenuring::desired_survivor_bytes`): the young trigger AS
/// CONFIGURED for a from-space of `semi_capacity` bytes
/// (`young_trigger_percent`), not the live `young_gc_threshold`.
///
/// HotSpot Serial's `adjust_desired_tenuring_threshold` reads
/// `to()->capacity()`, which only a heap resize moves; Serial has no pause
/// goal. The live threshold here is the pause-goal loop's output (it halves
/// it toward `capacity / 16`), and on the sweep arm the moving one at that,
/// so tying the target to it lowered the tenuring threshold whenever the loop
/// shortened the nursery -- a coupling HotSpot does not have (gcf f1/y review
/// finding 8).
pub fn tenuring_eden_bytes(semi_capacity: usize, trigger_percent: usize) -> usize {
    semi_capacity.saturating_mul(trigger_percent) / 100
}

/// gci i1/yg: [`sweep_survivor_mark`] for a cycle whose survivors age by
/// `step` (the HotSpot age clock, [`age_clock_advance`], or gc wave 9 / lane
/// gcC's [`cycle_age_step`]: 0 on a cycle the Serial-eden aging epoch holds;
/// both folded by [`merged_age_step`]): step 1 is `sweep_survivor_mark`
/// exactly; step 0 clears the mark and keeps the age. Pure.
#[inline]
pub fn sweep_survivor_mark_by(mark: u32, step: u8) -> (u32, u8) {
    let cleared = mark & !ObjectHeader::mark_with_gc_flags(0, GC_FLAG_MARKED);
    let new_age = ObjectHeader::gc_age_of(cleared)
        .saturating_add(step)
        .min(cratonvm_types::MAX_GC_AGE);
    (ObjectHeader::mark_with_gc_age(cleared, new_age), new_age)
}

/// The opt-out non-moving young collector does not need Cheney-copy headroom:
/// it reclaims dead spans in place and falls back to allocation-failure GC for
/// fragmentation.  Let transient allocation fill most of the active semi-space
/// before paying the O(heap) mark/sweep cost.  The default moving path retains
/// the conservative 50% trigger above so the to-space can hold all survivors.
/// Keeping a 10% reserve also leaves room for TLAB refill granularity and avoids
/// turning every near-capacity refill into an allocation-failure collection.
pub const NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT: usize = 90;

/// Largest free block below which the non-moving young sweep declares its
/// arena defragmentable — see [`young_arena_needs_defragmentation`].
///
/// 64 KiB, chosen to sit well above what a single allocation asks for and well
/// below anything a healthy arena has: the request that exposed this was an
/// 8 KiB `byte[]`, and a young generation that cannot find eight times that
/// contiguously has stopped being an allocation arena. Deliberately absolute
/// rather than a fraction of capacity — the harm is "this specific request no
/// longer fits", which does not scale with heap size.
pub const DEFRAG_LARGEST_FREE_FLOOR: usize = 64 * 1024;

/// Fraction of capacity the bump cursor must have consumed before the sweep
/// will consider defragmenting: `used >= capacity - capacity/64`, i.e. 98.4 %.
/// Below that there is still bump room, so a small largest-free-block says
/// nothing — the arena has simply not been carved up yet.
pub const DEFRAG_BUMP_EXHAUSTED_RECIPROCAL: usize = 64;

/// Has the young from-space degenerated into a free list nobody can allocate a
/// modest contiguous object out of?
///
/// Both halves are required, and each rules out a different false positive:
///
/// * **bump-exhausted** — while the cursor still has room, `largest_free_block`
///   is small simply because nothing has been freed yet. Escalating there would
///   promote a young nursery's entire contents on its first collection.
/// * **largest hole below the floor** — a big `free_list_bytes` with a big
///   largest block is a perfectly healthy swept arena. The fragmentation face is
///   specifically *many bytes free, none of them together*.
///
/// Pure, and split out from the sweep, because the sweep is 3,000 lines long
/// and this predicate decides whether hundreds of megabytes get tenured.
///
/// # gen r4/sweep (2026-09-23): the bump TAIL is a contiguous block too
///
/// `largest_free` is `Arena::largest_free_block()`, which is FREE-LIST ONLY —
/// it never folds in the un-bumped tail `capacity - used`. "Bump-exhausted"
/// here means `used >= capacity - capacity/64`, so the tail can still be up to
/// `capacity / 64` bytes: 1 MiB on the default 64 MiB semi-space, 8 MiB on a
/// 512 MiB one. Every such tail above the 64 KiB floor serves exactly the
/// request whose failure this predicate exists to detect, so reporting
/// "no 64 KiB block" beside it was a false positive — one needless
/// promote-every-unpinned-survivor cycle each time the free list happened to be
/// carved up while the tail still had room. The tail now counts as a
/// candidate largest block. Arenas of 4 MiB or less are unaffected (their
/// bump-exhausted tail is below the floor by construction).
pub fn young_arena_needs_defragmentation(used: usize, capacity: usize, largest_free: usize) -> bool {
    if capacity == 0 {
        return false;
    }
    let bump_exhausted =
        used >= capacity.saturating_sub(capacity / DEFRAG_BUMP_EXHAUSTED_RECIPROCAL);
    // The un-bumped tail is allocatable exactly like a free block (a young
    // semi-space has no high region; see `debug_assert_young_arena_is_low_only`).
    let largest_contiguous = largest_free.max(capacity.saturating_sub(used));
    // A tiny arena (the unit tests') can have a capacity below the floor; there
    // "no 64 KiB block" is not a defect, so require the floor to be meaningful.
    bump_exhausted
        && capacity > DEFRAG_LARGEST_FREE_FLOOR
        && largest_contiguous < DEFRAG_LARGEST_FREE_FLOOR
}

/// Selective-promotion census for the non-moving young sweep.
///
/// `tracing::debug!` is compiled out of release builds (the workspace pins
/// `tracing`'s `release_max_level_info`), so a debug line here would have been
/// invisible in exactly the builds that run the suites. These counters are the
/// diagnostic instead, and they answer the one question the aggregate numbers
/// cannot: when `promoted` stays at 0 for hundreds of cycles, is the pass not
/// running, running and finding nothing tenurable, or finding candidates and
/// rejecting every one?
pub static SP_CENSUS: SelectivePromotionCensus = SelectivePromotionCensus {
    sweeps: AtomicU64::new(0),
    sweeps_selective: AtomicU64::new(0),
    sweeps_defrag: AtomicU64::new(0),
    candidates: AtomicU64::new(0),
    pinned: AtomicU64::new(0),
    unaged: AtomicU64::new(0),
    evacuated: AtomicU64::new(0),
    old_full: AtomicU64::new(0),
};

/// See [`SP_CENSUS`].
pub struct SelectivePromotionCensus {
    /// Non-moving young sweeps entered.
    pub sweeps: AtomicU64,
    /// …of which ran the selective-promotion pass at all.
    pub sweeps_selective: AtomicU64,
    /// …of which escalated to defragmentation.
    pub sweeps_defrag: AtomicU64,
    /// Marked survivors the evacuation walk examined.
    pub candidates: AtomicU64,
    /// …rejected because a root/finalizer value pinned them.
    pub pinned: AtomicU64,
    /// …rejected because they had not reached `PROMOTION_AGE`.
    pub unaged: AtomicU64,
    /// …actually copied into old gen.
    pub evacuated: AtomicU64,
    /// Evacuations that failed because old gen was full.
    pub old_full: AtomicU64,
}

/// Snapshot of [`SP_CENSUS`] as
/// `(sweeps, selective, defrag, candidates, pinned, unaged, evacuated, old_full)`.
pub fn selective_promotion_census() -> (u64, u64, u64, u64, u64, u64, u64, u64) {
    (
        SP_CENSUS.sweeps.load(Ordering::Relaxed),
        SP_CENSUS.sweeps_selective.load(Ordering::Relaxed),
        SP_CENSUS.sweeps_defrag.load(Ordering::Relaxed),
        SP_CENSUS.candidates.load(Ordering::Relaxed),
        SP_CENSUS.pinned.load(Ordering::Relaxed),
        SP_CENSUS.unaged.load(Ordering::Relaxed),
        SP_CENSUS.evacuated.load(Ordering::Relaxed),
        SP_CENSUS.old_full.load(Ordering::Relaxed),
    )
}

// gcf f3/q: indices of `GenerationalHeap::young_premium_census`
// (`[GC] young_premium:`; see `young_premium_census_line`).
pub const YP_MEMO_HITS: usize = 0;
pub const YP_BATCH_WALKS: usize = 1;
pub const YP_3A_REPLAYS: usize = 2;
pub const YP_3A_WALKS: usize = 3;
// gcg g4/yg: slots 4-8 (`yp_3c_listed_*`, `yp_evac_reserved`,
// `yp_coalesce_*`) went with the deleted `CRATONVM_GEN_SWEEP_3C_LISTED_SLOTS`,
// `CRATONVM_GEN_SWEEP_EVAC_RESERVE` and `CRATONVM_GEN_SWEEP_INCREMENTAL_COALESCE`;
// gcg g5/yg: slot 4 (`yp_frag_spill_advised`) went with
// `CRATONVM_GEN_YOUNG_FRAG_SPILL` (vg3 `y1b_frag_prem_on_*`: 3.2x the wall time).
// gch h1/yy: the three opt-ins of the young-performance lane (engagement
// counters for their rows; all stay 0 with the switches off).
/// Selective walks (2) that ran as parallel chunks
/// (`CRATONVM_GEN_SWEEP_PARALLEL_PROMOTION_WALK`).
pub const YP_PAR_WALKS: usize = 4;
/// ...that were attempted and refused (the serial walk then ran).
pub const YP_PAR_WALK_FALLBACKS: usize = 5;
/// Dirty-card seeds run on the pool (`CRATONVM_GEN_SWEEP_PARALLEL_CARD_SEED`).
pub const YP_PAR_CARD_SEEDS: usize = 6;
// gch h3/yc: slot 7 (`yp_refill_spare_caps`) went with the deleted
// `CRATONVM_GEN_TLAB_REFILL_SPARES_LARGE_BLOCKS` (vh1-yy: `hashmap-pair` 3-8x
// slower alone; vh2-ob B: SERIAL_EDEN with it 0/64 3/3).
// gci i22/dr (P2/yy): slots 7-8 for `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`
// (opt-in, [`gen_tlab_refill_smallest_whole_enabled`]); both stay 0 with it off.
/// Refills served by a WHOLE free block smaller than the request -- the
/// refills whose chunk differs from the default arm's (which would have cut a
/// larger block or bumped).
pub const YP_REFILL_WHOLE_BELOW_REQUEST: usize = 7;
/// Refills the switch served at the full request (an exact-size block, or a
/// request-sized head of the smallest larger one: the default arm's chunk).
pub const YP_REFILL_AT_REQUEST: usize = 8;
// gcj j2/yg2: slots 9-10 for `CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY` (default ON,
// [`gen_young_known_zero_carry_enabled`]); both stay 0 with it `=0`.
/// Bytes of the previous known-zero set a sweep kept claiming because they were
/// still on the free list (`Arena::known_zero_within`), summed over sweeps.
pub const YP_KZ_CARRIED_BYTES: usize = 9;
/// Sweeps that carried a non-empty part of the previous set.
pub const YP_KZ_CARRY_SWEEPS: usize = 10;
/// JIT round 23 wave 6 (`CRATONVM_GEN_SWEEP_FUSED_WALK`): sweeps whose
/// parallel prefix took the chunk results the promotion walk decided in the
/// same pass, instead of walking the arena a second time.
pub const YP_FUSED_SWEEPS: usize = 11;
// JIT round 23 wave 8, lane W8-L3: slots 12-14 for
// `CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP` ([`gen_sweep_dead_chunk_skip_enabled`]);
// all stay 0 with it `=0`.
/// Anchored chunks the fused walk declared dead whole without reading a header.
pub const YP_DEAD_CHUNKS: usize = 12;
/// Bytes of those chunks (free-list bytes inside them included).
pub const YP_DEAD_CHUNK_BYTES: usize = 13;
/// Fused walks the skip was declined for (a published reserved-TLAB skip span).
pub const YP_DEAD_CHUNK_DECLINED: usize = 14;
pub const YP_LEN: usize = 15;

/// gcf f3/q (r15w5/gcpage patch 2): the selective-promotion walk's per-object
/// counters, summed on the collecting thread and published once per walk
/// (`CRATONVM_GEN_SWEEP_BATCH_CENSUS`, default ON; `=0` restores one locked
/// RMW per counted object). Every counter is Relaxed and none has a reader
/// inside the walk (`SP_CENSUS` is read by `selective_promotion_census`,
/// diagnostics; the two `HeapStats` counters after the cycle), so publishing
/// the sums leaves the same final values.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvacWalkTally {
    pub candidates: u64,
    pub pinned: u64,
    pub unaged: u64,
    pub evacuated: u64,
    pub old_full: u64,
    pub promoted_objects: u64,
    pub promoted_bytes: u64,
}

impl EvacWalkTally {
    /// Retract `objects` unwound candidates of `bytes` bytes, as
    /// `unwind_evac` retracts them from the atomics on the unbatched path.
    /// Every unwound copy was added here since the watermark at exactly its
    /// size (the copy is byte-exact), so the subtraction cannot underflow; it
    /// saturates anyway rather than wrap a diagnostic.
    pub fn retract(&mut self, objects: u64, bytes: u64) {
        self.promoted_bytes = self.promoted_bytes.saturating_sub(bytes);
        self.promoted_objects = self.promoted_objects.saturating_sub(objects);
        self.evacuated = self.evacuated.saturating_sub(objects);
    }

    /// Add the sums to `SP_CENSUS` and `stats`, one RMW per non-zero counter.
    pub fn publish(&self, stats: &HeapStats) {
        let add = |c: &AtomicU64, v: u64| {
            if v != 0 {
                c.fetch_add(v, Ordering::Relaxed);
            }
        };
        add(&SP_CENSUS.candidates, self.candidates);
        add(&SP_CENSUS.pinned, self.pinned);
        add(&SP_CENSUS.unaged, self.unaged);
        add(&SP_CENSUS.evacuated, self.evacuated);
        add(&SP_CENSUS.old_full, self.old_full);
        add(&stats.objects_promoted, self.promoted_objects);
        add(&stats.bytes_promoted, self.promoted_bytes);
    }
}

/// `CRATONVM_GEN_SWEEP_BATCH_CENSUS` (default ON). gcf f3/q. Read once per sweep.
pub fn gen_sweep_batch_census_enabled() -> bool {
    true
}



/// Byte spacing at which the parallel sweep SUBSAMPLES the object grid the
/// allocator records (`arena.rs`). Small enough that a multi-hundred-megabyte
/// young gen yields far more chunks than workers (so a chunk that stops early
/// cannot skew the split), large enough that the anchor list stays trivial.
///
/// `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` overrides it. Lowering it is how the
/// GC-stress matrix drives the parallel sweep on a small young gen, where the
/// 8 MiB default would produce a single chunk and silently never exercise it.
#[inline]
pub fn sweep_anchor_stride() -> usize {
    cratonvm_gc_core::gc_flags().gc_sweep_anchor_stride
}

/// Tripwire for the ONE assumption every young linear walk in this file makes
/// and none of them states: that `Arena::used()` is the low bump cursor.
///
/// gengc-round2-core2 (2026-09-20), closing
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// `Arena` has TWO bump regions and `used()` is documented as "both bump
/// regions together" — `cursor + (data.len() - high_cursor)`. That is the
/// right number for an OCCUPANCY question ("how much of this arena is spoken
/// for": `needs_gc`'s trigger triple, `live_bytes_estimate`,
/// `young_from_occupancy`, `report_fatal_oom`) and the wrong one for an
/// EXTENT question ("where does the low region end"). Several from-space
/// walks use it as the latter — `mark_young_to_old_refs`,
/// `fixup_young_old_refs`, the two `jit_tlab_skip_offsets(base, base +
/// young_from.used())` windows, `collect_garbage_inner`'s object-start walk,
/// and `sweep_young_non_moving`'s walks (which pass it on through
/// `SweepCtx::used` and the anchor list's terminal entry).
///
/// They are correct today for a property of the CALL GRAPH, not of the code:
/// nothing outside `arena.rs`'s own tests calls `Arena::alloc_high`, so a
/// young semi-space has `high_cursor == capacity` and `used() == cursor`
/// exactly. `Arena::grow` preserves that (it moves `high_cursor` to the new
/// capacity and debug-asserts it was at the old one), so the invariant holds
/// across expansion too.
///
/// The day someone gives the young arena a large-object end — which ZGC's
/// arena already has, and which is a reasonable thing for a future round to
/// try — every walk above strides past the low cursor into the UN-BUMPED
/// MIDDLE, which `decommit_unbumped_middle` / `uncommit_evacuated_young` hand
/// back to the OS. `mark_young_to_old_refs` would parse it as headers;
/// `fixup_young_old_refs` **writes** through `forward_ref_slots`, and
/// `Arena::decommit_free_blocks` records that a released granule FAULTS on
/// write rather than reading back zero. There would be no compile error and
/// no test failure until a workload happened to allocate high.
///
/// So: fail loudly in test/debug builds instead. `high_region_bytes()` is
/// `capacity - high_cursor`, so asserting it is zero is exactly
/// `used() == low_cursor` without needing an `Arena::low_cursor()` accessor
/// that lives in another lane's file. When this fires, the fix is the gap
/// page's step 2 — add `Arena::low_cursor()` and use it at every EXTENT site
/// — not to delete the assert.
#[inline]
pub fn debug_assert_young_arena_is_low_only(arena: &Arena, site: &'static str) {
    debug_assert_eq!(
        arena.high_region_bytes(),
        0,
        "[{site}] a young semi-space has served alloc_high: `Arena::used()` \
         ({}) is no longer the low bump cursor, and every young linear walk in \
         gen_heap.rs uses it as one. See \
         docs/feature-designs/generative-gc-architecture.md",
        arena.used(),
    );
}

/// Is `addr` an object BASE in the old-generation object grid `walked` yielded
/// for this pause?
///
/// gengc-round2-core2 (2026-09-20): the mark phase used to carry a SECOND,
/// `usize`-only copy of `OldGen::walk_objects()`' output ("`walked_bases`")
/// purely so this question could be a bare `binary_search`. That is an
/// n-element allocation and copy of data already materialised, made inside the
/// stop-the-world pause and proportional to the old live set;
/// `close_live_set_over_old_gen` had always searched the original slice by key
/// instead, which is what this does. See
/// `docs/feature-designs/generative-gc-architecture.md`.
#[inline]
pub fn walked_grid_has_base(walked: &[(*mut u8, usize)], addr: usize) -> bool {
    walked
        .binary_search_by_key(&addr, |&(p, _)| p as usize)
        .is_ok()
}

/// gen r5w5/old9 (2026-09-27) — the old-gen mark's ADMISSION ORACLE, as a
/// trait, so the mark's push sites can run over either of its two forms:
///
/// * the walked object grid (`[(*mut u8, usize)]`, `OldGen::walk_objects`'
///   output for this pause) — every collection by default, exactly as before
///   (the methods below are the expressions the sites used to inline);
/// * the block-offset table's bounded query (`live_sweep::LiveOracle`, over
///   `OldGen::object_at`) — the O(live) collection, `CRATONVM_GC_OLD_LIVE_SWEEP`
///   (DELETED by gcg g5/og; today the walked grid and the shared start set,
///   [`MarkAdmission`], are the implementors).
///
/// `gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep`
/// counts seven consumers of the grid; the ones the O(live) collection runs
/// (`mark_and_push_old_gen_in`, `rescue_mark_candidate_in`,
/// `scan_object_for_old_refs_in`, `mark_young_to_old_refs`) are generic over
/// this. The others (the pinned-compaction pins, the walk-gap seed, the
/// finalizer pass, the walked closure) run only on the walked path.
pub trait OldMarkGrid {
    /// Is `addr` an object base?
    fn has_base(&self, addr: usize) -> bool;
    /// The object `addr` lies STRICTLY inside, as `(base, size)`.
    fn interior_base(&self, addr: usize) -> Option<(usize, usize)>;
    /// `Some(is_base)` where this oracle is authoritative for `addr`;
    /// `None` where only the byte screen can decide (the walked grid below
    /// its first base).
    fn authoritative_base(&self, addr: usize) -> Option<bool>;
    /// A push site just marked `addr` and pushed it. The O(live) oracle
    /// records it (its kept set); the walked grid needs nothing.
    #[inline]
    fn note_marked(&self, _addr: usize) {}
    /// A push site found `addr` ALREADY marked. `true` asks the site to push
    /// it anyway: the O(live) oracle answers `true` once for a mark this pause
    /// did not set (a stale bit), so the object is traced and kept rather
    /// than skipped and — outside the kept set — freed. The walked path's
    /// closure and free loop cover a stale mark over the whole grid instead.
    #[inline]
    fn retrace_stale_mark(&self, _addr: usize) -> bool {
        false
    }
}

impl OldMarkGrid for [(*mut u8, usize)] {
    #[inline]
    fn has_base(&self, addr: usize) -> bool {
        walked_grid_has_base(self, addr)
    }
    #[inline]
    fn interior_base(&self, addr: usize) -> Option<(usize, usize)> {
        old_gen_interior_root_base(addr as *mut u8, self)
    }
    /// Bounded by the FIRST base only — see `mark_and_push_old_gen_in`.
    #[inline]
    fn authoritative_base(&self, addr: usize) -> Option<bool> {
        match self.first() {
            Some(&(lo_ptr, _)) if addr > lo_ptr as usize => Some(walked_grid_has_base(self, addr)),
            _ => None,
        }
    }
}

impl OldMarkGrid for Vec<(*mut u8, usize)> {
    #[inline]
    fn has_base(&self, addr: usize) -> bool {
        self.as_slice().has_base(addr)
    }
    #[inline]
    fn interior_base(&self, addr: usize) -> Option<(usize, usize)> {
        self.as_slice().interior_base(addr)
    }
    #[inline]
    fn authoritative_base(&self, addr: usize) -> Option<bool> {
        self.as_slice().authoritative_base(addr)
    }
}

/// gcf f2/c — step 1 of
/// `gengc-r5w5-old9-proposal-parallel-old-gen-phases-on-the-evac-pool-20260927`:
/// the per-phase wall time of one stop-the-world old-gen collection
/// (`old_gen_gc_inner`), so the dominant phase -- the one worth running on the
/// evacuation pool -- is measured before anything is parallelised. Printed on
/// stderr under `CRATONVM_DBG_GCPHASE` (the young sweep's `[gcphase]` lines'
/// switch), one line per phase: `[gcphase] old-gen <phase>: phase=<us>us
/// total=<us>us`. Phases, in order: `live-sweep` (the O(live) collection
/// answered; nothing else follows), `walk`, `roots`, `young-seed`, `mark`
/// (BFS, finalizer round, in-mark closures), then one arm:
/// `compact-around-pins`; or `close` and `free`; or `compact` and `fixup`.
/// Off, two `Instant::now()` per collection.
pub struct OldPhaseTimer {
    pub on: bool,
    pub start: std::time::Instant,
    pub last: std::time::Instant,
}

impl OldPhaseTimer {
    pub fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            on: gc_flags().dbg_gcphase,
            start: now,
            last: now,
        }
    }

    #[inline]
    pub fn phase(&mut self, name: &str) {
        if !self.on {
            return;
        }
        let now = std::time::Instant::now();
        eprintln!(
            "[gcphase] old-gen {name}: phase={}us total={}us",
            now.duration_since(self.last).as_micros(),
            now.duration_since(self.start).as_micros(),
        );
        self.last = now;
    }

    /// gcg g3/e (the g2/e report's hunk B): a sub-total measured inside the
    /// phase the previous `phase` call closed: `[gcphase] old-gen <name>:
    /// part=<us>us`. A reader summing `phase=` per name keeps these apart
    /// (they carry `part=`, not `phase=`, and their own names).
    #[inline]
    pub fn part(&self, name: &str, us: u128) {
        if self.on {
            eprintln!("[gcphase] old-gen {name}: part={us}us");
        }
    }

    /// gch h4/pc: a count taken inside the phase the previous `phase` call
    /// closed: `[gcphase] old-gen <name>: count=<n>` (neither `phase=` nor
    /// `part=`, so a reader summing those is unaffected).
    #[inline]
    pub fn count(&self, name: &str, n: usize) {
        if self.on {
            eprintln!("[gcphase] old-gen {name}: count={n}");
        }
    }

    /// Is the timer printing? (The caller then times its sub-parts.)
    #[inline]
    pub fn is_on(&self) -> bool {
        self.on
    }
}

/// gcf f2/c — the walked grid's object BASES as a bitmap, one bit per 8-byte
/// granule from the first base to the last. Exactly the set
/// [`walked_grid_has_base`] searches, answered in O(1).
///
/// gcg g3/e: the bits are an `old_gen::WalkStartBits` (the closures' g2/e
/// start set: the same layout, so the same answers), so one set built per
/// collection serves the mark AND every closure call
/// (`CRATONVM_GC_OLD_SHARED_STARTS`, [`old_shared_starts_enabled`]).
pub struct WalkedStarts(pub crate::old_gen::WalkStartBits);

impl WalkedStarts {
    /// `None` for an empty grid, or one that is not ascending or holds an
    /// unaligned base (neither happens on a sane generation; the caller then
    /// keeps the binary search).
    pub fn build(walked: &[(*mut u8, usize)]) -> Option<Self> {
        let mut prev: Option<usize> = None;
        for &(p, _) in walked {
            let a = p as usize;
            if a % 8 != 0 || prev.is_some_and(|q| a <= q) {
                return None;
            }
            prev = Some(a);
        }
        crate::old_gen::WalkStartBits::build(walked).map(Self)
    }

    #[inline]
    pub fn has(&self, addr: usize) -> bool {
        self.0.contains(addr)
    }

    /// The closures' view of the same set (`OldGen::close_live_set_shared`).
    #[inline]
    pub fn bits(&self) -> &crate::old_gen::WalkStartBits {
        &self.0
    }
}

/// gcg g3/e — `CRATONVM_GC_OLD_SHARED_STARTS` (default ON since gcg g6/og,
/// `=0` off; vg5 `og5_ss_*`: the live-heavy probe faster in 5/5 interleaved
/// pairs, the steady probe not slower in pairs, every row SAME):
/// `gcg-g2e-proposal-one-walked-start-bitmap-per-major`. The walked
/// stop-the-world old-gen collection builds ONE start set right after its
/// walk ([`WalkedStarts`]) and admits through it on the BFS (as the deleted
/// `CRATONVM_GC_OLD_MARK_ADMISSION=bitmap` did) AND on every closure call -- the in-mark ones of a
/// true-root major and the sweep's -- instead of a binary search per edge
/// (or, under `CRATONVM_GC_OLD_CLOSE_BITMAP`, a fresh bitmap per closure
/// call). Same set of bases as the grid, so the same answers. Read once per
/// walked collection.
pub fn old_shared_starts_enabled() -> bool {
    true
}

/// gcf f2/c — membership in a set for NON-DECREASING queries: the members
/// `keep` admits, sorted once, and a cursor that only moves forward. The
/// in-place sweep's free loop asks "is this object watched?" once per walked
/// object in ascending order; this answers it in O(1) amortised without
/// hashing, where the set's `contains` cost one SipHash per object, live or
/// dead. A query below an earlier one may miss a member (callers never make
/// one; the debug build asserts it).
pub struct AscendingMembership {
    pub sorted: Vec<usize>,
    pub at: usize,
    pub last: usize,
}

impl AscendingMembership {
    pub fn new(set: Option<&cratonvm_gc_core::gc_quiescence::WatchedSnapshot>, keep: impl Fn(usize) -> bool) -> Self {
        let mut sorted: Vec<usize> = set
            .map(|s| s.iter().copied().filter(|&a| keep(a)).collect())
            .unwrap_or_default();
        sorted.sort_unstable();
        Self {
            sorted,
            at: 0,
            last: 0,
        }
    }

    #[inline]
    pub fn contains(&mut self, addr: usize) -> bool {
        debug_assert!(
            addr >= self.last,
            "AscendingMembership queried out of order"
        );
        self.last = addr;
        while self.at < self.sorted.len() && self.sorted[self.at] < addr {
            self.at += 1;
        }
        self.sorted.get(self.at) == Some(&addr)
    }
}

/// gcf f2/c — the walked collection's mark admission on the BFS's slot scan
/// and the legacy young seed: the shared start set when
/// `CRATONVM_GC_OLD_SHARED_STARTS` built one, else the walked grid itself,
/// expression for expression. (gcg g4/og deleted the `bot` arm with
/// `CRATONVM_GC_OLD_MARK_ADMISSION`: triage tg1 measured no gain.)
pub struct MarkAdmission<'a> {
    pub walked: &'a [(*mut u8, usize)],
    pub starts: Option<&'a WalkedStarts>,
}

impl OldMarkGrid for MarkAdmission<'_> {
    #[inline]
    fn has_base(&self, addr: usize) -> bool {
        if let Some(s) = self.starts {
            return s.has(addr);
        }
        walked_grid_has_base(self.walked, addr)
    }
    #[inline]
    fn interior_base(&self, addr: usize) -> Option<(usize, usize)> {
        old_gen_interior_root_base(addr as *mut u8, self.walked)
    }
    /// The walked grid's rule (bounded by the FIRST base), with the base
    /// question answered by this admission.
    #[inline]
    fn authoritative_base(&self, addr: usize) -> Option<bool> {
        match self.walked.first() {
            Some(&(lo_ptr, _)) if addr > lo_ptr as usize => Some(self.has_base(addr)),
            _ => None,
        }
    }
}

/// The defining loader of `class_id` for a YOUNG collector of this heap: the
/// `loader_pin` row inside `young_from` when several VMs share the class id.
///
/// gc-common w36-d, the Generational marker half of
/// `common-w4b-loader-pin-collision-unroots-another-vms-statics`. Every young
/// site that follows the instance->loader edge acts only on a loader inside
/// its own from-space (it forwards or marks it), so "the row in my
/// from-space" is exactly the row it needs: with a colliding id the VM-less
/// `loader_pin_addr` could name another VM's loader, the site then did
/// nothing, and THIS VM's young loader lost its only instance->loader edge.
/// With one row, or no row in this from-space, the answer is the VM-less one,
/// and the site's own `contains` test rejects a foreign address as before.
///
/// gci i10/lp: `pins` is the cycle's snapshot (`PromotionQueue::loader_pins`,
/// `CRATONVM_GEN_YOUNG_LOADER_PIN_SNAPSHOT`); `Some` answers from it with the
/// same row choice, `None` is the per-object registry lookup.
#[inline]
pub fn young_loader_pin(
    class_id: u32,
    young_from: &Arena,
    pins: Option<&FxHashMap<u32, usize>>,
) -> Option<usize> {
    cratonvm_types::loader_pin::loader_pin_addr_where_in(pins, class_id, |a| {
        young_from.contains(a as *mut u8)
    })
}

/// The old generation's finalizer-resurrection pass (gc-common w36-d, the
/// w6-d handoff `handoff-w6d-generational-old-gen-finalizer-resurrection.md`).
///
/// Both young collectors judge only YOUNG finalizer candidates (the moving
/// Phase 2.5's `young_from.contains`, `sweep_young_non_moving`'s `in_young`),
/// so until w36-d a finalizable object that died in the OLD generation was
/// freed in place by `old_gen_gc` and its `finalize()` never ran
/// (`tools/probes/OldGenFinalizeProbe.java`: 0/64 on Generational, 64/64 on
/// HotSpot, G1 and ZGC). `old_gen_gc_inner` now runs [`Self::seed`] once its
/// root closure is complete and traces what it seeds.
///
/// * `candidates` — the collection's `finalizer_addrs` (every registered,
///   unclaimed finalizable plus the queued ones), PRE-collection addresses;
///   old-gen objects have not moved when the pass runs.
/// * `resurrected` — the candidates the pass found unmarked (dead) and kept,
///   at their PRE-compaction addresses. The caller maps them through the
///   collection's map and reports them with the young collectors' dead
///   finalizers.
/// * `closure` — every object the resurrection round marked (the resurrected
///   ones included), PRE-collection addresses: the depth-2 set of
///   `VmHeap::take_resurrection_closure`. Only objects the strong closure had
///   NOT marked can appear, because the round starts with the strong closure's
///   worklist empty and every push site marks before it pushes. gcj j4/rf
///   (`CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`): under the true-root seed this
///   includes the YOUNG objects the round reached (the trace's grid), and a
///   popped promotion destination is recorded as its young SOURCE -- the old
///   address it has now is not the pre-collection address the reference
///   processor's rows hold. Off: old pops only, destinations as popped.
/// * `deferred_seeds` — gcj j2/rf2 (ec47,
///   `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`): addresses a caller would
///   otherwise have made ROOTS of the strong closure only because a
///   finalizable needs them kept (the moving cycle's promoted resurrected
///   finalizables, `finalizers_in_old`, and the moving true-root major's
///   finalizer seeds, at their post-copy addresses). `old_gen_gc_inner` marks
///   them in the resurrection round instead, so what only they reach lands in
///   `closure`, and the reference processor clears weak references to it
///   before `finalize()` runs (HotSpot's order). ALWAYS honoured when
///   non-empty, whatever the switch reads in the callee: a deferred seed left
///   unmarked would be freed under its finalizer row.
/// * `young_resurrected` — gcj j3/rf3 (ec47, the legacy seed,
///   `CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`): the CURRENT addresses,
///   sorted and deduplicated, of this pause's young resurrection set (the dead
///   young finalizables and the young objects only they reach,
///   [`GenerationalHeap::young_resurrection_members`]). The legacy young seed
///   (`mark_young_to_old_refs_deferring`) moves the old referents of a member
///   into `deferred_seeds` instead of marking them, so what only a dead young
///   finalizable reaches in old gen lands in `closure`. Empty: every young
///   object seeds as before. Taken by `old_gen_gc_inner`'s legacy arm.
/// * `closure_young_post` — gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals`
///   item 1, `CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`): the young objects the
///   resurrection round reached on a moving (P-C) major, at their POST-copy
///   addresses (the trace's grid is the copies there); the moving caller maps
///   them back to their pre-copy sources into `closure`. On every other major
///   the round's young objects are in place and go to `closure` directly.
/// * `defers_seeds` — gcj j3/x2 R3: `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`
///   read ONCE, when the pass is built, and read from here by both readers of
///   one moving major (its Phase 5 caller and `old_gen_gc_inner`), which used
///   to read the flag snapshot each. (A disagreement was safe both ways --
///   `deferred_seeds` are always honoured -- but the one read is what the
///   switch's doc promised.)
pub struct OldGenFinalizerPass<'a> {
    pub candidates: &'a [usize],
    pub resurrected: Vec<usize>,
    pub closure: Vec<usize>,
    pub deferred_seeds: Vec<usize>,
    pub young_resurrected: Vec<usize>,
    pub closure_young_post: Vec<usize>,
    pub defers_seeds: bool,
}

impl<'a> OldGenFinalizerPass<'a> {
    pub fn new(candidates: &'a [usize]) -> Self {
        Self {
            candidates,
            resurrected: Vec::new(),
            closure: Vec::new(),
            deferred_seeds: Vec::new(),
            young_resurrected: Vec::new(),
            closure_young_post: Vec::new(),
            defers_seeds: gen_major_defers_finalizer_seeds_enabled(),
        }
    }

    /// Mark and push every old-gen candidate the root closure left unmarked,
    /// recording it in `resurrected`. Called with the closure's worklist
    /// EMPTY; the caller drains what this pushes.
    ///
    /// Two refusals keep the verdict "unmarked means dead" honest:
    ///
    /// * **An incomplete walk** (it covers fewer bytes than are allocated and
    ///   the gap was not recovered): objects behind the break were never
    ///   traced, so an unmarked candidate may be live. The in-place sweep
    ///   retains the whole generation on such a cycle and the compactor
    ///   abandons, so nothing is freed and nothing is resurrected — the
    ///   candidates stay registered for a later collection.
    /// * **A mark gap**: the mark's push sites fail open (GCAUD-8), which is
    ///   why the in-place sweep and the compactor both close the live set
    ///   over marked referrers before they free. Run the same closure FIRST
    ///   when a candidate is unmarked, so a candidate that a marked object
    ///   still references is retained as LIVE rather than finalized while
    ///   reachable. Paid only on a cycle with an unmarked old candidate.
    ///
    /// gch h4/pc: `live_set_closed` -- the caller's own closure already ran to
    /// a fixed point on exactly this mark state (the true-root seed's
    /// pre-close returned `n == 0` and nothing was marked since), so this
    /// closure would rescue nothing: it is skipped
    /// (`CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE`).
    pub fn seed(
        &mut self,
        old_gen: &OldGen,
        walked: &[(*mut u8, usize)],
        walk_gaps: &[(usize, usize)],
        gap_recovery: bool,
        worklist: &mut Vec<*mut u8>,
        live_set_closed: bool,
    ) {
        let is_unmarked_old_base = |addr: usize| -> bool {
            if addr == 0
                || addr % 8 != 0
                || !old_gen.contains(addr as *const u8)
                || !walked_grid_has_base(walked, addr)
            {
                return false;
            }
            // SAFETY: `addr` is an object base the walked grid yielded under
            // the old-gen lock held for this pause, so its header is readable.
            let header = unsafe { &*(addr as *const ObjectHeader) };
            header.gc_flags() & GC_FLAG_MARKED == 0
        };
        if !self.candidates.iter().any(|&a| is_unmarked_old_base(a)) {
            return;
        }
        let walked_bytes: usize = walked.iter().map(|&(_, size)| size).sum();
        if walked_bytes != old_gen.used() && !gap_recovery {
            OLD_FINALIZER_PASS_WALK_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if !live_set_closed {
            let _ = if gap_recovery {
                old_gen.close_live_set_with_gaps(walked, walk_gaps)
            } else {
                old_gen.close_live_set(walked)
            };
        }
        for &addr in self.candidates {
            if !is_unmarked_old_base(addr) {
                continue; // young, not a base, reached, or a duplicate
            }
            // SAFETY: as in `is_unmarked_old_base` — a walked old-gen base.
            let header = unsafe { &mut *(addr as *mut ObjectHeader) };
            header.add_gc_flags(GC_FLAG_MARKED);
            worklist.push(addr as *mut u8);
            self.resurrected.push(addr);
        }
        if !self.resurrected.is_empty() {
            OLD_FINALIZERS_RESURRECTED.fetch_add(self.resurrected.len() as u64, Ordering::Relaxed);
        }
    }
}

/// gc-common w36-d: old-gen finalizables the old collectors kept for
/// `finalize()` (`OldGenFinalizerPass`), run total.
pub static OLD_FINALIZERS_RESURRECTED: AtomicU64 = AtomicU64::new(0);
/// gc-common w36-d: old-gen collections whose finalizer pass declined to judge
/// because the object walk did not cover the generation (see
/// `OldGenFinalizerPass::seed`).
pub static OLD_FINALIZER_PASS_WALK_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// DBG (`CRATONVM_DBG_YOUNG_TRIGGER=1`): print every 4096th young-GC trigger
/// evaluation so a wedged run can be told apart from a genuinely full heap —
/// `live` is `cursor - free_list`, the metric the trigger actually uses.
pub fn young_trigger_debug(
    used: usize,
    free_list: usize,
    live: usize,
    threshold: usize,
    non_moving: bool,
) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        false    }) {
        return;
    }
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n % 4096 != 0 {
        return;
    }
    eprintln!(
        "[young-trigger] n={n} cursor={}MB free_list={}MB live={}MB threshold={}MB non_moving={non_moving}",
        used / 1048576, free_list / 1048576, live / 1048576, threshold / 1048576,
    );
}

/// Which occupancy fires the next young collection.
///
/// `pause_goal_armed` is what makes [`GenerationalHeap::adapt_young_trigger_to_pause`]
/// anything other than a counter that moves. Without it the adapted value is
/// consulted ONLY on the moving branch, and the branch is chosen by
/// [`next_young_gc_is_guaranteed_non_moving`] — which, on any JIT-heavy
/// workload, answers "non-moving" at essentially every allocation because an
/// unregistered compiled frame is on the stack. `CRATONVM_DBG_YOUNG_TRIGGER`
/// on the OAuth2 6-lane repro prints `threshold=460MB non_moving=true` for the
/// whole run while `[gcpause]` reports the trigger dutifully adapting
/// `262144KB -> 131072KB -> 65536KB -> 32768KB` and every collection still
/// arriving with `young_bytes_before` ~272 MB. The prediction is not even
/// right: those cycles run the MOVING path (they report `cheney_drain`).
///
/// A pause goal is a promise about the PAUSE, not about which code path takes
/// it, so when one is armed the adapted value bounds both branches. With no
/// goal (`CRATONVM_GC_YOUNG_PAUSE_MS=0`) this is byte-for-byte the previous
/// behaviour.
///
/// gcd d5/q: a goal IS the default (`DEFAULT_YOUNG_PAUSE_GOAL_MS`, 200 ms,
/// since 2026-08-11); this doc used to call "no goal" the default. So on a
/// JIT-warm program, where every cycle sweeps in place, the sweep ran at the
/// MOVING threshold -- at most `young_trigger_percent()` (50 %) of the semi,
/// less after the loop halved it -- about 4-5x HotSpot Serial's young cycle
/// count on `GenR4W4EvacThroughputProbe`
/// (`gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`). The
/// opt-in that gave the sweep arm its own loop (gcd d5/q) was deleted by gcg
/// g6/yg after two failed A/Bs.
#[inline]
pub const fn young_gc_trigger_bytes(
    capacity: usize,
    moving_threshold: usize,
    non_moving_young: bool,
    pause_goal_armed: bool,
) -> usize {
    if non_moving_young {
        let non_moving = capacity * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100;
        if pause_goal_armed && moving_threshold < non_moving {
            moving_threshold
        } else {
            non_moving
        }
    } else {
        moving_threshold
    }
}





/// t1 wave 8, lane PIN10: the mark word a pinned or kept survivor of a pinned
/// in-place cycle is left with under `CRATONVM_GEN_PINNED_COPY_AGES_IN_PLACE`
/// -- the age `step` higher, saturating at `MAX_GC_AGE` -- and that new age.
/// Unlike [`sweep_survivor_mark_by`] it leaves every flag alone: the in-place
/// copy never header-marks a survivor. Step 0 (a held cycle) is the identity.
/// Pure.
#[inline]
pub fn in_place_aged_mark(mark: u32, step: u8) -> (u32, u8) {
    let new_age = ObjectHeader::gc_age_of(mark)
        .saturating_add(step)
        .min(cratonvm_types::MAX_GC_AGE);
    (ObjectHeader::mark_with_gc_age(mark, new_age), new_age)
}

/// t1 wave 8, lane PIN10: age one pinned or kept survivor of a pinned in-place
/// cycle in place ([`in_place_aged_mark`]) and return its `(old, new)` age.
///
/// # Safety
/// `obj` must be a from-space object base whose mark word is NOT a forward
/// (the serial arm never writes one there; the parallel arm calls this after
/// `absorb_parallel_in_place` restored it), with every mutator stopped and no
/// collector thread reading or writing the word.
#[inline]
pub unsafe fn age_in_place_survivor(obj: usize, step: u8) -> (u8, u8) {
    // SAFETY: the caller's contract: a whole, quiescent header at `obj`.
    let h = unsafe { &*(obj as *const ObjectHeader) };
    let mark = h.mark_word.load(Ordering::Relaxed);
    let (aged, new_age) = in_place_aged_mark(mark, step);
    h.mark_word.store(aged, Ordering::Relaxed);
    (ObjectHeader::gc_age_of(mark), new_age)
}

/// `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK` -- default on (default on),
/// read once per heap at construction. gcj j1/yg, the narrowing of
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// A term-4 cycle (unrewritable conservative JIT roots) is offered to the
/// pinned in-place copy only when term 4 is its ONLY divert term
/// (`term4_alone` in `collect_garbage_inner_with_pins`). One of the terms that
/// takes the offer away is `honor_promotion_oom_risk` (both generations at
/// least 90 % full, with conservative roots), whose whole reason is the
/// MOVING copy's `process::abort()` when neither old gen nor to-space can take
/// a survivor. The pinned copy has no such abort: it never touches to-space,
/// and a survivor neither a free from-space span nor old gen can take STAYS
/// (`InPlaceEvac::kept`, the parallel arm's `in_place_kept`) -- the argument
/// [`parallel_young_copy_permitted`] already makes for its in-place arm. So
/// on such a cycle the guard sends the copy's cycle to the sweep for a hazard
/// the copy does not have. vi8-pe A (`vi47w-verdicts-jw3.md`, i48k) measured
/// that about 45 % of the term-4 cycles of `jor` (an OOME-shaped, near-full
/// heap) never reached the copy (`pycopy_*` declines 0, the rest
/// `nonmoving-unrewritable-conservative-jit-roots`, the reason chain's label
/// for term 4 with a second term). With this switch those cycles copy.
/// default on: it changes which arm a near-full JIT-warm heap collects with,
/// so the deciding rows (`docs/feature-designs/generative-gc-architecture.md`)
/// come first.
pub fn gen_pinned_copy_takes_promotion_risk_enabled() -> bool {
    true
}

/// gcj j2/yg2: `CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY` -- default ON
/// (`runtime_flag_default_on`, `=0` kills), read once per heap at
/// construction ([`GenerationalHeap::young_known_zero_carry`]).
///
/// The cost the gcj j1 A/B charged to `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`
/// (`GcfF3yYoungPremiumProbe` `alloc-ring` 1.14x, `alloc-dead` 1.05x) is a
/// second MEMSET, not the refill's search: on the vi22-dr YY rows the switch
/// moved `gen_zeroed_reuse_bytes` from 1.47 GB to 2.57-2.65 GB of 5.4 GB
/// carved (`gen_zero_skipped_bytes` 4.87 -> 3.66-3.74 GB) at about the same
/// refill count. A refill skips its memset only for a free-list hand-out
/// wholly inside the sweep's known-zero set (`Arena::hand_out_is_known_zero`),
/// and that set held only the spans the LAST sweep reclaimed: a block that was
/// already free (and zero) when the sweep ran, a coalesced block made of such
/// a block and a reclaimed span, and every block after a TLAB tail return
/// (which forgot the whole set, pause-time retires included) were memset
/// again. The default carve cuts request-sized heads off the large reclaimed
/// spans, so it rarely meets those blocks; the smallest-whole refill takes
/// exactly them first.
///
/// On: the sweep keeps the claim for the bytes of the old set that are still
/// on the free list (`Arena::known_zero_within`, unioned with its own spans by
/// `arena::union_known_zero_spans`), and the tail sink returns tails through
/// `Arena::return_unused_tail_keeping_known_zero` (a sunk tail is zero by the
/// sink's own contract). Counted on `[GC] young_premium:` as
/// `yp_kz_carried_bytes=` / `yp_kz_carry_sweeps=`. `=0`: the historical set
/// (this sweep's spans only; a tail return forgets it).
pub fn gen_young_known_zero_carry_enabled() -> bool {
    true
}

/// JIT round 23 wave 4, lane L4: `CRATONVM_GEN_IN_PLACE_LAZY_ZERO` (default
/// ON, `0` restores the in-pause memset). A pinned in-place young cycle
/// poisons the headers of the objects in its dead spans and leaves the rest of
/// the dead bytes to be zeroed at hand-out (`finish_in_place_young_cycle`, the
/// arena's lazy-dirty set). Read per cycle.
pub fn gen_in_place_lazy_zero_enabled() -> bool {
    !gen_young_zero_on_reclaim_enabled()
}

/// t1 wave 8, lane YGZERO (book `gcj-j2yg2`): `CRATONVM_GEN_YOUNG_ZERO_ON_RECLAIM`
/// (default OFF, the measuring arm). HotSpot Serial's shape: young memory is
/// zeroed when a pause reclaims it and never again at hand-out. On, it forces
/// both lazy pause doors off ([`gen_in_place_lazy_zero_enabled`],
/// [`gen_sweep_lazy_zero_enabled`]) -- the sweep then zeroes and claims its
/// spans as before -- and closes the one door that zeroed without claiming:
/// the pinned in-place rebuild now claims the dead spans it just zeroed
/// ([`zero_on_reclaim_claim`], `finish_in_place_young_cycle`), so a free-list
/// hand-out wholly inside one skips its memset (`gen_zero_skipped_bytes`).
/// The lazy-dirty check at both hand-out doors still wins over any claim.
pub fn gen_young_zero_on_reclaim_enabled() -> bool {
    false
}

/// JIT round 23 wave 6, lane L4: `CRATONVM_GEN_SWEEP_LAZY_ZERO` (default ON,
/// `0` restores the in-pause memset). The non-moving young sweep leaves its
/// reclaimed spans to the arena's lazy-dirty set and its exact mirror instead
/// of zeroing them inside the pause (`sweep_young_non_moving`, zero-and-publish).
/// Latched when the heap is built ([`GenerationalHeap::sweep_lazy_zero`]).
pub fn gen_sweep_lazy_zero_enabled() -> bool {
    !gen_young_zero_on_reclaim_enabled()
}



/// JIT round 23 wave 4, lane L4: zero the HEADER of every object `starts`
/// names inside the dead spans `dead` (offsets from `base`), clamped to the
/// span. Returns the number of headers poisoned. One bitmap scan plus one
/// header store per dead object, against a memset of every dead byte.
///
/// # Safety
/// Stop-the-world; every span `[base + off, base + off + len)` is writable
/// arena memory no live object occupies, and `starts` covers it (the walk of
/// this cycle).
pub unsafe fn poison_dead_object_headers(
    base: usize,
    dead: &[(usize, usize)],
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
) -> usize {
    // r23 w4 review (R-L4z): a LAZY start bitmap (`ObjectStartBits::new_lazy`)
    // whose chunk walk FAILED holds no bits for that chunk, so the scan below
    // would silently skip every header in it and leave whole corpse headers
    // in a free-listed span. Prove every chunk a dead span touches first --
    // all of them before the first header store, so no chunk is walked over
    // bytes this function already zeroed -- and memset in full any span that
    // cannot be proven (the pre-L4 behaviour for that span). An eager bitmap
    // answers `true` without work.
    let unproven: Vec<bool> = if starts.is_lazy() {
        dead.iter()
            .map(|&(off, len)| !starts.ensure_span(base + off, base + off + len))
            .collect()
    } else {
        Vec::new()
    };
    let mut n = 0usize;
    for (i, &(off, len)) in dead.iter().enumerate() {
        let lo = base + off;
        let hi = lo + len;
        if unproven.get(i).copied().unwrap_or(false) {
            // SAFETY: the caller's contract -- the span is writable arena
            // memory no live object occupies.
            unsafe { std::ptr::write_bytes(lo as *mut u8, 0, len) };
            continue;
        }
        let mut at = starts.next_start_at_or_above(lo);
        while let Some(a) = at {
            if a >= hi {
                break;
            }
            let end = (a + HEADER_SIZE).min(hi);
            // SAFETY: `[a, end)` lies inside the dead span (the caller's
            // contract); `a` is 8-aligned and the span ends on an 8-byte
            // boundary, so whole words are written.
            unsafe { std::ptr::write_bytes(a as *mut u8, 0, end - a) };
            n += 1;
            at = starts.next_start_at_or_above(a + 8);
        }
    }
    n
}

/// JIT round 23 wave 4, lane L4: `CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`
/// (proposal 9 item 1). A moving young cycle's exact object-start
/// bitmap is filled one chunk at a time, on the first question about an
/// address in the chunk (`young_mark::LazyStartGrid`), instead of by the
/// whole-from-space walk. Read per cycle.
///
/// JIT round 24 item A2: default ON, `0` kills. `R23LeanLoop long16`
/// (`-XX:+UseGenerationalGC -Xmx1g`, moving cycles), four interleaved reps on
/// the shared host: median young pause 25-33 ms -> 9-12 ms and 45-55 -> 34-38
/// ns/op; `objstart_lazy_failed` / `_merged` 0 over 29 cycles; 22 Generational
/// GC probes print the same with and without it, and the TLAB tripwires stay
/// at 0 under `CRATONVM_DBG_GC_STRESS` with `CRATONVM_DBG_STALE_OBJREF`.
pub fn gen_lazy_young_object_starts_enabled() -> bool {
    true
}

/// gkr (`CRATONVM_GEN_LAZY_YOUNG_STARTS_DENSE_BACKOFF`, opt-in, default OFF):
/// eager cycles after a lazy cycle whose grid walked at least half of
/// from-space. The lazy grid wins when few chunks are asked about
/// (`R23LeanLoop`); on JIT-warm javac the survivors and the dirty-card values
/// name ~80 % of eden, so it walks nearly all of it anyway, one chunk at a time
/// and partly before the card seed (`pre_cards_misc` 333-346 ms against the
/// eager walk's 96-282). The next lazy cycle re-measures.
///
/// OFF: on 176.57 the eager arm won one 3-rep round (526 / 469 / 505 ms
/// against 613 / 628 / 493) and the backoff lost the next 2-rep round (683
/// against 632 ms median; the drain the eager walk should shorten went 282
/// -> 257 ms, inside that host's noise). Not a measured gain, so not a default.
pub fn gen_lazy_young_starts_dense_backoff_enabled() -> bool {
    false
}

/// Eager cycles run after a dense lazy cycle before the grid is tried again.
pub const LAZY_DENSE_BACKOFF_CYCLES: u32 = 7;

/// gkr: the per-heap memory of [`gen_lazy_young_starts_dense_backoff_enabled`].
/// Collections are stop-the-world with one driver, so `Relaxed` is enough.
#[derive(Debug, Default)]
pub struct LazyStartsBackoff {
    pub eager_left: std::sync::atomic::AtomicU32,
}

impl LazyStartsBackoff {
    /// Does this cycle build the lazy grid? `lazy_on` is the lazy switch; an
    /// eager cycle spent here counts one off the backoff.
    pub fn use_lazy(&self, lazy_on: bool) -> bool {
        if !lazy_on {
            return false;
        }
        let left = self.eager_left.load(Ordering::Relaxed);
        if left == 0 {
            return true;
        }
        self.eager_left.store(left - 1, Ordering::Relaxed);
        false
    }

    /// Record a lazy cycle that walked `walked` of `used` from-space bytes.
    pub fn note_lazy_cycle(&self, walked: usize, used: usize) {
        let dense = used > 0 && walked.saturating_mul(2) >= used;
        let left = if dense { LAZY_DENSE_BACKOFF_CYCLES } else { 0 };
        self.eager_left.store(left, Ordering::Relaxed);
    }
}

/// JIT round 23 wave 4b, lane L4: `CRATONVM_GEN_IN_PLACE_DEAD_MIRROR_ONLY`
/// (default ON since wave 6, see below), and implied by `CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`. With
/// `CRATONVM_GEN_IN_PLACE_LAZY_ZERO` on, a pinned in-place cycle that has the
/// arena's exact lazy-dirty mirror (review R-L4z) skips the dead-header
/// poisoning: the mirror already makes every dead byte "not an object" to the
/// validators (`finish_in_place_young_cycle`). Read per cycle.
///
/// R23-W6-FLIP (wave 6): default ON, `0` kills -- the non-moving sweep's
/// lazy zero (`CRATONVM_GEN_SWEEP_LAZY_ZERO`, default ON) rests on the same
/// argument (the mirror alone keeps corpse headers from the validators), and
/// on n23-w5 in-place cycles the arm cut 3-6 ns/op off `R21AllocLean
/// lean-static-long16` (`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS=1`:
/// 37-39 -> 31-35 ns/op, gcms 17-18 -> 12).
pub fn gen_in_place_dead_mirror_only() -> bool {
    true
        || gen_lazy_young_object_starts_enabled()}

/// JIT round 23 wave 4c, lane L4: `CRATONVM_GEN_LAZY_YOUNG_STARTS_INDUCTION`
/// (default OFF). With the lazy grid on, prove each chunk by induction from
/// offset 0 (the eager walk's proof, paid for every chunk below the highest one
/// asked about) instead of trusting the arena-certified allocator anchors.
/// The belt-and-braces arm, and the A/B for the certificate. Read per cycle.
pub fn gen_lazy_young_starts_induction() -> bool {
    false
}





/// gcj pcp: the sorted young pointer map's bands, `1 << shift` bytes of
/// from-space each (1 MiB): a few hundred bands on a JIT-warm javac young
/// generation, enough for the bucket sort's per-band runs to stay small.
pub const YOUNG_POINTER_MAP_BAND_SHIFT: u32 = 20;

/// gcj pcp: the most bytes of kept sorted-map buffers per kind (pairs, index).
pub const SORTED_MAP_BUFFER_CAP: usize = 64 << 20;

/// gcj pcp: wait out a forwarding claim in flight on `h` (`FORWARDED | BUSY`:
/// a worker won the claim and has not published its target yet). Returns at
/// once for any other mark. Unbounded, as `gen_evac`'s own wait for a claimed
/// forward is: the winner publishes two stores after its claim, and yielding
/// lets a descheduled winner run.
pub fn settle_forwarding_claim(h: &ObjectHeader) {
    loop {
        let mark = h.mark_word.load(Ordering::Acquire);
        if !(ObjectHeader::is_forwarded_mark(mark) && mark & cratonvm_types::MARK_FWD_BUSY != 0) {
            return;
        }
        std::thread::yield_now();
    }
}



/// JIT round 23 wave 8, lane W8-L3: `CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`
/// (default ON, `0` walks every chunk as before). Proposal 14 of
/// `docs/feature-designs/jit-ir-optimizations.md`: in the FUSED
/// promotion/sweep walk ([`gen_sweep_fused_walk_enabled`]) an anchored chunk
/// that holds no marked base, no conservative candidate / pin / unresolved
/// mark, and that no marked object below it reaches into, is not walked at
/// all: its bytes outside the free list are one dead span
/// ([`dead_chunk_sweep`]). The non-moving young sweep then reads headers only
/// in the chunks that hold live data, so its last walk is O(live chunks), not
/// O(allocated). Declined for a cycle with a published reserved-TLAB skip span
/// (an in-JIT peer's un-retired TLAB). Read per sweep; census
/// `yp_dead_chunks=` / `yp_dead_chunk_bytes=` / `yp_dead_chunk_declined=`.
pub fn gen_sweep_dead_chunk_skip_enabled() -> bool {
    true
}



/// Wave 8: the fine dead-chunk stride (about one TLAB).
pub const FINE_DEAD_CHUNK_STRIDE: usize = 256 * 1024;

/// Wave 8 (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_FINE`): `grid` (the sweep's
/// allocator-certified anchor grid: ascending, starting at 0, ending at
/// `used`) subsampled at [`FINE_DEAD_CHUNK_STRIDE`], exactly as the sweep
/// anchors are subsampled at `sweep_anchor_stride()`, with `used` as the
/// terminal entry. Empty when `grid` does not have that shape (the caller then
/// keeps the coarse anchors).
pub fn fine_dead_chunk_anchors(grid: &[usize], used: usize) -> Vec<usize> {
    if grid.first() != Some(&0) || grid.last() != Some(&used) || used == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(grid.len());
    let mut next = 0usize;
    for &a in grid {
        if a >= next && out.last().is_none_or(|&l| a > l) {
            out.push(a);
            next = a - a % FINE_DEAD_CHUNK_STRIDE + FINE_DEAD_CHUNK_STRIDE;
        }
    }
    if out.last() != Some(&used) {
        out.push(used);
    }
    out
}

/// Wave 4: the lazy grid's chunk stride. Finer than the parallel walk's
/// (`sweep_anchor_stride`, 8 MiB): a chunk is the unit of work a question
/// about one address pays for, so it should be about a TLAB, not a share of
/// the arena. The allocator records at most one anchor per 4 KiB bucket.
pub const LAZY_YOUNG_START_STRIDE: usize = 64 * 1024;

/// Wave 4: the lazy grid's chunk bounds (offsets), built exactly as the
/// parallel object-start walk builds its anchor list -- offset 0, the
/// allocator's anchors, the end of every skip block; anchors at or past
/// `used` or strictly inside a skip block dropped -- then subsampled at
/// `stride` and closed with `used`. `ObjectStartBits::new_lazy` re-checks the
/// shape (from 0 to `used`, strictly increasing) and refuses anything else.
pub fn lazy_young_start_bounds(
    alloc_anchors: &[usize],
    skips: &[(usize, usize)],
    used: usize,
    stride: usize,
) -> Vec<usize> {
    let mut anchors: Vec<usize> = Vec::with_capacity(alloc_anchors.len() + skips.len() + 2);
    anchors.push(0);
    anchors.extend_from_slice(alloc_anchors);
    anchors.extend(skips.iter().filter_map(|&(off, sz)| off.checked_add(sz)));
    anchors.retain(|&o| o < used);
    anchors.sort_unstable();
    anchors.dedup();
    anchors.retain(|&o| {
        let i = skips.partition_point(|&(off, _)| off <= o);
        if i == 0 {
            return true;
        }
        let (off, sz) = skips[i - 1];
        !(o > off && o < off + sz)
    });
    let stride = stride.max(8);
    let mut bounds: Vec<usize> = Vec::new();
    let mut next = 0usize;
    for &a in &anchors {
        if a >= next {
            bounds.push(a);
            next = a - a % stride + stride;
        }
    }
    if bounds.last() != Some(&used) {
        bounds.push(used);
    }
    bounds
}

/// Wave 4: the lazy grid's chunk walker -- [`objstart_chunk`], the parallel
/// walk's proven chunk walker, unchanged. A refusal is reported here (rare: it
/// is a from-space layout the allocator did not write), naming the chunk.
pub fn lazy_objstart_chunk_walker(
    young_base: usize,
    lo: usize,
    hi: usize,
    skips: &[(usize, usize)],
    bits: &cratonvm_gc_core::young_mark::ObjectStartBits,
) -> bool {
    // Wave 4b, fail closed on a forwarded header. A lazy chunk is walked
    // before anything in it is forwarded (every forward asks `contains`
    // first), so a FORWARDED header here means a forward that bypassed the
    // bitmap -- e.g. a chunk first walked after the drain (a census lookup, the
    // poisoning pre-pass). `objstart_chunk` sizes headers through the shape
    // word, which a forward rewrites, so whatever it proved is not trusted:
    // any recorded start whose header is forwarded (self-forwards included)
    // refuses the chunk, and the grid withdraws its bits.
    //
    // gcj pcp (`CRATONVM_GEN_PRECISE_EDGES_SKIP_STARTS`, default ON): precise
    // edges no longer ask the grid, so walking a chunk after a forward is the
    // ordinary case, not a bypass. The header sizing follows a forward to its
    // copy (`ObjectHeader::resolved_shape`; the comment above predates the
    // 8-byte header, whose shape reads do), and the walk waits out a claim in
    // flight (`objstart_chunk_with`), so the chain it proves is the one the
    // allocator wrote and its forwarded starts are kept.
    let ok = {
        objstart_chunk_with(young_base, lo, hi, skips, bits, true).is_some()
    };
    if !ok {
        tracing::warn!(
            lo,
            hi,
            "GC: lazy young object-start chunk walk refused -- every address in \
             [0x{:x}, 0x{:x}) of from-space reads as not-an-object-start",
            young_base + lo,
            young_base + hi,
        );
    }
    ok
}

/// Wave 4: prove the lazy chunks under the 4 KiB PAGE of each of `words`
/// (from-space addresses) -- the page, not just the word's chunk, because a
/// pinned plan resolves whole pages (every object overlapping a pinned page,
/// `build_pinned_young_plan`). Wave 4c: with certified anchors (the default)
/// exactly those chunks are proven, the pages claimed by `threads` workers;
/// under induction (`CRATONVM_GEN_LAZY_YOUNG_STARTS_INDUCTION`) proving a
/// chunk proves every chunk below it, so the prefix up to the highest such
/// chunk is walked speculatively on the workers, then proven in order.
/// `false` when a needed chunk cannot be proven. An eager bitmap, or no page
/// overlapping the span, answers `true`.
pub fn lazy_young_starts_prove_words(
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
    words: &[usize],
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
) -> bool {
    if !starts.is_lazy() || words.is_empty() {
        return true;
    }
    let (base, span) = starts.extent();
    if span == 0 {
        return true;
    }
    let end = base.saturating_add(span);
    if !starts.lazy_induction() {
        // gcj j1yg-pause: the distinct pages, ascending, from a bitmap over
        // the span's pages rather than a sort of every word's page: the dirty
        // cards name ~700 K words a JIT-warm javac pause, over ~44 K pages.
        let first = base & !4095;
        let page_count = (end - 1 - first) / 4096 + 1;
        let mut bits = vec![0u64; page_count.div_ceil(64)];
        for &w in words {
            let p = w & !4095;
            if p < end && p.saturating_add(4096) > base {
                let i = (p - first) / 4096;
                bits[i / 64] |= 1 << (i % 64);
            }
        }
        let mut pages: Vec<usize> = Vec::new();
        for (k, &word) in bits.iter().enumerate() {
            let mut m = word;
            while m != 0 {
                let i = k * 64 + m.trailing_zeros() as usize;
                pages.push(first + i * 4096);
                m &= m - 1;
            }
        }
        return lazy_young_starts_prove_items(starts, &pages, threads, pool, |s, p| {
            s.ensure_span(p, p.saturating_add(4096))
        });
    }
    let top = words
        .iter()
        .map(|&w| (w & !4095).saturating_add(4095))
        .filter(|&last| last >= base && (last & !4095) < end)
        .map(|last| last.min(end - 1))
        .max();
    let Some(top) = top else {
        return true;
    };
    let Some(max_chunk) = starts.lazy_chunk_of_addr(top) else {
        return true;
    };
    lazy_young_starts_prove_prefix(starts, max_chunk, threads, pool)}

/// Wave 4: prove EVERY chunk of a lazy bitmap (the poisoning pre-pass when
/// the dead-span mirror is off). `false` when any chunk cannot be proven.
pub fn lazy_young_starts_prove_all(
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
) -> bool {
    let n = starts.lazy_chunk_count();
    if n == 0 {
        return true;
    }
    if !starts.lazy_induction() {
        let idx: Vec<usize> = (0..n).collect();
        return lazy_young_starts_prove_items(starts, &idx, threads, pool, |s, i| {
            s.ensure_chunk_index(i)
        });
    }
    lazy_young_starts_prove_prefix(starts, n - 1, threads, pool)
}

/// Wave 4c (certified anchors): `prove(starts, item)` for every item, the
/// items claimed one at a time by `threads` workers. `false` when any fails.
pub fn lazy_young_starts_prove_items(
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
    items: &[usize],
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
    prove: impl Fn(&cratonvm_gc_core::young_mark::ObjectStartBits, usize) -> bool + Sync,
) -> bool {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    let worker = || loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        let Some(&item) = items.get(i) else {
            return;
        };
        if !prove(starts, item) {
            failed.store(true, Ordering::Relaxed);
        }
    };
    match pool {
        Some(pool) if threads > 1 && items.len() > 1 => {
            run_chunk_workers_elastic(pool, threads.min(items.len()), &worker)
        }
        _ => worker(),
    }
    !failed.load(Ordering::Relaxed)
}

/// Wave 4c: prove chunks `0..=last`: walk them speculatively in parallel
/// (`walk_chunk_index`, one claim per chunk, no answer yet), then prove the
/// prefix in order on this thread (`ensure_chunk_index(last)`), which only
/// re-checks the walks' landings and re-walks nothing. The parallelism is the
/// eager parallel walk's; the induction is its proof.
pub fn lazy_young_starts_prove_prefix(
    starts: &cratonvm_gc_core::young_mark::ObjectStartBits,
    last: usize,
    threads: usize,
    pool: Option<&cratonvm_gc_core::evac_pool::EvacPool>,
) -> bool {
    let n = last + 1;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    let worker = || loop {
        if failed.load(Ordering::Relaxed) {
            return;
        }
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= n {
            return;
        }
        if !starts.walk_chunk_index(i) {
            failed.store(true, Ordering::Relaxed);
        }
    };
    match pool {
        Some(pool) if threads > 1 && n > 1 => run_chunk_workers_elastic(pool, threads.min(n), &worker),
        _ => worker(),
    }
    // A speculative failure leaves the prefix unprovable at that chunk; the
    // ordered proof below reports it (and walks nothing the workers skipped
    // beyond it that it does not need).
    starts.ensure_chunk_index(last)
}

thread_local! {
    /// gcj j3/yg3: [`set_young_side_table_rows_follow`]'s per-cycle flag, on
    /// the collecting thread (the root gatherer and the collector run there).
    pub static YOUNG_SIDE_TABLE_ROWS_FOLLOW: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// gcj j3/yg3 (`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS`, default ON): the
/// root scan on this thread left young user mirrors out of the root set
/// (`vm::memory::roots` step 6, the conc10 widening) and promises them to their
/// owners' `mirror_pin` rows. A moving young phase that follows this flag
/// forwards those rows like the licensed moving `System.gc()` (gci i9/yl:
/// `side_table_young_rows_of` / `side_table_values_due`); the in-place
/// marker follows the rows anyway. Cleared by the root gatherer at the top of
/// every scan, set by its step 14w'.
pub fn set_young_side_table_rows_follow() {
    YOUNG_SIDE_TABLE_ROWS_FOLLOW.with(|c| c.set(true));
}

/// gcj j3/yg3: clear [`set_young_side_table_rows_follow`]'s flag.
pub fn clear_young_side_table_rows_follow() {
    YOUNG_SIDE_TABLE_ROWS_FOLLOW.with(|c| c.set(false));
}

/// gcj j3/yg3: was [`set_young_side_table_rows_follow`] called by this
/// thread's last root scan?
pub fn young_side_table_rows_follow() -> bool {
    YOUNG_SIDE_TABLE_ROWS_FOLLOW.with(|c| c.get())
}

/// gcj j3/yg3: serial Cheney survivors tenured because young to-space could
/// not take them (`CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES`); printed as
/// `cheney_overflow_promotions=` on `[GC] par_evac:`. 0 while the Cheney
/// invariant (to-space holds all of from-space) holds, i.e. on every
/// configuration today.
pub static CHENEY_OVERFLOW_PROMOTIONS: AtomicU64 = AtomicU64::new(0);



/// gcj j3/yg3: where the serial Cheney copy puts a survivor young to-space
/// could not take -- old gen through the cycle's promotion buffer, counted in
/// [`CHENEY_OVERFLOW_PROMOTIONS`] -- or `None` (the caller's abort) when the
/// switch is off or old gen is full too.
pub fn cheney_overflow_destination(
    promoted_worklist: &mut PromotionQueue,
    old_gen: &mut OldGen,
    total_size: usize,
) -> Option<*mut u8> {
    let ptr = promoted_worklist.alloc(old_gen, total_size)?;
    CHENEY_OVERFLOW_PROMOTIONS.fetch_add(1, Ordering::Relaxed);
    Some(ptr)
}

/// gcj yg3-residuals R3: survivors a young copy tenured because copying them
/// young would have taken to-space past the Serial geometry's survivor cap
/// (both arms; `ygeo_cap_promotions=` on `[GC] young_geometry:`).
pub static GEOMETRY_CAP_PROMOTIONS: AtomicU64 = AtomicU64::new(0);
