#[allow(unused_imports)]
use super::*;

pub(in crate::runtime::interpreter) type CachedInvokeTarget = GenericCachedInvokeTarget<cratonvm_jit::RetainedCode>;

// ---------------------------------------------------------------------------
// Diagnostic gates
// ---------------------------------------------------------------------------
//
// (The GC trigger helper that used to be documented here, `maybe_gc`, lives
// in `interpreter/gc_and_alloc.rs` and carries its doc there.)

/// Cached `CRATONVM_DBG_STRAYSTACK` gate (bc math-ec `0x4`): dump the Java
/// stack at a putfield whose receiver header is the stray/stale signature.
#[inline]
pub(in crate::runtime::interpreter) fn straystack_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_REFPROC_AUDIT` gate (DoHead-family Generational NPEs,
/// 2026-09-06). `CRATONVM_DBG_NO_REFPROC=1` cut that family 136 -> 49 NPE
/// lines over two matched-parallelism rounds — the first arm besides
/// `NO_MOVING_YOUNG` to move it — but it disables the WHOLE subsystem, so it
/// names a suspect rather than a write.
///
/// The guard census that came next (`CRATONVM_DBG_STRAYSTACK`, 4 classes)
/// showed why a subsystem-level ablation was as far as that method could get:
/// in ~90 collections the ONLY guard in `process_references_after_gc` that
/// ever fired was `SKIP dead ENQUEUE` (1130x). Every shape, identity and
/// restore refusal read zero — and, decisively, THIS function's three
/// refusals (`is_subclass_of`, identity stamp, `num_fields >= 2`) print
/// nothing and count nothing, so that zero never covered them at all. This
/// function performs ~342 `Object(None)` writes per collection into live
/// objects' slot 0 and depends entirely on the post-GC restore pass to put
/// them back; both halves are audited here, because a null that is written
/// and never restored is indistinguishable at the crash site from a null
/// written to the wrong object, and this family's symptom is exactly a
/// reference field read back null.
#[inline]
pub(crate) fn refaudit_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// gcg g2/r: per collection, at most this many `[refaudit] ... REFUSED` detail
/// lines per refusal kind (the counts in the summary line stay exact).
pub(in crate::runtime::interpreter) const REFAUDIT_DETAIL_MAX: u64 = 8;

/// The class name the refproc audit prints for `cid` (`<unresolved cid=N>`
/// when the class manager does not know it).
pub(in crate::runtime::interpreter) fn refaudit_class_name(class_manager: &crate::classloading::ClassManager, cid: ClassId) -> String {
    class_manager
        .get_class(cid)
        .map(|c| c.name.as_ref().to_string())
        .unwrap_or_else(|| format!("<unresolved cid={}>", cid.as_u32()))
}

/// Cached `CRATONVM_DBG_ARRSTORE` gate (bc math-ec 0x4 smear hunt): validate
/// primitive-array-store receivers at the write.
#[inline]
pub(in crate::runtime::interpreter) fn arrstore_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_CCE_BT` gate (WildFly `parallel-extension-add` CCE
/// family): print receiver identity (class + address, and `via_pin` where
/// applicable) at the moment a `ClassCastException` is constructed, so a
/// wrong-object read can be correlated against GC cycle logs and the
/// `CRATONVM_DBG_STALE_OBJREF` quarantine ring. The native-collections
/// natural-order sites have a matching hook (with native backtrace) behind
/// the same variable.
#[inline]
pub fn dbg_cce_bt_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_NO_REFPROC` gate (bc math-ec 0x4): skip ALL post-GC
/// reference processing — subsystem-level exclusion experiment.
#[inline]
pub(crate) fn no_refproc() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// HIB-CV-24 (Manifestation B) — end-to-end Weak/Phantom reference *clearing*.
///
/// CratonVM's mark phase traces every reference slot strongly, including a
/// `java.lang.ref.Reference`'s `referent`, so a *live* Weak/Phantom reference
/// pins its referent forever and `get()` never returns null / phantom queues
/// never fire (e.g. Hibernate's `ClassLoaderLeaksUtilityTest`). When ON
/// (default), the VM nulls Weak/Phantom referent slots *before* a collection so
/// the unmodified marker cannot keep the referent alive, lets the existing
/// reference processor clear/enqueue dead ones, then restores the slots of
/// survivors afterwards. Opt-out `CRATONVM_WEAKREF_CLEAR=0` restores the legacy
/// (never-clearing) behavior — byte-identical, the safety net.
pub(crate) fn weakref_clear_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        None::<String>
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// zt t23/st1 (item 29a of `zgc-zt13x10-review-residuals-CLOSED-20261019.md`;
/// `CRATONVM_ZGC_REFERENT_READS_WEAK`, default ON, `=0` kill): does the VM's
/// reference round read a referent slot with a NON-marking load
/// (`VmHeap::get_field_weak`, ZGC's `ZBarrierKind::Weak`)? Read once per pass
/// (a flag-snapshot lookup), never per row. Only an armed ZGC read barrier
/// (`CRATONVM_ZGC_LOAD_BARRIER=colored`) reads differently; under the color
/// flip (`CRATONVM_ZGC_COLOR_FLIP=1`) the strong read MARKED the referent of
/// every slot no mutator had loaded during the phase, at the closing
/// collection's pre-pass (before the remap flip): those weak references were
/// never cleared. `=0`: the strong read, as before.
pub(crate) fn zgc_referent_reads_weak_enabled() -> bool {
    true
}

thread_local! {
    /// Armed by the allocation-failure escalation ladder immediately before the
    /// collection it runs as its final attempt, so that collection clears every
    /// SoftReference rather than only the idle ones — the `java.lang.ref`
    /// last-ditch guarantee (see
    /// `ReferenceProcessor::condemn_all_soft_refs`).
    ///
    /// Thread-local rather than a process-global flag for two reasons. It is
    /// armed and read on ONE thread: `last_ditch_reclaim` arms it, calls
    /// `maybe_gc_forced`, and that function either initiates the collection on
    /// this same thread — running `weakref_null_referents_pre_gc` here, where
    /// the flag is visible — or declines because another thread is already
    /// collecting, in which case no collection of ours happens and the flag
    /// must not affect anyone else's. And a process can host more than one VM,
    /// which a `static AtomicBool` would silently share; nothing about
    /// "this thread is out of memory" belongs to a sibling VM.
    pub(in crate::runtime::interpreter) static LAST_DITCH_SOFT_CLEAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with the last-ditch soft-clear rule armed for any collection it
/// initiates. Restores the previous value on the way out, including on unwind,
/// so a panic inside the collection cannot leave the rule latched on for every
/// later GC on this thread.
pub(crate) fn with_last_ditch_soft_clear<R>(f: impl FnOnce() -> R) -> R {
    struct Disarm(bool);
    impl Drop for Disarm {
        fn drop(&mut self) {
            LAST_DITCH_SOFT_CLEAR.with(|c| c.set(self.0));
        }
    }
    let _restore = Disarm(LAST_DITCH_SOFT_CLEAR.with(|c| c.replace(true)));
    f()
}

thread_local! {
    /// zr z2/cp: did THIS thread's pre-collection pass apply the last-ditch
    /// "condemn every soft reference" rule? Set by
    /// [`weakref_null_referents_pre_gc`], consumed by the same pause's
    /// `process_references_after_gc` (both run on the collecting thread; a
    /// pass that ran elsewhere leaves it `false`, the pre-z2 behaviour).
    pub(in crate::runtime::interpreter) static SOFT_CLEAR_ALL_APPLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Take [`SOFT_CLEAR_ALL_APPLIED`] (reads and resets it).
pub(crate) fn soft_clear_all_applied_take() -> bool {
    SOFT_CLEAR_ALL_APPLIED.with(|c| c.replace(false))
}

/// Read [`SOFT_CLEAR_ALL_APPLIED`] without consuming it (zr z5/rv5: the
/// collection's soft-precise probe is armed between the pre-collection pass
/// and the collection).
pub(crate) fn soft_clear_all_applied_peek() -> bool {
    SOFT_CLEAR_ALL_APPLIED.with(std::cell::Cell::get)
}

/// Whether the collection about to run is the allocation failure's last
/// attempt. See [`with_last_ditch_soft_clear`].
pub(in crate::runtime::interpreter) fn last_ditch_soft_clear_armed() -> bool {
    LAST_DITCH_SOFT_CLEAR.with(|c| c.get())
}

/// [`dbg_weakref`] for the reference pass in `gc_and_alloc` (zr z2/cp).
pub(crate) fn dbg_weakref_on() -> bool {
    dbg_weakref()
}

/// `CRATONVM_DBG_WEAKREF` — trace the Weak/Phantom referent null/restore passes.
pub(in crate::runtime::interpreter) fn dbg_weakref() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Wall-clock milliseconds since the Unix epoch — the clock
/// `SoftReference.<init>`/`.get()` stamp soft-reference entries with, so the
/// LRU policy's idle windows are directly comparable against it.
pub(in crate::runtime::interpreter) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // Cast: u128 millis -> u64 (fits for any clock this side of year 584 million)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// HIB-CV-24 — pre-collection pass: null the `referent` slot of every active
/// Weak/Phantom reference (and every soft reference the LRU policy — or the
/// armed last-ditch rule — condemns) so the mark phase does NOT keep the
/// referent alive through the (live) Reference object. Surviving referents are
/// restored by `process_references_after_gc`. MUST run under STW, after
/// `collect_roots` and immediately before `collect_garbage` (the Reference
/// objects are still at their pre-collection addresses and no mutator can
/// observe the transient null). No-op when the gate is off or no Weak/Phantom
/// references exist.
pub fn weakref_null_referents_pre_gc(shared: &SharedVm) {
    // gcj j1yg-pause round 3: a stash an earlier pause left (one whose
    // reference round did not run) goes back before anything reads the rows.
    shared.mem.ref_processor.lock().restore_settled_old_rows();
    if !weakref_clear_enabled() || no_refproc() {
        bind_allocated_references_pre_gc(shared);
        refresh_soft_rows_from_java_timestamps(shared);
        publish_watched_reference_addrs(shared);
        crate::memory::string_table::watch_young_pool_entries(shared);
        return;
    }
    let keep_referents = zgc_young_cycle_keeps_references(shared);
    if keep_referents {
        // Young ZGC cycle traces referents strongly and never clears them:
        // absorb notes so relocate/remove_collected remap survivors and prune
        // dead TLAB references without binding millions of short-lived rows.
        shared.mem.ref_processor.lock().absorb_allocation_notes();
    } else {
        bind_allocated_references_pre_gc(shared);
    }
    refresh_soft_rows_from_java_timestamps(shared);
    // zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE`; default ON since
    // zt t20/gn7, `=0` kill; only on such a young cycle): the processing
    // round that ends it keeps the soft LRU policy's free-at-last-gc figure
    // the last whole-heap round stored, so the next major's allowance reads
    // the free heap after the last major. Decided by `Zt12Gn4SoftFree minor`
    // against HotSpot 25: `gn5_SF4_1h` / `_1hc` SAME `kept=true`, the off arm
    // `kept=false` (t16 row verdicts).
    let hold_soft_free = keep_referents;
    // zt t22/zc3 (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in; non-generational ZGC
    // only): the slot KEEPS its referent. Every row this pass would null is
    // handed to the collection instead, whose markers hide its slot 0 and
    // whose pause nulls it when the referent dies -- before the sweep, with
    // the mark final -- so the round below finds every slot as the null pass
    // left it, and no live referent's slot ever reads a transient `null`
    // (`gc/src/zgc/concurrent_refs.rs`). One call per collection.
    let hide_referents = !keep_referents && shared.mem.heap.zgc_concurrent_refs_skip_mode();
    let persistent_skip = hide_referents && shared.mem.heap.zgc_persistent_skip_bits_enabled();
    // Heap headroom for the soft-reference LRU policy below. Read BEFORE the
    // reference-processor lock is taken -- the heap's own locks rank above L7.
    // gch h2/jr: the same helper as the post-GC path (`soft_lru_free_mb`).
    let soft_free_mb = crate::memory::ref_discovery::soft_lru_free_mb(shared);
    // gci i48/rf48: the count the G1 full collection's armed undiscovered set
    // must still match (read before L7, as the free headroom above).
    let full_gc_stamp = shared.mem.heap.collection_count();
    // gcj j1yg-pause round 3: on a certainly-minor Generational pause the
    // weak / phantom rows with an old `Reference` and an old referent are set
    // aside for both passes (`ReferenceProcessor::stash_settled_old_rows`);
    // the pause driver puts them back after its reference round. Read before
    // the processor lock, as the heap's own locks rank above it.
    let stash_old = gen_young_pause_sets_aside_old_rows(shared).then(|| {
        let (base, size) = shared.mem.heap.old_gen_info();
        (base, base.saturating_add(size))
    });
    // P3 item 1 (`p3-collection-object-hot-paths-review-RETIRED-20261107`): the same
    // set-aside on a G1 pause that is provably young-only with no mark open --
    // the weak rows wholly in `Old` regions cost 2-3 us each per pause
    // otherwise (`WeakRowPopulationPause`: 1.1 s a pause at 400 K rows). The
    // epoch lets the pause's end notice an escalation that collected old.
    let g1_old = (!last_ditch_soft_clear_armed())
    .then(|| shared.mem.heap.g1_young_pause_old_ranges())
    .flatten();
    let old_epoch = shared.mem.heap.g1_old_collecting_pauses().unwrap_or(0);
    let (pairs, soft_pairs, watch_addrs) = {
        let mut rp = shared.mem.ref_processor.lock();
        let stashed = match &g1_old {
            Some(ranges) => Some(rp.stash_settled_rows(
                &|a| {
                    ranges
                        .binary_search_by(|&(lo, hi)| {
                            if hi <= a {
                                std::cmp::Ordering::Less
                            } else if lo > a {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Equal
                            }
                        })
                        .is_ok()
                },
                old_epoch,
            )),
            None => stash_old.map(|old| rp.stash_settled_old_rows(old)),
        };
        rp.note_settled_old_last(stashed);
        rp.set_young_round_holds_soft_free(hold_soft_free);
        let mut weak_phantom = if persistent_skip {
            Vec::new()
        } else {
            rp.weak_phantom_active_triples()
        };
        // gci i48/rf48 (`gci-i15rf-...-20261014` residual): on the compaction
        // pause of a G1 full collection, the `Reference`s that collection left
        // undiscovered (armed by the door, `g1_full_gc_arm_undiscovered`) keep
        // their slot 0, so the pause traces the referent as HotSpot's one full
        // collection does, and the round leaves their rows alone. Empty (one
        // test) on every other pause.
        let held = rp.take_armed_full_gc_undiscovered(full_gc_stamp);
        if !held.is_empty() {
            rp.note_undiscovered_references(&held);
            weak_phantom.retain(|&(r, _, _)| held.binary_search(&r).is_err());
        }
        // SOFT-CLEAR GAP (2026-08-15). This is the measured answer to the
        // residual left by the retired `zgc-resourceleakdetector-corpse-read`
        // write-up: "the marker traces referents as strong edges; whether the
        // VM-level pass compensates is not established". It compensates for
        // WEAK and PHANTOM -- that is what the loop below does. It did NOT for
        // SOFT, on any collector: `process_soft_refs` skips an entry whose
        // referent `is_marked`, and a soft referent is always marked through
        // its own `SoftReference`'s slot 0, so the LRU policy beneath that
        // check could never fire. In a 64 MiB heap HotSpot cleared the soft
        // reference and allocated 30 MiB past it while CratonVM threw
        // `OutOfMemoryError` under both ZGC and G1.
        //
        // `condemn_idle_soft_refs` applies the policy here instead, and only
        // the entries it condemns join the null pass; the rest stay traced
        // strongly and are retained exactly as before.
        //
        // The clock is a real `SystemTime` reading, not the `0` that
        // `process_references_after_gc` passes. `0` means "use the last value a
        // mutator handed `touch_soft_reference`", which is the moment of the
        // most recent `SoftReference.get()` in the process — so the idle window
        // of the reference that made that call is zero, and a program looping
        // on its own soft-referenced cache never opens one. This caller is
        // ordinary VM code and has a clock; `SoftReference.<init>` and `.get()`
        // stamp entries from the same `SystemTime` epoch, so the two are
        // directly comparable.
        //
        // `|`, not `||` (gc-common w5-a, `handoff-w4d-last-ditch-soft-clear-
        // per-vm-request`): a per-VM request (`last_ditch_clear_soft_refs` on
        // ANY thread) must be consumed by this collection even when this
        // thread's own arming already selects the rule, or it would leak into
        // the next ordinary collection. The request is what lets the rule
        // reach a collection another thread initiated after this thread's own
        // attempts all lost the pause race (residuals §4).
        //
        // zt t7/cc: under `keep_referents` nothing is pending (the arm
        // declines while a request or the last-ditch rule is), and the
        // request, if one arrives, is left to the pass that serves it.
        let clear_all = if keep_referents {
            false
        } else {
            rp.take_clear_all_soft_request() | last_ditch_soft_clear_armed()
        };
        // zr z2/cp: remembered for this pause's post-collection pass
        // (`soft_clear_all_applied_take`).
        SOFT_CLEAR_ALL_APPLIED.with(|c| c.set(clear_all));
        let soft = if clear_all {
            // The allocation has already failed and this is the last
            // collection before `OutOfMemoryError`. The specification requires
            // every softly-reachable object to be released first, whatever the
            // LRU policy thinks.
            // gcj j1/rf: HotSpot advances the soft master clock after EVERY
            // processing round (`update_soft_ref_master_clock`), the
            // clear-all one included; only the LRU arm recorded this
            // collection's clock, so the next ordinary collection judged
            // ages against a stale clock and cleared fewer soft references.
            if gen_soft_clear_all_advances_clock(shared) {
                rp.note_round_clock(now_ms());
            }
            rp.condemn_all_soft_refs()
        } else if keep_referents {
            // zt t7/cc: a minor clears no soft reference (HotSpot's LRU
            // policy runs in its old-generation reference processing).
            Vec::new()
        } else {
            rp.condemn_idle_soft_refs(soft_free_mb, now_ms())
        };
        // Same stamp column the weak/phantom half carries -- the write loop
        // below screens both the same way.
        let soft: Vec<(usize, usize, i32)> = soft
            .into_iter()
            .filter(|&(r, _)| held.binary_search(&r).is_err())
            .map(|(r, t)| (r, t, rp.identity_stamp(r).unwrap_or(0)))
            .collect();
        // The watch list below, read under this same acquisition rather than
        // a second one (see the note there for what it is).
        // zr z1/rf: nothing on a ZGC heap reads the watched set (see
        // `watched_set_unread`), so it is published empty there.
        let watch = if watched_set_unread(shared) {
            Vec::new()
        } else {
            rp.active_round_tracked_addrs()
        };
        (weak_phantom, soft, watch)
    };
    // zt t5/gn (`CRATONVM_ZGC_GEN_SOFT_CLEAR_MAJOR`, default ON, `=0` kill;
    // ZGC generational mode only): a clear-all-soft collection (the
    // last-ditch rung, a per-VM request, the near-full soft clear) must be
    // WHOLE-HEAP. A young cycle pre-marks the old generation, so an old soft
    // referent (every cache that outlived three collections) reads as marked
    // and is restored, while the request has already been consumed above --
    // the clear silently did nothing. HotSpot's clear-all collection is a
    // major. The collecting thread's full request is what `collect_garbage`
    // reads for that (`CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR`), and this pass
    // runs on that thread immediately before it.
    if soft_clear_all_applied_peek()
        && shared.mem.heap.zgc_generational_enabled()
    {
        cratonvm_gc::gc_quiescence::request_major_gc();
    }
    // RandomizedContext WeakHashMap<Thread,...> fix: publish this cycle's
    // referent addresses so the non-moving young sweep can recognize a
    // kept-in-place (unpromoted) survivor as having genuinely survived (see
    // gc_quiescence::is_watched_referent). Unconditional — even an empty
    // list must be published so a previous cycle's entries can never leak
    // into this one.
    //
    // Young-GC live-reclaim ROOT FIX (2026-07-07, RRWL/ThreadLocalMap$Entry
    // IMSE/hang family): watch the REFERENCE OBJECTS' own addresses too, not
    // just their referents. The post-GC restore pass and `remove_collected`
    // judge survival by `pointer_map.contains_key(addr) || is_addr_live(addr)`,
    // and for the Generational heap `is_addr_live` is old-gen-only — so after
    // a NON-MOVING young sweep (which produces NO pointer_map entries for
    // kept-in-place survivors) a live YOUNG Reference object was judged dead:
    // its pre-GC-nulled referent slot was never restored and its processor
    // entry was pruned. A live young `ThreadLocalMap$Entry` (a WeakReference)
    // then answered `refersTo(null) == true`, so the mutator itself expunged
    // the live entry — losing the RRWL `readHolds` hold counter (the
    // `IllegalMonitorStateException: attempt to unlock read lock` /
    // permanent all-parked hang in `RwlReadTearingProbe` and Elasticsearch
    // `LongRandomBinaryDocValuesRangeQueryTests.testAllEqual`), and losing
    // WeakHashMap entries generally (the RandomizedContext residual). The
    // watched set already flows into identity `pointer_map` entries for every
    // kept-in-place survivor the sweep retains (side-marked or header-marked),
    // which is exactly the survival proof the restore pass needs — the
    // referent half of this fix simply never covered the reference objects.
    // Old-gen reclamation fix (HIB-CV-32 family, TestMVStoreCachePerformance):
    // publish EVERY address the processor holds, not just the weak/phantom
    // pairs' and their queues'. `process_references` and `remove_collected`
    // consult the survival predicate for soft/cleaner/finalizer entries too,
    // and after an old-gen reclamation that predicate is only exact for
    // addresses the collector was asked to prove — see
    // `VmHeap::watched_pre_gc_addr_survived`. The set is bounded by the number
    // of live `Reference` objects, and a non-surviving address simply never
    // gets an entry, so widening it costs one hash insert per tracked
    // reference and cannot over-retain. (`watch_addrs` is read in the
    // reference-processor block above, after the condemnations.)

    if crate::runtime::env_cache::dbg_watchref() {
        // gch h4/wr item 7: the count at the first level; the whole set
        // (2000+ addresses per collection) only under CRATONVM_DBG_WATCHREF=2.
        if crate::runtime::env_cache::dbg_watchref_list() {
            eprintln!(
                "[watchref] publishing {} watched referent(s): {:x?}",
                watch_addrs.len(),
                watch_addrs
            );
        } else {
            eprintln!("[watchref] publishing {} watched referent(s)", watch_addrs.len());
        }
    }
    // gcj j1/rf: keyed by this VM's heap (`watched_set_publish_key`).
    // gcj j3/rf3: an unchanged list may reuse the row (opt-in).
    publish_watched_list(shared, watch_addrs);
    // gch h2/jr (`gch-h2og-true-root-major-frees-the-old-referents-of-a-pooled-young-string-FIXED-20261012`):
    // the weak String table's young entries join the set, AFTER the `set`
    // above (which replaces it), so a true-root major seeds them.
    crate::memory::string_table::watch_young_pool_entries(shared);
    if pairs.is_empty() && soft_pairs.is_empty() {
        // zt t22/zc3: an empty set still marks the collection as a hiding one
        // (the census's `vr_mode=`).
        if hide_referents {
            shared.mem.heap.zgc_set_stw_reference_skip(Vec::new());
        }
        return;
    }
    if dbg_weakref() {
        eprintln!(
            "[weakref] pre-gc null pass: {} weak/phantom referent(s), \
             {} policy-condemned soft referent(s)",
            pairs.len(),
            soft_pairs.len()
        );
    }
    // THE PRE-GC PASS WAS THE UNSCREENED WRITE SITE.
    //
    // `process_references_after_gc`'s cleared / enqueue / restore loops were
    // given a class-shape guard on 2026-08-16 (`is_reference_shaped`) after two
    // measured corruptions -- a `java.lang.String` published as a
    // `ReferenceQueue` head, and field 0 of a `String` nulled. THIS loop, which
    // performs the same kind of write through the same kind of address, kept
    // only the `num_fields >= 2` test, which almost every class passes: an
    // `org.h2.engine.SessionLocal` passes it, and so does every `org.h2.value.Value`.
    // So a processor entry whose `Reference` had been reclaimed and its address
    // re-issued nulled slot 0 of whatever now lived there -- the
    // `NullPointerException: Cannot invoke "org.h2.value.Value.getValueType()"
    // because "v" is null` half of the H2 `TestMultiThread` MVStore-writer
    // report, whose own analysis records that the failure "survives the shape
    // guard that now screens every reference-processor write". It did, because
    // this write was not one of the screened ones.
    //
    // Screened here with BOTH tests:
    //
    //  * the same class-shape guard as the post-GC loops, and
    //  * the identity stamp, which is the exact version of it: a reclaimed
    //    `Reference`'s address re-issued to ANOTHER `Reference` is shape-clean
    //    and identity-wrong, and H2 allocates a `CloseWatcher` (a
    //    `PhantomReference`) per connection, so same-class reuse is the common
    //    case rather than the exotic one.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    // The referent's CLASS, read from slot 0 on the way past and handed to the
    // processor below. This is the only point in a collection that holds a
    // referent and the heap at the same time, and the post-GC restore pass
    // needs it: that pass writes an object back into slot 0 from an ADDRESS,
    // and an address is not an identity once a compacting collector has
    // re-issued it. See `ReferenceProcessor::referent_class_stamps`.
    let mut referent_classes: Vec<(usize, u64)> = Vec::new();
    // REFPROC AUDIT (see `refaudit_enabled`): this loop's three refusals are
    // otherwise silent, so a straystack census reads zero for them whether they
    // fire ten thousand times or never. `nulled_by_class` is the direct answer
    // to the question a subsystem ablation cannot ask — WHAT is this write
    // landing on. Anything in it that is not a `java.lang.ref.*` is the
    // producer, named.
    let audit = refaudit_enabled();
    let (mut n_nulled, mut d_shape, mut d_stamp, mut d_fields) = (0u64, 0u64, 0u64, 0u64);
    // gci i21/hr: slot 0 named another object than the row's referent; the
    // restore below would install `map(row)` over it.
    let mut d_split = 0u64;
    let mut nulled_by_class: std::collections::BTreeMap<String, u64> = Default::default();
    // zt t22/zc3: the rows whose slot is hidden instead of nulled.
    let mut hidden: Vec<usize> = Vec::new();
    // zt t23/st1 (item 29a): the slot-0 read below never marks.
    let weak_reads = zgc_referent_reads_weak_enabled();
    // gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item
    // 4, `CRATONVM_GEN_YOUNG_PAUSE_KEEPS_OLD_REFERENTS`, default ON, `=0`
    // kills): on a Generational pause certain to collect no old storage
    // (`VmHeap::gen_next_pause_certainly_minor`), an OLD referent is judged
    // alive whatever its slot holds, so nulling it here and restoring it after
    // was pure cost -- HotSpot's young collection never discovers it. Such a
    // weak / phantom row keeps its slot; it is taken back out of the examined
    // set (`ReferenceProcessor::forget_pre_gc_examined`) so the post-collection
    // retirer does not read "examined, not nulled" as an application clear.
    // Condemned soft rows are nulled as before.
    let n_weak_phantom = pairs.len();
    let old_range = if gen_young_pause_keeps_old_referents(shared) {
        let (base, size) = shared.mem.heap.old_gen_info();
        Some((base, base.saturating_add(size)))
    } else {
        None
    };
    let mut kept_old: Vec<usize> = Vec::new();
    for (row_index, (ref_obj_addr, row_referent, stamp)) in
        pairs.into_iter().chain(soft_pairs).enumerate()
    {
        // The Reference object is live (or dead-but-not-yet-collected) at this
        // point, so its memory is valid; writing its referent slot is safe.
        // SAFETY: `ref_obj_addr` is a current Reference-object address held by
        // the reference processor (kept current by `update_after_gc` /
        // `remove_collected`); it points at a valid heap object header.
        let ref_obj = unsafe { ObjectRef::from_raw(ref_obj_addr as *mut u8) };
        // HIB-WEAKREF-RECYCLE.1 (2026-07-31): a real `java.lang.ref.Reference`
        // always has >= 2 instance fields (referent, queue) — the same
        // invariant `process_references_after_gc`'s cleared/enqueue loops
        // already rely on ("also covers old-gen reuse after a major GC").
        // `>= 1` was too weak in exactly the case those loops call out: an
        // entry whose Reference object was reclaimed by an old-gen sweep keeps
        // a `reference_obj` address that `is_addr_live` still answers `true`
        // for (`is_old_gen_addr` is a pure range check), so the entry is never
        // pruned and this pass nulls slot 0 of whatever now occupies that
        // memory. When the new occupant has 0 fields the `gen_heap` OOB guard
        // catches it (observed 174x running Hibernate's
        // `DefaultCatalogAndSchemaTest` under `--nojit`, receiver reading
        // `class_id=0 java/lang/Object` and garbage `<unresolved>` ids, ending
        // in SIGSEGV); when it has exactly 1 the write lands SILENTLY on a
        // real field. Requiring >= 2 declines both, and can never skip a
        // genuine Reference.
        // Not a `Reference` any more ⇒ the address no longer names what this
        // processor recorded. `None` (class not loaded) admits: no Reference
        // object can exist yet, so the guard has nothing to judge.
        if let Some(cid) = reference_cid {
            if !class_manager.is_subclass_of(shared.mem.heap.class_id_of(ref_obj), cid) {
                d_shape += 1;
                // gcg g2/r (`gcd-d1c-proposal-reference-rows-pruned-at-every-reclamation`
                // step 3): under the audit a refusal is a bug report, not a
                // silent skip -- a row outlived its `Reference` and its block
                // was re-issued. Named, so the census that must read zero
                // before the screen can become an assertion says WHAT.
                if audit && d_shape <= REFAUDIT_DETAIL_MAX {
                    eprintln!(
                        "[refaudit] pre-gc-null REFUSED shape ref=0x{ref_obj_addr:x} now={}",
                        refaudit_class_name(&class_manager, shared.mem.heap.class_id_of(ref_obj))
                    );
                }
                continue;
            }
        }
        // Still a `Reference`, but is it THE Reference? `identity_hash_code`
        // mints for an object that has none, so a re-issued address answers
        // with a fresh counter value rather than the recorded one. A `0` answer
        // is "cannot tell" (the object is thin-locked, so its hash is not in
        // the mark word) and falls back to the shape guard above rather than
        // declining a legitimate entry.
        if stamp != 0 {
            let now = shared.mem.heap.identity_hash_code(ref_obj);
            if now != 0 && now != stamp {
                d_stamp += 1;
                // gcg g2/r (d1c step 3): see the shape refusal above.
                if audit && d_stamp <= REFAUDIT_DETAIL_MAX {
                    eprintln!(
                        "[refaudit] pre-gc-null REFUSED stamp ref=0x{ref_obj_addr:x} \
                         recorded={stamp} now={now} class={}",
                        refaudit_class_name(&class_manager, shared.mem.heap.class_id_of(ref_obj))
                    );
                }
                continue;
            }
        }
        if shared.mem.heap.num_fields(ref_obj) < 2 {
            d_fields += 1;
        } else {
            if audit {
                n_nulled += 1;
                let name =
                    refaudit_class_name(&class_manager, shared.mem.heap.class_id_of(ref_obj));
                *nulled_by_class.entry(name).or_insert(0) += 1;
            }
            // Read the referent out of the slot BEFORE nulling it, and record
            // its class. The slot is authoritative here in a way the
            // processor's recorded `referent` address is not: every guard
            // above has just established that this object IS the Reference
            // that was discovered, so whatever slot 0 holds right now is its
            // referent by construction.
            // zt t23/st1 (item 29a): a non-marking read -- this pass decides
            // the referent's fate, and at a concurrent cycle's closing
            // collection the mark is still active here.
            let slot0 = if weak_reads {
                shared.mem.heap.get_field_weak(ref_obj, 0)
            } else {
                shared.mem.heap.get_field(ref_obj, 0)
            };
            // gcj j4/rf: see `old_range`; keyed on what the slot holds.
            if let (Some((lo, hi)), Value::Object(Some(rt))) = (old_range, &slot0) {
                let at = rt.as_ptr() as usize;
                if row_index < n_weak_phantom && at >= lo && at < hi {
                    kept_old.push(ref_obj_addr);
                    continue;
                }
            }
            if let Value::Object(Some(rt)) = slot0 {
                if audit && rt.as_ptr() as usize != row_referent {
                    d_split += 1;
                    if d_split <= REFAUDIT_DETAIL_MAX {
                        eprintln!(
                            "[refaudit] pre-gc-null SLOT/ROW SPLIT ref=0x{ref_obj_addr:x} slot=0x{:x} row=0x{row_referent:x}",
                            rt.as_ptr() as usize
                        );
                    }
                }
                // Kind-aware stamp: the restore screen tells `Integer[]` from
                // `Integer` (round 11 wave 16,
                // `r11w16-vh-interpreter-referent-kind-stamp-patch-FIXED-20260925`).
                let kind_bits = cratonvm_gc::reference::REFERENT_STAMP_KIND_KNOWN
                    | if shared.mem.heap.kind_of(rt) == cratonvm_types::ObjectKind::Array {
                        cratonvm_gc::reference::REFERENT_STAMP_ARRAY
                    } else {
                        0
                    };
                referent_classes.push((
                    ref_obj_addr,
                    u64::from(shared.mem.heap.class_id_of(rt).as_u32()) | kind_bits,
                ));
            }
            // Slot 0 = REF_FIELD_REFERENT (matches the real JDK Reference layout
            // and the synthetic constant in native-builtins).
            //
            // INT-8: PROTOCOL write — SATB pre-barrier suppressed. This null
            // is restored (for survivors) before mutators resume, so the
            // snapshot graph is unchanged; letting it fire logged EVERY
            // active referent as a G1 mark root at EVERY mid-cycle young
            // pause, which kept weakly-reachable Old objects bitmap-marked
            // and made remark-time reference processing inert.
            //
            // zt t7/cc: not under `keep_referents` -- the slot keeps its
            // referent, so the young cycle traces it; the row is still
            // recorded as handled above, so the post-collection pass treats
            // it as a survivor and the application-cleared rows (no referent
            // here) as cancelled, exactly as on a nulled cycle.
            //
            // zt t22/zc3: under `hide_referents` the slot keeps its referent
            // too, but the collection does NOT trace it: the row joins the
            // hidden set handed in below (the same screened rows the null
            // would have reached, so a refused row stays traced as before).
            if hide_referents {
                hidden.push(ref_obj_addr);
            } else if !keep_referents {
                shared
                    .mem
                    .heap
                    .set_field_suppress_satb(ref_obj, 0, Value::Object(None));
            }
        }
    }
    drop(class_manager);
    if hide_referents {
        shared.mem.heap.zgc_set_stw_reference_skip(hidden);
    }
    if audit {
        eprintln!(
            "[refaudit] pre-gc-null nulled={n_nulled} declined_shape={d_shape} \
             declined_stamp={d_stamp} declined_fields={d_fields} slot_row_split={d_split} classes={:?}",
            nulled_by_class
        );
    }
    if !referent_classes.is_empty() || !kept_old.is_empty() {
        let mut rp = shared.mem.ref_processor.lock();
        for (ref_obj_addr, class_id) in referent_classes {
            rp.stamp_referent_class(ref_obj_addr, class_id);
        }
        rp.forget_pre_gc_examined(&kept_old);
    }
}

/// gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item 4):
/// may [`weakref_null_referents_pre_gc`] leave the OLD referents of weak and
/// phantom rows in place for the collection about to run?
/// `CRATONVM_GEN_YOUNG_PAUSE_KEEPS_OLD_REFERENTS` (default ON, `=0` kills;
/// memoised) and the Generational heap's conservative prediction that the
/// pause runs no old-generation collection
/// (`GenerationalHeap::next_pause_certainly_minor`). `false` on every other
/// collector and under a pending clear-all-soft rule (that collection is the
/// last before an `OutOfMemoryError` and is never predicted young).
/// gcj j1yg-pause round 3: `CRATONVM_GEN_YOUNG_PAUSE_SETS_ASIDE_OLD_ROWS`
/// (default ON, `=0` kills). On a pause [`gen_young_pause_keeps_old_referents`]
/// admits (Generational, certainly minor, no last-ditch soft clear), the weak /
/// phantom rows whose `Reference` and referent are both old skip both
/// reference passes: neither can change them on such a pause. JIT-warm javac
/// holds 10-17 K of them; visiting them was most of 35-140 ms a pause.
pub(crate) fn gen_young_pause_sets_aside_old_rows(shared: &SharedVm) -> bool {
    gen_young_pause_keeps_old_referents(shared)
}

pub(in crate::runtime::interpreter) fn gen_young_pause_keeps_old_referents(shared: &SharedVm) -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let on = match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    };
    on && !last_ditch_soft_clear_armed() && shared.mem.heap.gen_next_pause_certainly_minor()
}

/// zt t7/cc — may [`weakref_null_referents_pre_gc`] leave every referent in
/// place for the collection about to run? Only on a ZGC generational heap with
/// `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` on (one call and no lock
/// otherwise), only when no clear-all-soft rule is pending (that collection
/// owes a clear; `CRATONVM_ZGC_GEN_SOFT_CLEAR_MAJOR` makes it whole-heap),
/// and only when the heap predicts a young cycle
/// (`ZgcRealHeap::arm_young_refs_strong`, which also records the answer for
/// the collection). The request is read, not consumed: the pass that serves
/// it takes it.
pub(in crate::runtime::interpreter) fn zgc_young_cycle_keeps_references(shared: &SharedVm) -> bool {
    if !shared.mem.heap.zgc_young_skip_references_on() {
        return false;
    }
    let clear_all_pending =
        shared.mem.ref_processor.lock().clear_all_soft_requested() || last_ditch_soft_clear_armed();
    shared.mem.heap.zgc_arm_young_refs_strong(!clear_all_pending)
}

/// gcg g2/r — the watched-set half of [`weakref_null_referents_pre_gc`] on
/// its own, for the `CRATONVM_WEAKREF_CLEAR=0` arm: publish every address the
/// reference processor holds (`ReferenceProcessor::all_tracked_addrs`) so the
/// collectors emit identity `pointer_map` entries for the watched survivors.
/// Any concurrent-sweep spans the processor still holds are pruned first, as
/// the default path's first processor call does.
pub(crate) fn publish_watched_reference_addrs(shared: &SharedVm) {
    let watch_addrs = {
        let mut rp = shared.mem.ref_processor.lock();
        rp.flush_deferred_sweep();
        if watched_set_unread(shared) {
            Vec::new()
        } else {
            rp.all_tracked_addrs()
        }
    };
    publish_watched_list(shared, watch_addrs);
}

/// gcj j3/rf3 (`gcj-j1rf-proposal-watched-set-maintained-by-the-processor`,
/// its publication slice) — publish this VM's watched list under its key
/// ([`watched_set_publish_key`]). `CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED`
/// (opt-in, `=1`; Generational only, G1 and ZGC unchanged; memoised): through
/// `gc_quiescence::set_watched_referents_reusing_for`, which reuses the row's
/// set when the list is element for element the one it was built from, so a
/// pause whose reference rows did not change (old `Reference`s to old
/// referents) re-hashes nothing. The published contents are the list on both
/// arms.
pub(in crate::runtime::interpreter) fn publish_watched_list(shared: &SharedVm, watch_addrs: Vec<usize>) {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let reuse = match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    };
    let key = watched_set_publish_key(shared);
    if reuse && shared.mem.heap.is_generational() {
        use cratonvm_gc::gc_quiescence as gq;
        use std::sync::atomic::Ordering::Relaxed;
        let reused = gq::set_watched_referents_reusing_for(key, watch_addrs);
        if crate::runtime::env_cache::dbg_watchref() {
            eprintln!(
                "[watchref] watched list {} (reused {} rebuilt {})",
                if reused { "reused" } else { "rebuilt" },
                gq::WATCHED_SET_REUSED.load(Relaxed),
                gq::WATCHED_SET_REBUILT.load(Relaxed),
            );
        }
    } else {
        cratonvm_gc::gc_quiescence::set_watched_referents_for(key, &watch_addrs);
    }
}

/// gcj j1/rf (`common-watched-referent-set-is-per-thread-not-per-heap-20261012`
/// section 2) — the key this VM publishes its watched-referent set under
/// (`gc_quiescence::set_watched_referents_for`). A Generational heap's key is
/// its old generation's base (`GenerationalHeap::watched_set_key`, the card
/// table's base: the same word `VmHeap::jit_card_table_info` reports), which
/// its collectors read their row by, so a second VM's publication nested on
/// this thread between this VM's publish and its collection can neither
/// replace nor be read for this VM's set. G1 and ZGC read no row: their
/// publication is keyed [`WATCHED_KEY_NO_READER`] so it never resets a
/// Generational VM's row on the same thread (what they publish, and that
/// nobody reads it, is unchanged).
///
/// `CRATONVM_GC_WATCHED_SET_PER_HEAP` (default ON, `=0` kill): `=0` publishes
/// unkeyed (`WATCHED_UNKEYED`), the one-slot set exactly. Read once per
/// process (a `MemoSlot`; a test's flag override resets it).
pub(crate) fn watched_set_publish_key(shared: &SharedVm) -> usize {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let per_heap = match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    };
    if !per_heap {
        return cratonvm_gc::gc_quiescence::WATCHED_UNKEYED;
    }
    match shared.mem.heap.jit_card_table_info() {
        Some((_, base, _)) if base != cratonvm_gc::gc_quiescence::WATCHED_UNKEYED => base,
        _ => WATCHED_KEY_NO_READER,
    }
}

/// gcj j1/rf — does a clear-all-soft collection record its clock for the
/// soft LRU (`ReferenceProcessor::note_round_clock`, committed at the round's
/// end) as the LRU arm does? Generational only (ZGC and G1 unchanged);
/// `CRATONVM_GEN_SOFT_CLEAR_ALL_ADVANCES_CLOCK` (default ON, `=0` kill),
/// memoised. Only the HotSpot LRU policy reads the clock
/// (`note_round_clock` is a no-op otherwise).
pub(in crate::runtime::interpreter) fn gen_soft_clear_all_advances_clock(shared: &SharedVm) -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    let on = match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    };
    on && shared.mem.heap.is_generational()
}

/// gcj j1/rf — the watched-set key of a heap that reads no row (G1, ZGC):
/// non-zero, so the publication is keyed, and odd, so it is never a
/// Generational heap's (page-aligned) old-generation base.
pub(crate) const WATCHED_KEY_NO_READER: usize = 1;

/// zr z1/rf — `CRATONVM_ZGC_SKIP_WATCHED_SET` (default ON, `=0` restores the
/// full set): is this VM's watched-referent set
/// (`gc_quiescence::set_watched_referents`) read by nobody, so the
/// pre-collection pass may publish it EMPTY instead of every address the
/// reference processor holds (`ReferenceProcessor::all_tracked_addrs`: the
/// `Reference`, referent and queue of every row, three entries per row, into
/// a SipHash `HashSet`, once per collection)?
///
/// True on a ZGC heap only. The set's readers are the Generational young
/// collectors and the old-gen collector (`gen_heap.rs`, `old_gen.rs`:
/// `is_watched_referent` / `watched_referents_snapshot`, which turn a watched
/// survivor into an identity `pointer_map` entry); ZGC (`gc/src/zgc*`) never
/// consults it, and `VmHeap::watched_pre_gc_addr_survived`'s ZGC arm is
/// `is_addr_live`, not the set. G1 is left as it was (out of this lane's
/// scope; it does not read the set either). The empty set is still
/// PUBLISHED, so a previous collection's entries never leak into this one.
pub(crate) fn watched_set_unread(shared: &SharedVm) -> bool {
    !shared.mem.heap.is_generational()
        && !shared.mem.heap.is_g1()
}

/// gcg g6/rs (`gcg-g5rs-shadowed-softreference-get-never-touches-the-lru-row`):
/// under `CRATONVM_REF_DISCOVER_AT_ALLOCATION`, a `SoftReference` whose `get()`
/// runs as bytecode (the `java/lang/ref` shadow) never reaches
/// `touch_soft_reference`; HotSpot's policy reads the Java field it writes,
/// `timestamp = SoftReference.clock`. So before the LRU policy condemns
/// anything, every active soft row whose Java `timestamp` holds a plain
/// millisecond clock later than the row's own stamp is touched with it.
/// `SoftReference.clock` is published by `publish_soft_reference_clock` at the
/// end of each processing round, on the processor's scale.
///
/// A no-op (one relaxed load) with the switch off: on the default path the
/// `get()` bridge touches the row itself and the field holds its encoded
/// stamp (`soft_touch_stamp`, milliseconds shifted left 20), which
/// [`java_soft_timestamp_is_newer`] rejects.
///
/// Lock order: the processor alone, the class manager alone, the processor
/// again.
pub(crate) fn refresh_soft_rows_from_java_timestamps(shared: &SharedVm) {
    if !shared.mem.ref_discovery.engaged() {
        return;
    }
    // gcj j2/rf2 (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item
    // 5): held concurrent-sweep spans name rows whose `Reference` the sweep
    // already freed (`CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`, opt-in): prune
    // them first, as `bind_allocated_references_pre_gc` does, so the slot read
    // below never touches a freed or reused block. A no-op with no held span.
    let stamps = {
        let mut rp = shared.mem.ref_processor.lock();
        rp.flush_deferred_sweep();
        rp.active_soft_stamps()
    };
    if stamps.is_empty() {
        return;
    }
    let soft_cid = shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/ref/SoftReference");
    let Some(soft_cid) = soft_cid else {
        return;
    };
    let Some(slot) =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, soft_cid, "timestamp")
    else {
        return;
    };
    let heap = &shared.mem.heap;
    let newer: Vec<(usize, u64)> = {
        let cm = shared.classes.class_manager.read();
        stamps
            .into_iter()
            .filter_map(|(addr, row_ms)| {
                // SAFETY: a row address the processor keeps exact (pruned with
                // its object by every pruner, relocated by `update_after_gc`);
                // the class test screens it before the slot is read.
                let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
                if heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array
                    || !cm.is_subclass_of(heap.class_id_of(obj), soft_cid)
                    || heap.num_fields(obj) <= slot
                {
                    return None;
                }
                match heap.get_field_as(obj, slot, b'J') {
                    Value::Long(ms) if java_soft_timestamp_is_newer(ms, row_ms) => {
                        // Cast: the guard above admits only `ms > 0`, so i64 -> u64 keeps the value.
                        Some((addr, ms as u64))
                    }
                    _ => None,
                }
            })
            .collect()
    };
    if newer.is_empty() {
        return;
    }
    let mut rp = shared.mem.ref_processor.lock();
    for (addr, ms) in newer {
        rp.touch_soft_reference(addr, ms);
    }
}

/// Is a Java `SoftReference.timestamp` value a plain millisecond clock (what
/// the `get()` bytecode copies from `SoftReference.clock`), and later than the
/// processor row's stamp? A bridge stamp (`soft_touch_stamp`: milliseconds
/// shifted left 20) is far above `2^43` and is never one.
pub(in crate::runtime::interpreter) fn java_soft_timestamp_is_newer(field: i64, row_ms: u64) -> bool {
    // Cast: both i64 -> u64 casts run only after `field > 0` (short-circuit), so neither wraps.
    field > 0 && (field as u64) < (1u64 << 43) && field as u64 > row_ms
}

/// gcg g6/rs: HotSpot advances `SoftReference.clock` after each processing
/// round; under `CRATONVM_REF_DISCOVER_AT_ALLOCATION` this VM does too, with
/// the processor's own soft clock (`ReferenceProcessor::soft_gc_clock`), so
/// what a shadowed constructor or `get()` copies into `timestamp` is on the
/// scale [`refresh_soft_rows_from_java_timestamps`] compares. A no-op with the
/// switch off, for a clock of `0`, or without the static.
pub(crate) fn publish_soft_reference_clock(shared: &SharedVm, clock_ms: u64) {
    if clock_ms == 0 || !shared.mem.ref_discovery.engaged() {
        return;
    }
    let target = {
        let cm = shared.classes.class_manager.read();
        cm.find_bootstrap_class_by_name("java/lang/ref/SoftReference")
            .and_then(|cid| {
                let class = cm.get_class(cid)?;
                let index = class
                    .fields
                    .iter()
                    .filter(|f| f.is_static())
                    .position(|f| &*f.name == "clock" && &*f.descriptor == "J")?;
                Some((cid, index))
            })
    };
    if let Some((cid, index)) = target {
        crate::vm::set_static_shared(shared, cid, index, Value::Long(clock_ms as i64));
    }
}

/// gcg g3/rw (`gcg-g2r-proposal-reference-discovery-at-allocation-not-at-the-constructor-bridge`,
/// step 3): bind every `Reference` registered at ALLOCATION
/// (`ReferenceProcessor::note_allocated_reference`) to the referent and queue
/// its own slots hold now, so it joins THIS collection's null pass like a row
/// the constructor bridge registered. One lock and a length test when nothing
/// was registered at allocation, which is every run until an allocation site
/// calls it (the sites are hunks on the proposal page).
///
/// Per entry, at its current (pre-collection) address:
///
/// * an active row already names it (the bridge ran): dropped;
/// * the object there is no longer of the allocated class, or has fewer than
///   two fields: dropped (the address no longer names that `Reference`; every
///   pruner keeps the list exact, so this is a screen, as in the null pass);
/// * slot 1 (`queue`) is null: its constructor has not run yet (the JDK
///   constructor stores `ReferenceQueue.NULL` for "no queue"), requeued;
/// * otherwise bound: slot 0 is the referent (a null one is never
///   discovered, HotSpot's rule), slot 1 the queue, `None` for a
///   `ReferenceQueue$Null` (`NULL` / `ENQUEUED`). Stamped with its identity
///   hash, as the bridge stamps.
///
/// Lock order as the null pass: processor, then the class manager alone, then
/// the processor again.
pub(crate) fn bind_allocated_references_pre_gc(shared: &SharedVm) {
    let pending = {
        let mut rp = shared.mem.ref_processor.lock();
        if rp.allocated_unbound_len() == 0 {
            return;
        }
        // Held sweep spans name pre-collection addresses: prune first, so a
        // freed entry is never read.
        rp.flush_deferred_sweep();
        let all = rp.take_allocated_unbound();
        // The bridge already registered these: nothing to bind, whatever
        // their slots say.
        let unregistered: Vec<_> = if rp.has_rows_outside_bind() {
            all.into_iter()
                .filter(|a| !rp.has_active_reference_row(a.row.reference_obj))
                .collect()
        } else {
            all
        };
        unregistered
    };
    enum Verdict {
        Drop,
        Requeue,
        Bind {
            referent: usize,
            queue: Option<usize>,
            stamp: i32,
        },
    }
    let null_queue_cid = shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/ref/ReferenceQueue$Null");
    let heap = &shared.mem.heap;
    let weak_reads = zgc_referent_reads_weak_enabled();
    let skip_stamp = heap.zgc_persistent_skip_bits_enabled();
    const CHUNK: usize = 4096;
    let mut slot_cache: rustc_hash::FxHashMap<u32, (usize, usize)> =
        rustc_hash::FxHashMap::default();
    let mut last_cid: u32 = u32::MAX;
    let mut last_slots: (usize, usize) = (0, 1);
    let mut verdicts: Vec<Verdict> = Vec::with_capacity(CHUNK);
    let mut skip_addrs: Vec<usize> = Vec::with_capacity(CHUNK);
    {
        let mut rp = shared.mem.ref_processor.lock();
        rp.reserve_active_rows_no_index(pending.len());
    }
    let mut census = [0usize; 4]; // dropped, requeued, bound, bind refused
    let mut pending_iter = pending.into_iter();
    loop {
        verdicts.clear();
        let chunk: Vec<_> = pending_iter.by_ref().take(CHUNK).collect();
        if chunk.is_empty() {
            break;
        }
        for a in &chunk {
            // SAFETY: an address the processor keeps exact (relocated by
            // `update_after_gc`, pruned with its object by every pruner); the
            // class test below screens it before any slot is read.
            let obj = unsafe { ObjectRef::from_raw(a.row.reference_obj as *mut u8) };
            let cid = heap.class_id_of(obj);
            if cid.as_u32() != a.class_id || heap.num_fields(obj) < 2 {
                verdicts.push(Verdict::Drop);
                continue;
            }
            let (ref_slot, queue_slot) = if cid.as_u32() == last_cid {
                last_slots
            } else {
                let s = *slot_cache.entry(cid.as_u32()).or_insert_with(|| {
                    let r = crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, cid, "referent")
                        .unwrap_or(0);
                    let q = crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, cid, "queue")
                        .unwrap_or(1);
                    (r, q)
                });
                last_cid = cid.as_u32();
                last_slots = s;
                s
            };
            let queue = match heap.get_field(obj, queue_slot) {
                Value::Object(Some(q)) => q,
                _ => {
                    verdicts.push(Verdict::Requeue);
                    continue;
                }
            };
            let referent_value = if weak_reads {
                heap.get_field_weak(obj, ref_slot)
            } else {
                heap.get_field(obj, ref_slot)
            };
            let referent = match referent_value {
                Value::Object(Some(r)) => r.as_ptr() as usize,
                _ => 0,
            };
            let queue = if null_queue_cid.is_some_and(|nq| heap.class_id_of(queue) == nq) {
                None
            } else {
                Some(queue.as_ptr() as usize)
            };
            let stamp = if referent != 0 && !skip_stamp {
                heap.identity_hash_code(obj)
            } else {
                0
            };
            verdicts.push(Verdict::Bind {
                referent,
                queue,
                stamp,
            });
        }
        skip_addrs.clear();
        let mut rp = shared.mem.ref_processor.lock();
        for (a, verdict) in chunk.into_iter().zip(verdicts.drain(..)) {
            let addr = a.row.reference_obj;
            match verdict {
                Verdict::Drop => census[0] += 1,
                Verdict::Requeue => {
                    census[1] += 1;
                    rp.requeue_allocated_unbound(a);
                }
                Verdict::Bind {
                    referent,
                    queue,
                    stamp,
                } => {
                    if rp.bind_allocated_reference_deferred_skip(a, referent, queue, &mut skip_addrs)
                        == cratonvm_gc::reference::BindOutcome::Bound
                    {
                        census[2] += 1;
                        rp.stamp_reference(addr, stamp);
                    } else {
                        census[3] += 1;
                    }
                }
            }
        }
        rp.persistent_skip_set_batch(&skip_addrs);
    }
    if dbg_refdisc_enabled() {
        eprintln!(
            "[refdisc] bind dropped={} requeued={} bound={} refused={}",
            census[0], census[1], census[2], census[3]
        );
    }
}


/// Cached `CRATONVM_DBG_REFDISC` gate: trace every `discover_reference` with
/// the reference object's CLASS NAME. The numeric `ref_type` cannot tell an
/// ordinary `PhantomReference` from a `jdk.internal.ref.Cleaner` (the latter
/// is a subclass, so its `super(referent, dummyQueue)` arrives with the same
/// phantom tag) — which is exactly the question
/// `direct-bytebuffers-are-never-reclaimed-20260805.md` needed answered
/// before its Cleaner routing could be written.
#[inline]
pub fn dbg_refdisc_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_NO_CLEANERS` gate (bc math-ec 0x4 bisect): skip ONLY
/// `run_cleaner_actions` + `run_finalizers` (the Java invokes on queued —
/// possibly stale — addresses), keeping `process_references_after_gc` live.
/// Discriminates "the invokes corrupt" from "the refproc loops corrupt".
#[inline]
pub(in crate::runtime::interpreter) fn no_cleaners() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_AIOOBE` gate (perf P1, audit `vm-runtime.md`): the
/// array load/store AIOOBE diagnostic. The previous call sites invoked
/// `None::<String>` (which locks the process env and
/// allocates a `String`) on the array-bounds error path; cache it once like
/// the sibling gates above.
#[inline]
pub(in crate::runtime::interpreter) fn aioobe_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_AIOOBE2` gate (perf P1): secondary array-bounds
/// diagnostic; same rationale as [`aioobe_dbg`].
#[inline]
pub(in crate::runtime::interpreter) fn aioobe2_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// `CRATONVM_DBG_LINKAGE=1` — trace VM-raised `LinkageError`s through the
/// interpreter's error routing. A linkage error that never reaches
/// `throw_linkage_error` stays a `VmError` and unwinds past every Java handler,
/// which looks identical to a VM crash; this names the routing arm it took and
/// the Java frames it took it from.
pub(crate) fn dbg_linkage() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Dump `where` plus the live Java frames, deepest first. `CRATONVM_DBG_LINKAGE` only.
pub(crate) fn dbg_linkage_dump(thread: &JvmThread, where_: &str, detail: &str) {
    eprintln!("[DBG_LINKAGE] {where_}: {detail}");
    for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
        eprintln!(
            "[DBG_LINKAGE-STK {i}] {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc
        );
    }
}

/// Validate a primitive-array-store receiver header; dump receiver + Java
/// stack when it is not a plausible array (the stale-ref smear signature).
/// Reads raw header BYTES (not enum fields) — a garbage `kind`/`element_type`
/// byte outside the declared variants would be UB to materialize as the enum.
pub(in crate::runtime::interpreter) fn arrstore_check(
    _shared: &SharedVm,
    thread: &JvmThread,
    array_ref: cratonvm_types::ObjectRef,
    index: i32,
    op: &str,
) {
    let p = array_ref.as_ptr();
    // SAFETY: `array_ref` was decoded from the operand stack as an in-heap
    // pointer; reading the first 16 header bytes of managed memory is safe
    // (arenas stay mapped) even if the contents are garbage.
    let (class_id_raw, kind_byte, elem_byte, array_len) = unsafe {
        (
            // Cast: reinterpret raw header byte pointer as *const u32 to read packed fields
            (p as *const u32).read_unaligned(),
            cratonvm_types::kind_tag_at(p),
            cratonvm_types::element_type_tag_at(p),
            // Cast: reinterpret raw header byte pointer as *const u32 to read packed fields
            (p.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32).read_unaligned(),
        )
    };
    // ObjectKind::Array == 1; ArrayElementType has < 16 variants; a real
    // array_length is <= i32::MAX. Anything else = garbage header = stale ref.
    // Widening: i32::MAX -> u32 (positive constant fits in u32)
    let plausible = kind_byte == 1 && elem_byte < 16 && array_len <= i32::MAX as u32;
    if plausible {
        return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let k = N.fetch_add(1, Ordering::Relaxed);
    if k >= 8 {
        return;
    }
    eprintln!(
        "[arrstore] #{k} {op} through GARBAGE-HEADER receiver @0x{:x}: class_id_raw={} kind_byte={} elem_byte={} array_len={} index={}",
        // Cast: raw pointer to integer address for diagnostic formatting
        p as usize, class_id_raw, kind_byte, elem_byte, array_len, index,
    );
    eprintln!("[arrstore] Java stack (top first):");
    for f in thread.frames.iter().rev().take(28) {
        eprintln!(
            "[arrstore]   {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc,
        );
    }
}

/// Memoized `class_id -> is a watched EC holder` decision for the
/// `CRATONVM_DBG_ECWATCH` software watchpoint (only the few-instance curve /
/// asn1.x9 holders). The class-name resolution happens once per class id;
/// subsequent reference-field stores pay only a hashmap lookup, so the
/// per-ref-putfield overhead stays negligible. Reached only behind the
/// ECWATCH gate.
pub(in crate::runtime::interpreter) fn ec_is_watched_class(shared: &SharedVm, cid: cratonvm_types::ClassId) -> bool {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    // PER-VM STATE (P0, `docs/architecture/per-vm-state.md`): the memo answers
    // "does this ClassId's name match the EC watch list?", and `ClassId`s are
    // allocated per-VM, so the key must carry `vm_identity` or a second VM
    // reads the first VM's verdict for an unrelated class.
    static MEMO: OnceLock<Mutex<std::collections::HashMap<(usize, u32), bool>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = (shared.vm_identity, cid.as_u32());
    if let Some(&v) = memo.lock().get(&key) {
        return v;
    }
    let v = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cid)
            .map(|c| {
                c.name.contains("/asn1/x9/")
                    || c.name.contains("Curve")
                    // bc math-ec 0x4: the mutator-written victim VARIES across
                    // runs and is often a small, frequently-allocated JDK class
                    // (the pre-GC young 0x4 was measured on java/util/HexFormat
                    // fld[1] and java/util/logging/Level). Watch those too so the
                    // per-native detect (CRATONVM_DBG_ECWATCH_NATIVE) names the
                    // writer whichever object the stray write lands on.
                    || c.name.contains("java/util/HexFormat")
                    || c.name.contains("java/util/logging/Level")
            })
            .unwrap_or(false)
    };
    memo.lock().insert(key, v);
    v
}

// ---------------------------------------------------------------------------
// Allocation, GC, and the root protocol
// ---------------------------------------------------------------------------
//
// TLABs, the GC entry points, the STW takeover protocol, root snapshots and
// the moving-collector remap, plus the provenance tracing built to debug
// them: `interpreter/gc_and_alloc.rs`.

// ---------------------------------------------------------------------------
// Instruction execution result (internal)
// ---------------------------------------------------------------------------

/// The result of executing a single instruction.
pub(in crate::runtime::interpreter) enum InstructionResult {
    /// Continue to next instruction.
    Continue,
    /// Method returned a value (or void = None).
    Return(Option<Value>),
    /// A new bytecode frame was pushed; caller should update frame_idx.
    FramePushed,
}

/// Result of a cached inline call attempt (stackless dispatch).
#[derive(Debug)]
pub(in crate::runtime::interpreter) enum CachedCallResult {
    /// Bytecode frame pushed onto thread.frames. Caller should update frame_idx.
    FramePushed,
    /// Call fully handled (native method executed, return value pushed).
    Handled,
    /// No cache hit — fall through to slow path.
    CacheMiss,
}

// ---------------------------------------------------------------------------
// H6 — native-stack-aware re-entrant recursion guard
// ---------------------------------------------------------------------------
//
// The `execute` re-entrancy guard (see the `EXEC_DEPTH` block inside
// `execute`) must trip — throwing a *catchable* `StackOverflowError` —
// BEFORE deep re-entrant native dispatch (e.g. reflective `Method.invoke`
// cascades) exhausts the OS thread's native stack and aborts the whole
// process (uncatchable).
//
// The ceiling cannot be a single hard-coded constant because different
// threads run on very different native stacks: the main VM thread is spawned
// with a 128 MiB stack, whereas worker / `java.lang.Thread` carriers default
// to 8 MiB (see `vm_exec.rs`, honouring `RUST_MIN_STACK`). A constant tuned
// for one is wrong for the other — too low throttles legitimate deep
// recursion on the big stack, too high lets a worker blow its 8 MiB stack
// before the guard ever trips.
//
// So we derive a per-thread ceiling from the thread's ACTUAL native stack
// size. Threads that host Java execution call
// [`init_thread_exec_depth_ceiling`] with their configured native stack size
// at start-up; the guard reads the resulting per-thread ceiling.

/// Conservative estimate of how many bytes of native stack a single
/// re-entrant `execute` level can consume. Each `execute` call adds several
/// Rust frames (the recursive `execute_frame`, invoke dispatch, and the JIT
/// entry trampoline). The original guard assumed ~6.4 KiB/level (10 000
/// levels for a 64 MiB stack); we use a deliberately pessimistic 8 KiB so the
/// derived ceiling trips with margin to spare before the OS guard page.
pub(in crate::runtime::interpreter) const NATIVE_STACK_BYTES_PER_EXEC_LEVEL: usize = 8 * 1024;

/// Fraction of the native stack we are willing to spend on re-entrant
/// `execute` recursion before tripping the guard. The remainder is reserved
/// head-room for the deepest single Java frame's own locals/operands plus any
/// native callee (the verifier, GC, JIT) running on top of the deepest level.
/// `2` => use at most half the stack for recursion depth.
pub(in crate::runtime::interpreter) const NATIVE_STACK_SAFETY_DIVISOR: usize = 2;

/// Default native stack assumption for a thread that never called
/// [`init_thread_exec_depth_ceiling`]. The process main VM thread is spawned
/// with a 128 MiB stack (see `vm-cli` `main`), so defaulting to 128 MiB keeps
/// the deep-recursion head-room that workloads such as `binaryTrees` rely on
/// even if the explicit init call is not wired for that thread. Worker carrier
/// threads (8 MiB) DO call the init function and therefore get a correctly
/// lowered ceiling rather than this default.
pub(in crate::runtime::interpreter) const DEFAULT_NATIVE_STACK_BYTES: usize = 128 * 1024 * 1024;

/// Absolute floor for the derived ceiling. Even on a tiny stack we allow at
/// least this many re-entrant levels so ordinary (non-pathological) call
/// graphs are never spuriously rejected.
pub(in crate::runtime::interpreter) const MIN_EXEC_DEPTH_CEILING: u32 = 256;

/// Compute the re-entrant `execute` depth ceiling for a thread whose native
/// stack is `native_stack_bytes` large.
#[inline]
pub(in crate::runtime::interpreter) fn derive_exec_depth_ceiling(native_stack_bytes: usize) -> u32 {
    // Diagnostic / safety override: `CRATONVM_EXEC_DEPTH_CEILING=<n>` forces the
    // `execute` recursion ceiling. Useful to (a) confirm a runaway recursion goes
    // through `execute` by forcing an early *catchable* StackOverflowError (whose
    // Java stack trace reveals the cycle), and (b) cap deep frames whose real
    // per-level native-stack cost exceeds NATIVE_STACK_BYTES_PER_EXEC_LEVEL.
    if let Ok(v) = cratonvm_types::flags::runtime_var("CRATONVM_EXEC_DEPTH_CEILING") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return n.max(1);
        }
    }
    let usable = native_stack_bytes / NATIVE_STACK_SAFETY_DIVISOR;
    let levels = usable / NATIVE_STACK_BYTES_PER_EXEC_LEVEL;
    // Clamp into u32 and apply the floor.
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    let levels = levels.min(u32::MAX as usize) as u32;
    levels.max(MIN_EXEC_DEPTH_CEILING)
}

/// Conservative estimate of how many bytes of native stack a single
/// re-entrant JIT *dispatch* level can consume. The helper contains argument
/// decoding, MIC/PIC handling, a SATB flush, and a compiled-Java frame. Its
/// release footprint is nevertheless within the interpreter's proven 8 KiB
/// per-level budget.
///
/// The counter tracks every active JIT-to-JIT dispatch, not only recursive
/// calls.  Production Lucene vector search legitimately keeps more than 128
/// such calls live on an 8 MiB worker carrier, so the former 32 KiB estimate
/// converted an ordinary call chain into a spurious `StackOverflowError`.
/// Reserve 8 KiB per level, matching the interpreter's proven conservative
/// frame budget: an 8 MiB carrier still retains half its stack as head-room
/// and the guard continues to turn pathological recursion into a catchable
/// Java exception before the native guard page.
pub(in crate::runtime::interpreter) const NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL: usize = 8 * 1024;

/// Absolute floor for the derived JIT-dispatch ceiling. Distinct from (and
/// lower than) [`MIN_EXEC_DEPTH_CEILING`]. It was sized when the JIT per-level
/// budget was 32 KiB, 4x the exec budget: the 256-level exec floor would then
/// have implied 8 MiB of recursion, a whole worker carrier stack. The budget is
/// 8 KiB now (see [`NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL`]), so 64 levels
/// is 512 KiB — still never above stack capacity on the smallest carrier, and
/// the floor only matters for stacks under 1 MiB.
pub(in crate::runtime::interpreter) const MIN_JIT_DISPATCH_DEPTH_CEILING: u32 = 64;

/// Compute the re-entrant JIT-dispatch depth ceiling for a thread whose native
/// stack is `native_stack_bytes` large. Mirrors [`derive_exec_depth_ceiling`]
/// with the JIT-dispatch per-level estimate and a lower floor (see
/// [`MIN_JIT_DISPATCH_DEPTH_CEILING`]). Unlike the exec ceiling it does not
/// honour `CRATONVM_EXEC_DEPTH_CEILING`.
#[inline]
pub(in crate::runtime::interpreter) fn derive_jit_dispatch_depth_ceiling(native_stack_bytes: usize) -> u32 {
    let usable = native_stack_bytes / NATIVE_STACK_SAFETY_DIVISOR;
    let levels = usable / NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL;
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    let levels = levels.min(u32::MAX as usize) as u32;
    levels.max(MIN_JIT_DISPATCH_DEPTH_CEILING)
}

/// The carrier size up to which a recorded thread's JIT-dispatch ceiling is
/// [`derive_jit_dispatch_depth_ceiling`]'s: the default Java-thread carrier
/// (8 MiB), the carrier `vm_exec`'s `DEFAULT_CARRIER_STACK_BYTES` sizes.
pub(in crate::runtime::interpreter) const JIT_DISPATCH_DEFAULT_CARRIER_BYTES: usize = 8 << 20;

/// The native stack a carrier larger than the default keeps below the
/// dispatch levels its ceiling admits: the 4 MiB the default carrier keeps
/// (half of it), which is also the headroom the compiled self-recursion floor
/// leaves on such a carrier.
pub(in crate::runtime::interpreter) const JIT_DISPATCH_REQUESTED_HEADROOM_BYTES: usize = 4 << 20;

/// The JIT-dispatch ceiling [`init_thread_exec_depth_ceiling`] installs for a
/// thread whose carrier has `native_stack_bytes` (interpreter round i1 wave
/// 40, lane L2;
/// `docs/feature-designs/real-jdk-mode.md`).
/// [`derive_jit_dispatch_depth_ceiling`]'s value for the default carrier and
/// below. A larger carrier (a thread that asked for a stack, or `-Xss`: the
/// request plus the default) keeps only the default's 4 MiB of headroom
/// rather than half of itself, so compiled recursion through the dispatch
/// helpers goes as deep as the compiled self-recursion floor lets a direct
/// self call go on it (wave 39): a 32 MiB request (40 MiB carrier) admits
/// 4608 levels where half the carrier admitted 2560. The per-level estimate is
/// unchanged, and `jit::helpers::jit_native_stack_exhausted` still checks the
/// real stack pointer at every helper entry.
pub(in crate::runtime::interpreter) fn derive_recorded_jit_dispatch_depth_ceiling(native_stack_bytes: usize) -> u32 {
    let derived = derive_jit_dispatch_depth_ceiling(native_stack_bytes);
    if native_stack_bytes <= JIT_DISPATCH_DEFAULT_CARRIER_BYTES {
        return derived;
    }
    let levels = (native_stack_bytes - JIT_DISPATCH_REQUESTED_HEADROOM_BYTES)
        / NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL;
    derived.max(u32::try_from(levels).unwrap_or(u32::MAX))
}

thread_local! {
    /// Per-thread re-entrant `execute` depth ceiling, derived from the
    /// thread's native stack size. Defaults to the main-VM-thread value
    /// (`DEFAULT_NATIVE_STACK_BYTES`) so a thread that never calls
    /// [`init_thread_exec_depth_ceiling`] still gets a safe (high) ceiling.
    pub(in crate::runtime::interpreter) static EXEC_DEPTH_CEILING: std::cell::Cell<u32> =
        std::cell::Cell::new(derive_exec_depth_ceiling(DEFAULT_NATIVE_STACK_BYTES));

    /// Per-thread re-entrant JIT-dispatch depth ceiling, derived from the same
    /// native stack size as [`EXEC_DEPTH_CEILING`] but with the larger
    /// per-level JIT-dispatch frame budget. Read by the JIT dispatch helpers
    /// (`jit_invoke_dispatch` / `jit_invoke_virtual_mic` in `jit/helpers.rs`)
    /// via [`jit_dispatch_depth_ceiling`] so a JIT→JIT recursion (e.g.
    /// `binaryTrees(18)` deep `make()` recursion) throws a *catchable*
    /// `StackOverflowError` before the OS native stack overflows.
    pub(in crate::runtime::interpreter) static JIT_DISPATCH_DEPTH_CEILING: std::cell::Cell<u32> =
        std::cell::Cell::new(derive_jit_dispatch_depth_ceiling(DEFAULT_NATIVE_STACK_BYTES));

    /// The `native_stack_bytes` the two ceilings above were last derived
    /// from by [`init_thread_exec_depth_ceiling`]; `usize::MAX` = never. A
    /// repeat call with the same size (a virtual-thread carrier, once per
    /// mount) returns without re-deriving, which reads an environment variable.
    pub(in crate::runtime::interpreter) static EXEC_DEPTH_CEILING_STACK_BYTES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(usize::MAX) };
}

/// Per-thread re-entrant JIT-dispatch depth ceiling.
///
/// The JIT→JIT call path (`jit_invoke_dispatch` → compiled entry → … and
/// `jit_invoke_virtual_mic` → compiled entry → …) bypasses
/// [`execute`] entirely, so the interpreter's `EXEC_DEPTH` guard never fires
/// for purely-compiled recursion. The JIT dispatch helpers therefore maintain
/// their OWN depth counter and trip it against this ceiling, throwing a
/// catchable `java/lang/StackOverflowError` instead of letting deep compiled
/// recursion blow the OS native stack (an uncatchable rc=127 abort).
///
/// Derived per-thread from the same native stack size recorded by
/// [`init_thread_exec_depth_ceiling`]; defaults to the safe 128 MiB-derived
/// value for threads that never called it.
#[inline]
pub fn jit_dispatch_depth_ceiling() -> u32 {
    JIT_DISPATCH_DEPTH_CEILING.with(|c| c.get())
}

thread_local! {
    /// This OS thread's re-entrant [`execute`] depth: the count the
    /// [`EXEC_DEPTH_CEILING`] guard in `execute` trips against. Was a
    /// thread-local inside `execute` until the tier-1 round (wave 2, lane K).
    pub(in crate::runtime::interpreter) static EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Replace this OS thread's re-entrant `execute` depth with `depth` and
/// answer the previous one (tier-1 round wave 2, lane K; item (b) of the t1
/// status of `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`).
///
/// A virtual-thread slice run on its carrier's OS thread
/// (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`) counts from 0, as it does on a
/// runner, and the carrier's count comes back when the slice ends (every
/// level the slice entered has unwound by then: a yield unwinds to
/// `run_slice`). Without it the carrier's own levels below `enterSpecial`
/// counted against the virtual thread, which then hit `StackOverflowError`
/// that many levels sooner than on a runner. The native stack itself stays
/// guarded by `reentry_native_stack_exhausted`, which reads the real stack
/// pointer against this OS thread's stack bounds, carrier frames included.
pub(crate) fn replace_exec_depth(depth: u32) -> u32 {
    EXEC_DEPTH.with(|d| d.replace(depth))
}

/// Record the calling thread's native stack size so the re-entrant `execute`
/// recursion guard can derive a correct [`StackOverflowError`] ceiling for it.
///
/// Call this ONCE, from inside the thread, before it begins executing Java
/// bytecode. Worker / `java.lang.Thread` carriers (which default to an 8 MiB
/// native stack) MUST call this so the guard trips before their smaller stack
/// overflows; the process main thread may also call it but otherwise inherits
/// the safe 128 MiB-derived default.
///
/// `native_stack_bytes` is the value passed to
/// `std::thread::Builder::stack_size` for the calling thread.
pub fn init_thread_exec_depth_ceiling(native_stack_bytes: usize) {
    if EXEC_DEPTH_CEILING_STACK_BYTES.with(|b| b.replace(native_stack_bytes)) == native_stack_bytes
    {
        return;
    }
    let ceiling = derive_exec_depth_ceiling(native_stack_bytes);
    EXEC_DEPTH_CEILING.with(|c| c.set(ceiling));
    // Derive the companion JIT-dispatch ceiling from the SAME native stack
    // size so the JIT→JIT recursion guard (see `jit_dispatch_depth_ceiling`)
    // is calibrated for this thread's real stack too. Worker carriers (8 MiB)
    // get a correspondingly lower ceiling; the main 128 MiB thread keeps deep
    // head-room for legitimate recursion like `binaryTrees`.
    // A carrier larger than the default keeps the default's headroom, not
    // half of itself (wave 40, lane L2).
    let jit_ceiling = derive_recorded_jit_dispatch_depth_ceiling(native_stack_bytes);
    if native_stack_bytes > JIT_DISPATCH_DEFAULT_CARRIER_BYTES
        && crate::runtime::env_cache::dbg_jitc()
    {
        // Once per thread (a repeat with the same size returned above): the
        // positive control of the scaled ceiling.
        eprintln!(
            "[cratonvm-jitc] jit-dispatch depth ceiling scaled: carrier={native_stack_bytes} ceiling={jit_ceiling}"
        );
    }
    JIT_DISPATCH_DEPTH_CEILING.with(|c| c.set(jit_ceiling));
}


// ---------------------------------------------------------------------------
// Interpreter round i6 wave 133, lane L4: native stack-pointer guard for
// nested interpreter entries (`CRATONVM_EXEC_NATIVE_STACK_GUARD`, a parity fix,
// default ON, `=0` restores the count-only bound).
// docs/feature-designs/jit-ir-optimizations.md
// ---------------------------------------------------------------------------
//
// The `EXEC_DEPTH` ceiling above COUNTS nested `execute` levels against an
// 8 KiB-per-level estimate; it never looks at the stack pointer. A recursion
// closed through a native door whose own Rust frames are larger than that
// (`rec -> MethodHandle.invokeWithArguments` served by the `lang_invoke`
// doors, `--nojit`: `L4W130ExactPushAndLinkMemo` row `B soe`) ran out of
// native stack while the count, and the 8192-frame `max_stack_depth` cap, were
// both still under their limits, and the process died with a native
// `EXCEPTION_STACK_OVERFLOW` (exit 127) instead of a catchable
// `StackOverflowError`. Nested entries reached through
// [`run_pushed_frame_to_completion`] (`install_and_run_cached_frame`, the doors'
// fast path) were not counted at all. The JIT has had the real check since
// round 13 (`jit_native_stack_exhausted`), which is why the JIT arm was SAME.
//
// So both nested entries also compare the stack pointer against a per-thread
// floor a fixed margin above the thread's stack low end. The margin sits below
// the JIT's compiled-recursion floor (whose guard part is up to 1 MiB above the
// low end), so with the JIT on the compiled guard keeps tripping first and this
// one only catches what no compiled check saw. The error is built in place with
// this guard suspended (the constructor runs Java, below the floor by
// definition), as `raise_jit_stack_overflow` does for the JIT floor.

/// Largest reserve between a thread's stack low end and the nested-entry
/// floor: room for one `StackOverflowError` construction (`Throwable.<init>`,
/// `fillInStackTrace`) and the native code between two nested entries.
pub(in crate::runtime::interpreter) const REENTRY_STACK_MARGIN_MAX: usize = 512 * 1024;

/// Smallest reserve, for small stacks (never more than half the stack).
pub(in crate::runtime::interpreter) const REENTRY_STACK_MARGIN_MIN: usize = 128 * 1024;

/// The reserve below the nested-entry floor for a stack of `size` bytes.
pub(in crate::runtime::interpreter) fn reentry_stack_margin(size: usize) -> usize {
    (size / 16)
        .clamp(REENTRY_STACK_MARGIN_MIN, REENTRY_STACK_MARGIN_MAX)
        .min(size / 2)
}

thread_local! {
    /// `(low, floor)` of this thread's stack for [`reentry_native_stack_exhausted`]:
    /// `low == usize::MAX` = not yet queried, `(0, 0)` = bounds unknown (the
    /// guard never trips). A thread's stack never moves, so it is queried once.
    pub(in crate::runtime::interpreter) static REENTRY_STACK_RANGE: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((usize::MAX, 0)) };
    /// Set while [`raise_reentry_stack_overflow`] builds its error.
    pub(in crate::runtime::interpreter) static REENTRY_STACK_GUARD_SUSPENDED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cold]
pub(in crate::runtime::interpreter) fn compute_reentry_stack_range() -> (usize, usize) {
    match crate::jit::helpers::thread_stack_bounds() {
        Some((low, high)) => (low, low.saturating_add(reentry_stack_margin(high - low))),
        None => (0, 0),
    }
}

/// Is this thread's native stack pointer at or below the nested-entry floor?
/// One TLS load and two compares on the common path; the switch is read only
/// below the floor. A stack pointer outside `[low, floor]` (unknown bounds, or
/// a stack the cached bounds do not describe) never trips.
#[inline]
pub(in crate::runtime::interpreter) fn reentry_native_stack_exhausted() -> bool {
    let probe = 0u8;
    let sp_now = std::ptr::addr_of!(probe) as usize;
    let (low, floor) = REENTRY_STACK_RANGE.with(|r| {
        let v = r.get();
        if v.0 != usize::MAX {
            v
        } else {
            let computed = compute_reentry_stack_range();
            r.set(computed);
            computed
        }
    });
    sp_now >= low
        && sp_now <= floor
        && !REENTRY_STACK_GUARD_SUSPENDED.with(std::cell::Cell::get)
}

/// Build the catchable `StackOverflowError` a tripped
/// [`reentry_native_stack_exhausted`] raises, with the guard suspended while
/// the constructor runs.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn raise_reentry_stack_overflow(shared: &SharedVm, thread: &mut JvmThread) -> MethodCallFailed {
    struct Resume;
    impl Drop for Resume {
        fn drop(&mut self) {
            REENTRY_STACK_GUARD_SUSPENDED.with(|s| s.set(false));
        }
    }
    REENTRY_STACK_GUARD_SUSPENDED.with(|s| s.set(true));
    let _resume = Resume;
    dump_stack_on_soe(thread);
    crate::runtime::exceptions::throw_runtime_error(shared, thread, RuntimeError::StackOverflowError)
}

/// `CRATONVM_EXEC_NATIVE_STACK_GUARD_LAZY_ERROR` (interpreter round i6 wave
/// 134, lane L4; parity, default ON, `=0` builds the error in place as wave
/// 133 did): does `execute`'s native stack trip return the lazy
/// `StackOverflowError` the `EXEC_DEPTH` trip returns? Read only on a trip.
#[cold]
/// May [`run_pushed_frame_to_completion`]'s native stack trip pop what was
/// pushed (wave 134, lane L4)? Only one frame that has not run: a frame
/// fresh from a push is at bytecode index 0 with no instruction executed.
/// A deopt resume's frames (a chain, or one frame mid-body) have run.
#[inline]
pub(in crate::runtime::interpreter) fn reentry_trip_applies(thread: &JvmThread, frames_depth_before_push: usize) -> bool {
    thread.frames.len() == frames_depth_before_push + 1
        && thread
            .frames
            .last()
            .is_some_and(|top| top.pc == 0 && top.last_instr_pc == 0)
}

/// [`pop_root_frames_keeping_result`] for frames removed by the native stack
/// trip (wave 134, lane L4): each pop is an exception unwind
/// (`pop_and_recycle_frame_with_reason(.., true)`: `MethodExit` and
/// `FramePop` report `was_popped_by_exception`, the held monitor is
/// released), with a thrown `result` pinned across the pops while a
/// `FramePop` listener exists.
pub(in crate::runtime::interpreter) fn pop_unrun_frames_by_exception_keeping_result(
    shared: &SharedVm,
    thread: &mut JvmThread,
    depth_before_push: usize,
    result: &mut MethodCallResult,
) {
    let held = match &*result {
        Err(MethodCallFailed::ExceptionThrown(e)) => Some(*e),
        _ => None,
    };
    let pin = held.filter(|_| {
        crate::runtime::jvmti::any_frame_pop_listener_active()
            || crate::runtime::jvmti::any_method_exit_listener_active()
    });
    let pin_base = thread.native_pin_roots.len();
    if let Some(r) = pin {
        thread.native_pin_roots.push(r);
    }
    while thread.frames.len() > depth_before_push {
        pop_and_recycle_frame_with_reason(shared, thread, true);
    }
    if pin.is_some() {
        let moved = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        if let Err(MethodCallFailed::ExceptionThrown(e)) = result {
            *e = moved;
        }
    }
}


/// The native stack size the calling thread recorded through
/// [`init_thread_exec_depth_ceiling`] (its carrier's `stack_size`), `None`
/// for a thread that never did (the process main thread). Read once per
/// thread by the JIT's compiled self-recursion floor
/// (`jit::helpers::compute_self_call_stack_floor`), which scales its budget
/// with a carrier larger than the default (interpreter round i1 wave 39, lane
/// L2; `i38-L7-compiled-recursion-ignores-a-threads-requested-stack-size`).
pub fn recorded_thread_native_stack_bytes() -> Option<usize> {
    let bytes = EXEC_DEPTH_CEILING_STACK_BYTES.with(|b| b.get());
    (bytes != usize::MAX).then_some(bytes)
}


// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// activate-ir-optimizer (runtime wiring): cached `CRATONVM_JIT_C2_FIRST_CALL`
/// flag. The `fn execute` first-call compile path uses the single-pass backend
/// (`jit::x64::compile`) directly and caches a non-IR body on call #1, which
/// preempts the optimizing IR pipeline (`jit::try_compile`) on every later path
/// (dispatcher warmup, OSR, background worker all probe `jit_cache` first). When
/// this flag is set, that eager first-call single-pass compile is replaced by an
/// invocation-counted upgrade through `try_jit_compile_callee` →
/// `jit::try_compile(optimize=true)` (which SUBSUMES single-pass), so hot methods
/// actually reach the IR optimizer. Read once and cached (this is on the hot
/// uncached-invocation path). Default-OFF → behaviour is byte-for-byte unchanged.
pub(in crate::runtime::interpreter) fn c2_first_call_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()
    })
}



/// `CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL` (default OFF): the measurement arm of
/// tier proposal W2-1. Under `CRATONVM_JIT_EAGER_ORDINARY_DOOR` (default on)
/// the eager first-call door then hands EVERY method to `jit::try_compile`
/// through the wrapped-entry door, as it already does for a method with
/// handlers or a long/double parameter, and its own front end
/// (`compile_with_param_slots` below) is never reached. Off, nothing changes.
/// Retiring that front end waits on the startup and regression comparison
/// under `CRATONVM_BG_COMPILE=0`; the known differences are listed in
/// `r12w3-tier-eager-first-call-front-end-not-yet-equivalent-CLOSED-20261006.md`.
/// Read once and cached, like the flags above.
pub(in crate::runtime::interpreter) fn eager_door_routes_every_method() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG
        .get_or_init(|| false)
}



/// `CRATONVM_JIT_TIERUP_SINK_REFUSAL_THROWS` (default ON; `0` restores the
/// VM-internal error): how `execute`'s first-call tier-up sink answers its own
/// stash that no arm could resume and whose replay from entry would commit
/// again. On, it answers as the `jit_bridge` doors do: the re-run is counted
/// (and the method retired) in the door census, a redefined class re-runs,
/// and anything else is a Java `InternalError` thrown at the call (round 13
/// wave 1, lane replay). Not memoised: read only on that path, which the
/// compile-time replay checks argue unreachable.
pub(in crate::runtime::interpreter) fn tierup_sink_refusal_throws() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Real-JDK mode: kind-aware native resolution for this file's dispatch sites
// ---------------------------------------------------------------------------

/// The one native lookup used by every native-dispatch site in this file
/// (`docs/book/src/user-guide/real-jdk-mode.md` §7).
///
/// Replaces the bare `NativeMethodRegistry::find` calls that handed back a
/// `NativeCallback` with the `NativeKind` thrown away. A lookup that discards
/// the kind cannot tell a reviewed `Intrinsic` from a `SyntheticStub`, so it
/// cannot enforce §1.3 or §1.4 — it was a policy bypass sitting *beside*
/// `resolve_dispatch` rather than routing through it. Every `find` in
/// `fn execute`'s no-`Code` rescue chain now comes through here.
///
/// Why the wave-1 name-only adapter and not [`crate::vm::resolve_dispatch`]
/// itself: these sites hold a class *name* (and in the receiver-walk case a
/// `ClassId` whose `&Class` borrow is already pinned by an in-scope
/// `class_manager` read guard), but never the resolved `&Method` — the whole
/// point of this chain is that the resolved method has **no** `Code`, so the
/// `&Method` §7 wants is the abstract declaration we are trying to get away
/// from. Re-resolving one just to satisfy the signature would add a
/// class-manager read lock plus a hierarchy walk per rescue. The adapter takes
/// the identical policy decision from the names, and `bytecode_available` is
/// the one §7 step-3 input the names cannot supply.
///
/// Cost is unchanged: `resolve_id` is the *same* single triple hash `find`
/// already paid (`resolve_id(..).and_then(callback_of)` is documented to equal
/// `find(..)`, descriptor-quirk fallback included), and the slot handle it
/// returns makes the §4 census increment one relaxed add instead of a second
/// hash. No allocation, no formatting; violation objects are built only on the
/// reject path, inside the adapter's `#[cold]` constructors.
///
/// Returns:
/// * `Ok(Some(cb))` — dispatch `cb`. The invocation is already counted.
/// * `Ok(None)` — no native registered, or (under `JdkOnly`) bytecode wins.
///   The caller continues down its existing fallback chain, exactly as it did
///   when `find` returned `None`.
/// * `Err(violation)` — `JdkOnly` refusal (§1.3). Surface it as
///   `VmError::JdkOnly`; never silently fall back to another implementation.
///
/// **`Compatible` mode is bit-for-bit today's behaviour.** `compat_native_wins`
/// is passed `true`, which is exactly the unconditional "a registered native
/// wins here" that the `find` call sites encoded, and in `Compatible` mode
/// `resolve_native_dispatch_wave1` is a pure function of that boolean.
///
/// ORCHESTRATOR — this function is the **single** point of coupling between
/// this file and agent E's `vm/src/vm/vm_exec.rs`. All five native-dispatch
/// sites in `fn execute` funnel through it, so if E's wave-1 surface lands with
/// a different shape, this body is the only thing to reconcile. It depends on
/// exactly three items, all re-exported through `crate::vm` by
/// `pub use vm_exec::*`:
///
/// * `dispatch_policy(&SharedVm) -> cratonvm_types::compat::ExecutionPolicy`
/// * `resolve_native_dispatch_wave1(policy, class_name, method_name,
///   descriptor, Option<(NativeCallback, NativeKind)>, compat_native_wins:
///   bool, bytecode_available: bool) -> Option<DispatchDecision<'static>>`
/// * `DispatchDecision::{Reject, native_callback}`
///
/// The contract's §7 `resolve_dispatch(policy, &Class, &Method, native)` is
/// deliberately not called here — see the paragraph above on why these sites
/// have no `&Method`. If E's name-only adapter is dropped, the replacement is
/// to re-acquire the `class_manager` read lock at each of the five sites and
/// call `resolve_dispatch` proper; that is a correctness-neutral but
/// measurably more expensive shape, which is why it was not done first.
#[inline]
pub(in crate::runtime::interpreter) fn resolve_native_for_dispatch(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    bytecode_available: bool,
) -> Result<Option<cratonvm_native_api::NativeCallback>, cratonvm_types::error::JdkOnlyViolation> {
    let registry = &shared.natives.native_methods;
    let id = match registry.resolve_id(class_name, method_name, method_descriptor) {
        Some(id) => id,
        None => return Ok(None),
    };
    let callback = match registry.callback_of(id) {
        Some(callback) => callback,
        None => return Ok(None),
    };
    // `kind_of_id` reports the slot's true kind. `find_with_kind` would report
    // `Bridge` on its descriptor-quirk cold path; the difference is invisible in
    // `Compatible` mode (every kind yields the same callback) and strictly more
    // accurate under `JdkOnly`.
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    match crate::vm::resolve_native_dispatch_wave1_in(
        &shared.natives.native_shadow_sink,
        crate::vm::DispatchDoor::NoCodeRescue,
        crate::vm::dispatch_policy(shared),
        class_name,
        method_name,
        method_descriptor,
        Some((callback, kind)),
        // JDK-ONLY-WAVE2 §10 — AUDITED 2026-08-06, not a live defect, and
        // deliberately still a constant.
        //
        // The `true` reproduces the pre-§7 "a registered native unconditionally
        // wins here" of the `find` calls this adapter replaced, and that is
        // faithful rather than lazy: `resolve_native_dispatch_wave1` uses the
        // flag only to choose between "the site preferred bytecode" and the
        // kind-based ladder, and this site never preferred bytecode. The
        // `NativeShadowsBytecode` observation a `false` would have produced is
        // still recorded — by the `Bridge if bytecode_available` arm one level
        // down — so nothing is lost from the report either.
        //
        // The record asks for "the real per-site compatibility verdict once
        // `force_native_over_real_jdk_bytecode` and the forced-native `String`
        // list are unified". That unification is §11's exercise, not this
        // site's: until those name lists collapse there is no per-site verdict
        // to read, and inventing one here would be a THIRD answer for methods
        // that already have two. Revisit when §11's Compatible-mode deletion
        // lands; under `JdkOnly` the chain is already not consulted.
        true,
        bytecode_available,
    ) {
        Some(crate::vm::DispatchDecision::Reject(violation)) => Err(violation),
        Some(decision) => match decision.native_callback() {
            Some(callback) => {
                // §4 census: one relaxed increment, no hashing, no allocation.
                // The acceptance criterion is *zero* synthetic-stub invocations
                // through **any** path, and an uncounted path is unverifiable.
                registry.record_invocation(id);
                Ok(Some(callback))
            }
            // `Bytecode` is unreachable from the name-only adapter, but treat it
            // as "no native" rather than assuming.
            None => Ok(None),
        },
        // JdkOnly, §7 step 3: concrete bytecode beats this bridge.
        None => Ok(None),
    }
}

/// The concrete class whose registered natives `execute`'s "Path B" borrows
/// when an interface method resolved to an abstract declaration and the
/// receiver matched no class in the store — or `""` when this interface has no
/// such stand-in.
///
/// Extracted from the `match` that used to sit inline at the Path B call site
/// (JDK-ONLY-WAVE2 §8) for one reason: the array refusal immediately after it
/// has to ask "is this one of the substituted interfaces?", and a second
/// hard-coded copy of these six names would be a list that can drift from the
/// list it is supposed to mirror. There is now exactly one copy, and
/// `g13_array_receiver_tests` pins it.
///
/// **Deliberately absent: `java/lang/Cloneable` and `java/io/Serializable`.**
/// They are the only two interfaces an array type actually implements
/// (JLS 4.10.3), they declare no methods, and nothing may map them here — that
/// absence is what makes "canonical is non-empty AND the receiver is an array"
/// a sound proof of `IncompatibleClassChangeError` rather than a heuristic.
pub(in crate::runtime::interpreter) fn canonical_concrete_for_interface(iface: &str) -> &'static str {
    match iface {
        "java/util/Set" | "java/util/Collection" => "java/util/HashSet",
        // Iterable has no collection shape of its own. Keep synthetic
        // List-style receivers on the established ArrayList bridge after
        // lambda proxies have already had a chance to dispatch their SAM
        // implementation.
        "java/lang/Iterable" => "java/util/ArrayList",
        "java/util/List" => "java/util/ArrayList",
        "java/util/Map" => "java/util/HashMap",
        "java/util/Iterator" => "java/util/HashMap$KeyItr",
        _ => "",
    }
}

thread_local! {
    /// The declared-method index the next [`execute`] of `ClassId` on this
    /// thread may use instead of its by-name scan
    /// (`CRATONVM_NATIVE_OWN_BODY_HINT`, interpreter round i3 wave 76, lane
    /// L4). Set by `vm_exec::invoke_special_bytecode_only_shared_hinted` right
    /// before it calls [`execute`], taken (cleared) by the first [`execute`]
    /// of a VM with the switch on. A hint left behind (an `execute` that
    /// returned before its lookup, or a VM with the switch off) is harmless:
    /// it is used only for the same class id and only when the index still
    /// names the `(name, descriptor)` being executed.
    pub(in crate::runtime::interpreter) static EXECUTE_METHOD_HINT: std::cell::Cell<Option<(ClassId, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Hand the next [`execute`] of `class_id` on this thread the method's
/// declared index (see `EXECUTE_METHOD_HINT`).
pub(crate) fn set_execute_method_hint(class_id: ClassId, method_index: u32) {
    EXECUTE_METHOD_HINT.with(|h| h.set(Some((class_id, method_index))));
}

/// Take this thread's pending method hint when it was left for `class_id`.
pub(in crate::runtime::interpreter) fn take_execute_method_hint(class_id: ClassId) -> Option<u32> {
    EXECUTE_METHOD_HINT
        .with(std::cell::Cell::take)
        .and_then(|(c, i)| (c == class_id).then_some(i))
}


/// The part of a method's `Code` attribute [`execute`] reads after it drops
/// the class-manager lock: the bytecode (an `Arc` view), the two sizes and
/// the exception table, already in the `Arc` a frame holds (interpreter round
/// i3 wave 77, lane L4). Field names are the `CodeAttribute` ones, so the
/// uses below read as before.
#[derive(Clone)]
pub(in crate::runtime::interpreter) struct ExecuteCode {
    pub(in crate::runtime::interpreter) max_stack: u16,
    pub(in crate::runtime::interpreter) max_locals: u16,
    pub(in crate::runtime::interpreter) code: cratonvm_reader::ByteView,
    pub(in crate::runtime::interpreter) exception_table: Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>,
}

/// The class's source file as [`execute`] hands it to the frame it pushes
/// (and to the eager compile's and the precise handler's method records).
/// Interpreter round i5 wave 114, lane L7 (`CRATONVM_EXECUTE_LEAN_REENTRY`,
/// default off): `Deferred` copies it only when one of those readers asks,
/// so a re-entry whose compiled body returns copies nothing; `Copied` is the
/// copy made under the class-manager guard, as before.
pub(in crate::runtime::interpreter) enum ExecuteSourceFile {
    Copied(Option<Arc<str>>),
    Deferred { class_id: ClassId, present: bool },
}

impl ExecuteSourceFile {
    /// The source file for a reader that keeps its own `Arc`: a refcount
    /// bump of the copy, or (deferred) a copy read now.
    pub(in crate::runtime::interpreter) fn get(&self, shared: &SharedVm) -> Option<Arc<str>> {
        match self {
            Self::Copied(file) => file.clone(),
            Self::Deferred { class_id, present } => {
                execute_deferred_source_file(shared, *class_id, *present)
            }
        }
    }

    /// The source file for the frame, the last reader.
    pub(in crate::runtime::interpreter) fn into_value(self, shared: &SharedVm) -> Option<Arc<str>> {
        match self {
            Self::Copied(file) => file,
            Self::Deferred { class_id, present } => {
                execute_deferred_source_file(shared, class_id, present)
            }
        }
    }
}

/// [`ExecuteSourceFile::Deferred`]'s copy: the class's `SourceFile` as it
/// stands now (`present`: the class had one under `execute`'s guard; without
/// one there is nothing to read). `read_recursive`: a reader may run with a
/// class-manager read guard of its caller's still held.
#[cold]
pub(in crate::runtime::interpreter) fn execute_deferred_source_file(
    shared: &SharedVm,
    class_id: ClassId,
    present: bool,
) -> Option<Arc<str>> {
    if !present {
        return None;
    }
    let cm = shared.classes.class_manager.read_recursive();
    cm.get_class(class_id)
        .and_then(|class| class.source_file.as_deref())
        .map(Arc::from)
}

/// The key of [`execute`]'s method-index memo (`JitRealm::execute_method_index`,
/// `CRATONVM_EXECUTE_METHOD_INDEX_MEMO`, interpreter round i6 wave 115, lane
/// L7, default off): the class id and an FxHash of the method's name and
/// descriptor. A hash, not the strings, so a probe allocates nothing; a
/// collision is harmless because a hit is used only when the declared method
/// at the stored index still has exactly this name and descriptor.
#[inline]
pub(in crate::runtime::interpreter) fn execute_method_index_key(class_id: ClassId, name: &str, descriptor: &str) -> (ClassId, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    name.hash(&mut hasher);
    descriptor.hash(&mut hasher);
    (class_id, hasher.finish())
}

/// File the declared index `position` [`execute`]'s by-name scan found under
/// `key` (see [`execute_method_index_key`]). Off the hot path: a memo miss.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn execute_note_method_index(shared: &SharedVm, key: (ClassId, u64), position: Option<usize>) {
    if let Some(index) = position.and_then(|i| u32::try_from(i).ok()) {
        shared.jit.execute_method_index.write().insert(key, index);
    }
}

/// `class.find_method(name, descriptor)` (the first declared method with that
/// name and descriptor; `class_id` is `class`'s id) through the per-VM
/// method-index memo (`JitRealm::execute_method_index`,
/// `CRATONVM_EXECUTE_METHOD_INDEX_MEMO`, interpreter round i6 wave 115, lane
/// L7): a filed index is used when the method there still has exactly that
/// name and descriptor (a class declares at most one method per name and
/// descriptor, so it is the method the scan would find); otherwise the scan
/// runs and files its answer. The caller decides whether the switch is on.
/// `pub(crate)` for the native re-entry doors in `vm_exec.rs`, which scan the
/// same classes before they reach [`execute`].
pub(crate) fn find_declared_method_memoized<'c>(
    shared: &SharedVm,
    class_id: ClassId,
    class: &'c crate::classloading::Class,
    name: &str,
    descriptor: &str,
) -> Option<&'c cratonvm_reader::method::ClassFileMethod> {
    let key = execute_method_index_key(class_id, name, descriptor);
    let filed = shared.jit.execute_method_index.read().get(&key).copied();
    if let Some(method) = filed
        .and_then(|index| class.methods.get(index as usize))
        .filter(|m| &*m.name == name && &*m.descriptor == descriptor)
    {
        return Some(method);
    }
    let position = class
        .methods
        .iter()
        .position(|m| &*m.name == name && &*m.descriptor == descriptor)?;
    execute_note_method_index(shared, key, Some(position));
    class.methods.get(position)
}


/// The background-compile arm of [`execute`]'s first-call JIT block
/// (`bg_compile`, default on): count the invocation on the canonical key every
/// door counts on and, at a stride boundary past the threshold, make sure the
/// worker runs and tell the tiered manager the real count. The caller then
/// interprets. Interpreter round i6 wave 117, lane L7: moved out of the eager
/// compile closure (whose 13 KiB frame every uncompiled `execute` paid) as a
/// pure move; the comments are the closure's.
#[inline]
pub(in crate::runtime::interpreter) fn execute_count_for_background_compile(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) {
    // The canonical key, shared with every other door that counts this
    // method. This used to be an open-coded 32-bit `31 * h` hash, under
    // which overloads merged.
    let invoc_key =
        cratonvm_jit_api::invoc_key_parts(class_id.as_u32(), method_name, method_descriptor);
    let n = shared.jit.profile_store.increment_invocation(invoc_key);
    let threshold = shared.config.jit_invocation_threshold;
    // The stride every dispatch door uses. This door had none and consulted
    // the manager on EVERY invocation past the threshold: a global mutex and
    // a freshly built key per call, for a method that may never compile.
    if should_offer_tier_up(n, threshold) {
        execute_offer_background_tier_up(
            shared,
            class_id,
            class_name,
            method_name,
            method_descriptor,
            n,
        );
    }
}

/// [`execute_count_for_background_compile`]'s stride-boundary offer, out of
/// line (rare: once per stride past the threshold).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn execute_offer_background_tier_up(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    n: u32,
) {
    ensure_bg_compiler_started(shared);
    let tiered_key = crate::jit::tiered::MethodKey::with_class_id(
        class_id,
        class_name,
        method_name,
        method_descriptor,
    );
    // Pass the REAL per-method invocation count: this hook only fires at
    // stride boundaries, and the manager's historical `+= 1` counting
    // deflated its hotness view 64x (first C1 recommendation at
    // ~threshold + 64×c1_threshold real calls).
    let recommended = shared
        .jit
        .tiered_manager
        .on_method_invocation_observed(&tiered_key, u64::from(n));
    // JIT round 21 lane reflect3/4 (page
    // `jr21-reflect3-bytecode-only-fallback-bodies-always-interpret`): a
    // RE-ENTRY-ONLY method (`JitCache::mark_reentry_only_row`) has no door
    // but this one: no caller compiles, binds or recompiles it. Its body is
    // published under exactly the key `execute` reads, so a miss there while
    // the manager holds the method above `Interpreter` is a lost body, never
    // one keyed elsewhere. MEASURED (`R21Reflect4Rows`, `--real-jdk`, ~1 run
    // in 10 under load): a trap evicted `BuiltinClassLoader.loadClassOrNull`'s
    // C1 body and demoted the method, then the C2 compile already in flight
    // finished `success` (it had read back the C1 body before the eviction)
    // and raised the tier to C2 with nothing published. The method was
    // settled, so it ran interpreted for the rest of the process. Demote it
    // the way the cold-body sweep does (`note_body_withdrawn`) and offer it
    // again. Once per retry stride, on this cold path only.
    if recommended.is_none()
        && shared
            .jit
            .jit_cache
            .is_reentry_only_key(class_name, method_name, method_descriptor)
        && shared.jit.tiered_manager.current_tier(&tiered_key)
            != crate::jit::tiered::CompilationTier::Interpreter
        && shared
            .jit
            .jit_cache
            .read()
            .get_for_reentry(class_name, method_name, method_descriptor, class_id)
            .is_none()
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] reentry-only body lost: {class_name}.{method_name}{method_descriptor} \
                 has no published body above Interpreter; demoted and offered again"
            );
        }
        shared.jit.tiered_manager.note_body_withdrawn(&tiered_key);
        let _ = shared
            .jit
            .tiered_manager
            .on_method_invocation_observed(&tiered_key, u64::from(n));
    }
}

/// `table` as the `Arc` an [`ExecuteCode`] keeps and the frame takes: one
/// copy, in either state of `CRATONVM_EXECUTE_SHARED_METHOD_VIEW` (a pure
/// refactor, wave 77). It used to be a `Vec` clone here (an allocation when
/// the method has handlers) and a second copy into the frame's `Arc` (always
/// one), so no state allocates more than before. (A shared empty table would
/// need a thread-local, which `vm/tests/per_vm_state_statics_ratchet.rs`
/// counts; an empty `Arc<[T]>` is one small header allocation.)
pub(in crate::runtime::interpreter) fn execute_exception_table(
    table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Arc<[cratonvm_reader::attribute::ExceptionTableEntry]> {
    Arc::from(table)
}

/// How [`execute`] names its method in the JIT gate's skip key and in the
/// frame it pushes (`CRATONVM_EXECUTE_SHARED_METHOD_VIEW`, interpreter round
/// i3 wave 77, lane L4, default off, `VmConfig::execute_shared_method_view`).
///
/// On (`shared`), every name is a refcount bump of the class's own
/// `Class::name` and of the method's own `ClassFileMethod::name` /
/// `descriptor` (`names`), and the frame takes the source file and exception
/// table `Arc`s `execute` built once under the lock. Off, every use makes the
/// fresh copy it always made. The text is the same either way; nothing
/// compares these names by address except the throwable-capture memo
/// (`stackwalker::MemoFrameMethod::Owned`), which then also matches two
/// frames of the same method at the same pc, as it does for cached frames.
pub(in crate::runtime::interpreter) struct ExecuteMethodView {
    pub(in crate::runtime::interpreter) shared: bool,
    pub(in crate::runtime::interpreter) names: Option<(Arc<str>, Arc<str>)>,
}

impl ExecuteMethodView {
    /// The class name for a skip key or a frame.
    #[inline]
    pub(in crate::runtime::interpreter) fn class_name(&self, class_name: &Arc<str>) -> Arc<str> {
        if self.shared {
            Arc::clone(class_name)
        } else {
            Arc::from(&**class_name)
        }
    }

    /// The method name (`text` is the same string).
    #[inline]
    pub(in crate::runtime::interpreter) fn method_name(&self, text: &str) -> Arc<str> {
        match &self.names {
            Some((name, _)) => Arc::clone(name),
            None => Arc::from(text),
        }
    }

    /// The method descriptor (`text` is the same string).
    #[inline]
    pub(in crate::runtime::interpreter) fn descriptor(&self, text: &str) -> Arc<str> {
        match &self.names {
            Some((_, descriptor)) => Arc::clone(descriptor),
            None => Arc::from(text),
        }
    }
}

/// What [`execute`] reads under the class-manager guard for the per-VM record
/// its frame is pushed from under `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME`
/// (interpreter round i3 wave 79, lane L7, default off,
/// `VmConfig::execute_cached_reentry_frame`; proposal
/// `i77-L4-proposal-a-native-re-entry-pushes-a-cached-frame-20261110`).
pub(in crate::runtime::interpreter) struct ExecuteReentryKey {
    /// The method's declared index in its class (`Class::methods`).
    pub(in crate::runtime::interpreter) index: u32,
    /// The method's own name and descriptor `Arc`s.
    pub(in crate::runtime::interpreter) name: Arc<str>,
    pub(in crate::runtime::interpreter) descriptor: Arc<str>,
    /// `class_redefinition_count()` under the guard the code was copied
    /// under (`CachedBytecodeMethod::pool_generation`).
    pub(in crate::runtime::interpreter) pool_generation: u64,
}

/// The per-VM record of `class_id`'s method `key.index`
/// (`JitRealm::execute_reentry_records`): the row when it was built from the
/// very `Code` bytes `code_attr` holds (a clone of the class's own view,
/// copied under the guard `key` was read under), else a fresh record built as
/// `dispatch_static.rs` `populate_invoke_cache` builds one, filed in place of
/// the old row. A redefinition, an EMCP reload or a synthetic class upgraded
/// in place gives the method another code allocation, so it always misses
/// here (`ExecuteReentryRecord::built_from`).
///
/// Off the dispatch loop: [`execute`] calls it once per entry, and only with
/// the switch on.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn execute_reentry_record(
    shared: &SharedVm,
    class_id: ClassId,
    key: ExecuteReentryKey,
    class_name: &Arc<str>,
    source_file: &ExecuteSourceFile,
    code_attr: &ExecuteCode,
    is_synchronized: bool,
    is_static: bool,
) -> Arc<CachedBytecodeMethod> {
    let slot = (class_id, key.index);
    if let Some(row) = shared.jit.execute_reentry_records.read().get(&slot) {
        if row.built_from(&code_attr.code) {
            return Arc::clone(&row.cached);
        }
    }
    let cached = Arc::new(
        CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: class_id,
            class_name: Arc::clone(class_name),
            method_name: Arc::clone(&key.name),
            method_descriptor: Arc::clone(&key.descriptor),
            // A refcount bump of the copy, or (`CRATONVM_EXECUTE_LEAN_REENTRY`)
            // the deferred copy, read only here, when the record is built.
            source_file: source_file.get(shared),
            // The per-method memo, as every frame of the method gets it.
            code: crate::runtime::frame::padded_bytecode_for_method(
                class_id,
                &key.name,
                &key.descriptor,
                &code_attr.code,
                &code_attr.exception_table,
            ),
            exception_table: Arc::clone(&code_attr.exception_table),
            max_stack: code_attr.max_stack,
            max_locals: code_attr.max_locals,
            // Cast: at most 255 parameters (JVMS §4.3.3).
            num_params: count_method_params(&key.descriptor) as u16,
            is_synchronized,
            is_static,
        })
        .with_pool_generation(key.pool_generation),
    );
    shared.jit.execute_reentry_records.write().insert(
        slot,
        crate::vm::realms::jit_realm::ExecuteReentryRecord {
            code_view: code_attr.code.clone(),
            cached: Arc::clone(&cached),
        },
    );
    cached
}


/// `CRATONVM_NO_EXECUTE_SYNC=1`: [`execute`] does not take an
/// `ACC_SYNCHRONIZED` method's monitor (the pre-wave-2 behaviour), and
/// `invoke_on_class_shared_inner` takes it around its bytecode tail instead —
/// it asks this, so the monitor is entered exactly once either way.
pub(crate) fn execute_sync_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| None::<std::ffi::OsString>.is_some())
}


/// t1 wave 11, lane INTA (`CRATONVM_FINALIZER_WITHDRAW_AT_CTOR_ENTRY_PUSH_FAILURE`):
/// [`execute`] raises `StackOverflowError` for a `<init>` before its frame
/// exists (the native-stack floor, the `EXEC_DEPTH` ceiling, the frame
/// limit), so the receiver (`args[0]`) never had its `Object.<init>` return
/// and HotSpot never registers it for finalization. Withdraws the
/// allocation-time registration
/// (`exception_dispatch::withdraw_at_ctor_entry_push_failure`). No safepoint
/// lies between `execute`'s entry and any of these trips, so `args[0]` is
/// still the object's address. A name compare for any other method.
#[inline]
pub(in crate::runtime::interpreter) fn withdraw_ctor_receiver_at_entry_trip(
    shared: &SharedVm,
    thread: &JvmThread,
    method_name: &str,
    args: &[Value],
) {
    if method_name != "<init>" {
        return;
    }
    if let Some(Value::Object(Some(receiver))) = args.first().copied() {
        exception_dispatch::withdraw_at_ctor_entry_push_failure(shared, thread, receiver);
    }
}

