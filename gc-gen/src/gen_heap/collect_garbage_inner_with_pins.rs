#[allow(unused_imports)]
use super::*;

impl GenerationalHeap {
    /// [`Self::collect_garbage_inner`], with the pinned in-place young copy's
    /// pin set injectable (gen r4w5/pinned5).
    ///
    /// `forced_pin_words` is for unit tests only: `Some(words)` runs the
    /// pinned copy with exactly those words on any cycle that would otherwise
    /// MOVE (no JIT frame, no ledger, no flag needed), and still applies the
    /// 1/8 bound. `None` is production: the words come from
    /// `gc_quiescence::pause_young_pin_words`, and only on a cycle that the
    /// conservative-JIT-root term would divert, under
    /// `CRATONVM_GEN_PINNED_YOUNG_COPY`.
    pub fn collect_garbage_inner_with_pins(
        &self,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn MonitorCleanup,
        forced_pin_words: Option<&[usize]>,
    ) -> (GcResult, Vec<usize>) {
        // gen r5w5/pin9: the last pinned in-place cycle's vacated spans answer
        // `young_vacated_probe` from that cycle's end until the next collection
        // starts, never longer (see `young_in_place_vacated`).
        self.young_in_place_vacated.lock().clear();
        // gch h4/wr: and so does the last true-root major's watched verdict.
        self.clear_true_root_watched_left_out();
        // DBG (CRATONVM_DBG_GCPAUSE): time every collection and report slow
        // ones — pairs with CRATONVM_DBG_PARKLAT to split the RRWL
        // crawl/join-stall between "GC pauses dominate" and "wake latency".
        //
        // gen r4w2/obs (2026-09-23): two consumers now. `print` is the
        // `[gcpause]` line (`CRATONVM_DBG=gcpause`, above
        // `CRATONVM_DBG_GCPAUSE_MIN_US`); `stash` hands the phase marks to the
        // VM's JFR emitter (`jdk.GCPhasePause`) through
        // `take_last_pause_phases`, and is armed only while a JFR recording
        // is running — the marks cost one `Instant::now()` per phase, so the
        // default path (neither on) is unchanged.
        struct PauseTimer {
            t0: Option<std::time::Instant>,
            print: bool,
            stash: bool,
        }
        impl Drop for PauseTimer {
            fn drop(&mut self) {
                if let Some(t0) = self.t0 {
                    let us = t0.elapsed().as_micros();
                    // Drain unconditionally: marks from a fast cycle must not
                    // survive to be reported against the next slow one.
                    let marks = moving_phase_marks_take();
                    let counts = moving_phase_counts_take();
                    // The report threshold, in the unit the marks are kept in:
                    // 100 ms unless `CRATONVM_DBG_GCPAUSE_MIN_US` says
                    // otherwise (`0` = every cycle).
                    if self.print && us >= u128::from(gc_flags().dbg_gcpause_min_us) {
                        eprintln!("{}", render_gcpause_line(us, &marks, &counts));
                    }
                    if self.stash {
                        last_pause_phases_store(us, marks);
                    }
                }
            }
        }
        let jfr_phases = cratonvm_jfr::phases_wanted();
        let _pause_timer = {
            use std::sync::OnceLock;
            static G: OnceLock<bool> = OnceLock::new();
            let print = *G.get_or_init(|| gc_flags().dbg_gcpause);
            PauseTimer {
                t0: (print || jfr_phases).then(std::time::Instant::now),
                print,
                stash: jfr_phases,
            }
        };
        let mv_phase_on = gc_flags().dbg_gcpause || jfr_phases;
        #[allow(unused_assignments, unused_variables)]
        let mut mv_last = std::time::Instant::now();
        macro_rules! mv_phase {
            ($name:expr) => {
                if mv_phase_on {
                    let now = std::time::Instant::now();
                    // MICROSECONDS (gengc-round2-core2): see
                    // `moving_phase_marks_push`. The rows used to be integer
                    // milliseconds, so a cycle of twenty sub-millisecond
                    // phases printed twenty `=0ms` rows and the table could
                    // not sum even when coverage was complete.
                    moving_phase_marks_push($name, now.duration_since(mv_last).as_micros());
                    mv_last = now;
                }
            };
        }
        // Fresh cycle: no old-gen reclamation has happened yet. Phase 5 below
        // sets this when the major GC runs (an in-place mark-sweep by default,
        // mark-compact only under `CRATONVM_OLDGEN_COMPACT`), and
        // `run_non_moving_young_cycle` does the same for the in-place old
        // sweep. See `gc_quiescence::OLD_GEN_RECLAIMED`.
        cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(false);
        // gcg g6/yg: a fresh cycle has no mark.
        let _ = self.take_escalation_mark();
        // Likewise fresh: this cycle has not refused yet. Cleared here rather
        // than by the public entry points so the non-moving divert below
        // (which returns straight out of `run_non_moving_young_cycle`) cannot
        // inherit a previous cycle's refusal.
        self.young_cycle_skipped.store(false, Ordering::Relaxed);
        // gc-common w36-d: and has resurrected nothing yet (the depth-2 set of
        // `take_resurrection_closure`), whichever door or divert it takes.
        self.resurrection_closure.lock().clear();
        // Phase 6 #1: spin-yield until every live `SafepointToken` has
        // dropped. While a kernel is reading a JVM array on the GPU, we
        // must not move that array — a single token held anywhere on
        // any thread defers this collection until it is released.
        // No-op when the `gpu-offload` feature is off.
        let _gpu_drain = cratonvm_gc_core::gpu_critical::wait_for_gpu_critical_drain();

        // Refresh the allocation / live-byte denominators the card-cost report
        // normalizes by. Here rather than at the end of the cycle because the
        // function has several exit paths (young-skip backstops, the non-moving
        // early return) and this is the one point every one of them passes
        // through — and because neither the young nor the old lock is held yet.
        self.publish_gc_metrics_occupancy();

        // SAFETY: If any thread is currently inside a JIT call, we MUST NOT
        // run a *moving* collection. JIT frames hold raw object pointers in
        // their spill slots / registers which are NOT precisely described by
        // an oop map — the GC sees them only through the *conservative*
        // stack scan (`conservative_roots::scan_active_jit_frames`). A Cheney
        // copy would relocate those objects but it cannot safely rewrite a
        // conservatively-discovered slot: a stack word that merely *looks*
        // like a heap pointer might actually be an `i64`, and rewriting it
        // would corrupt the mutator's data.
        //
        // Previously the collector simply *skipped* the cycle entirely while
        // any JIT frame was live. Because a busy program always has a JIT
        // frame on the stack, the young gen would fill and the process would
        // OOM (the user-visible "young gen exhausted" abort).
        //
        // The fix: when JIT frames are active, run a **non-moving**
        // mark-sweep of the young generation instead of skipping. A
        // non-moving collection never relocates an object, so every JIT
        // spill slot stays valid no matter what it points at. Marking is
        // precise enough — it starts from the full root set (interpreter
        // frames, statics, JNI handles, *and* the conservative JIT-frame
        // roots that the caller already folded into `roots`) plus the
        // old→young dirty-card references — and dead young objects are
        // reclaimed into the from-space free list. Conservative false
        // positives only over-retain; they can never free a live object.
        //
        // This reclaims memory while JIT frames are live and eliminates the
        // OOM. A precise compacting collection still runs once every JIT
        // call has returned (quiescence ends), so fragmentation introduced
        // by the non-moving sweep is transient.
        // DBG: CRATONVM_DBG_FORCE_MOVING forces the moving (Cheney) collection
        // even when quiescence says JIT frames are active — to test whether the
        // non-moving sweep (wedged on by a leaked JitEntryGuard) is the
        // heap-corruption source. UNSAFE if a JIT frame is genuinely live
        // (relocates JIT-held raw pointers); diagnostic only.
        let force_moving = gc_flags().dbg_force_moving;
        // Fix A (2026-06-05): under live JIT frames the CORRECT young collector is
        // the NON-MOVING sweep + selective promotion — now DEFAULT-ON (see
        // `selective_on` in `sweep_young_non_moving`), giving bt18 = 68332206 =
        // HotSpot. It marks conservatively (over-marking is safe) and drains by
        // PINNING conservative roots + tenuring only heap-interior nodes, so no
        // live make/check node is ever lost. The moving Cheney UNDER-COUNTS bt18
        // (the long-mislabelled "golden" 67674804): a semispace cannot pin a
        // conservative JIT root nor rewrite a register-resident one, so some live
        // nodes go stale after the swap. `CRATONVM_SHADOW_STACK` tried to make
        // moving safe via precise rewritable roots but is incomplete (68199090)
        // AND its push/reload codegen is incompatible with the non-moving sweep,
        // so it still routes to the moving Cheney here — it is the INFERIOR path;
        // the DEFAULT (no gate) is now the correct one. `CRATONVM_DBG_FORCE_MOVING`
        // also forces the (under-counting) moving cycle for diagnostics.
        // B-K kafka fix (PROTOTYPE): the shadow stack publishes the operand-stack
        // `Reg` oops the conservative stack scan misses (register-invisibility).
        // Previously `shadow_roots` forced the MOVING Cheney (to remap those oops
        // precisely), but moving UNDER-COUNTS bt18 (67674804) — the non-moving
        // sweep + card fix is the correct collector. Decouple: keep the NON-MOVING
        // sweep AND let the shadow oops be scanned as roots → PINNED (kept alive,
        // not relocated). The reload is then a no-op (pinned objects never move),
        // so the "incompatible with non-moving" concern does not apply. This keeps
        // bt18 = 68332206 while closing the kafka register-invisible reclamation.
        let _shadow_roots = cratonvm_types::flags().jit.shadow_stack;
        // Promotion-OOM avoidance: a MOVING (Cheney) young collection aborts the
        // PROCESS when it cannot relocate a survivor — old gen is full AND the
        // young to-space overflowed while promotion fell back to it (see the
        // `process::abort()` arms in `forward_object_impl` and
        // `gen_evac::ParEvac::evacuate`; the latter now tenures early on a
        // to-space overflow before it gives up). That can only
        // happen once the old generation can no longer absorb the surviving young
        // set. When old gen cannot hold a full young's worth of survivors, run
        // the NON-MOVING sweep instead: it never relocates (so it can never hit
        // that abort), reclaims any dead young in place, and simply leaves
        // un-promotable survivors in young (graceful `old_full = true`). A
        // genuinely exhausted heap then surfaces a *catchable*
        // `OutOfMemoryError` via the allocation paths / GC-overhead limit instead
        // of aborting. The non-moving sweep is already the default (and superior)
        // young collector while JIT frames are active, so this only widens when
        // it runs. Opt out with `CRATONVM_NO_GC_PROMOTION_GUARD` (reverts to the
        // moving collector, which may abort the process on a full heap).
        let promotion_oom_risk = !gc_flags().no_gc_promotion_guard && {
            // The moving collector's promotion abort needs BOTH generations
            // nearly full at once: old gen cannot absorb the aged survivors AND
            // the young to-space cannot hold the (then unpromotable) surviving
            // set. Gate on exactly that precondition (each ≥ 90% full). A large
            // young object over a near-empty old gen — e.g. the
            // `large_array_survives_gc` / `multiple_large_arrays_survive_gc`
            // tests, where young is big and old is ~empty — must still use the
            // moving (compacting) collector, so old-gen fullness is required too.
            let old_cap = self.old_gen_capacity();
            let young_cap = self.young_semi_capacity();
            old_cap > 0
                && young_cap > 0
                && (self.old_gen_used() as u128) * 10 >= (old_cap as u128) * 9
                && (self.young_from_used() as u128) * 10 >= (young_cap as u128) * 9
        };
        // A5 fix: `unregistered_jit_frame_on_stack()` — the VM root scan found a
        // guard-less JIT frame on the mutator's native stack (e.g. the compiled
        // entry-point `main` while a clinit/interpreted callee runs). Its live
        // objects were conservatively MARKED by the full-stack scan but cannot be
        // relocated (raw register/spill slots can't be rewritten), so run the
        // NON-MOVING sweep exactly as for a registered JIT frame (`is_active()`).
        let has_conservative_roots = cratonvm_gc_core::gc_quiescence::compiled_frames_live()
            || cratonvm_gc_core::gc_quiescence::conservative_peers_live();
        // System.gc() requests an old-gen-inclusive cycle. Route that cycle
        // through the non-moving marker so it can follow collection-overlay
        // edges from live owners instead of globally rooting every overlay.
        //
        // # gengc-round2-core2 (2026-09-20) — DO NOT remove this term without
        // # changing two other files in the same commit.
        //
        // `docs/feature-designs/generative-gc-architecture.md`
        // proposes deleting it from `divert_non_moving` (keeping it only as a
        // reason to run the OLD half), on the argument that the overlay
        // precision it protects is a claim about the old-gen marker and not
        // about whether the young half may copy. That analysis is incomplete,
        // and the missing half makes the change UNSAFE as written.
        //
        // This term is not a private policy of the collector. It is a PROMISE
        // PUBLISHED TO THE ROOT GATHERER, consumed before the collector runs:
        //
        //   * `gc_quiescence::young_marker_follows_side_tables()` returns
        //     `true` on `major_gc_requested()` alone — its own doc calls that
        //     arm "certain, and independent of moving-young" and states that
        //     "a false positive would DROP a live root". It is what lets
        //     `VmHeap::mirror_pin_deferrable` leave a class mirror OUT of the
        //     unconditional root set.
        //   * `vm::memory::roots::conditional_loader_metadata` asks the same
        //     question for loader metadata, and its Generational arm is
        //     `is_active() || major_gc_requested()`.
        //
        // Both are evaluated while roots are being gathered, i.e. BEFORE this
        // function decides anything. Remove `explicit_full_gc` here and a
        // `System.gc()` under moving-young would relocate against a root set
        // that was deliberately thinned on the promise that the cycle would
        // sweep in place — the `TestDefaultInstanceManager.testClassUnloading`
        // failure mode `young_marker_follows_side_tables`' own doc records
        // from the last time these two halves drifted apart.
        //
        // So the change needs the root-gatherer contract renegotiated first,
        // in `gc_quiescence.rs` and `vm/src/memory/roots.rs`, which this lane
        // does not own. The gap page is updated with this finding.
        //
        // gen r4w2/youngpolicy (2026-09-23): renegotiated, as an OPT-IN. All
        // three parties — this term, `young_marker_follows_side_tables` and
        // `conditional_loader_metadata` — now ask ONE predicate,
        // `gc_quiescence::explicit_full_gc_sweeps_young_in_place()`, which is
        // `major_gc_requested()` unless `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` is
        // set. Default: byte-for-byte the old behaviour. With the flag, the
        // root gatherer roots the side tables unconditionally on a
        // `System.gc()` and this term stops diverting, so the cycle may copy;
        // the request itself is still consumed by Phase 5 below (or by
        // `run_non_moving_young_cycle` if another term diverts), so the
        // old-generation half still runs.
        let explicit_full_gc = cratonvm_gc_core::gc_quiescence::explicit_full_gc_sweeps_young_in_place();
        // HIB-CV-22/32/33 ROOT FIX: only honor `promotion_oom_risk` as a reason
        // to divert into the non-moving sweep when there are un-rewritable
        // conservative JIT roots to protect. The non-moving sweep exists SOLELY
        // because a moving (Cheney) cycle cannot rewrite a JIT register/spill
        // slot it discovered conservatively. When NO JIT frame is on any stack
        // (`--nojit`, or a JIT-quiescent collection) there are no such roots:
        //   * the moving collector is then fully precise and correct, AND
        //   * its to-space is a fresh semispace the size of from-space, so the
        //     packed live survivor set always fits (promotion failure falls back
        //     to to-space) — the `process::abort()` that `promotion_oom_risk`
        //     was added to avoid is effectively unreachable on this path, and
        //   * the moving path runs the Phase-5 major GC (in-place by default) that the
        //     non-moving sweep's early return SKIPS, relieving the very old-gen
        //     pressure that keeps `promotion_oom_risk` latched on (otherwise the
        //     heap wedges in non-moving mode under load — exactly when the bug
        //     bites).
        // Meanwhile the non-moving sweep, run on this no-conservative-root path,
        // relies on conservative over-marking it does NOT have, exposing a
        // precise-root/remap gap that reclaims a still-live young object — the
        // boxed `java.lang.Byte` (HIB-CV-32), the JUnit `AtomicBoolean`
        // (HIB-CV-22), and the SessionFactory oop (HIB-CV-33). `FORCE_MOVING`
        // makes all three disappear, confirming the moving path is clean here.
        // Opt out (restore the old broad diversion) with
        // `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`.
        let honor_promotion_oom_risk =
            promotion_oom_risk && (has_conservative_roots || gc_flags().promotion_oom_guard_broad);
        // Moving young gen — the collector-side half of the decision.
        //
        // When moving-young is in effect the JIT publishes a COMPLETE
        // rewritable precise root map (shadow stack) for every live frame and
        // the conservative scan is suppressed (see roots.rs), so a live JIT
        // frame no longer forces the non-moving sweep — run the moving (Cheney)
        // cycle instead.
        //
        // arch-2026-07-26 (`moving-young-precise-roots`): this block used to
        // contain a SECOND, independent opt-in gate:
        //
        //     let fail_closed_non_moving =
        //         is_active() && !gc_flags().allow_moving_young;
        //
        // ORed unconditionally into `divert_non_moving`. Because
        // `gc_quiescence::is_active()` is true whenever ANY thread holds a live
        // JIT frame — i.e. in every steady-state workload once the
        // 500-invocation JIT threshold trips — that term made the young
        // generation stop being a copying collector the moment the JIT engaged.
        // Worse, it did NOT consult `moving_young_requested`, so
        // `CRATONVM_MOVING_YOUNG=1` on its own could never run a moving cycle
        // under a live JIT frame, which is the ONLY case the feature exists
        // for; `CRATONVM_ALLOW_MOVING_YOUNG` had to be set as well. Both the
        // term and the flag are DELETED, not defaulted: a flag whose sole job
        // is to permit correct behaviour is not a safety mechanism.
        //
        // What that change left was two diversions:
        //
        //   1. `has_conservative_roots && !moving_young` — when moving-young is
        //      NOT in effect, a live JIT frame's roots are discovered
        //      conservatively (a stack word that merely looks like a pointer
        //      may be an `i64`), so they can be marked but never rewritten and
        //      the collector must not relocate. The original NEW-1.5 rule.
        //      **Widened 2026-09-21 to plain `!moving_young`** — see "TERM 0"
        //      at the `divert_non_moving` expression below. The
        //      `has_conservative_roots` conjunct meant the documented opt-out
        //      did nothing on a process with no compiled frame, which is the
        //      shape of every small bisection repro.
        //   2. `honor_promotion_oom_risk` — the genuine safety fallback: with
        //      BOTH generations ~full the moving path can `process::abort()` on
        //      a promotion failure, so that cycle takes the abort-free sweep.
        //
        // Everything else that used to divert became a per-cycle COVERAGE PROOF
        // published by the VM's root gatherer, and its failure is reported at
        // `warn` level by default rather than counted behind a flag.
        //
        // gengc-round1-core (2026-09-20) — THE LIST ABOVE IS NO LONGER THE
        // WHOLE LIST, and a reader who stops there will mis-attribute the
        // measured fallback rate. `divert_non_moving` has FIVE terms today:
        //
        //   3. `divert_for_incomplete_moving_coverage` — the coverage proof
        //      itself, folded into the OR below (it is the proof's verdict, not
        //      an extra policy).
        //   4. `unrewritable_conservative_jit_roots` (2026-09-06) — the
        //      Cheney-cannot-honour-a-pin term added for the QDox SIGSEGV. It
        //      is `has_conservative_roots && conservative_jit_scans() > 0`,
        //      i.e. "any thread is in compiled code AND somebody ran a
        //      conservative scan this cycle", and its own comment records that
        //      the count "is positive whenever anything is compiled". So in a
        //      JIT-warm process it reinstates the pre-2026-07-26 blanket rule
        //      that item 1 was written to replace, whatever the coverage proof
        //      says. It is also why `record_moving_young_cycle()` below is
        //      effectively unreachable in production — see
        //      `docs/feature-designs/default-moving-young-gen.md`.
        //   5. `explicit_full_gc` — EVERY `System.gc()`, unconditionally, with
        //      no reference to JIT frames or to the coverage proof. A workload
        //      built out of `System.gc()` calls therefore never compacts young.
        //      See `docs/feature-designs/generative-gc-architecture.md`.
        //
        // Terms 4 and 5 are the two that dominate the `[moving-young] fallback`
        // rate `docs/gc-tuning.md` names as this backend's throughput problem.
        // Neither is removed here: 4 is backed by a measured 3/3 SIGSEGV repro
        // and 5 by an overlay-precision argument, and narrowing either needs a
        // measurement this session could not take.
        //
        // gengc-round3 (2026-09-21) — THERE ARE SIX. The list above is written
        // as a list of things that STOP a copy, and it silently assumed the
        // operator's own request was already one of them. It was not: see
        // "TERM 0" at the `divert_non_moving` expression, added because
        // `CRATONVM_NO_MOVING_YOUNG` was measured to have no effect at all on a
        // process with no live compiled frame.
        let moving_young_requested = cratonvm_gc_core::gc_quiescence::moving_young_enabled();
        let force_non_moving_jit_roots = cratonvm_gc_core::gc_quiescence::force_non_moving_jit_roots();
        // An unregistered JIT frame (A5 — a compiled frame live WITHOUT a
        // `JitEntryGuard`, e.g. the compiled entry-point `main` while a
        // clinit/interpreted callee runs) is not reachable from the JIT entry
        // chain, so it publishes no shadow homes and no per-safepoint map: its
        // oops are conservatively marked but un-rewritable. That is an
        // incomplete coverage proof under moving-young exactly as much as it is
        // a conservative-root condition on the legacy path. Fold it in here so
        // the moving branch can never relocate behind such a frame even if a
        // future root-scan caller forgets to mark the cycle.
        let coverage_incomplete = cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete()
            || cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack()
            // The one take-over licence (gc-common w3-g): a Cheney copy cannot
            // honour a pin set, so any frozen peer / helper window / unread
            // peer is non-moving here -- the same decision `XT_TAKEOVER` already
            // produced, now from the predicate G1 and ZGC read too.
            || cratonvm_gc_core::gc_quiescence::takeover_forbids_unpinnable_move();
        let divert_for_incomplete_moving_coverage =
            moving_young_requested && (force_non_moving_jit_roots || coverage_incomplete);
        let moving_young = moving_young_requested && !divert_for_incomplete_moving_coverage;
        // A device DMA against the heap arena that the bounded GPU
        // critical-section wait could not outlast. Nothing may move this
        // cycle, and not even the debug force-moving flag overrides it: the
        // veto is a correctness fact, not a policy. See
        // `vm_heap::gpu_relocation_forbidden`.
        let gpu_relocation_forbidden = self.external_gpu.gpu_relocation_forbidden();
        // A CHEENY COPY CANNOT HONOUR A PIN (2026-09-06).
        //
        // G1 and ZGC answer a conservatively-discovered JIT root by PINNING it
        // -- G1 excludes its region from the collection set, ZGC withholds its
        // page from the relocation set. A Cheney copy has no such move: every
        // live object in from-space is copied, so the only way to honour a pin
        // is not to copy at all. That is exactly what this collector did until
        // the cross-thread JIT coverage handshake (2026-08-23) let the moving
        // cycle run while peers are in JIT -- and the deposit paths' own
        // comment still asserted the old rule ("they can only over-retain: the
        // young sweep runs non-moving while any thread is in JIT, so nothing
        // is relocated") for a year of code that no longer honoured it.
        //
        // Term 1 above (`!moving_young` since 2026-09-21; it was
        // `has_conservative_roots && !moving_young` when this comment was
        // written) covers the legacy path. Under moving-young the cycle proceeds on the per-cycle
        // COVERAGE PROOF instead -- and that proof is about the frames the JIT
        // ENTRY CHAIN names, while a conservative scan of the same threads'
        // native stacks finds a strictly larger set: `is_object_address`
        // accepts any word that looks like a heap object base, so a raw `long`
        // or an interior pointer can present as a root, and the slot it came
        // from may not be a slot at all. Relocating one of those and rewriting
        // only the map-named slots leaves the raw word naming evacuated,
        // zeroed space -- measured as a SIGSEGV inside compiled
        // `java/lang/StringUTF16.compress` on the QDox 4-thread repro, 3 runs
        // out of 3, with the faulting address inside a span the collector had
        // just decommitted.
        //
        // `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` restores the pre-2026-09-06
        // behaviour for a one-binary A/B.
        // The test is "this cycle was handed ANY conservatively-discovered JIT
        // root", not "one of them lands in young-from".
        //
        // The narrower young-from test was measured and is NOT enough: it took
        // the QDox 4-thread repro from 3/3 SIGSEGV to 1/3, and one crash in
        // three is not a fix. The registry is over- AND under-approximate at
        // once -- `is_object_address` admits any word that looks like an
        // object base, and drops interior/derived pointers that name no base
        // at all -- so "no pin is in from-space" does not imply "nothing in
        // from-space is named by an un-rewritable word". G1 and ZGC absorb
        // that gap because they pin at REGION / PAGE granularity, where a
        // neighbour's pin covers the miss; an all-or-nothing per-cycle
        // decision has no such slack, so it has to key on the presence of the
        // population rather than on a per-address test inside it.
        //
        // Nor is it "the published pin set is non-empty": that was measured
        // too, and gave 3/3 SIGSEGV again. The set is EMPTY on cycles that
        // still relocate under a live compiled frame -- a peer whose last
        // deposit found no JIT frames publishes an empty vector and its entry
        // is removed, and the initiator's own scan can be a no-op -- so
        // "no pins" does not mean "no un-rewritable roots", it means the
        // registry could not see them.
        //
        // What the term keys on instead: a conservative JIT scan RAN this
        // cycle, and a compiled frame is live. `conservative_jit_scans()` is
        // reset by `begin_moving_young_coverage_cycle` and bumped by every
        // publication INCLUDING an empty one, which is exactly the "this
        // thread looked" fact its own doc says the set cannot hold. The
        // conjunct is not decoration: it is what separates a real collection
        // (every mutator deposits on its way into the pause, so the count is
        // positive whenever anything is compiled) from a cycle where nobody
        // scanned -- a unit test driving the collector directly, or
        // `CRATONVM_GC_PRECISE_ONLY_ROOTS=1`, where the operator has asserted
        // that the precise maps are the whole root set and there is no
        // conservative population to protect.
        //
        // gcd d1/e (2026-09-27): the ledger's verdict is kept apart
        // (`term4_ledger_cleared`) because a cycle option B lets through must
        // still pin the blocked peers' interior native-stack words, which the
        // ledger does not hold; see `plan_takes_blocked_peer_words`.
        let term4_population = moving_young
            && !gen_no_peer_pin_divert()
            && has_conservative_roots
            && cratonvm_gc_core::gc_quiescence::conservative_jit_scans() > 0;
        // gcj j1pn ledger item 2: option B decides "empty" over the words that
        // name from-space as allocated now, not over both semispaces' capacity.
        let term4_ledger_cleared = term4_population && {
            let from_screen = {
                let from = self.lock_young_from();
                (from.base_ptr() as usize, from.used())
            };
            cratonvm_gc_core::gc_quiescence::young_pin_ledger_clears_term4(Some(from_screen))
        };
        let unrewritable_conservative_jit_roots = term4_population && !term4_ledger_cleared;
        // TERM 0 — THE OPERATOR ASKED FOR NO COMPACTION (2026-09-21, round 3).
        //
        // `!moving_young` — not `has_conservative_roots && !moving_young`. The
        // conjunct was the defect: with moving-young switched OFF and no live
        // compiled frame anywhere, NO term of this OR fired, so the cycle fell
        // through to the Cheney copy. The collector had already computed
        // `moving_young_requested = false` and recorded it on the decision, and
        // then relocated anyway. Measured on `BinT 10` at `-Xmx256m` under
        // `CRATONVM_DBG_GC_STRESS=250000`: 1512 of 1512 cycles `kind=moving`,
        // `reason=moving-no-jit-frames-live`,
        // `(moving_young_requested=false jit_active=false)`. See
        // `docs/feature-designs/default-moving-young-gen.md`.
        //
        // The override was SAFE — with zero JIT frames there are no
        // unrewritable conservative roots, so relocating is sound. Safety was
        // never what was broken. What was broken is that
        // `CRATONVM_NO_MOVING_YOUNG` (alias `CRATONVM_GC=-moving-young`) is
        // documented in `types/src/flags.rs:560` and `docs/GC.md` as THE
        // supported compatibility opt-out, and is the only bisection lever this
        // backend offers for a suspected relocation defect. An operator on a
        // short repro — anything that has not tiered up, which is exactly the
        // shape people bisect with — set it, saw byte-identical behaviour, and
        // concluded relocation was not the cause. That is a false negative
        // produced by the instrument meant to establish the fact.
        //
        // Widening the term to plain `!moving_young` makes "a cycle relocates
        // only while `moving_young` is true" structural rather than emergent.
        // It subsumes `divert_for_incomplete_moving_coverage` (which is one of
        // the two ways `moving_young` goes false) — that term is KEPT below
        // because it still has to be named separately in the reason chain and
        // it is what arms `record_moving_young_coverage_fallback`.
        //
        // `CRATONVM_DBG_FORCE_MOVING` still overrides this, at the gate below,
        // and says so on the decision line (`moving-forced-by-debug-flag`). A
        // debug flag beating a compatibility flag is fine as long as the
        // report cannot be mistaken for the compatibility flag having worked.
        let divert_non_moving = !moving_young
            || unrewritable_conservative_jit_roots
            || honor_promotion_oom_risk
            || divert_for_incomplete_moving_coverage
            || explicit_full_gc
            || gpu_relocation_forbidden;
        // gen r4w5/pinned5 (2026-09-24): THE PINNED IN-PLACE YOUNG COPY — one
        // guarded branch, deliberately outside term 4's own predicate (which
        // lane pinwords5 may narrow independently).
        //
        // `CRATONVM_GEN_PINNED_YOUNG_COPY`: when term 4 is the ONLY reason
        // this cycle would divert, ask the pause's pin ledger for every
        // unrewritable conservative word in young (interior and derived words
        // included) and, if it is complete and names at most 1/8 of
        // from-space's pages, run the moving branch below in pinned mode
        // instead of the sweep: the objects on those pages stay put, every
        // other survivor is copied. A ledger that is incomplete, or pins too
        // much, diverts exactly as before, under its own reason code. See the
        // `InPlaceEvac` block near the top of this file.
        //
        // `forced_pin_words` (unit tests) enters the same way on a cycle that
        // would move anyway.
        let mut pinned_young_words: Option<Vec<usize>> = None;
        // gcj hmg: the pinned copy took this cycle through the take-over or
        // the unproven-frame arm, whose ledger stands in for compiled frames
        // with no proven oop map (`pc_in_place_admitted` below).
        let mut pinned_ledger_stands_in = false;
        // gcj j1/pn item 1: the subset of the ledger read's words that came
        // from a layout-free band, a register or a peer band (empty on the
        // forced path, whose words carry no source).
        let mut pinned_layout_free_words: Vec<usize> = Vec::new();
        let mut pinned_decline: Option<u8> = None;
        {
            use cratonvm_gc_core::gc_metrics::decision_reason as dr;
            // gcj j1/yg: the conjunction is `term4_offer_verdict` (identical
            // with `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK` off), and
            // every term-4 cycle is counted by the first term that kept it
            // from the copy (`[GC] young_term4_offer:`): the reason chain
            // below labels all of them `nonmoving-unrewritable-conservative-
            // jit-roots`, so the histogram cannot say why the copy never saw
            // them. Two relaxed adds per term-4 cycle, none otherwise.
            let term4_alone = unrewritable_conservative_jit_roots && {
                let (offered, slot) = term4_offer_verdict(
                    honor_promotion_oom_risk,
                    divert_for_incomplete_moving_coverage,
                    explicit_full_gc,
                    gpu_relocation_forbidden,
                    self.pinned_copy_takes_promotion_risk,
                );
                self.term4_offer_census[T4O_CYCLES].fetch_add(1, Ordering::Relaxed);
                if offered {
                    self.term4_offer_census[T4O_OFFERED].fetch_add(1, Ordering::Relaxed);
                }
                if let Some(slot) = slot {
                    self.term4_offer_census[slot].fetch_add(1, Ordering::Relaxed);
                }
                offered
            };
            // gcd d3/m, THE TAKE-OVER ARM (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`,
            // opt-in, with the pinned copy's own flag): term 3 fired, and ONLY
            // through the take-over licence -- a pause whose unrewritable peers
            // are all helper windows the pass pinned whole, i.e. the licence a
            // pinning backend gets is `MovePinned`
            // (`gc_quiescence::takeover_verdict_admits_pinned_copy`). The Cheney
            // copy still cannot honour that pin set; the pinned copy can, from
            // the pause's ledger, into which the pass read every such window's
            // band raw (bases included). Nothing else may divert: see
            // `pinned_copy_takes_takeover_cycle`. A decline diverts exactly as
            // without the flag, under the coverage reason, and is counted in
            // `pinned_takeover_census`, never in `pinned_young_census` (whose
            // term-4 identities it would break). Proposal
            // `gcd-d2h-proposal-pinned-copy-takes-helper-window-cycles-DONE-20260928`.
            //
            // gcd d5/s: every cycle that reaches the arm's test is counted in
            // `pinned_takeover_declines` (slot 0), and a decline under the
            // FIRST condition that failed (`pinned_takeover_decline_reason`),
            // so a run printing `ptko_candidates=0` says why.
            let takeover_arm = forced_pin_words.is_none()
                && divert_for_incomplete_moving_coverage
                && gc_flags().gen_pinned_young_copy
                && cratonvm_gc_core::gc_quiescence::pinned_young_copy_takeover_enabled()
                && {
                    let other_coverage_incomplete =
                        cratonvm_gc_core::gc_quiescence::moving_young_coverage_incomplete()
                            || cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack();
                    // gci i11/tk: no `oldgen_compact_enabled()` term -- the
                    // Phase 5 veto (`phase5_major_no_compact`) keeps this
                    // cycle's major in place; see `takeover_other_divert_terms`.
                    let other_divert_terms = takeover_other_divert_terms(
                        honor_promotion_oom_risk,
                        explicit_full_gc,
                        gpu_relocation_forbidden,
                    );
                    let verdict = cratonvm_gc_core::gc_quiescence::takeover_verdict();
                    let take = pinned_copy_takes_takeover_cycle(
                        moving_young_requested,
                        force_non_moving_jit_roots,
                        other_coverage_incomplete,
                        other_divert_terms,
                        verdict,
                    );
                    self.pinned_takeover_declines[takeover_decline::REACHED]
                        .fetch_add(1, Ordering::Relaxed);
                    if !take {
                        if let Some(slot) = pinned_takeover_decline_reason(
                            moving_young_requested,
                            force_non_moving_jit_roots,
                            other_coverage_incomplete,
                            other_divert_terms,
                            verdict,
                        ) {
                            self.pinned_takeover_declines[slot].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    take
                };
            if takeover_arm {
                self.pinned_takeover_census[0].fetch_add(1, Ordering::Relaxed);
            }
            // gcj j1/yg item 1, THE UNPROVEN-FRAME ARM
            // (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`): term 3 fired
            // for a frame whose words the ledger holds (an unregistered or
            // unpublished compiled frame, a peer the ledger must account
            // for, a pinned take-over window) and for nothing else; see
            // `PINNED_COPY_UNPROVEN_REASONS`. Counted in
            // `pinned_unproven_census` like the take-over arm's own.
            let unproven_arm = !takeover_arm
                && forced_pin_words.is_none()
                && divert_for_incomplete_moving_coverage
                && gc_flags().gen_pinned_young_copy
                && pinned_copy_takes_unproven_frames_enabled()
                && {
                    let takeover_forbids =
                        cratonvm_gc_core::gc_quiescence::takeover_forbids_unpinnable_move();
                    pinned_copy_takes_unproven_cycle(
                        moving_young_requested,
                        force_non_moving_jit_roots,
                        cratonvm_gc_core::gc_quiescence::moving_young_incomplete_reason_mask(),
                        takeover_other_divert_terms(
                            honor_promotion_oom_risk,
                            explicit_full_gc,
                            gpu_relocation_forbidden,
                        ),
                        takeover_forbids,
                        takeover_forbids
                            && cratonvm_gc_core::gc_quiescence::takeover_verdict_admits_pinned_copy(
                                cratonvm_gc_core::gc_quiescence::takeover_verdict(),
                            ),
                    )
                };
            if unproven_arm {
                self.pinned_unproven_census[0].fetch_add(1, Ordering::Relaxed);
            }
            // gcf f2/y (`CRATONVM_GEN_PINNED_COPY_DEFRAG`, opt-in): a term-4
            // cycle copies only when the previous sweep left young
            // fragmented; otherwise it sweeps, under term 4's own reason, as
            // without the pinned copy. The take-over arm and the forced
            // (unit-test) path are not gated.
            let defrag_admits = if self.pinned_copy_defrag
                && forced_pin_words.is_none()
                && gc_flags().gen_pinned_young_copy
                && term4_alone
            {
                let pending = self.young_defrag_pending.swap(false, Ordering::Relaxed);
                self.pinned_copy_defrag_census[if pending { 2 } else { 3 }]
                    .fetch_add(1, Ordering::Relaxed);
                pending
            } else {
                true
            };
            let candidate = match forced_pin_words {
                Some(_) => !divert_non_moving,
                None => {
                    (gc_flags().gen_pinned_young_copy && term4_alone && defrag_admits)
                        || takeover_arm
                        || unproven_arm
                }
            };
            if candidate {
                let mut words: Vec<usize> = Vec::new();
                let complete = match forced_pin_words {
                    Some(w) => {
                        words.extend_from_slice(w);
                        true
                    }
                    // gcj j1/yg item 1: the unproven-frame arm's read
                    // tolerates exactly the reasons the arm took.
                    // gcj j1/pn item 1: tagged, so the plan can tell a
                    // layout-free / register word from a compiled-band one.
                    None => cratonvm_gc_core::gc_quiescence::pause_young_pin_words_tagged(
                        &mut words,
                        &mut pinned_layout_free_words,
                        unproven_arm.then_some(PINNED_COPY_UNPROVEN_REASONS),
                    ),
                };
                if !complete && takeover_arm {
                    self.pinned_takeover_census[2].fetch_add(1, Ordering::Relaxed);
                } else if !complete && unproven_arm {
                    self.pinned_unproven_census[2].fetch_add(1, Ordering::Relaxed);
                } else if !complete {
                    self.pinned_young_census[3].fetch_add(1, Ordering::Relaxed);
                    // gch h2/yg: the reason the raw-JNI-locals page asks to
                    // measure first (the read itself does not say which of its
                    // fail-closed terms fired). Production only: the forced
                    // path is always complete.
                    if cratonvm_gc_core::gc_quiescence::raw_jni_locals_open() {
                        self.pinned_ledger_incomplete_raw_jni
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    pinned_decline = Some(dr::NON_MOVING_PIN_LEDGER_INCOMPLETE);
                } else {
                    let (from_lo, from_used) = {
                        let from = self.lock_young_from();
                        (from.base_ptr() as usize, from.used())
                    };
                    // gcj j1pn ledger item 2: production words are screened
                    // to from-space before the census and the 1/8 bound are
                    // charged with them (to-space and above-top words name
                    // nothing a cycle moves). The forced (unit-test) words
                    // are taken as given.
                    if forced_pin_words.is_none()
                        && cratonvm_gc_core::gc_quiescence::young_pin_from_space_screen_enabled()
                    {
                        cratonvm_gc_core::gc_quiescence::screen_young_pin_words_to_from_space(
                            &mut words, from_lo, from_used,
                        );
                    }
                    // gen r4w6/pinstale6: the bound is taken over the ledger's
                    // words PLUS every blocked-peer native-stack word in
                    // from-space. The plan (after the object-start walk) pins
                    // the NON-BASE subset of the latter, so this is an
                    // over-estimate and the plan can never pin more pages than
                    // were checked here. Production only: the forced
                    // (unit-test) path supplies its own words and must not
                    // mix in the capture buffer (per VM since gc-common w18-c,
                    // the pause ledger's `stack_slots`; an unbound test thread
                    // reads its own empty fallback).
                    let mut bound_words = words.clone();
                    if forced_pin_words.is_none() {
                        let from_end = from_lo.saturating_add(from_used);
                        cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut bound_words, |v| {
                            v >= from_lo && v <= from_end
                        });
                    }
                    let pages = pinned_young_pages(&bound_words, from_lo, from_used);
                    let over = pinned_pages_over_bound(pages.len(), from_used);
                    if over && takeover_arm {
                        self.pinned_takeover_census[3].fetch_add(1, Ordering::Relaxed);
                    } else if over && unproven_arm {
                        self.pinned_unproven_census[3].fetch_add(1, Ordering::Relaxed);
                    } else if over {
                        self.pinned_young_census[2].fetch_add(1, Ordering::Relaxed);
                        pinned_decline = Some(dr::NON_MOVING_PINNED_PAGES_OVER_BOUND);
                    } else {
                        if takeover_arm {
                            self.pinned_takeover_census[1].fetch_add(1, Ordering::Relaxed);
                        }
                        if unproven_arm {
                            self.pinned_unproven_census[1].fetch_add(1, Ordering::Relaxed);
                        }
                        pinned_ledger_stands_in = takeover_arm || unproven_arm;
                        pinned_young_words = Some(words);
                    }
                }
            }
        }
        // A decline diverts even on the forced (test) path, so the bound is
        // exercised exactly as production takes it.
        let divert_non_moving =
            (divert_non_moving || pinned_decline.is_some()) && pinned_young_words.is_none();
        if watchref_dbg() {
            eprintln!(
                "[watchref] collect_garbage_inner: has_conservative_roots={has_conservative_roots} moving_young_requested={moving_young_requested} divert_non_moving={divert_non_moving} force_moving={force_moving}"
            );
        }
        if divert_non_moving
            && (!force_moving || divert_for_incomplete_moving_coverage || gpu_relocation_forbidden)
        {
            if divert_for_incomplete_moving_coverage {
                // Warn-level and ON BY DEFAULT (see the function's doc): a
                // silent slide back to the non-moving sweep is the failure mode
                // this whole item exists to make impossible.
                cratonvm_gc_core::gc_quiescence::record_moving_young_coverage_fallback();
            }
            // Ground truth for the collector-decision report. Recorded from the
            // branch that actually decides, so `gc_metrics::
            // collector_decision_report()` can never drift from the code the
            // way `docs/GC.md` and `ARCHITECTURE.md` drifted from each other
            // (see `tlab-and-card-audit.md` §3).
            //
            // The arms below are ordered by ACTIONABILITY, not by the order of
            // the terms in `divert_non_moving`: when several terms hold at once
            // the reason reported is the one an operator has to fix FIRST to
            // get a moving cycle back. `divert_for_incomplete_moving_coverage`
            // leads because it is the only one that also names an
            // `unproven` sub-reason, and because it is the one term that
            // survives `force_moving`. `explicit_full_gc` is last because a
            // `System.gc()` is a request, not a defect. (The comment here used
            // to claim the two orders matched; they never have, and a reader
            // checking the claim against the code would have concluded one of
            // the two was wrong.)
            {
                use cratonvm_gc_core::gc_metrics::decision_reason as dr;
                let (reason, unproven) = if let Some(code) = pinned_decline {
                    // gen r4w5/pinned5: term 4 fired alone and the pinned copy
                    // declined (incomplete ledger / over the 1/8 bound).
                    (code, cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE)
                } else if divert_for_incomplete_moving_coverage {
                    (
                        dr::NON_MOVING_COVERAGE_INCOMPLETE,
                        cratonvm_gc_core::gc_quiescence::moving_young_incomplete_reason(),
                    )
                } else if unrewritable_conservative_jit_roots {
                    // gen r4w2/youngpolicy (2026-09-23): its own code. Tested
                    // BEFORE the legacy arm because `has_conservative_roots`
                    // holds in both; this one needs `moving_young`, the legacy
                    // one `!moving_young`, so the two are disjoint and the
                    // order only matters for readability.
                    (
                        dr::NON_MOVING_UNREWRITABLE_CONSERVATIVE_JIT_ROOTS,
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                    )
                } else if has_conservative_roots && !moving_young {
                    (
                        dr::NON_MOVING_CONSERVATIVE_JIT_ROOTS,
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                    )
                } else if honor_promotion_oom_risk {
                    (
                        dr::NON_MOVING_PROMOTION_OOM_RISK,
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                    )
                } else if gpu_relocation_forbidden && !explicit_full_gc {
                    (
                        dr::NON_MOVING_GPU_CRITICAL,
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                    )
                } else if explicit_full_gc {
                    (
                        dr::NON_MOVING_EXPLICIT_FULL_GC,
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                    )
                } else {
                    // TERM 0 alone: the operator set `CRATONVM_NO_MOVING_YOUNG`
                    // (alias `CRATONVM_GC=-moving-young`) and nothing else about
                    // this cycle would have diverted it. Reachable only with
                    // `!moving_young_requested`: `moving_young` is
                    // `moving_young_requested && !divert_for_incomplete_moving_
                    // coverage`, and that second term is the FIRST arm above, so
                    // by here `moving_young == moving_young_requested`.
                    //
                    // The arms above are deliberately left ahead of this one.
                    // When a real hazard (a near-full heap, a GPU critical
                    // section, `System.gc()`) would have forced the sweep
                    // anyway, naming the hazard is worth more to the reader than
                    // repeating a setting they typed themselves.
                    //
                    // Its own code since gen r4w2/youngpolicy (2026-09-23). It
                    // was recorded as `nonmoving-conservative-jit-roots` as an
                    // interim, which named conservative JIT roots on cycles
                    // that had none (`jit_active=false
                    // unregistered_jit_frame=false` printed right beside it).
                    // See `docs/feature-designs/generative-gc-architecture.md`.
                    (
                        dr::NON_MOVING_YOUNG_COMPACTION_DISABLED,
                        cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                    )
                };
                cratonvm_gc_core::gc_metrics::record_collector_decision("generational", reason, unproven);            }
            tracing::debug!(
                "running non-moving young-gen mark-sweep (jit_active={}, \
                 unregistered_jit_frame={}, promotion_oom_risk={}, honored={}, moving_young={}, \
                 force_non_moving_jit_roots={}) — compaction deferred.",
                cratonvm_gc_core::gc_quiescence::is_active(),
                cratonvm_gc_core::gc_quiescence::unregistered_jit_frame_on_stack(),
                promotion_oom_risk,
                honor_promotion_oom_risk,
                moving_young,
                force_non_moving_jit_roots,
            );
            // gen r4w4/young4 (2026-09-24): size the prize for a narrower term
            // 4. Only on the cycles term 4 decided (it implies `moving_young`,
            // so the coverage proof passed and nothing ahead of it in the
            // reason chain fired); one snapshot of the pin registry and one
            // young lock per such cycle, none otherwise. See
            // `ConservativeDivertSample`.
            if unrewritable_conservative_jit_roots {
                let (lo, hi) = {
                    let from = self.lock_young_from();
                    let lo = from.base_ptr() as usize;
                    (lo, lo + from.used())
                };
                let pins = cratonvm_gc_core::gc_quiescence::pinned_jit_roots_snapshot();
                self.note_conservative_divert(ConservativeDivertSample::from_pins(
                    &pins,
                    lo,
                    hi,
                    cratonvm_gc_core::gc_quiescence::unrewritable_jit_root_count() > 0,
                ));
            }
            // PROMOTE-ON-PRESSURE ACROSS A NON-MOVING CYCLE (gen r4w2/youngpolicy,
            // 2026-09-23). The arm's contract is "the NEXT young cycle tenures
            // every survivor"; only the moving copy consumes it, and the sweep
            // below never reads it, so without this an arm set by moving cycle
            // N survives any run of non-moving cycles and tenures the whole
            // survivor set of an unrelated moving cycle much later. By default
            // the arm is still CARRIED (behaviour unchanged) and the carry is
            // recorded so the consumer can count a stale consumption;
            // `CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES` drops it here instead. The
            // three refusal exits further down keep it armed either way: a
            // refused cycle is the same cycle, retried. See
            // `docs/feature-designs/generative-gc-architecture.md`.
            //
            // gen r4w4/young4 (2026-09-24): WIRED, and the carry is gone. The
            // non-moving cycle IS "the next minor GC", so it consumes the arm
            // here and honours it: the sweep's selective-promotion walk tenures
            // every unpinned survivor regardless of age, through the same
            // `should_tenure` slot its defragmentation escalation already uses
            // (pin-by-value keeps that safe; the age gate was never part of the
            // safety argument). A sweep that cannot promote this cycle
            // (`selective_on` false) drops the arm and counts it, rather than
            // carrying it to an unrelated moving cycle later. So an arm can no
            // longer outlive the cycle after the one that set it, whichever
            // collector runs that cycle; `consumed_stale` stays 0 by
            // construction and is kept as the regression detector.
            // gce e2/o: the bisection lever that dropped the arm at the divert
            // instead (`CRATONVM_GC_PROMOTE_PRESSURE_EXPIRES`) is removed; the
            // census slot it fed (`promote_pressure_census[3]`) stays 0.
            let promote_pressure_on_sweep = self.force_promote_all.swap(false, Ordering::Relaxed);
            return self.run_non_moving_young_cycle(
                roots,
                finalizer_addrs,
                monitors,
                promote_pressure_on_sweep,
            );
        }
        // The moving arm of the decision record (see the non-moving arm above).
        // The three cases are genuinely different claims and an operator needs
        // to tell them apart: "nothing was compiled, so relocation is trivially
        // safe" is not evidence that the per-cycle coverage proof works, and a
        // `CRATONVM_DBG_FORCE_MOVING` cycle is not evidence of anything at all.
        //
        // DECIDED HERE, RECORDED LATER (2026-09-21). Both of these used to fire
        // on this line, which is upstream of all THREE ways this cycle can
        // still refuse to run — to-space cannot cover from-space, the
        // object-start walk did not complete, and the T-3 reserved-TLAB-tail
        // tripwire. So a heap that refused every single young cycle published
        // `moving_cycles_total=N` and `moving_cycles_under_live_jit=N` for N
        // cycles that relocated nothing at all, which is the same "a number
        // that means the instrument fired, read as a result it did not
        // produce" defect the refusal's own reason code was added to close.
        // The recording site is now immediately past the last refusal gate:
        // search for `moving_decision_reason`.
        let count_moving_young_cycle = moving_young && has_conservative_roots;
        let moving_decision_reason = {
            use cratonvm_gc_core::gc_metrics::decision_reason as dr;
            if divert_non_moving {
                // We are here despite `divert_non_moving` — only `force_moving`
                // gets you past the branch above.
                dr::MOVING_FORCED_BY_DEBUG_FLAG
            } else if has_conservative_roots {
                dr::MOVING_WITH_PROVEN_JIT_COVERAGE
            } else {
                dr::MOVING_NO_JIT_FRAMES
            }
        };

        // The previous cycle's off-pause wipe must be complete before this
        // cycle reads or writes the arena it covers (it is this cycle's
        // to-space). Normally long finished: the wipe takes tens of
        // milliseconds and cycles are seconds apart.
        self.join_evacuated_wipe();
        let mut young_from = self.lock_young_from();
        let mut young_to = self.young_to.lock();
        let mut old_gen = self.old_gen.lock();
        old_gen.note_collection_prologue();
        debug_assert_young_arena_is_low_only(&young_from, "collect_garbage_inner/from");
        debug_assert_young_arena_is_low_only(&young_to, "collect_garbage_inner/to");
        // gcd d4/m: gcd d3/m consumed the fragmentation request here on every
        // ledger-pinned cycle (`arm_fragmentation_compaction` +
        // `take_fragmentation_compaction_request`) to keep Phase 5 from sliding
        // old gen. That was not what the sweep it replaces does: the sweep's
        // `major_trigger_decision` ARMS the pending request and runs an old-gen
        // collection for it (declining only the compaction), so the consumption
        // also suppressed the fragmentation-triggered old sweep on every pinned
        // cycle. Removed; the compaction-only veto belongs to Phase 5 (lane n):
        // `gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words-FIXED-20260928`.

        // ---- Cheney invariant: to-space must be able to hold ALL of from-space.
        //
        // The end-of-cycle expansion further down grows only `young_to`, which
        // at that point is the arena that was just reset. After the NEXT swap
        // that grown arena becomes FROM-space while its sibling is still at the
        // old capacity, so the two semispaces end up a full doubling apart. A
        // cycle in which more than to-space's worth of data survives then ran
        // out of destination mid-copy, and `forward_object` had nowhere to put
        // the object:
        //
        //   FATAL: OutOfMemoryError: GC could not relocate a live object -
        //   young to-space is full (tried 65568 bytes, to-space has
        //   16724264/16777216 used).
        //
        // ...followed by `std::process::abort()`. That killed the VM
        // un-catchably where HotSpot throws a perfectly ordinary
        // `OutOfMemoryError`. Reproduced with a 25-line probe under `-Xmx64m`,
        // with `[youngstate] arena-grow 16777216 -> 33554432` one cycle earlier
        // naming the asymmetry exactly.
        //
        // Equalise here instead of at the growth site: `young_to` was reset at
        // the end of the previous cycle so it is empty and growing it is legal
        // (`Arena::grow` asserts precisely that, because `Vec::resize` may
        // reallocate), and the target is a capacity the sibling arena already
        // holds - so it is within `max_young_semi_size` by construction and
        // needs no re-clamping. With this in place `young_to.capacity() >=
        // young_from.capacity() >= young_from.used()`, and since every
        // destination byte this cycle can spend comes from a from-space object
        // (promotion to old gen only ever REMOVES demand), the copy phase can
        // no longer run out of to-space.
        if young_to.used() == 0 && young_to.capacity() < young_from.capacity() {
            let target = young_from.capacity();
            tracing::debug!(
                to_capacity = young_to.capacity(),
                from_capacity = target,
                "GC: equalising young semispaces before the copy phase"
            );
            young_to.grow(target);
        }

        // Backstop for the same failure. If to-space still cannot cover
        // from-space (only reachable if the equalisation above was skipped
        // because to-space was somehow non-empty), SKIP this young collection
        // rather than start a copy that cannot finish. Over-retaining for one
        // cycle is recoverable - the allocation slow path spills to old gen,
        // and if there is genuinely no memory left it raises a *catchable*
        // `OutOfMemoryError` through `alloc_object_shared`'s normal ladder.
        // Aborting mid-copy is not recoverable by anyone. Same shape, and the
        // same "nothing destructive has happened yet" reasoning, as the
        // object-start-walk skip further down.
        let to_headroom = young_to.capacity().saturating_sub(young_to.used());
        if to_headroom < young_from.used() {
            tracing::warn!(
                to_headroom,
                from_used = young_from.used(),
                "GC: young to-space cannot cover from-space - skipping this young \
                 collection (over-retain; the allocator raises a catchable OOM if \
                 memory is genuinely exhausted)"
            );
            // gengc-round1-core: make "retried next cycle" mean a NEXT cycle.
            // Nothing about the arena changed, so without a floor `needs_gc`
            // is still true and the very next allocation re-enters this pause.
            self.note_skipped_young_cycle(&young_from);
            // 2026-09-21: and make it VISIBLE. This is the third refusal arm,
            // and until now it was the one that moved no counter at all — the
            // 2026-09-20 round closed the object-start-walk and T-3 arms and
            // did not see this one, which returns from a different place with
            // the same all-zero `GcResult`. `verbose_gc_end` already named it
            // in the same breath as the other two ("to-space cannot cover
            // from-space, or the object-start walk could not complete"), so
            // the omission read as deliberate. It was not.
            cratonvm_gc_core::gc_metrics::record_collector_decision(
                "generational",
                cratonvm_gc_core::gc_metrics::decision_reason::SKIPPED_YOUNG_TO_SPACE_UNDERSIZED,
                cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
            );
            return (
                GcResult {
                    stats: cratonvm_gc_core::gc::GcStats {
                        objects_copied: 0,
                        bytes_copied: 0,
                        bytes_freed: 0,
                    },
                    pointer_map: cratonvm_types::PointerMap::default(),
                },
                Vec::new(),
            );
        }
        // CRATONVM_DBG_STALE_OBJREF: only locked/touched below when the flag
        // is set (see the `quarantine` field's doc comment); an uncontended
        // lock of an unused, zero-capacity arena otherwise.
        let mut quarantine = self.quarantine.lock();

        // Publish current (pre-collection) region bounds for the lock-free
        // `is_object_address` used during this cycle's marking/scanning. The
        // young arenas may swap/grow later in this function; the matching end
        // refresh republishes the post-collection bounds.
        self.store_region_bounds_locked(&young_from, &young_to, &old_gen);

        // bc math-ec 0x4 seed-phase bisect (CRATONVM_DBG_SEEDHUNT): count
        // `Object(Some(0<p<0x1000))` slots in old gen at GC ENTRY. Compared
        // against the post-Cheney and post-major counts below to localize the
        // collector path that SEEDS the `0x4`. See the helper docs above.
        let mut sh_printed: usize = 0;
        let sh_cap: usize = 40;
        let sh_entry_old: usize = if seedhunt_enabled() {
            let mut c = 0usize;
            for (op, _) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let h = unsafe { &*(op as *const ObjectHeader) };
                c += seedhunt_scan_obj(op, h, "entry", "O", &mut sh_printed, sh_cap);
            }
            c
        } else {
            0
        };

        // Fold any BUFFERED card offsets into the bitmap before the dirty-card
        // scan. Today this is an empty-registry walk plus an empty-queue
        // check: the mutator barrier (`write_barrier` →
        // `CardTable::mark_dirty_lockfree`) writes the card byte directly and
        // buffers nothing, and has since gc-genpause F5.2 (gen r4w2, 2026-09-23
        // — this comment used to describe mutators flushing a thread-local
        // buffer "on the way in" to the safepoint, which none does). Kept as
        // the STW fold point for a future buffered or refinement-thread
        // barrier; `flush_all` is sound only here, with every mutator stopped.
        self.card_table.flush_all();
        self.card_table.drain_pending();
        let card_table: &CardTable = &self.card_table;

        // ---- DBG (bc math-ec): pre-GC remembered-set audit ----------------
        // For each old-gen object, check every old->young reference edge
        // against the card bitmap (post flush+drain). A CLEAN card on a live
        // old->young edge is a write-barrier / remembered-set MISS: the young
        // referent will be skipped by scan_dirty_cards and reclaimed though
        // still reachable — the FixedPointTest premature-reclamation corruption
        // (ECCurve$Fp/ECFieldElement$Fp fields decaying to ZEROED/OFF-HEAP).
        // Caught at GC entry, BEFORE reclamation, with the offending old obj.
        if gc_flags().dbg_rset_audit {
            let mut edges = 0usize;
            let mut misses = 0usize;
            let mut reported = 0usize;
            for (obj_ptr, _sz) in old_gen.walk_objects() {
                // SAFETY: `walk_objects` yields the start of each live old-gen object,
                // so `obj_ptr` targets a valid, fully-initialized `ObjectHeader`.
                let hdr = unsafe { &*(obj_ptr as *const ObjectHeader) };
                let addr = obj_ptr as usize;
                // gen r4w4/cards4: the card is decided PER EDGE, not per holder.
                // With `CRATONVM_GC_PRECISE_ARRAY_CARDS` on (the default) a store
                // into a reference array dirties the ELEMENT's card, so reading
                // only the header card reported every such edge as a MISS that
                // the dirty-card scan in fact delivers. See
                // `rset_edge_card_is_dirty` and
                // `docs/feature-designs/generative-gc-architecture.md`.
                // stw-residual-close FIX (2026-07-23): this used to hand-walk
                // `0..hdr.num_slots()` as legacy 16-byte `Value` cells, which is
                // WRONG for compact-ref-layout objects (num_slots/slot stride
                // differ under `GC_FLAG_COMPACT` — see compact-ref-field-layout
                // design doc) and reliably walked off the object's real body
                // into unmapped memory, SIGSEGVing ~1 boot in 6 under a WildFly
                // campaign. `for_each_ref_slot` is the unified array/compact/
                // legacy-aware helper already used by `scan_all_old_to_young`
                // just above — reuse it here instead of re-deriving field
                // offsets by hand.
                // SAFETY: `obj_ptr`/`hdr` are a valid, fully-initialized old-gen
                // object/header pair from `old_gen.walk_objects()`.
                unsafe {
                    for_each_ref_slot(obj_ptr, hdr, |raw, slot_idx| {
                        let p = raw as usize;
                        if p != 0 && p < 0x1000 {
                            let cn = cratonvm_gc_core::gc::resolve_class_info(hdr.class_id.as_u32())
                                .map(|(n, _)| n)
                                .unwrap_or_else(|| "<unresolved>".to_string());
                            eprintln!(
                                "[small4] PRE-GC OLD {} @0x{:x} fld[{}] -> 0x{:x}",
                                cn, addr, slot_idx, p,
                            );
                        }
                        if young_from.contains(raw) {
                            edges += 1;
                            let (dirty, card_idx) =
                                rset_edge_card_is_dirty(card_table, addr, hdr, slot_idx);
                            if !dirty {
                                misses += 1;
                                if reported < 60 {
                                    reported += 1;
                                    let cn = cratonvm_gc_core::gc::resolve_class_info(hdr.class_id.as_u32())
                                        .map(|(n, _)| n)
                                        .unwrap_or_else(|| "<unresolved>".to_string());
                                    eprintln!(
                                        "[rset-miss] OLD {} @0x{:x} fld[{}] -> young 0x{:x} CLEAN(card={})",
                                        cn, addr, slot_idx, p, card_idx,
                                    );
                                }
                            }
                        }
                    });
                }
            }
            if edges > 0 {
                eprintln!(
                    "[rset-audit] old->young edges={} clean-card MISSES={}",
                    edges, misses,
                );
            }
            // Pre-GC linear scan of young_from for small-payload object refs
            // (mutator/native-written 0x4). If found here, the 0x4 PRE-EXISTS
            // the GC (it is NOT created by the collector). Linear walk may
            // truncate on a bad header (logged) — but the bump-allocated
            // young space is contiguous so it usually reaches everything.
            //
            // stw-residual-close FIX (2026-07-23): gated behind its OWN flag,
            // separate from `CRATONVM_DBG_RSET_AUDIT` above. Unlike the
            // `young_object_starts` walk further below (hardened 2026-07-16
            // for cce0079 with explicit free-list/TLAB-gap skipping) or
            // `old_gen.walk_objects()` used by the audit above (which owns
            // its own object-start bookkeeping), THIS walk has no such
            // protection: it advances purely by `gen_object_total_size`, so
            // landing on a stale TLAB-tail/free-block byte pattern that
            // happens to parse as a plausible-but-wrong header desyncs it
            // for the rest of the arena. Three rounds of narrower fixes
            // (compact-ref-layout-aware field reads, then a num_slots
            // plausibility cap) each closed one crash shape a WildFly
            // campaign found, but the walk remains unsound in general — it
            // would need the same start-set hardening as the walk below to
            // be truly safe, which is out of scope for a "was there ever a
            // 0x4 in young-gen" diagnostic nobody currently depends on.
            // Splitting the flag keeps `RSET_AUDIT`'s actually-load-bearing
            // old→young remembered-set check (fixed and verified above)
            // usable without inheriting this walk's fragility.
            if gc_flags().dbg_rset_audit_young_scan {
                let ybase = young_from.base_ptr_mut() as usize;
                let yused = young_from.used();
                let mut ycur = 0usize;
                let mut found4 = 0usize;
                while ycur < yused {
                    // SAFETY: the young space is bump-allocated and contiguous; `ycur < yused`
                    // keeps `ybase + ycur` inside the live region, where a valid `ObjectHeader`
                    // begins (a bad header is detected by the size check below).
                    let h = unsafe { &*((ybase + ycur) as *const ObjectHeader) };
                    let size = gen_object_total_size(h);
                    // stw-residual-close FIX (2026-07-23): this walk (unlike the
                    // free-list/TLAB-gap-aware `young_object_starts` walk further
                    // below, hardened 2026-07-16 for the cce0079 desync) has no
                    // protection against landing mid-object on a stale/leftover
                    // TLAB-tail header: a garbage-but-plausible `num_slots` (e.g.
                    // ~200K, observed) still produces a `size` that fits within
                    // the remaining `yused` budget, so the existing `size == 0 ||
                    // ycur + size > yused` guard doesn't catch it, and the field
                    // loop below then walks `num_slots` slots off the end of the
                    // real (much smaller) object into unmapped memory — SIGSEGV.
                    // No real class has anywhere near this many declared fields;
                    // cap `num_slots` the same way `set_field`/`get_field` cap
                    // theirs against corrupt headers, then bail the whole
                    // best-effort walk rather than trust a header that fails it.
                    let implausible_slots =
                        h.kind() == ObjectKind::Object && h.num_slots() as usize > 4096;
                    if size == 0 || ycur + size > yused || implausible_slots {
                        eprintln!(
                            "[small4] young walk truncated at off={} used={} implausible_slots={}",
                            ycur, yused, implausible_slots,
                        );
                        break;
                    }
                    let optr = (ybase + ycur) as *mut u8;
                    // stw-residual-close FIX (2026-07-23): same compact-ref-layout
                    // gap as the old-gen audit above (`for_each_ref_slot` handles
                    // array/compact/legacy uniformly instead of hand-deriving a
                    // legacy-only 16-byte stride, which walked off compact
                    // objects' real bodies and SIGSEGV'd). The one-shot HEXDUMP is
                    // dropped: with three different offset semantics (byte offset
                    // for compact, slot index for legacy, element index for
                    // arrays) a single "corrupt payload" marker can no longer be
                    // computed generically, and the victim/size boundaries alone
                    // (still printed via the eprintln above it) are what mattered
                    // for the "smear vs surgical" triage this was added for.
                    // SAFETY: `optr`/`h` are a valid, fully-initialized young-gen
                    // object/header pair (the `size`/`yused` bounds were checked
                    // above before entering this iteration).
                    unsafe {
                        for_each_ref_slot(optr, h, |raw, slot_idx| {
                            let p = raw as usize;
                            if p != 0 && p < 0x1000 && found4 < 40 {
                                found4 += 1;
                                let cn = cratonvm_gc_core::gc::resolve_class_info(h.class_id.as_u32())
                                    .map(|(n, _)| n)
                                    .unwrap_or_else(|| "<unresolved>".to_string());
                                eprintln!(
                                    "[small4] PRE-GC YOUNG {} @0x{:x} fld[{}] -> 0x{:x}",
                                    cn,
                                    ybase + ycur,
                                    slot_idx,
                                    p,
                                );
                            }
                        });
                    }
                    ycur += size;
                }
            }
        }
        // -------------------------------------------------------------------

        let bytes_before = young_from.used();
        // Conservative JIT/native stack scans can produce aligned interior
        // addresses inside young objects. Build an exact pre-forwarding object
        // start set so only allocator-written headers can ever receive a
        // forwarding pointer; an interior false positive must merely be ignored.
        //
        // cce0079 ROOT FIX (2026-07-16): this walk previously assumed a
        // contiguous bump-allocated young space and BREAK'd at the first
        // implausible header. But young_from legitimately contains free-list
        // and TLAB gap ranges (zeroed, headerless) — under WildFly's
        // ~40-thread `parallel-extension-add` the walk reliably hit one a few
        // MB in and silently dropped EVERY later young object from the start
        // set. `forward_object_impl` treats "not in the start set" as a
        // conservative interior word and returns the address UNMOVED — for
        // every root (precise roots and native pins included) and every
        // scanned reference slot — so entire swaths of live young objects
        // were never evacuated and every reference to them dangled into
        // recycled memory after the semispace swap. That is the mechanism
        // behind the WildFly boot `ClassCastException: java.lang.Object
        // cannot be cast to X` / stale-ObjectRef family (canary-confirmed
        // live via the CRATONVM_DBG_STALE_OBJREF quarantine ring: the walk
        // warning printed seconds before stale reads surfaced in
        // native_map_get bucket contents and EnhancedQueueExecutor node
        // fields). Walk the same free-list-aware grid as the non-moving
        // exact walk (`skip_free_blocks`); if the walk STILL cannot complete
        // (genuinely corrupt header), the moving collector is unsound this
        // cycle — divert to the non-moving sweep, which tolerates a partial
        // view (conservative over-marking, never relocates).
        // gc-genpause F0: split the object-start walk out of `pre_evacuate`.
        //
        // `pre_evacuate` was the whole interval from the top of this function
        // to the card scan -- the safepoint-token spin, the arena locks, the
        // bounds publish, the card-buffer drain AND the exact from-space
        // object-start walk, under one name. It measured 110-152 ms of a
        // ~229 ms steady-state pause, and there was no way to tell which part
        // of that list it was. Attributing half a pause to a phase mark that
        // wide is a hypothesis, not a measurement (`READ where a diagnostic's
        // value is PRODUCED`), and the walk is the thing anyone would try to
        // optimise. So it gets its own mark and its own size counter.
        mv_phase!("pre_walk");
        let young_base = young_from.base_ptr() as usize;
        let young_used = young_from.used();
        // One bit per 8 bytes of from-space rather than one hash-table entry
        // per object. The set's size tracks the young space, not the live set,
        // so on bt18 at -Xmx8g the old `FxHashSet` made `HashMap::insert` +
        // `reserve_rehash` 49% of the whole process and one moving cycle cost
        // ~6.1 s. See `ObjectStartBits`.
        let mut young_object_starts =
            cratonvm_gc_core::young_mark::ObjectStartBits::new(young_base, young_used);
        // This cycle's refusals only — see `forward_refusal_reason`. This
        // heap's ledger (gcd d3/m), not a process one.
        self.forward_refusals.reset();
        let mut start_walk_complete = true;
        // T-3: set when a reserved TLAB tail is published inside THIS
        // from-space on the moving path — see the escalation note below.
        let mut moving_with_reserved_tails = false;
        // Merge the free-block list with un-retired TLAB tails (reserved,
        // never on the free list — the exact gap class this walk was
        // breaking on) — the same skip set the non-moving exact walk builds
        // via its local `merge_skips`. Both inputs are ascending & disjoint.
        let start_skips = {
            let mut v = young_from.free_blocks_sorted();
            let tails = self.jit_tlab_skip_offsets(young_base, young_base + young_used);
            // TLAB AUDIT TRIPWIRE (tlab-and-card-audit.md §1, defect
            // T-3). We are on the MOVING path: from-space is about to be
            // evacuated, swapped and reset. A published reserved tail means
            // some ALIVE thread still owns a TLAB `[cursor, end)` in the arena
            // being reset, and that thread is not one of the OS-frozen in-JIT
            // peers — freezing a peer makes the VM call
            // `mark_moving_young_coverage_incomplete`, which would have sent
            // this cycle down the non-moving path instead. So the owner is a
            // parked or blocked mutator that reached its exclusion point
            // WITHOUT retiring, and after the swap its stale cursor bump-
            // allocates into memory the collector has already handed back.
            //
            // The walk itself is correct either way (the tail is skipped, not
            // parsed); the hazard is entirely AFTERWARDS, when the owner
            // resumes and bump-allocates from a cursor into memory this cycle
            // handed back.
            //
            // T-3 ESCALATION (2026-07-31). This was a rate-limited warn and
            // nothing else, on the argument that "diverting a live moving cycle
            // on a heuristic would be a worse trade than naming the offending
            // transition". Two things make the refusal the better trade now:
            //
            //  * It is not a heuristic. `jit_tlab_skip_offsets` has already
            //    clipped the published tails to `[young_base, young_base +
            //    young_used)` — this exact from-space. A non-empty result IS
            //    the hazard condition, not a proxy for it.
            //  * It is expected to be unreachable. Per the transition table in
            //    `tlab-and-card-audit.md` §1.2 every alive thread
            //    retires at its exclusion point, and the one intentional
            //    un-retired case (an OS-frozen in-JIT peer) makes the VM call
            //    `mark_moving_young_coverage_incomplete`, which sends the cycle
            //    down the non-moving path before it ever gets here. So the
            //    refusal costs nothing on a correct transition graph and
            //    prevents a use-after-free on an incorrect one.
            //
            // Refusing means `start_walk_complete = false`, which skips this
            // young collection entirely (see the block after the walk):
            // over-retain for one cycle, spill to old gen, retry on the next
            // trigger — by which point the owner has almost certainly retired.
            // That is strictly safer than diverting to the non-moving sweep,
            // which on this precise-root path lacks the conservative
            // over-marking it needs.
            //
            // The warn stays, and stays rate-limited, because its occurrence
            // count is the reproducer budget for finding the producing
            // transition (audit §5 item 6). The FIRST occurrence is always
            // visible.
            if !tails.is_empty() {
                static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 8 || n.is_power_of_two() {
                    tracing::warn!(
                        tails = tails.len(),
                        "[tlab-audit] a MOVING young collection was about to run with {} \
                         published reserved TLAB tail(s) inside from-space. No in-JIT peer was \
                         frozen (that would have forced the non-moving sweep), so an alive \
                         mutator left a TLAB un-retired across its exclusion point — after the \
                         semispace swap its cursor would point into recycled memory. REFUSING \
                         this young collection (over-retain; retried next cycle). Occurrence \
                         #{}.",
                        tails.len(),
                        n,
                    );
                }
                moving_with_reserved_tails = true;
            }
            v.extend(tails);
            v.sort_by_key(|&(off, _)| off);
            v
        };
        // ----- gc-genpause F1: the parallel attempt ---------------------
        //
        // The walk below is a sequential header chase over ALLOCATED bytes.
        // It measured ~120 ms of a ~229 ms steady-state minor pause, against
        // ~29 ms for the Cheney copy it precedes -- the largest phase of the
        // collection, and the one phase whose cost tracks how much garbage was
        // produced rather than how much survived.
        //
        // The non-moving sweep -- the FALLBACK path -- already solved exactly
        // this: split the chain at anchors the ALLOCATOR supplied rather than
        // ones a walk has to rediscover, walk the chunks in parallel, and
        // prove each chunk by requiring its chain to land exactly on the next
        // anchor. The moving path, default since 2026-07-30, had none of it.
        // This is that machinery, applied to the walk that now runs by
        // default; `objstart_chunk` is the per-chunk walker and
        // `parallel_objstart_walk` is the driver, both modelled on
        // `sweep_chunk` / `parallel_sweep_walk`.
        //
        // Anchor sources, same three as the sweep's grid: offset 0, the
        // allocator's own verified object starts (TLAB refills, young
        // slow-path allocations, Cheney copies), and the END of every
        // free/TLAB skip block -- because a span that survived an earlier
        // collection is never re-handed-out and so contributes no allocator
        // anchor of its own. `used` terminates the list.
        //
        // Anchors landing INSIDE a skip block are dropped: such an anchor is
        // not an object start, and one minted by two adjacent uncoalesced
        // blocks would abort the whole attempt for no reason.
        let mut walked_in_parallel = false;
        let mut objstart_chunk_count = 0usize;
        // JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`,
        // default OFF; proposal 9 item 1): no whole-from-space walk. The same
        // anchor grid, at `LAZY_YOUNG_START_STRIDE`, becomes a lazy bitmap whose
        // chunks are walked by `objstart_chunk` the first time anything asks
        // about an address in them. The walk's divert stays BEFORE any forward:
        // the chunks every root / pin / conservative word names are proven
        // below, ahead of the unrecorded-root guard, and a failure there takes
        // the incomplete-walk exit exactly as a failed eager walk does.
        // Wave 4c (review `r23-w4b-...-lose-the-anchor-induction`): a chunk
        // answers only once it and every chunk below it are walked and landed
        // (the eager walk's induction from offset 0, so no allocator anchor is
        // trusted on its own), and once forwarding starts the grid is SEALED:
        // a chunk that cannot be proven then (first reached through an object
        // field, on a from-space layout the allocator did not write) is a
        // fatal stop, never a refusal that would leave the field naming
        // from-space across its reset. An unusable grid walks eagerly.
        let mut lazy_starts = false;
        if young_used > 0
            && self
                .lazy_starts_backoff
                .use_lazy(gen_lazy_young_object_starts_enabled())
        {
            let bounds = lazy_young_start_bounds(
                &young_from.alloc_anchors_snapshot(),
                &start_skips,
                young_used,
                LAZY_YOUNG_START_STRIDE,
            );
            if let Some(mut bits) = cratonvm_gc_core::young_mark::ObjectStartBits::new_lazy(
                young_base,
                young_used,
                bounds,
                start_skips.clone(),
                lazy_objstart_chunk_walker,
            ) {
                // Wave 4c: the anchors are arena-certified boundaries
                // (`Arena::forget_alloc_anchors_inside`); `=1` proves every
                // chunk by induction from offset 0 instead.
                bits.set_lazy_induction(gen_lazy_young_starts_induction());
                objstart_chunk_count = bits.lazy_chunk_count();
                young_object_starts = bits;
                lazy_starts = true;
            }
        }
        if !lazy_starts {
            let threads = cratonvm_gc_core::young_mark::young_gc_threads(young_used);
            if threads > 1 && young_used > 0 {
                let mut anchors: Vec<usize> = Vec::with_capacity(start_skips.len() + 64);
                anchors.push(0);
                anchors.extend(young_from.alloc_anchors_snapshot());
                anchors.extend(
                    start_skips
                        .iter()
                        .filter_map(|&(off, sz)| off.checked_add(sz)),
                );
                anchors.retain(|&o| o < young_used);
                anchors.sort_unstable();
                anchors.dedup();
                // Drop anchors inside a skip block. `start_skips` is sorted by
                // offset, so this is a binary search per anchor rather than
                // the O(anchors * skips) scan the shape invites.
                anchors.retain(|&o| {
                    let i = start_skips.partition_point(|&(off, _)| off <= o);
                    if i == 0 {
                        return true;
                    }
                    let (off, sz) = start_skips[i - 1];
                    !(o > off && o < off + sz)
                });
                // Subsample to the same stride the sweep's chunks use, so one
                // knob (`CRATONVM_GC_SWEEP_ANCHOR_STRIDE`) sizes both and a
                // TLAB-granular grid does not become one chunk per TLAB.
                let anchor_stride = sweep_anchor_stride();
                let mut chunk_anchors: Vec<usize> = Vec::new();
                let mut next_anchor = 0usize;
                for &a in &anchors {
                    if a >= next_anchor {
                        chunk_anchors.push(a);
                        next_anchor = a - a % anchor_stride + anchor_stride;
                    }
                }
                if chunk_anchors.last() != Some(&young_used) {
                    chunk_anchors.push(young_used);
                }
                // Preconditions the proof depends on, checked rather than
                // assumed: a full cover starting at 0, strictly increasing,
                // and with at least one interior boundary to be worth it.
                let usable = chunk_anchors.len() > 2
                    && chunk_anchors[0] == 0
                    && chunk_anchors[chunk_anchors.len() - 1] == young_used
                    && chunk_anchors.windows(2).all(|w| w[0] < w[1]);
                if usable {
                    // gen r4/sweep: on the persistent pool (design proposal #1).
                    walked_in_parallel = parallel_objstart_walk(
                        young_base,
                        &chunk_anchors,
                        &start_skips,
                        &young_object_starts,
                        threads,
                        self.evac_pool(threads),
                    );
                    if !walked_in_parallel {
                        // Abandoned wholesale. A partially-filled bitmap is
                        // WORSE than none -- a bit set by a chunk that walked
                        // from a bogus anchor names a non-object, and
                        // `forward_object` relocates through exactly those --
                        // so the sequential walk gets a clean one. `vec![0u64]`
                        // is a zero-page mapping, not a memset.
                        young_object_starts =
                            cratonvm_gc_core::young_mark::ObjectStartBits::new(young_base, young_used);
                    }
                }
                objstart_chunk_count = chunk_anchors.len().saturating_sub(1);
            }
        }
        // The ENGAGEMENT census, pushed unconditionally rather than from inside
        // the attempt. A counter that is absent when the attempt was never made
        // cannot be told apart from one that is absent because the phase did
        // not run, and "the instrument was armed where it cannot fire" is how a
        // zero gets read as a result. `objstart_chunks=0` means no attempt (one
        // worker, or an unusable anchor grid); a non-zero chunk count with
        // `objstart_parallel=0` means the attempt was made and REFUSED, which
        // is a different thing and worth chasing.
        if mv_phase_on {
            moving_phase_count_push("objstart_chunks", objstart_chunk_count as u128);
            moving_phase_count_push("objstart_parallel", u128::from(walked_in_parallel));
            moving_phase_count_push("objstart_lazy", u128::from(lazy_starts));
        }

        let mut start_free_iter = start_skips.iter().peekable();
        // Wave 4: a lazy grid walks nothing here (its chunks are walked on
        // demand), so the sequential loop below is skipped as after a
        // successful parallel walk.
        let mut young_cursor = if walked_in_parallel || lazy_starts { young_used } else { 0 };
        // Same word-batching cursor the chunk walker uses. Dropped at the end
        // of this block, which flushes the last partial word -- including on
        // every `break` out of the loop below.
        let mut start_run = young_object_starts.run();
        // gen r5w1/young5: the grid rules are `probe_grid_at`'s. `Parse` is
        // this walk's zero-run behaviour (an all-zero header is one empty
        // object, which gets its own start bit), and every anomaly still fails
        // the walk closed.
        //
        // Bug-D (2026-06-12) parity with every other young walk: a GAP-filler
        // sentinel (a sub-`HEADER_SIZE` TLAB tail, `install_tail_filler`) is
        // strided BEFORE any size read — its `num_slots` offset lies outside
        // the gap, so parsing it as an object reads garbage and the walk would
        // abort. This was the gap class that actually broke this walk in the
        // WildFly repro (cursor a few MB in, at the first thread's retired
        // TLAB tail).
        let start_rules = GridRules {
            base: young_base,
            used: young_used,
            zero_run_hi: young_used,
            side_sorted: &[],
            zero_runs: ZeroRunRule::Parse,
            bad_gap_filler_is_anomaly: true,
            phantom: false,
        };
        let mut no_phantom_probe = 0usize;
        while young_cursor < young_used {
            if skip_free_blocks(&mut young_cursor, &mut start_free_iter).0 {
                continue;
            }
            let obj_ptr = (young_base + young_cursor) as *mut u8;
            let next_free = start_free_iter.peek().map(|&&(off, _)| off);
            // Free/TLAB ranges were skipped; the cursor is on an
            // allocator-written object boundary inside `young_used`.
            let verdict =
                probe_grid_at(young_cursor, next_free, &start_rules, &mut no_phantom_probe);
            let size = match verdict {
                GridProbe::Gap { len } => {
                    young_cursor += len;
                    continue;
                }
                GridProbe::Object { size } => size,
                GridProbe::BadGapFiller => {
                    // SAFETY: the probe just read this sentinel's length word
                    // at offset 4, inside the >= 8-byte gap.
                    let gap = unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) }
                        as usize;
                    tracing::warn!(
                        young_cursor,
                        young_used,
                        gap,
                        "GC: young object-start walk hit a corrupt GAP-filler — \
                         diverting this cycle to the non-moving sweep"
                    );
                    start_walk_complete = false;
                    break;
                }
                GridProbe::CrossesFreeBlock { free_off: off, .. } => {
                    tracing::warn!(
                        young_cursor,
                        off,
                        "GC: young object-start walk crossed a free/TLAB range — \
                         diverting this cycle to the non-moving sweep"
                    );
                    start_walk_complete = false;
                    break;
                }
                // `Parse` yields no zero-run verdicts and `phantom` is off:
                // this arm is the implausible extent.
                GridProbe::BadSize { .. }
                | GridProbe::EmptyRun { .. }
                | GridProbe::ZeroSpan { .. }
                | GridProbe::Phantom { .. } => {
                    tracing::warn!(
                        young_cursor,
                        young_used,
                        "GC: young object-start walk stopped at an implausible extent — \
                         diverting this cycle to the non-moving sweep"
                    );
                    start_walk_complete = false;
                    break;
                }
            };
            if !start_run.insert(obj_ptr as usize) {
                // Only reachable if an object start is not 8-byte aligned, which
                // `gen_object_total_size`'s rounding makes impossible — but a
                // bitmap cannot represent it, and silently aliasing a
                // neighbour's bit would make `forward_object` relocate through
                // an interior word. Fail closed, like every other way this walk
                // can fail to complete.
                tracing::warn!(
                    young_cursor,
                    young_used,
                    "GC: young object-start walk hit a misaligned object start — \
                     diverting this cycle to the non-moving sweep"
                );
                start_walk_complete = false;
                break;
            }
            young_cursor += size;
        }
        // FLUSH THE LAST PARTIAL WORD before anything reads the bitmap.
        // `StartRun` also flushes on drop, but its drop is at the end of this
        // function -- long after `forward_object` starts consulting these bits
        // -- so relying on it would leave up to 63 object starts unrecorded
        // for the whole collection, and an unrecorded start is one
        // `forward_object` refuses to relocate. Explicit, immediately after
        // the walk, is the only correct placement.
        drop(start_run);
        // Publish the cycle for the TLAB-side watch reports, which hold no heap
        // handle of their own, and say what THIS walk concluded about the
        // watched address — the one fact that dates a refusal to a cycle.
        cratonvm_gc_core::tlab::WATCH_COLLECTION.store(
            self.stats.minor_gc_count.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        if cratonvm_gc_core::tlab::watch_covers(young_base, young_base + young_used) {
            let w = cratonvm_gc_core::tlab::watched();
            cratonvm_gc_core::tlab::watch_note(format_args!(
                "OBJECT-START WALK verdict: in_from_space=true is_object_start={} arena=[0x{young_base:x} used=0x{young_used:x}] starts={} complete={start_walk_complete}",
                young_object_starts.contains(w),
                young_object_starts.len(),
            ));
        }
        // gc-genpause F0: the walk proper ends here. `objstart_walk_bytes` is
        // the from-space extent it chased -- ALLOCATED bytes, not live ones --
        // so a slow walk beside a small `objects_copied` is the O(garbage)
        // shape this phase is expected to have, and a slow walk beside a small
        // `objstart_walk_bytes` is something else entirely.
        mv_phase!("objstart_walk");
        if mv_phase_on {
            moving_phase_count_push("objstart_walk_bytes", young_used as u128);        }
        if false {
            let skip_bytes: usize = start_skips.iter().map(|&(_, sz)| sz).sum();
            let head: Vec<String> = start_skips
                .iter()
                .take(4)
                .map(|&(off, sz)| format!("0x{off:x}+0x{sz:x}"))
                .collect();
            self.note_objstart_walk(format!(
                "collection={} used=0x{young_used:x} starts_recorded={} parallel={walked_in_parallel} \
                 chunks={objstart_chunk_count} complete={start_walk_complete} \
                 skips={} skip_bytes=0x{skip_bytes:x} first_skips=[{}] \
                 reserved_tails={moving_with_reserved_tails}",
                // The cycle number, so a reader can tell the collection that DROPPED
                // a reference from the many later ones that merely report the
                // re-served address again. Without it every refusal of a recycled
                // address reads as the same event.
                self.stats.minor_gc_count.load(Ordering::Relaxed),
                young_object_starts.len(),
                start_skips.len(),
                head.join(" "),
            ));
        }
        // gcd d2/h (2026-09-27), `gengc-r5w6-sizer10-refused-young-root-dangles`:
        // a PRECISE root naming from-space at an address this walk did not
        // record would be refused by `forward_object_impl` ("not-an-object-start")
        // and left dangling once from-space is reset. On a cycle whose root set
        // is precise (no compiled frame live, so no conservative word fed
        // `roots`; no injected pin words), refuse the cycle instead, through the
        // incomplete-walk exit below: nothing destructive has happened yet, and
        // the dirty cards are taken only after this point. Bounded to one
        // refusal per offending address (see the helper). Skipped when the
        // cycle is already refused.
        //
        // gcf f4/y, residual (b) of that page: a cycle WITH a live compiled
        // frame used to skip the guard outright, because `roots` then holds
        // conservative words whose interior hits are refused by design. On
        // the two default-path moving cycles that have such a frame -- the
        // ledger cleared term 4 (`term4_ledger_cleared`), or the pinned copy
        // took the cycle (`pinned_young_words`) -- the conservative
        // population is KNOWN: the ledger's words and the blocked peers'
        // native-stack words. Under `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_LEDGER`
        // (default ON with the pinned copy since the user's flip of 2026-10-13,
        // `=0` kills; opt-in before) those words are excluded and the guard
        // runs on the rest, which are precise. A conservative word the ledger
        // does not hold (a source it does not model) refuses a cycle per
        // distinct address -- over-retention, never a dangling root, but a
        // stream of refusals if such words exist (`urr_max_streak`).
        let guard_exclusion: Option<FxHashSet<usize>> = if forced_pin_words.is_some() {
            None
        } else if !has_conservative_roots && pinned_young_words.is_none() {
            // gcg g6/jn (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS`, default
            // ON since gch h2/yg; the VM publishes nothing with it `=0`, so
            // this is the empty set then): a blocked or parked peer's
            // snapshot deposited while some
            // thread ran compiled code holds conservatively probed locals, dead
            // ones included. No compiled frame being live NOW does not make them
            // precise (`gcg-g5jn-unrecorded-root-guard-reads-a-conservative-deposit-as-precise`).
            let mut words: Vec<usize> = Vec::new();
            let (lo, hi) = (young_base, young_base.saturating_add(young_used));
            cratonvm_gc_core::gc_quiescence::conservative_deposit_words_into(&mut words, |v| {
                v >= lo && v < hi
            });
            Some(words.into_iter().collect())
        } else if term4_ledger_cleared || pinned_young_words.is_some() {
            let mut words: Vec<usize> = pinned_young_words.clone().unwrap_or_default();
            let (lo, hi) = (young_base, young_base.saturating_add(young_used));
            cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut words, |v| {
                v >= lo && v <= hi
            });
            // gci i8/pe (sizer10 (b)): the roots the take-over / helper-window
            // probes made of those words (`w & !7`, and the companion 8 below).
            extend_with_conservative_probe_echoes(&mut words, lo, hi);
            // gch h1/yp: the blocked peers' conservatively probed interpreter
            // locals are a population the ledger does not model either (it
            // holds compiled-frame words); exclude them here too. Empty with
            // `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_DEPOSITS=0`.
            cratonvm_gc_core::gc_quiescence::conservative_deposit_words_into(&mut words, |v| {
                v >= lo && v < hi
            });
            self.unrecorded_root_census[3].fetch_add(1, Ordering::Relaxed);
            Some(words.into_iter().collect())
        } else {
            None
        };
        // JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`):
        // prove, BEFORE anything is forwarded, every lazy chunk a root, pin or
        // conservative word of this cycle names -- the roots (precise and
        // conservative alike), the forced and ledger pin words, the guard's
        // exclusion set, the blocked peers' stack words and their probe
        // echoes, the conservative deposits, and the overlay roots. These are
        // the populations the eager walk's divert protected; a chunk among
        // them that fails takes the incomplete-walk exit below, as a failed
        // eager walk does. (The dirty-card walk-gap words are proven after the
        // card scan, at its own refusal exit.)
        if lazy_starts && start_walk_complete && !moving_with_reserved_tails {
            let (lo, hi) = (young_base, young_base.saturating_add(young_used));
            // gcj pcp (debug, `CRATONVM_DBG_GCPAUSE`): how many of each
            // population's words lie in from-space, and how many distinct
            // lazy chunks they name (`rp_<population>_words` / `_chunks`).
            let mut census_at = 0usize;
            let mut census = |name_words: &'static str, name_chunks: &'static str, words: &[usize]| {
                if !mv_phase_on {
                    return;
                }
                let part = &words[census_at..];
                census_at = words.len();
                let mut chunks: Vec<usize> = part
                    .iter()
                    .filter(|&&w| w >= lo && w < hi)
                    .filter_map(|&w| young_object_starts.lazy_chunk_of_addr(w))
                    .collect();
                let inside = chunks.len();
                chunks.sort_unstable();
                chunks.dedup();
                moving_phase_count_push(name_words, inside as u128);
                moving_phase_count_push(name_chunks, chunks.len() as u128);
            };
            let mut words: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            census("rp_roots_words", "rp_roots_chunks", &words);
            if let Some(w) = forced_pin_words {
                words.extend_from_slice(w);
            }
            census("rp_forced_words", "rp_forced_chunks", &words);
            if let Some(w) = pinned_young_words.as_ref() {
                words.extend_from_slice(w);
            }
            census("rp_ledger_words", "rp_ledger_chunks", &words);
            if let Some(ex) = guard_exclusion.as_ref() {
                words.extend(ex.iter().copied());
            }
            census("rp_guard_words", "rp_guard_chunks", &words);
            cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut words, |v| v >= lo && v <= hi);
            census("rp_peer_words", "rp_peer_chunks", &words);
            cratonvm_gc_core::gc_quiescence::conservative_deposit_words_into(&mut words, |v| {
                v >= lo && v < hi
            });
            extend_with_conservative_probe_echoes(&mut words, lo, hi);
            census("rp_deposit_words", "rp_deposit_chunks", &words);
            words.extend(
                cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|_| true)
                    .into_iter()
                    .map(|r| r.as_ptr() as usize),
            );
            census("rp_overlay_words", "rp_overlay_chunks", &words);
            // (Proven page by page: a pinned plan resolves whole 4 KiB pages.)
            let threads = cratonvm_gc_core::young_mark::young_gc_threads(young_used);
            let pool = (threads > 1).then(|| self.evac_pool(threads));
            if !lazy_young_starts_prove_words(&young_object_starts, &words, threads, pool) {
                tracing::warn!(
                    "GC: a lazy young object-start chunk named by a root or pin word \
                     failed its walk -- treating the object-start walk as incomplete"
                );
                start_walk_complete = false;
            }
        }
        if start_walk_complete && !moving_with_reserved_tails {
            if let Some(exclude) = guard_exclusion.as_ref() {
                if self.unrecorded_young_root_refuses_cycle(
                    roots,
                    &young_from,
                    &young_object_starts,
                    exclude,
                ) {
                    start_walk_complete = false;
                }
            }
        }
        // Captured BEFORE the T-3 arm forces `start_walk_complete` to false,
        // because that assignment is what makes the two refusal causes
        // indistinguishable from here on. This is the discriminator the
        // decision record needs (2026-09-21), and it is exact: the walk either
        // did or did not finish on its own.
        let walk_failed_on_its_own = !start_walk_complete;
        if moving_with_reserved_tails {
            // T-3 refusal. The walk above completed fine — the tails were
            // skipped, not parsed — but a live mutator owns a TLAB inside the
            // arena this cycle is about to evacuate, swap and reset. Take the
            // same exit as an incomplete walk: skip the collection rather than
            // hand the owner's cursor a recycled buffer. The warn naming the
            // condition was already emitted (rate-limited) where it was
            // detected.
            start_walk_complete = false;
        }
        if !start_walk_complete {
            // A partial start set makes the moving cycle unsound (see the
            // comment above). The non-moving sweep is NOT a safe fallback
            // here either: on this precise-root path it lacks the
            // conservative over-marking it needs and reclaims still-live
            // young objects (the HIB-CV-22/32/33 family — measured live in
            // this investigation: one diverted cycle produced all-zero-header
            // reads on live receivers 145 ms later). The only sound choice
            // is to SKIP this young collection entirely: over-retain for one
            // cycle, let the allocation slow paths spill to old gen, and
            // retry on the next trigger (by which point the unparseable
            // layout — normally a transient un-tail-filled gap — is gone).
            // Nothing destructive has happened yet: only the pre-collection
            // bounds publish and the card-buffer drain, both idempotent.
            // Which of the two causes fired, in the line itself. The
            // decision record below carries the same fact as a reason
            // CODE; this is the human half, and a reader who has one
            // log line and no metrics endpoint has only this.
            if walk_failed_on_its_own {
                tracing::warn!(
                    "GC: young object-start walk incomplete — skipping this \
                     young collection (over-retain; retried next cycle)"
                );
            } else {
                tracing::warn!(
                    "GC: a live mutator owns a reserved TLAB tail inside this \
                     from-space (T-3) — skipping this young collection \
                     (over-retain; retried next cycle)"
                );
            }
            // BOTH halves of this belong here, and they were written by two
            // concurrent GC rounds that each found one of them.
            //
            // gengc-round1-core: this is the exit the T-3 reserved-TLAB-tail
            // refusal also takes. A mutator that left a TLAB un-retired keeps
            // it un-retired for as long as it is parked, so without the floor
            // this refusal repeats at every allocation for the whole of that
            // window — re-running the O(young allocated) object-start walk each
            // time.
            self.note_skipped_young_cycle(&young_from);
            // And the refusal is the one collector outcome that used to move NO
            // counter at all, so a heap wedged in a refusal loop and an idle
            // heap published identical numbers. The floor above stops the
            // storm; this is what lets an operator SEE that it happened.
            //
            // The sub-reason is `NONE`, not `moving_young_incomplete_reason()`:
            // every code in that enum names a JIT *coverage* obligation, and
            // neither cause here is one. This refusal is either an unparseable
            // young object-start layout or the T-3 reserved-tail tripwire
            // above, so borrowing a coverage code would attribute the skip to
            // a proof that never failed.
            //
            // 2026-09-21: the two causes now have their OWN reason codes,
            // which was the residual the 2026-09-20 round left open ("giving
            // them their own codes is the next step, not this one"). They
            // share an exit and nothing else: one says the from-space layout
            // could not be parsed — a heap-integrity question, and the
            // `CRATONVM_DBG_ROOT_REMAP_AUDIT` walk report is where to look
            // next; the other says a perfectly parseable arena is still owned
            // by a live mutator — a TLAB-retirement question, with a different
            // fix and a different owner. One code for both gave an operator a
            // count and no direction.
            //
            // `walk_failed_on_its_own` takes priority when BOTH are true: a
            // walk that could not complete is the more serious finding, and
            // the tail tripwire would have refused a cycle that was already
            // being refused.
            cratonvm_gc_core::gc_metrics::record_collector_decision(
                "generational",
                if walk_failed_on_its_own {
                    cratonvm_gc_core::gc_metrics::decision_reason::SKIPPED_YOUNG_WALK_INCOMPLETE
                } else {
                    cratonvm_gc_core::gc_metrics::decision_reason::SKIPPED_YOUNG_RESERVED_TLAB_TAILS
                },
                cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
            );
            return (
                GcResult {
                    stats: cratonvm_gc_core::gc::GcStats {
                        objects_copied: 0,
                        bytes_copied: 0,
                        bytes_freed: 0,
                    },
                    pointer_map: cratonvm_types::PointerMap::default(),
                },
                Vec::new(),
            );
        }
        // gen r4w5/pinned5: resolve the pin words to the objects they pin, on
        // the exact start bitmap the walk above just built. No object on a
        // pinned page (the words land only in free space) means nothing needs
        // pinning: the ordinary Cheney copy runs, under its own reason code.
        //
        // gen r4w6/pinstale6: plus every word a BLOCKED peer's native stack
        // holds into from-space that is NOT an object start. The blocked-wake
        // fold rewrites that stack's words by an exact `pointer_map` lookup, so
        // a base is repaired at wake and an interior word is not: pinned, it
        // still names its object after the cycle. (The pause's ledger already
        // holds the peers' REGISTER words; `gc_quiescence::record_peer_reg`.)
        // Bounded by the decision above, which counted every such word.
        //
        // gen r5w2/roots6 — the dirty-card scan moved up to HERE (it used to
        // run just after `pre_evacuate`, below), so its old-gen WALK-GAP words
        // can join the pin words. `scan_dirty_cards` reports every young value
        // it found in old-gen bytes behind a header no walk can size
        // (`CardScanGapRoots`, gen r5w1/oldgen5): such a word may be a
        // reference nothing else roots, and it can never be rewritten (it may
        // not be a reference at all), so the object it names must stay alive
        // AND stay where it is. That is exactly a pinned in-place plan's
        // contract, so the gap words go into the plan -- alongside the ledger's
        // words on a pinned cycle, and ALONE on an ordinary moving cycle
        // (which then runs in place instead of as a Cheney copy). No 1/8
        // bound: correctness beats it, and the population is empty on a sane
        // heap, where nothing below changes. Closes the young half of
        // `gengc-r4w4-oldgen4-a-walk-gap-hides-old-to-young-roots-from-the-card-scan-FIXED-20260927.md`.
        // Nothing between the old position and this one reads or writes a
        // card or an old-gen object.
        // gcj j1yg-close: three marks split what `pre_evacuate` used to time
        // as one row -- everything before the card scan, the dirty-card scan
        // itself (`refinement_ms`), and the pinned plan. On 4-thread javac
        // the card scan was the largest single phase of the pinned copy.
        mv_phase!("pre_cards_misc");
        // gcj pcp: read once for the cycle -- the card proof below and the
        // parallel copy's evacuator must agree.
        let precise_edges = true;
        // The card values need no proof when the workers seed them as precise
        // edges (the parallel card seed, never under the full old-gen rset
        // scan; see `card_seed_par` below), so the scan need not collect them.
        let skip_card_proof =
            precise_edges && gc_flags().gc_par_evac_card_seed && !Self::full_old_rset_scan_enabled();
        if mv_phase_on {
            if let Some((_, walked, _, bytes)) = young_object_starts.lazy_census() {
                moving_phase_count_push("objstart_lazy_walked_rootproof", walked as u128);
                moving_phase_count_push("objstart_lazy_bytes_rootproof", bytes as u128);
            }
        }
        // gcj j1yg-pause: the heap's kept buffers (`card_scan_scratch`),
        // handed back after the copy phase.
        let (mut extra_roots, mut card_values) = std::mem::take(&mut *self.card_scan_scratch.lock());
        extra_roots.clear();
        card_values.clear();
        // (old_gen_obj, slot_index, _) for each old→young reference slot
        // gcj j1yg-pause: on the evacuation pool at the copy's own width.
        let card_scan_workers = cratonvm_gc_core::young_mark::young_gc_threads(young_used);
        let card_scan_pool = (card_scan_workers > 1).then(|| self.evac_pool(card_scan_workers));
        // `card_values`: the edges' young values, for the lazy proof below
        // (gcj j1yg-pause).
        // gcj pcp: the walk's per-group buffers, kept between cycles.
        let mut card_scan_parts = std::mem::take(&mut *self.card_scan_parts.lock());
        let card_gap_roots = Self::scan_dirty_cards_with(
            card_table,
            &old_gen,
            &young_from,
            &mut extra_roots,
            card_scan_pool.map(|p| (p, card_scan_workers, &mut card_scan_parts)),
            (young_object_starts.is_lazy() && !skip_card_proof).then_some(&mut card_values),
        );
        *self.card_scan_parts.lock() = card_scan_parts;
        mv_phase!("dirty_card_scan");
        // gci i10/gr (`gengc-r5w6-sizer10-refused-young-root-dangles` (a),
        // `CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`, opt-in; see
        // `gen_heap_gci_i10gr.rs`): the guard above judged `roots` only. A
        // dirty-card slot value and an overlay row value reach
        // `forward_object_impl` the same way (`seed_dirty_card_roots`,
        // `seed_overlay_roots`); an unrecorded from-space one is refused there
        // and dangles once from-space is reset. Judged here, right after the
        // take, on the cycles the guard ran on, with its exclusion set and its
        // one-refusal latch. Nothing destructive has happened but the take,
        // which a refusal undoes: every consumed card that held a young value
        // is re-dirtied (the non-moving sweep's rule), plus the walk-gap
        // cards, before the refused-walk exit. (Pairs `scan_all_old_to_young`
        // adds below, a diagnostic switch, are not judged.)
        if let Some(exclude) = guard_exclusion.as_ref() {
            if gci_i10gr::unrecorded_root_guard_cards_enabled() {
                // Engagement: a guarded cycle (census slot 8, `urr_cards_guarded`).
                self.unrecorded_root_census[8].fetch_add(1, Ordering::Relaxed);
                let overlay_values: Vec<usize> =
                    cratonvm_gc_core::external_roots::external_roots_for_matching_owners(&|_| true)
                        .into_iter()
                        .map(|r| r.as_ptr() as usize)
                        .collect();
                let precise_cards = card_table.precise_ref_array_marks();
                // SAFETY: every pair is one `scan_dirty_cards` produced this
                // pause (a live old object, an in-bounds reference slot); the
                // world is stopped.
                let found = unsafe {
                    gci_i10gr::card_guard_candidates(
                        &extra_roots,
                        &overlay_values,
                        |a| young_from.contains(a as *const u8),
                        |a| young_object_starts.contains(a),
                        precise_cards,
                    )
                };
                if found.first.is_some() {
                    let [refused_before, open_before, _] = self.unrecorded_young_root_census();
                    let refused = self.unrecorded_young_root_refuses_cycle(
                        &found.values,
                        &young_from,
                        &young_object_starts,
                        exclude,
                    );
                    gci_i10gr::note_card_guard_offenders(
                        &found,
                        refused,
                        refused_before + open_before,
                        &young_object_starts,
                    );
                    if refused {
                        // SAFETY: as above.
                        let mut redirty =
                            unsafe { gci_i10gr::card_guard_redirty_addrs(&extra_roots, precise_cards) };
                        redirty.extend_from_slice(&card_gap_roots.card_addrs);
                        card_table.mark_dirty_bulk(&redirty);
                        tracing::warn!(
                            "GC: a dirty-card or overlay value names an unrecorded from-space \
                             address -- skipping this young collection (over-retain; retried \
                             next cycle; cards restored)"
                        );
                        self.note_skipped_young_cycle(&young_from);
                        cratonvm_gc_core::gc_metrics::record_collector_decision(
                            "generational",
                            cratonvm_gc_core::gc_metrics::decision_reason::SKIPPED_YOUNG_WALK_INCOMPLETE,
                            cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                        );
                        return (
                            GcResult {
                                stats: cratonvm_gc_core::gc::GcStats {
                                    objects_copied: 0,
                                    bytes_copied: 0,
                                    bytes_freed: 0,
                                },
                                pointer_map: cratonvm_types::PointerMap::default(),
                            },
                            Vec::new(),
                        );
                    }
                }
            }
        }
        // JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`):
        // the dirty-card values (precise old-to-young slots) and the card
        // walk-gap words (which join the pinned plan) are known only now.
        // Prove their lazy chunks before anything is forwarded, and on a
        // failure take the card guard's refusal exit: nothing destructive has
        // happened but the card take, which the re-dirty undoes.
        if young_object_starts.is_lazy() {
            let precise_cards = card_table.precise_ref_array_marks();
            // gcj j1yg-pause: the pairs' values as the card scan read them
            // (`card_values`, index for index -- the slots are unchanged since,
            // the world is stopped), not re-read here pair by pair.
            // (gcj pcp: not collected at all when the proof is skipped.)
            debug_assert!(skip_card_proof || card_values.len() == extra_roots.len());
            let mut words: Vec<usize> = std::mem::take(&mut card_values);
            // gcj pcp: with precise edges the workers forward the card values
            // without asking the grid (`ParEvac::evacuate_precise`), so only
            // the walk-gap words -- conservative, and pinned below -- need
            // their chunks proven here.
            if skip_card_proof {
                words.clear();
            }
            words.extend_from_slice(&card_gap_roots.young_words);
            let threads = cratonvm_gc_core::young_mark::young_gc_threads(young_used);
            let pool = (threads > 1).then(|| self.evac_pool(threads));
            if !lazy_young_starts_prove_words(&young_object_starts, &words, threads, pool) {
                // SAFETY: as above.
                let mut redirty =
                    unsafe { gci_i10gr::card_guard_redirty_addrs(&extra_roots, precise_cards) };
                redirty.extend_from_slice(&card_gap_roots.card_addrs);
                card_table.mark_dirty_bulk(&redirty);
                tracing::warn!(
                    "GC: a lazy young object-start chunk named by a dirty-card value failed \
                     its walk -- skipping this young collection (over-retain; retried next \
                     cycle; cards restored)"
                );
                self.note_skipped_young_cycle(&young_from);
                cratonvm_gc_core::gc_metrics::record_collector_decision(
                    "generational",
                    cratonvm_gc_core::gc_metrics::decision_reason::SKIPPED_YOUNG_WALK_INCOMPLETE,
                    cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
                );
                return (
                    GcResult {
                        stats: cratonvm_gc_core::gc::GcStats {
                            objects_copied: 0,
                            bytes_copied: 0,
                            bytes_freed: 0,
                        },
                        pointer_map: cratonvm_types::PointerMap::default(),
                    },
                    Vec::new(),
                );
            }
            // gcj j1yg-pause: the buffer goes back to the scratch below.
            card_values = words;
            // Wave 4c (review `r23-w4b-...-lose-the-anchor-induction` item 2):
            // the last point this cycle can still be skipped. From here on a
            // chunk that cannot be proven (one first reached through an object
            // field) must not answer "not an object start": `forward_object`
            // would leave that field naming from-space, which the cycle resets
            // (or, in place, frees). Sealed, it is a fatal stop instead
            // (`LazyStartGrid::unprovable`). Unsealed again before the in-place
            // finish, whose poisoning memsets an unprovable span itself.
            young_object_starts.seal_lazy(true);
        }
        // gcj j1yg-pause: the card guard and the lazy proof of the card
        // values' pages, apart from the plan below.
        mv_phase!("card_words_prove");
        // gcd d1/e (2026-09-27): the blocked-peer interior words above are
        // also taken on a cycle option B
        // (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`) let through. The ledger it judged holds deposits and
        // peer REGISTER words only; without this, a blocked peer's `*const u8`
        // cursor into a young array was moved under it (gen r5w4/pin8). A
        // cycle whose capture holds no such word still runs as a Cheney copy
        // (the plan is empty).
        //
        // gch h2/yg (gcd d1/e proposal): the capture's young non-start words
        // are read once here, on every relocating cycle, for the census
        // (`blocked_peer_interior_census`): how often the default moving path
        // relocates under such a word. The same filter and the same words the
        // plan took before; nothing else reads them. (The h2/yg opt-in that
        // took them on EVERY relocating cycle was deleted by gch h3/yc:
        // vh2-yg block D found no default cycle with such a word.)
        let takes_blocked_peer_words = plan_takes_blocked_peer_words(
            pinned_young_words.is_some(),
            term4_ledger_cleared,
            forced_pin_words.is_some(),
        );
        let blocked_peer_interior: Vec<usize> = if forced_pin_words.is_none() {
            let mut w = Vec::new();
            let young_end = young_base.saturating_add(young_used);
            cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut w, |v| {
                v >= young_base && v <= young_end && !young_object_starts.contains(v)
            });
            self.note_blocked_peer_interior(w.len(), takes_blocked_peer_words);
            w
        } else {
            Vec::new()
        };
        // t1 wave 2 lane W: the exact-rule names of a page-rule plan, for a
        // P-C major on this cycle (`pc_in_place_seeds`); `None` otherwise.
        let mut pc_in_place_names: Option<Vec<usize>> = None;
        let pinned_plan: Option<PinnedYoungPlan> = if pinned_young_words.is_some()
            || takes_blocked_peer_words
            || !card_gap_roots.young_words.is_empty()
        {
            let mut all_words = pinned_young_words.clone().unwrap_or_default();
            if takes_blocked_peer_words {
                all_words.extend_from_slice(&blocked_peer_interior);
            }
            all_words.extend_from_slice(&card_gap_roots.young_words);
            let mut plan = build_pinned_young_plan_mode(
                &all_words,
                young_base,
                young_used,
                &young_object_starts,
                pinned_copy_object_windows_enabled(),
            );
            // gcf f5/y: root only the objects the words name (opt-in).
            // gcj j1/pn: `CRATONVM_GEN_PINNED_COPY_NAMED_ROOTS` (default ON since
            // gcj j1yg-pause, `=0` kills) takes
            // the narrower naming rule of the watched arm for every object
            // (`pinned_plan_named_roots`); it wins over the f5/y rule when
            // both are set.
            //
            // JIT round 23 wave 6, lane L4 (`CRATONVM_GEN_MOVING_FOLLOWS_NAMED_ROOTS`,
            // default ON, `=0` kills): the named rule is also taken on a cycle
            // that keeps the deferred young mirrors' promise by FOLLOWING their
            // rows (`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS`, default ON).
            // Under the page rule (or f5/y's exact rule, which also roots the
            // object ENDING at a start word) a conservative word next to a dead
            // class loader seeded it, the copy aged and promoted it, and it then
            // waited for an old collection: `GenR5W5ConcUnloadProbe` printed
            // `dead-loader-unloaded=false` in 5 of 9 runs on n23-w5 (HotSpot and
            // the forced in-place sweep, which resolves each word to the object
            // it names, print `true`). With the named rule: 10 of 10 `true`,
            // stdout equal to HotSpot's, every cycle `moving-pinned-pages`.
            let named_rule = pinned_copy_named_roots_enabled()
                || (young_side_table_rows_follow());
            if !plan.objects.is_empty() && named_rule {
                let is_array = |b: usize| {
                    // SAFETY: `b` is a plan object's base, an object start the
                    // exact pre-GC walk recorded in from-space; its header is
                    // whole and nothing has written it yet.
                    (unsafe { (*(b as *const ObjectHeader)).kind() }) == ObjectKind::Array
                };
                // gcj j1yg-pause round 3 (`CRATONVM_GEN_PINNED_COPY_ROOT_SLICE_NAMES`):
                // a ledger word names a root only if the pause's root slice
                // holds it too (`pinned_copy_root_slice_words`); a ledger-only
                // word still pins its page.
                let named_words = if forced_pin_words.is_none() && pinned_copy_root_slice_names_enabled() {
                    pinned_copy_root_slice_words(
                        &all_words,
                        roots.iter().map(|r| r.as_ptr() as usize),
                        &card_gap_roots.young_words,
                        if takes_blocked_peer_words { &blocked_peer_interior[..] } else { &[] },
                    )
                } else {
                    all_words.clone()                };
                plan.roots = Some(pinned_plan_named_roots(&named_words, &plan.objects, &is_array));
                plan.exact_rule = true;
            } else if !plan.objects.is_empty() && pinned_copy_exact_roots_enabled() {
                plan.roots = Some(pinned_plan_exact_roots(&all_words, &plan.objects));
                plan.exact_rule = true;
            }
            // t1 wave 2 lane W (`CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_IN_PLACE`):
            // a P-C major roots only what a pin word can NAME (f5/y + g1/y's
            // exact rule, a superset of the named rule), not every neighbour
            // the page rule seeded: a dead object that merely shares a pinned
            // page stays in place, unreached, as any unreached young survivor
            // of a moving P-C pause does.
            //
            // t1 wave 3 lane W (`CRATONVM_GC_MOVING_MAJOR_IN_PLACE_NAMED_SEEDS`):
            // the NAMED rule instead (`pinned_plan_named_roots`: an instance is
            // named only by a word inside it; one-past-the-end and
            // base-minus-offset words name arrays only, the only shapes the
            // JIT forms). Measured producers on `GchMj3OccupancyMajorWeakProbe`:
            // a stale allocation-top word (the NEXT object's base) in a dead
            // spill slot or in the suspended interpreter's Rust frames, and a
            // `child - 8` word in a helper frame, each rooting a dead instance
            // through the exact rule's one-past-the-end / slack-above reach.
            // The young phase still PINS every page those words name; this
            // narrows only what the P-C trace starts from.
            if !plan.objects.is_empty() {
                if true {
                    let is_array = |b: usize| {
                        // SAFETY: `b` is a plan object's base, an object start
                        // the exact pre-GC walk recorded in from-space; its
                        // header is whole and nothing has written it yet.
                        (unsafe { (*(b as *const ObjectHeader)).kind() }) == ObjectKind::Array
                    };
                    // Only the pin words the pause's ROOT SLICE also holds:
                    // the marking contract's own roots (precise slots and the
                    // conservative band / register words the root gatherer
                    // publishes). A word the ledger alone holds -- the Rust
                    // frames of the pause path, a spill slot above the live
                    // cursor, the suspended interpreter above the outermost
                    // entry -- pins its page (no move under it) but roots
                    // nothing in any other collection, so it roots nothing
                    // here either. (`pc_in_place_root_pin_words`.)
                    let words = pc_in_place_root_pin_words(
                        &all_words,
                        roots.iter().map(|r| r.as_ptr() as usize),
                    );
                    pc_in_place_names =
                        Some(pinned_plan_named_roots(&words, &plan.objects, &is_array));
                } else if !plan.exact_rule {
                    pc_in_place_names = Some(pinned_plan_exact_roots(&all_words, &plan.objects));
                }
            }
            // gci i30/wy: a watched object no pin word names is not a seed
            // (reached: kept in place as before; unreached: reclaimed, so its
            // weak reference clears). Default ON since the i35 WARNING flip
            // (`=0` kills; opt-in from i34/wy until then); i34 names by the
            // array-only slack rule.
            if !plan.objects.is_empty() {
                if let Some(watched) =
                    cratonvm_gc_core::gc_quiescence::watched_referents_snapshot_for(self.watched_set_key())
                {
                    let seeds: Vec<usize> = match plan.roots.as_ref() {
                        Some(r) => r.clone(),
                        None => plan.objects.iter().map(|&(b, _)| b).collect(),
                    };
                    let is_watched = |b: usize| watched.contains(&b);
                    let is_array = |b: usize| {
                        // SAFETY: `b` is a plan object's base, an object start
                        // the exact pre-GC walk recorded in from-space; its
                        // header is whole and nothing has written it yet.
                        (unsafe { (*(b as *const ObjectHeader)).kind() }) == ObjectKind::Array
                    };
                    // gci i35/wy: a start word names a watched object only if
                    // the root vector holds it (`=0` on the sub-switch: the
                    // i34 rule). One set per pinned cycle, switch ON only.
                    // gci i39/rv: a card walk-gap word is a heap root the
                    // card scan cannot trace, so it counts as rooted
                    // (`pinned_watched_start_rooted_set`).
                    // gcj-j1pn-pinned-cycle-review-findings item 1: so does a
                    // REGISTER word of the ledger (`=0` on
                    // `CRATONVM_GEN_PINNED_WATCHED_REGISTER_WORDS_ROOTED`
                    // restores the i39 set).
                    let start_needs_root = true;
                    let mut start_rooted_words: Vec<usize> = if start_needs_root {
                        pinned_watched_start_rooted_words(&card_gap_roots.young_words)
                    } else {
                        Vec::new()
                    };
                    // gcj j1/pn item 1 (twin): under its own switch, every
                    // layout-free / register / peer-band word of the ledger
                    // read counts as rooted too.
                    if start_needs_root && pinned_watched_start_layout_free_rooted_enabled() {
                        start_rooted_words.extend_from_slice(&pinned_layout_free_words);
                    }
                    let root_set: FxHashSet<usize> = if start_needs_root {
                        pinned_watched_start_rooted_set(
                            roots.iter().map(|r| r.as_ptr() as usize),
                            &start_rooted_words,
                        )
                    } else {
                        FxHashSet::default()
                    };
                    let in_root = |b: usize| root_set.contains(&b);
                    let need: Option<&dyn Fn(usize) -> bool> = if start_needs_root {
                        Some(&in_root)
                    } else {
                        None
                    };
                    let dropped = pinned_plan_seeds_without_unnamed_watched_rooted(
                        &all_words,
                        &plan.objects,
                        &seeds,
                        is_watched,
                        is_array,
                        true,
                        need,
                    );
                    if watchref_dbg() {
                        if let Some(kept) = dropped.as_ref() {
                            eprintln!(
                                "[watchref] pinned in-place plan: {} watched object(s) no pin word names are not seeds",
                                seeds.len() - kept.len()
                            );
                        }
                        pinned_plan_watched_census(
                            &all_words,
                            &plan.objects,
                            dropped.as_deref().unwrap_or(&seeds[..]),
                            &is_watched,
                            &is_array,
                            true,
                            roots,
                            start_needs_root,
                            &start_rooted_words,
                        );
                    }
                    if let Some(kept) = dropped {
                        plan.roots = Some(kept);
                    }
                }
            }
            // gci i31/fz: a registered finalizable no pin word names is not a
            // seed either (reached: kept in place as before; unreached: Phase
            // 2.5 resurrects it in place and reports it, so its `finalize()`
            // runs as on HotSpot). Default ON since the i35 WARNING flip
            // (`=0` kills).
            if !plan.objects.is_empty() && pinned_finalizable_unnamed_not_roots_enabled() {
                let fins = pinned_plan_finalizable_members(&plan.objects, finalizer_addrs);
                if !fins.is_empty() {
                    let seeds: Vec<usize> = match plan.roots.as_ref() {
                        Some(r) => r.clone(),
                        None => plan.objects.iter().map(|&(b, _)| b).collect(),
                    };
                    let dropped = pinned_plan_seeds_without_unnamed_watched(
                        &all_words,
                        &plan.objects,
                        &seeds,
                        |b| fins.binary_search(&b).is_ok(),
                        // SAFETY: as for the watched filter above -- `b` is a
                        // plan object's base, an object start the exact pre-GC
                        // walk recorded in from-space, header whole and unwritten.
                        |b| unsafe { (*(b as *const ObjectHeader)).kind() } == ObjectKind::Array,
                    );
                    if let Some(kept) = dropped {
                        if watchref_dbg() {
                            eprintln!(
                                "[watchref] pinned in-place plan: {} finalizable object(s) no pin word names are not seeds",
                                seeds.len() - kept.len()
                            );
                        }
                        plan.roots = Some(kept);
                    }
                }
            }
            (!plan.objects.is_empty()).then_some(plan)
        } else {
            None
        };
        mv_phase!("pinned_plan");
        let in_place_cycle = pinned_plan.is_some();
        let moving_decision_reason = {
            use cratonvm_gc_core::gc_metrics::decision_reason as dr;
            if in_place_cycle {
                dr::MOVING_PINNED_PAGES
            } else if pinned_young_words.is_some() {
                self.pinned_young_census[1].fetch_add(1, Ordering::Relaxed);
                dr::MOVING_NO_YOUNG_PIN_WORDS
            } else {
                moving_decision_reason
            }
        };
        // PAST THE LAST REFUSAL GATE. Every way this cycle could have declined
        // to run is now behind us, so this collection WILL relocate — which is
        // the claim both of these counters make and the reason they were moved
        // down here from the decision block above (2026-09-21). A refused
        // cycle records its own `skipped-*` reason at its own exit and bumps
        // neither of these.
        if count_moving_young_cycle {
            // A moving (Cheney) young collection running WHILE a JIT frame is
            // live — what the architecture advertises, and what only happens
            // once every live frame proved a rewritable root map. Counted so
            // "is the young generation actually copying?" is answerable at
            // runtime (paired with the coverage-fallback counter).
            cratonvm_gc_core::gc_quiescence::record_moving_young_cycle();
        }
        cratonvm_gc_core::gc_metrics::record_collector_decision(
            "generational",
            moving_decision_reason,
            cratonvm_gc_core::gc_quiescence::incomplete_reason::NONE,
        );
        let mut objects_copied: usize = 0;
        // Read & clear the promote-on-pressure flag set by the previous
        // minor GC. When true, every survivor of THIS cycle is promoted
        // to old gen regardless of age, breaking the long-lived-tree
        // semispace death spiral. Single AtomicBool::swap so the flag
        // doesn't latch across multiple consecutive cycles unless the
        // pressure persists.
        //
        // gen r4w2/youngpolicy: "the previous minor GC" is only true when no
        // non-moving cycle ran in between. gen r4w4/young4: it now always is —
        // a non-moving divert consumes the arm itself (see the divert above),
        // so an arm read here was set by the cycle immediately before this
        // one (or survived only this same cycle's refusal-and-retry).
        let promote_pressure_arm = self.force_promote_all.swap(false, Ordering::Relaxed);
        if promote_pressure_arm {
            self.promote_pressure_census[1].fetch_add(1, Ordering::Relaxed);
        }
        // gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip): the
        // moving young half of a `System.gc()`-door full collection
        // (`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`) tenures every survivor, as
        // HotSpot Serial's mark-compact leaves young empty. Not counted as a
        // promote-on-pressure arm.
        let force_promote_all =
            promote_pressure_arm || cratonvm_gc_core::gc_quiescence::system_gc_tenure_requested();
        // gen r4w6/young6: this cycle's promotion age (the fixed
        // `PROMOTION_AGE` unless adaptive tenuring is engaged), read ONCE so
        // the serial evacuator, the parallel evacuator and the age census all
        // see the same value; and whether to fill the age table.
        let tenuring_engaged = self.tenuring.engaged(adaptive_tenuring_flag());
        // gc wave 9 (lane gcC): `NON_AGING_PROMOTION_AGE` on a cycle the
        // Serial-eden aging epoch holds (`CRATONVM_GEN_AGE_BY_SERIAL_EDEN`);
        // the copy then ages nothing (`cycle_age_step`). Unarmed: unchanged.
        let promotion_age = self.cycle_promotion_age();
        let tenuring_print =
            self.tenuring.print_requested() || (tenuring_engaged && self.verbose_gc_enabled());
        // Every per-cycle input of the serial evacuator's per-object path, read
        // once (gen r4w2/youngpolicy; see `CopyDiag`).
        let copy_policy = CopyPolicy {
            force_promote_all,
            promotion_age,
            diag: CopyDiag::current(),
        };
        // CRIT-P2 fix: use FxHashMap to avoid SipHash overhead on every
        // forwarded pointer (N hash ops per GC for N live objects).
        // Converted back to std HashMap at the end for public-API
        // compatibility (`GcResult.pointer_map` and `MonitorCleanup`).
        //
        // gc-genpause F4: pre-sized from the previous cycle's survivor count
        // (see `prev_survivor_count`). One insert per survivor into a table
        // that starts empty is ~20 reallocate-and-rehash rounds on a 1.4 M
        // survivor cycle; a live set is stable enough between consecutive
        // collections for the last count to be a good guess, and a wrong guess
        // only costs the growth it would have paid anyway. The 1/8 headroom
        // keeps the table off its own load-factor boundary when the survivor
        // count is flat, which is the common case and the one where a resize
        // would be most annoying.
        let map_hint = {
            let prev = self
                .prev_survivor_count
                .load(std::sync::atomic::Ordering::Relaxed);
            prev.saturating_add(prev / 8)
        };
        //
        // gcj j1yg-pause: reserved on the serial arm only (below). The
        // parallel arm's fold reserves each shard on the worker that fills it
        // (`PointerMap::par_extend_pairs_with`); reserving here was a serial
        // allocation and fill of the whole table inside the pause.
        let mut pointer_map: cratonvm_types::PointerMap =
            cratonvm_types::PointerMap::with_capacity_and_hasher(0, Default::default());
        // CRIT-P2 fix: explicit worklist of promoted (old-gen) objects awaiting
        // a scan. Replaces the O(promoted^2) filter loop that previously
        // rebuilt `Vec<unscanned>` from `pointer_map.values()` per iteration.
        // Populated by `forward_object` whenever an object is promoted to
        // old gen; popped by the alternating Cheney scan below.
        //
        // gen r4w4/young4: plus the serial promotion buffer — see
        // `PromotionQueue`, and its two `retire_plab` calls below (after
        // `cheney_drain`, after Phase 2.5b).
        let mut promoted_worklist = PromotionQueue::new();
        // gcd d3/m: the serial evacuator's refusals go to THIS heap's ledger.
        promoted_worklist.refusal_ledger = Some(Arc::clone(&self.forward_refusals));
        // gen r4w6/young6: the age table is filled only when something reads it.
        promoted_worklist.age_census =
            tenuring_engaged || tenuring_print || self.tenuring.xlog_requested();
        // gci i1/yg: the HotSpot age clock's step for this cycle (1 unless armed),
        // folded with gcC's epoch (0 on a held cycle; merge-dev-20261015).
        promoted_worklist.age_step = merged_age_step(promotion_age, self.age_clock_step());
        promoted_worklist.in_place_tenure_age = Some(if force_promote_all {
            0
        } else {
            clock_promotion_age(promotion_age, promoted_worklist.age_step)
        });
        // gcd d2/h (cards3 item 3, producer (c)): a promoted reference array
        // defers its young elements' cards, not its header's, on an
        // element-precise table (`HolderCardDefer`).
        let element_card_base = card_table
            .precise_ref_array_marks()
            .then(|| card_table.base_addr());
        promoted_worklist.element_card_base = element_card_base;

        // Phase timing for the MOVING (Cheney) young cycle, on the same
        // `CRATONVM_DBG=gcpause` flag that already reports the total.
        //
        // `gcphase` instruments `sweep_young_non_moving` ONLY, so a workload
        // that never diverts to the non-moving sweep reports a slow collection
        // with no breakdown at all. That is exactly the state the OAuth2
        // 500 ms read-timeout investigation hit: `[gcpause] collection took
        // 566ms` with `gcphase` completely silent, because this arm ran
        // instead. Printed only when the cycle is already slow, so the steady
        // state pays two `Instant::now()` calls and nothing else.
        // Marks go to a thread-local that `collect_garbage_inner`'s existing
        // pause timer drains on drop, so the breakdown survives every exit path
        // this function has rather than only the one that falls through.

        mv_phase!("pre_evacuate");
        // Collect additional roots from dirty cards in old gen: `extra_roots`,
        // filled by the `scan_dirty_cards` call above `pinned_plan` (gen
        // r5w2/roots6 moved it there; this phase mark now times nothing but
        // is kept so the `[gcpause]` breakdown keeps its column).
        mv_phase!("scan_dirty_cards");
        // Card marking is a fast path only. A missed barrier must retain an
        // object for one extra collection, never reclaim a reachable child.
        if Self::full_old_rset_scan_enabled() {
            Self::scan_all_old_to_young(&old_gen, &young_from, &mut extra_roots);
        }
        // gc-genpause F2: the audit that replaces the unconditional scan. Runs
        // BEFORE Phase 1b, which rewrites the very slots it inspects.
        if gc_flags().verify_rset {
            let _ = Self::verify_old_to_young_rset(&old_gen, &young_from, &extra_roots, "moving");
        }
        mv_phase!("full_old_rset_scan");

        // Old-gen addresses needing card re-mark after Phase 3's `clear_all()`
        // (applied via `mark_dirty_bulk`). Declared before the seed phases so a
        // PERSISTENT old→young edge processed via a dirty card whose referent
        // STAYS young is re-remembered — otherwise the edge is remembered for
        // exactly one cycle (the promoting one) and the cycle after the dirty-
        // card fixup forgets it.
        //
        // gen r5w2/roots6 (cards3 item 2, `gengc-r4w3-cards3-precise-array-cards-residuals-FIXED-20260929.md`):
        // `(holder, offset)` pairs. The card re-marked is `holder + offset`,
        // with `holder` remapped first if Phase 5 compacts it. `offset` is 0
        // (the header card, "scan the whole object") for every producer
        // except `seed_dirty_card_roots` on an element-precise table, which
        // re-marks the reference-array ELEMENT's own card -- so a wide old
        // array with one young element keeps a one-card entry instead of
        // turning into a whole-array rescan at every later young pause.
        let mut deferred_dirty_cards: Vec<(usize, usize)> = Vec::new();
        // gen r5w2/roots6: a walk-gap word's card stays dirty across this
        // cycle's `clear_all()`, so the next young pause looks at it again --
        // the object it names stayed young (pinned in place, above), and the
        // gap bytes never move (every compaction refuses over a gap).
        deferred_dirty_cards.extend(card_gap_roots.card_addrs.iter().map(|&a| (a, 0)));
        // Whether the table carries element-precise reference-array marks
        // (`CRATONVM_GC_PRECISE_ARRAY_CARDS`); read once for the seeds below.
        let precise_array_cards = card_table.precise_ref_array_marks();
        // The same range test `OldGen::contains` performs, as two plain words.
        // The parallel evacuator holds the old-gen allocator behind a lock and
        // must not have to take it just to ask "did this land in old gen?".
        let old_extent = old_gen.extent();
        // HIB-CV-24: a live object keeps its class's defining ClassLoader alive
        // (the instance→loader edge HotSpot gets via `Class.getClassLoader`).
        // Cached once; `false` (and the registry's empty short-circuit) makes the
        // per-object lookup below a no-op for the common no-custom-loader case.
        let loader_pin_on = cratonvm_types::loader_pin::loader_pinning_enabled();
        // gci i10/lp: one snapshot of this from-space's loader_pin rows for the
        // serial copy, Phase 2.5b and the re-drains (opt-in; `None` keeps the
        // per-object lookup). The parallel arm takes its own in `ParEvac::new`.
        promoted_worklist.loader_pins = if loader_pin_on {
            crate::gen_evac::young_loader_pin_snapshot(|a| young_from.contains(a as *mut u8))
        } else {
            None
        };
        // gcj yg3-residuals R3: the copy keeps at most one survivor young.
        promoted_worklist.to_space_cap = self.young_geometry.map_or(usize::MAX, |g| g.survivor);
        // gci i8/pa (P-A of `gengc-core-system-gc-forces-non-moving-20260920`,
        // `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`, opt-in): a moving
        // `System.gc()` whose root scan deferred young side-table values to
        // their owners (the licence, `explicit_full_gc_follows_side_tables`).
        // Asked here, while the request is still pending (Phase 5 takes it),
        // so it is the very answer the root gatherer read on this thread.
        // Its young values are forwarded below as young-phase roots that are
        // NOT appended to `roots`: the copy keeps them, and the same cycle's
        // major (Phase 5) judges them through their owners. Empty (and no
        // registry read) on every other cycle.
        //
        // gci i9/yl (the residual 3c of `i8-pa-report.md`): the copy now
        // FOLLOWS the rows instead of keeping every young value. A row is
        // followed when its owner is live for this young phase: an owner
        // outside young from-space (old: minor-GC semantics treat it as live)
        // is seeded here, in Phase 1, in both evacuator arms; a young owner is
        // followed once the closure has forwarded it
        // (`side_table_values_due`, after the drain and the loader rescue,
        // and again inside Phase 2.5b for a resurrected owner). So a dead
        // YOUNG loader's young mirror is no longer copied, and the loader --
        // which its mirror's `classLoader` field used to keep -- dies with it
        // and unloads on this `System.gc()`, as on HotSpot. Every source table
        // of an uncopied value is keyed by that dead owner (a loader's
        // statics, mirrors and caches; an unstarted child's ITL bucket), the
        // very invariant the in-place licensed sweep already relies on.
        // `loader_pin` values are no longer seeded: copied and in-place
        // instances forward their loader themselves, and old instances' young
        // loaders are the loader rescue's (`loaders_of_old_instances`), which
        // also covers a non-strong hidden class's `loader_pin` mirror.
        let side_table_licence = cratonvm_gc_core::gc_quiescence::moving_system_gc_follows_side_tables();
        // gcj j3/yg3 (`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS`, default ON):
        // the root gatherer deferred young user mirrors to their loaders'
        // `mirror_pin` rows on THIS thread's scan and, instead of forcing the
        // in-place sweep, asked this young phase to follow the rows the way
        // the licensed moving `System.gc()` does (old owners seeded here, a
        // young owner once the closure forwarded it). The licence's Phase 5
        // half (the P-C re-key, `pc_rekey_loaders` below) stays the
        // `System.gc()`'s alone: this cycle's major, if any, roots its own.
        let side_table_follow = side_table_licence || young_side_table_rows_follow();
        let side_table_young_rows: Vec<(usize, Vec<usize>)> = if side_table_follow {
            side_table_young_rows_of(
                cratonvm_types::mirror_pin::snapshot().as_ref(),
                cratonvm_types::metadata_pin::snapshot().as_ref(),
                |a| young_from.contains(a as *mut u8),
            )
        } else {
            Vec::new()
        };
        // One flag per row: followed already (each row is followed once).
        let mut side_table_rows_done: Vec<bool> = vec![false; side_table_young_rows.len()];
        // Phase 1 seeds: the rows of every owner outside young from-space,
        // marked done here (their values are forwarded below as roots).
        let side_table_young_seeds: Vec<usize> = side_table_values_due(
            &side_table_young_rows,
            &mut side_table_rows_done,
            |a| young_from.contains(a as *mut u8),
            |_| false,
        );
        // Cheney bump cursor over young to-space, and the promoted-scan dedup
        // set. Both outlive the evacuator branch below because the finalizer
        // resurrection pass (Phase 2.5b) continues the same scan.
        let mut scan_cursor: usize = 0;
        // CRIT-P2 fix: FxHashSet (replaces std HashSet/SipHash) for cheap
        // dedup of promoted-object scans.
        let mut scanned_promoted: FxHashSet<usize> = FxHashSet::default();

        // ---- Which evacuator copies this cycle's survivors ----
        //
        // The parallel copy phase reuses the young collector's existing worker
        // policy, so it engages on exactly the cycles whose MARK phase already
        // goes parallel: a young generation past `CRATONVM_GC_PAR_MIN_BYTES`,
        // or an explicit `CRATONVM_GC_PAR_THREADS`. `ParEvac::plan` then has
        // the last word, and it declines (countably, via
        // `PAR_EVAC_DECLINED_SLACK`) only when to-space cannot cover from-space
        // at all — which the Cheney backstop above has already refused. A
        // thin-slack cycle runs BUFFERLESS rather than declining.
        //
        // Falling back to the serial copy is always correct: the two evacuators
        // seed from the same three sources (`seed_roots` and friends) and reach
        // the same forwarding map. That does NOT make `CRATONVM_GC_PAR_EVAC=0`
        // behaviour-neutral (gengc-round4-move, 2026-09-23 — this comment used
        // to say it was): to-space becomes the NEXT cycle's from-space, and the
        // parallel arm leaves PLAB tail fillers in it that the serial arm never
        // writes, so the next cycle's object-start walk, its anchors and its
        // trigger point all differ. (gce e2/o removed the second lever,
        // `CRATONVM_GC_PAR_EVAC_BUFFERLESS`, which kept the copy parallel and
        // the to-space dense; `gen_evac::EvacPlan::into_bufferless` is now a
        // unit-test helper.)
        // gen r4w5/pinned5: a pinned in-place cycle takes the SERIAL evacuator.
        // Its destinations are the free spans of from-space, scattered and
        // interleaved with live objects, not the contiguous to-space tail
        // `ParEvac::plan` reserves and carves into per-worker buffers; and a
        // pinned object's identity forward would need a CAS-claimed side bit
        // in every worker. The pinned set is a handful of objects (2.1 per
        // cycle on average, 9 at most, in the census that sized the design),
        // so the cost is the parallel copy's speed-up on those cycles only.
        //
        // gen r5w3/evac7 (limit L1, `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`):
        // the four changes that cost names are built (`gen_evac::ParInPlace`),
        // so with the flag a pinned in-place cycle takes the parallel
        // evacuator too — its young destinations the same from-space spans,
        // shared as a span pool, and no to-space region at all
        // (`EvacPlan::in_place`). Without the flag, byte-for-byte as before.
        let par_in_place = in_place_cycle && gc_flags().gen_pinned_young_copy_parallel;
        // gcg g6/yg (`gcg-g5oo-parallel-young-evacuation-aborts-when-promotion-and-plab-both-refuse`):
        // with both generations nearly full (`promotion_oom_risk`) the copy
        // runs SERIAL. The parallel arm's to-space region loses bytes to PLAB
        // tail fillers and to forwarding-CAS losers' abandoned copies, so a
        // young set that is all live with old gen full can exhaust it, and
        // `ParEvac::evacuate` then aborts the process (vg3 `o3_mirror_gen_*`,
        // 4/4, rc 134). The serial Cheney copy wastes nothing: to-space is a
        // semi-space as large as from-space, so every survivor old gen
        // refuses still fits there -- HotSpot's promotion-failure answer
        // (keep it young), after which the ladder's full collection and
        // `OutOfMemoryError` decide. An in-place cycle never needs to-space.
        let par_workers = if parallel_young_copy_permitted(
            gc_flags().gc_par_evac,
            in_place_cycle,
            par_in_place,
            promotion_oom_risk,
        ) {
            // gcj yg3-residuals (`CRATONVM_GEN_COPY_WORKERS_BY_SURVIVORS`):
            // sized by what the copy will move, not by eden's occupancy.
            copy_workers_for_survivors(
                cratonvm_gc_core::young_mark::young_gc_threads(bytes_before),
                self.last_collection_usage[0]
                    .load(Ordering::Relaxed)
                    .max(self.pinned_survivor_estimate.load(Ordering::Relaxed)),
                !cratonvm_gc_core::young_mark::young_gc_threads_configured(),
            )
        } else {
            1
        };
        let par_plan = if par_workers >= 2 {
            let (to_cursor_addr, to_headroom) = young_to.parallel_evacuation_region();
            if in_place_cycle {
                Some(crate::gen_evac::EvacPlan::in_place(
                    to_cursor_addr,
                    par_workers,
                ))
            } else {
                crate::gen_evac::ParEvac::plan(
                    to_headroom,
                    to_cursor_addr,
                    young_from.used(),
                    par_workers,
                )
            }
        } else {
            None
        };

        // Map what the workers may touch. The backing store commits lazily
        // (`cratonvm_gc_core::reservation`), and the parallel evacuator is the one
        // hand-out site that does not go through `Arena::hand_out` — it hands
        // a span to N threads to sub-allocate rather than one object to one
        // caller. Without this every worker writes into reserved-but-unmapped
        // memory: STATUS_ACCESS_VIOLATION, every parallel cycle, silently.
        // A refused commit is an allocation failure, so it takes the serial
        // path exactly as `hand_out`'s `None` does. (A parallel in-place plan
        // touches no to-space byte; its from-space spans are committed when
        // the parallel arm below arms them, as on the serial arm.)
        let par_plan = par_plan
            .filter(|p| in_place_cycle || young_to.commit_parallel_evacuation_region(p.reserved));

        // gci i40/gn (`CRATONVM_DBG_GEN_FIN_CHILD_CENSUS`, default OFF): the
        // pinned plan's seeds, kept for Phase 2.5's holder census only. Empty
        // (never filled) with the census off or on a cycle with no pinned plan.
        let fin_child_census = fin_child_census_enabled();
        let mut fin_census_seeds: Vec<usize> = Vec::new();
        // t1 wave 2 lane W (`CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_IN_PLACE`):
        // may this in-place cycle's Phase 5 major take P-C, and the plan's
        // seeds it must then root (`moving_major_gate_in_place_term`). Not
        // on a cycle the take-over / unproven-frame arms took, whose ledgers
        // stand in for frames with no proven oop map. Empty on every other
        // cycle. gcj hmg: decided per cycle
        // (`pc_in_place_refused_for_ledger_arms`).
        let pc_in_place_admitted = in_place_cycle
            && !pc_in_place_refused_for_ledger_arms(
                pinned_ledger_stands_in,
                cratonvm_gc_core::gc_quiescence::pinned_young_copy_takeover_enabled(),
                pinned_copy_takes_unproven_frames_enabled(),
                moving_major_in_place_per_cycle_arms_enabled,
            );
        let mut pc_in_place_seeds: Vec<usize> = Vec::new();

        if let Some(plan) = par_plan {
            // ------------------- PARALLEL COPY PHASE -------------------
            crate::gen_evac::note_par_evac_cycle();
            // The POLICY's width, kept for sizing the pool below.
            let policy_workers = par_workers;
            // It is the plan's number that sizes the shards, the pool dispatch
            // and the termination handshake. Today it equals the policy's ask
            // (`plan` stopped narrowing when thin-slack cycles learned to run
            // bufferless — the comment here used to say it "may have scaled
            // the count DOWN"); the shadowing is kept so a plan that narrows
            // again has one number to change.
            let par_workers = plan.workers;
            // gcj pcp: the previous cycle's shards, emptied, their lists'
            // capacity kept (`EvacShard::recycled`).
            let mut shards: Vec<crate::gen_evac::EvacShard> =
                std::mem::take(&mut *self.evac_shard_scratch.lock());
            shards.resize_with(par_workers, Default::default);
            // gen r4w6/young6 — proposal W5 of the parallel-evacuator scaling
            // page: pre-size each worker's forward list from the previous
            // cycle's survivor count (the same `map_hint` the pointer map is
            // sized from), split evenly. `forwards` takes one push per object
            // copied — the hottest write of the pause — and started every cycle
            // empty, so a worker copying ~180 K objects (1.4 M survivors over
            // eight workers) paid ~17 reallocate-and-copy rounds inside the
            // drain, on its own thread. The reservation itself is one
            // allocation per worker on the driver; the pages are touched by the
            // worker that fills them. A wrong guess costs only the growth it
            // would have paid anyway, as for `pointer_map`.
            let per_worker_hint = map_hint / par_workers.max(1);
            for shard in shards.iter_mut() {
                shard.forwards.reserve(per_worker_hint);
            }
            // gen r5w3/evac7 (L1): a pinned in-place cycle is armed exactly as
            // the serial arm arms it (`arm_in_place_young_cycle`), and the
            // workers get a span-pool VIEW of that state; after the drain the
            // view's results are folded back into it (`absorb_parallel_in_place`)
            // so every later serial phase — the loader rescue, finalizer
            // resurrection, the rebuild — sees the same state a serial
            // in-place cycle leaves. `None` on every other cycle.
            let (mut in_place_state, in_place_seeds): (Option<InPlaceEvac>, Vec<usize>) =
                match pinned_plan {
                    Some(pp) => {
                        let (ip, seeds) = Self::arm_in_place_young_cycle(
                            &mut young_from,
                            pp,
                            young_base,
                            young_used,
                        );
                        (Some(ip), seeds)
                    }
                    None => (None, Vec::new()),
                };
            if fin_child_census {
                fin_census_seeds.clone_from(&in_place_seeds);
            }
            if pc_in_place_admitted {
                pc_in_place_seeds = pc_in_place_seeds_of(&in_place_seeds, pc_in_place_names.as_deref());
            }
            let in_place_view = in_place_state
                .as_ref()
                .map(|ip| crate::gen_evac::ParInPlace::new(&ip.pinned, &ip.dest));
            // gen r5w3/evac7 (proposal W2, `CRATONVM_GC_PAR_EVAC_CARD_SEED`):
            // forward the dirty-card roots on the workers rather than on the
            // driver before the pool opens. Not under the full old-gen rset
            // scan, whose extra roots can repeat a slot the card scan already
            // reported (two workers would then write the same slot).
            let card_seed_par =
                gc_flags().gc_par_evac_card_seed && !Self::full_old_rset_scan_enabled();
            let to_cursor_end;
            {
                let evac = crate::gen_evac::ParEvac::new(
                    young_from.base_ptr() as usize,
                    young_from.used(),
                    young_from.capacity(),
                    &young_object_starts,
                    young_to.base_ptr() as usize,
                    young_to.capacity(),
                    &plan,
                    &mut old_gen,
                    force_promote_all,
                    promotion_age,
                    loader_pin_on,
                    fwd_resolve_strict(),
                )
                .with_element_cards(element_card_base)
                // gcf f2/y: the workers fill the age table (CAS winners only).
                .with_age_census(promoted_worklist.age_census)
                // gci i1/yg: the HotSpot age clock's step.
                .with_age_step(promoted_worklist.age_step)
                // gcj yg3-residuals R3: at most one survivor stays young.
                .with_to_space_cap(self.young_geometry.map(|g| g.survivor))
                // gcj pcp: precise edges skip the object-start proof.
                .with_precise_edges(precise_edges);
                let evac = match in_place_view.as_ref() {
                    Some(view) => evac.with_in_place(view),
                    None => evac,
                };
                {
                    // Phases 1 / 1a / 1b run on the DRIVER, into shard 0 —
                    // the same three seeds the serial arm below uses. Only
                    // `forward` differs.
                    let driver = &mut shards[0];
                    let mut forward = |p: *mut u8| -> *mut u8 {
                        // SAFETY: `p` is an aligned candidate address inside
                        // young from-space; every guard needed to decide
                        // whether it is a real object start is inside
                        // `evacuate`, which refuses (returning `p`) otherwise.
                        unsafe { evac.evacuate(driver, p) }
                    };
                    // gen r5w3/evac7 (L1): every SEED of the plan is a ROOT of
                    // the cycle, claimed in place before anything else can
                    // reach it — the serial arm's order. Empty otherwise.
                    // (gcj j2/pn2: the seeds are every planned object only when
                    // the plan's `roots` is `None`; the exact / named rules and
                    // the default-ON watched / finalizable filters narrow them.)
                    for &b in &in_place_seeds {
                        let _ = forward(b as *mut u8);
                    }
                    seed_roots(roots, &young_from, &mut forward);
                    mv_phase!("root_forward");
                    seed_overlay_roots(&young_from, &mut forward);
                    // gci i8/pa: the licensed moving System.gc()'s young
                    // side-table values (empty otherwise). gci i9/yl: only
                    // those of owners outside young from-space; a young
                    // owner's rows are followed after the drain.
                    seed_side_table_young_values(&side_table_young_seeds, &mut forward);
                    mv_phase!("overlay_forward");
                    if !card_seed_par {
                        // SAFETY: `extra_roots` names in-bounds reference slots
                        // of live old-gen objects (the dirty-card scan's own
                        // output).
                        unsafe {
                            seed_dirty_card_roots(
                                &extra_roots,
                                &|p| young_from.contains(p),
                                old_extent,
                                precise_array_cards,
                                &mut deferred_dirty_cards,
                                &mut forward,
                            );
                        }
                    }
                    mv_phase!("card_root_forward");
                }
                // Size the pool from the POLICY, not from whoever built it
                // first (gengc-round4-move, 2026-09-23; part (b) of
                // `gengc-alloc-evac-pool-width-frozen`). The pool lives in a
                // `OnceLock` and is built at the width of its FIRST request;
                // `ensure_helpers` is a lock-free no-op once the pool is wide
                // enough and spawns the difference (once, off every later
                // cycle's path) when it is not. `drain` still clamps its
                // dispatch to `plan.workers`.
                let pool = self.evac_pool(policy_workers);
                pool.ensure_helpers(policy_workers.saturating_sub(1));
                if card_seed_par {
                    // gen r5w3/evac7 (W2): worker `w` of `n` forwards the
                    // `w`-th equal slice of the card roots into its own shard,
                    // then joins the closure. Captures only `Sync` data (the
                    // roots and three scalars), never the arena guard: the
                    // young test is the evacuator's own extent.
                    let card_roots: &[(ObjectRef, usize, usize)] = &extra_roots;
                    // gcj pcp: the card roots are CLAIMED in chunks off one
                    // cursor rather than cut into one slice per worker index:
                    // the drain is an elastic dispatch, so a helper that never
                    // runs must leave no slice unseeded.
                    let card_next = AtomicUsize::new(0);
                    let seed: crate::gen_evac::WorkerSeed<'_, '_> =
                        &|ev, shard, _worker, _workers| {
                            let n = card_roots.len();
                            let mut deferred: Vec<(usize, usize)> = Vec::new();
                            loop {
                                let lo = card_next.fetch_add(CARD_SEED_CHUNK, Ordering::Relaxed);
                                if lo >= n {
                                    break;
                                }
                                let hi = (lo + CARD_SEED_CHUNK).min(n);
                                let mut fwd = |p: *mut u8| -> *mut u8 {
                                    // SAFETY: as for the driver's `forward`
                                    // above; `p` is a dirty-card slot's value
                                    // (gcj pcp: a precise edge).
                                    unsafe { ev.evacuate_precise(shard, p, true) }
                                };
                                // SAFETY: a chunk of the dirty-card scan's own
                                // output, as on the driver.
                                unsafe {
                                    seed_dirty_card_roots(
                                        &card_roots[lo..hi],
                                        &|p| ev.in_from_extent(p as usize),
                                        old_extent,
                                        precise_array_cards,
                                        &mut deferred,
                                        &mut fwd,
                                    );
                                }
                            }
                            shard.deferred_dirty_cards.extend(deferred);
                        };
                    // SAFETY: as for `drain` below; the seed forwards through
                    // `evacuate` only.
                    unsafe { evac.drain_seeded(pool, &mut shards, Some(seed)) };
                } else {
                    // SAFETY: every queued address is a destination the seeds
                    // above produced — a live copy in young to-space or in old
                    // gen (or, on a pinned in-place cycle, a from-space span
                    // copy or an object claimed in place).
                    unsafe { evac.drain(pool, &mut shards) };
                }
                // The copy proper ends here. Without this mark the next one
                // (`map_merge`) covers the drain as well, and a breakdown that
                // wide is a hypothesis rather than a measurement — the same
                // mistake `pre_evacuate` made before gc-genpause F0 split it.
                mv_phase!("evac_drain");
                for shard in shards.iter_mut() {
                    evac.retire_plab(shard);
                }
                to_cursor_end = evac.to_cursor_end();
            }
            // `evac` is dropped here: `old_gen` is borrowable again, and no
            // worker is still running — `EvacPool::scope`'s completion barrier
            // is what `drain` returns through, and it is also the
            // happens-before edge for reading the shards below.

            // gen r5w3/evac7 (L1): undo the in-place claims' self-forwards and
            // fold the workers' in-place results into the serial state BEFORE
            // anything below reads a from-space header (the refusal audit, the
            // age census) or forwards serially (the loader rescue, Phase 2.5).
            if let Some(mut ip) = in_place_state.take() {
                Self::absorb_parallel_in_place(&mut ip, in_place_view.as_ref(), &shards);
                promoted_worklist.in_place = Some(Box::new(ip));
                // t1 w8 PIN10 / gcki-pin-age: the serial arm's aging of the
                // survivors that stay in place, on the words
                // `absorb_parallel_in_place` just restored. Each claimed
                // object is on exactly one shard (one self-forward CAS won),
                // so each ages once.
                if promoted_worklist.in_place_tenure_age.is_some() {
                    for shard in &shards {
                        for &(addr, _) in
                            shard.in_place_pinned.iter().chain(shard.in_place_kept.iter())
                        {
                            // SAFETY: a from-space object base this cycle's
                            // workers claimed in place, its mark word restored
                            // above; every worker has returned and the pause
                            // is stopped.
                            let size =
                                gen_object_total_size(unsafe { &*(addr as *const ObjectHeader) });
                            Self::age_in_place_survivor_serial(
                                addr as *mut u8,
                                size,
                                &mut promoted_worklist,
                            );
                        }
                    }
                }
            }

            // gcj pcp: the in-place fold-back on its own row (it was inside
            // `map_merge`).
            mv_phase!("in_place_absorb");
            // Make every byte below the new cursor walkable BEFORE publishing
            // it. This arena becomes the NEXT cycle's from-space, and that
            // cycle's object-start walk strides it object by object.
            for shard in &shards {
                for &(addr, size) in &shard.plab_gaps {
                    // SAFETY: a retired per-worker buffer's tail — dead,
                    // 8-aligned, and inside the region `plan` reserved.
                    unsafe { crate::gen_evac::install_gap_filler(addr, size) };
                }
            }
            let to_base = young_to.base_ptr() as usize;
            young_to.commit_parallel_evacuation(to_cursor_end);

            // Merge the per-worker shards. EVERY forward is recorded, including
            // the ones a worker only OBSERVED (a lost forwarding CAS, an
            // already-forwarded hit): `update_all_roots` can only remap a root
            // whose old address is a key here, and the equivalent omission in
            // G1's parallel evacuator was a live root-dangling defect. (gen
            // r4w4/young4: an already-forwarded hit on a young TO-space target
            // is not pushed a second time — its CAS winner's shard carries it.)
            let to_cap_end = to_base + young_to.capacity();
            // Shard 0 is the driver's. Everything above it was copied by a
            // helper thread, and that split is the only evidence that the
            // parallel evacuator actually spread the work rather than merely
            // starting threads — see `PAR_EVAC_HELPER_SCANS`.
            crate::gen_evac::note_par_evac_helper_scans(
                shards
                    .iter()
                    .skip(1)
                    .map(|s| s.objects_scanned as u64)
                    .sum(),
            );
            // The two arms whose absence would not fail now: promotion takes
            // the old generation's lock (the copy phase's only shared-lock
            // contention point), and the deferred card is the old→young edge
            // whose loss surfaces a cycle later, somewhere else.
            crate::gen_evac::note_par_evac_arms(
                shards.iter().map(|s| s.tally[1]).sum(),
                shards
                    .iter()
                    .map(|s| s.deferred_dirty_cards.len() as u64)
                    .sum(),
            );
            for shard in &shards {
                objects_copied += shard.objects_copied;
                copy_tally_merge(&shard.tally);
                deferred_dirty_cards.extend(shard.deferred_dirty_cards.iter().copied());
                // The workers' refusals, into the SAME per-cycle ledger (and
                // the same `CRATONVM_DBG_ROOT_REMAP_AUDIT` report) the serial
                // evacuator writes (gengc-round4-move, 2026-09-23). From-space
                // is not reset yet, so the audit's header reads are still valid.
                for &(addr, reason) in &shard.refusals {
                    Self::note_forward_refusal(
                        Some(&*self.forward_refusals),
                        addr as *mut u8,
                        reason,
                        &young_from,
                        &young_object_starts,
                    );
                }
                // gcf f2/y: the age table comes from the workers' own tables,
                // filled by the forwarding-CAS winner at copy time
                // (`ParEvac::with_age_census`), to-space copies and in-place
                // span copies alike. It used to be re-read here from every
                // `forwards` pair's destination header: one random header
                // read per survivor on the driver, and a CAS loser's repeated
                // pair counted its object twice (biasing the threshold down).
                if promoted_worklist.age_census {
                    for (age, &bytes) in shard.ages.iter().enumerate() {
                        if bytes > 0 {
                            // `age` < `SHARD_AGE_TABLE_LEN` = 16, so it fits.
                            promoted_worklist.note_survivor_age(age as u8, bytes as usize);
                        }
                    }
                }
                // An in-place plan cannot allocate in young_to: every
                // destination is in from-space or old gen. Its from-space
                // anchors are rebuilt from `ip.live` at the end. Avoid a
                // serial pass over every forward just to reject all of them.
                if !in_place_cycle {
                    for &(_old, new) in &shard.forwards {
                        if new >= to_base && new < to_cap_end {
                            // perf/gc-oracle-anchors: a to-space destination is an
                            // object base, and after the swap this arena is the
                            // next cycle's from-space. Anchoring keeps the parallel
                            // sweep available there instead of starting the epoch
                            // with an empty anchor table.
                            young_to.note_object_start(new - to_base);
                        }
                    }
                }
            }
            // gcj pcp: the per-shard merges (refusals, ages, anchors) on their
            // own row; `map_merge` is the map build alone.
            mv_phase!("shard_fold");
            // gen-gc-five item 5: the shards' pair lists become the map on
            // `par_workers` threads rather than one insert at a time here.
            // With the copy itself parallel this fold was the largest
            // sequential term left in the pause (`map_merge` 15-25 ms of
            // ~105 ms on the r1 A/B); see `cratonvm_types::pointer_map`.
            let forwards: Vec<Vec<(usize, usize)>> = shards
                .iter_mut()
                .map(|s| std::mem::take(&mut s.forwards))
                .collect();
            // gce e1/y: on this heap's persistent pool (the evacuation that
            // just returned was its last dispatch), not on fresh threads. A
            // helper the pool cannot supply leaves its chunk to the fold's own
            // catch-up on this thread (`par_extend_pairs_with`).
            let map_pool = (par_workers > 1).then(|| self.evac_pool(par_workers));
            {
                // gcj pcp (`CRATONVM_GEN_POINTER_MAP_SORTED`, default ON, `=0`
                // kills): the map in the SORTED form, built on the pool by
                // G1's region bucket sort over young from-space in 1 MiB
                // bands, into the heap's kept buffers. The serial phases'
                // later inserts join its overlay without a thaw
                // (`cratonvm_types::pointer_map`). The hashed fold below was
                // one hash insert per survivor into tables allocated and
                // faulted in every pause (`map_merge` 24-34 ms on JIT-warm
                // javac). Every key is a from-space object; one outside the
                // bands falls back to the serial sorted build, same answer.
                debug_assert!(pointer_map.is_empty());
                let sources: Vec<&[(usize, usize)]> = forwards.iter().map(Vec::as_slice).collect();
                let bands = young_from.capacity().div_ceil(1 << YOUNG_POINTER_MAP_BAND_SHIFT);
                pointer_map = cratonvm_gc_core::region::sorted_pointer_map_by_region(
                    &sources,
                    young_from.base_ptr() as usize,
                    YOUNG_POINTER_MAP_BAND_SHIFT,
                    bands,
                    par_workers,
                    &|jobs, job| {
                        let next = AtomicUsize::new(0);
                        let body = |_helper: usize| loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= jobs {
                                break;
                            }
                            job(i);
                        };
                        // gcj pcp: elastic -- every job is claimed off `next`.
                        match map_pool {
                            Some(pool) if jobs > 1 => pool.scope_elastic(
                                jobs.saturating_sub(1).min(pool.helpers()),
                                &body,
                                || body(0),
                            ),
                            _ => body(0),
                        }
                    },
                    Some(&self.sorted_map_buffers),
                );
            }
            // gcj pcp: the lists go back to their shards, and the shards to
            // the heap for the next cycle.
            for (shard, list) in shards.iter_mut().zip(forwards) {
                shard.forwards = list;
            }
            *self.evac_shard_scratch.lock() =
                shards.into_iter().map(crate::gen_evac::EvacShard::recycled).collect();
            mv_phase!("map_merge");
            // The parallel drain reached a fixpoint over BOTH destinations, so
            // everything below the cursor is scanned. Phase 2.5b's resurrection
            // scan appends above it.
            scan_cursor = young_to.used();
        } else {
            // -------------------- SERIAL COPY PHASE --------------------
            pointer_map.reserve(map_hint);
            //
            // gen r4w5/pinned5: on a pinned in-place cycle, arm the evacuator
            // with the pinned objects and its destination spans before the
            // first forward. The destinations are the bytes of from-space that
            // were FREE at the start of the cycle — its free blocks and its
            // un-bumped tail (the latter bounded, and committed here, since
            // the lazy backing may not have mapped it) — minus the pinned
            // pages, so a copy never lands on a byte a live object occupies,
            // nor on a page a compiled frame holds a raw word into.
            // (gen r5w3/evac7: `arm_in_place_young_cycle`, shared with the
            // parallel arm; the arming itself is unchanged.)
            let pinned_seeds: Vec<usize> = match pinned_plan {
                Some(plan) => {
                    let (ip, seeds) = Self::arm_in_place_young_cycle(
                        &mut young_from,
                        plan,
                        young_base,
                        young_used,
                    );
                    promoted_worklist.in_place = Some(Box::new(ip));
                    seeds
                }
                None => Vec::new(),
            };
            if fin_child_census {
                fin_census_seeds.clone_from(&pinned_seeds);
            }
            if pc_in_place_admitted {
                pc_in_place_seeds = pc_in_place_seeds_of(&pinned_seeds, pc_in_place_names.as_deref());
            }
            // Scoped so the closure's `&mut` borrows of young_to / old_gen /
            // pointer_map end before the drain below takes them.
            {
                let mut forward = |p: *mut u8| -> *mut u8 {
                    Self::forward_object(
                        &young_from,
                        &young_object_starts,
                        &mut young_to,
                        &mut old_gen,
                        p,
                        &mut objects_copied,
                        &mut pointer_map,
                        &mut promoted_worklist,
                        copy_policy,
                    )
                };
                // gen r4w5/pinned5: every SEED of the plan is a ROOT of the
                // cycle — forwarded to itself (side bit set, queued for its
                // in-place scan) before anything else can reach it. (gcj
                // j2/pn2: every planned object only when the plan's `roots` is
                // `None`; see `arm_in_place_young_cycle`.)
                for &b in &pinned_seeds {
                    let _ = forward(b as *mut u8);
                }
                seed_roots(roots, &young_from, &mut forward);
                mv_phase!("root_forward");
                seed_overlay_roots(&young_from, &mut forward);
                // gci i8/pa: as in the parallel arm.
                seed_side_table_young_values(&side_table_young_seeds, &mut forward);
                mv_phase!("overlay_forward");
                // SAFETY: as in the parallel arm — `extra_roots` names in-bounds
                // reference slots of live old-gen objects.
                unsafe {
                    seed_dirty_card_roots(
                        &extra_roots,
                        &|p| young_from.contains(p),
                        old_extent,
                        precise_array_cards,
                        &mut deferred_dirty_cards,
                        &mut forward,
                    );
                }
            }
            mv_phase!("card_root_forward");

            // Phase 2 + 2b: Combined Cheney scan and promoted object scan.
            //
            // We alternate between scanning young_to (standard Cheney) and
            // scanning newly promoted old-gen objects until both are fully
            // processed. This is necessary because:
            //   - Scanning a young_to object may forward a reference that gets
            //     promoted to old gen (needs promoted scan).
            //   - Scanning a promoted old-gen object may forward a reference
            //     that lands in young_to (needs Cheney scan) or gets promoted
            //     itself (needs another promoted scan iteration).
            loop {
                let mut made_progress = false;

                // Cheney scan: process any unscanned objects in young_to
                while scan_cursor < young_to.used() {
                    made_progress = true;
                    // SAFETY: `scan_cursor` is within `young_to.used()` and advances by
                    // `total_size` per object, so this points to a valid object header
                    // in the young to-space arena.
                    let obj_ptr = unsafe { young_to.base_ptr_mut().add(scan_cursor) };
                    // SAFETY: `obj_ptr` points to a copied/promoted object with a valid header.
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    let total_size = gen_object_total_size(header);
                    // HIB-CV-24: capture the class id before any forwarding may grow
                    // young_to (which would invalidate `header`).
                    let cid = header.class_id.as_u32();

                    // Scan/forward ref slots. Ref arrays + compact objects store
                    // 8-byte pointers; legacy objects store 16-byte Value cells.
                    // SAFETY: `obj_ptr`/`header` are a valid copied object in young_to.
                    unsafe {
                        forward_ref_slots(obj_ptr, header, |ref_ptr| {
                            if young_from.contains(ref_ptr) {
                                Some(Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                ))
                            } else {
                                None
                            }
                        });
                    }

                    // HIB-CV-24: keep this object's defining ClassLoader alive. If the
                    // loader is a young object, evacuate it like any other survivor so
                    // a live instance pins its loader (else a leaked instance's loader
                    // would be wrongly reclaimed once the side-table stops rooting it).
                    if loader_pin_on {
                        if let Some(loader_old) = young_loader_pin(
                            cid,
                            &young_from,
                            promoted_worklist.loader_pins.as_ref(),
                        ) {
                            let lp = loader_old as *mut u8;
                            if young_from.contains(lp) {
                                Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    lp,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                );
                            }
                        }
                    }

                    scan_cursor += total_size;
                }

                // gen r4w5/pinned5: the in-place scan — pinned objects, copies
                // into from-space free spans, kept survivors. Empty on every
                // other cycle.
                while let Some(obj_ptr) = promoted_worklist.pop_in_place() {
                    made_progress = true;
                    Self::scan_in_place_young(
                        obj_ptr,
                        &young_from,
                        &young_object_starts,
                        &mut young_to,
                        &mut old_gen,
                        &mut objects_copied,
                        &mut pointer_map,
                        &mut promoted_worklist,
                        copy_policy,
                        loader_pin_on,
                    );
                }

                // Promoted object scan: process any unscanned promoted objects.
                //
                // CRIT-P2 fix: drain the explicit `promoted_worklist` instead of
                // rebuilding a Vec from `pointer_map.values()` on every outer
                // iteration (which was O(promoted^2) until fixpoint). Every
                // `forward_object` call that promotes an object to old gen
                // pushes its new address onto `promoted_worklist`, so popping
                // here is true Cheney-style O(promoted) scanning.
                //
                // `forward_object` only pushes on a fresh promotion (not on the
                // already-forwarded re-encounter path), so the worklist holds
                // each promoted address at most once. The `scanned_promoted`
                // check below is kept as a defensive idempotency guard.
                while let Some(obj_ptr) = promoted_worklist.pop() {
                    if !scanned_promoted.insert(obj_ptr as usize) {
                        // already scanned this cycle
                        continue;
                    }
                    made_progress = true;
                    // SAFETY: `obj_ptr` is a promoted object in old gen (it was
                    // pushed only when `forward_object` confirmed the allocation
                    // landed in `old_gen`), with a valid copied header.
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    // HIB-CV-24: capture class id before forwarding may grow young_to.
                    let promoted_cid = header.class_id.as_u32();

                    // Scan/forward ref slots (ref arrays + compact objects use
                    // 8-byte pointers; legacy objects use 16-byte Value cells). A
                    // forwarded ref that stays in young to-space is an old→young
                    // edge: defer-mark its card so the NEXT minor GC's dirty-card
                    // scan sees it (deferred_dirty_cards is re-marked after Phase
                    // 3's clear_all; a direct mark would be wiped). The object-field
                    // case once missed this (BouncyCastle X9ECParametersHolder.params
                    // -> young X9ECParameters lost its remembered-set entry); the
                    // unified helper applies it to every layout.
                    //
                    // gengc-round4-move (2026-09-23): the holder is deferred ONCE
                    // per object, not once per young-staying slot — the parallel
                    // arm's `scan_object` has always done it that way. A promoted
                    // `Object[]` of N young referents pushed the same address N
                    // times for `mark_dirty_bulk` to dirty one card N times.
                    //
                    // gcd d2/h (cards3 item 3, producer (c)): a reference array
                    // on an element-precise table defers each young-staying
                    // element's card instead (`HolderCardDefer`).
                    let mut card_defer = HolderCardDefer::new(
                        obj_ptr as usize,
                        header,
                        promoted_worklist.element_card_base,
                    );
                    // SAFETY: `obj_ptr`/`header` are a valid promoted old-gen object.
                    unsafe {
                        forward_ref_slots_at(obj_ptr, header, |off, ref_ptr| {
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                );
                                if !old_gen.contains(new_ref_ptr) {
                                    card_defer.slot_stays_young(off, &mut deferred_dirty_cards);
                                }
                                Some(new_ref_ptr)
                            } else {
                                None
                            }
                        });
                    }

                    // HIB-CV-24: keep this promoted object's defining ClassLoader
                    // alive (instance→loader). A young loader is evacuated; if it
                    // stays in young to-space it is an old→young edge, so defer-mark
                    // this object's card like the ref-slot case above.
                    if loader_pin_on {
                        if let Some(loader_old) = young_loader_pin(
                            promoted_cid,
                            &young_from,
                            promoted_worklist.loader_pins.as_ref(),
                        ) {
                            let lp = loader_old as *mut u8;
                            if young_from.contains(lp) {
                                let new_lp = Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    lp,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                );
                                if !old_gen.contains(new_lp) {
                                    card_defer.whole_object();
                                }
                            }
                        }
                    }
                    card_defer.finish(&mut deferred_dirty_cards);
                }

                if !made_progress {
                    break;
                }
            }
        }

        mv_phase!("cheney_drain");
        // gcj j1yg-pause: nothing below reads the card edges; their buffers
        // go back to the heap, cleared, for the next cycle.
        extra_roots.clear();
        card_values.clear();
        *self.card_scan_scratch.lock() = (extra_roots, card_values);
        // gen r4w4/young4: hand the serial promotion buffer's unused tail back
        // BEFORE anything below can walk old gen (the loader rescue's
        // `walk_objects` is the first). A no-op after a parallel copy phase,
        // which promotes through its own per-worker buffers.
        promoted_worklist.retire_plab(&mut old_gen);
        // gc-genpause F4: the capacity hint for the NEXT cycle's map. Taken
        // here rather than at the end of the collection because this is the
        // point where the transitive closure is complete and the number means
        // "survivors of a young collection" -- the later phases add a handful
        // of resurrected finalizables and, after a major GC, compose in a
        // compaction map, neither of which is the quantity being predicted.
        self.prev_survivor_count
            .store(pointer_map.len(), std::sync::atomic::Ordering::Relaxed);
        if mv_phase_on {
            // The numbers that decide what a slow drain MEANS. `bytes_before`
            // is the young occupancy the cycle started from, so
            // copied/bytes_before is the survival rate: a drain that is slow
            // with few survivors is a per-object cost problem, one that is slow
            // with most of young surviving is a promotion/sizing problem, and
            // the fixes have nothing in common.
            moving_phase_count_push("objects_copied", objects_copied as u128);
            moving_phase_count_push("young_bytes_before", bytes_before as u128);
            moving_phase_count_push("pointer_map_len", pointer_map.len() as u128);
        }

        // ----- loader_pin from OLD instances (gen r4w3/rooting) ------------
        //
        // The moving twin of the non-moving marker's
        // "loader_pin from OLD instances" block (gen r4w2/youngmark). Both copy
        // arms follow the instance -> defining-loader edge (`loader_pin`) from
        // every object they COPY or PROMOTE; neither ever looks at an instance
        // that was ALREADY old. With loader unloading on (the default) user
        // loaders are not rooted, so a YOUNG loader whose only live instances
        // were tenured on an earlier cycle was reached by nothing: left in
        // from-space and reset with it while `loader_pin` still named it (and
        // `gc_reconcile_defining_loaders` then pruned the row). Minor-GC
        // semantics treat the old generation as live, so an old instance keeps
        // its loader alive. See
        // `docs/feature-designs/default-moving-young-gen.md`.
        //
        // Priced like the non-moving fix: a healthy cycle pays one scan of the
        // (small) loader registry — `snapshot()` is a relaxed load when it is
        // empty — and the old-generation walk runs only on the cycle that is
        // about to lose a young pinned loader. It sits after the closure's
        // fix-point (either arm) and BEFORE finalizer resurrection, because
        // Phase 2.5's "already forwarded?" test must see everything the rescued
        // loader keeps alive. The re-drain is serial; the parallel arm's
        // resurrection pass below continues the same way.
        if loader_pin_on {
            // gc-common w36-d: per class id, the row inside this from-space
            // (see `young_loader_pin`).
            let unreached_loaders = moving_unreached_young_pinned_loaders(
                cratonvm_types::loader_pin::snapshot_where(|a| young_from.contains(a as *mut u8)),
                |addr| young_from.contains(addr as *mut u8),
                |addr| pointer_map.contains_key(&addr),
            );
            if !unreached_loaders.is_empty() {
                let candidates = unreached_loaders.len();
                // Owned: no borrow of `old_gen` survives into the forwarding
                // below. Includes this cycle's promotions, which is harmless —
                // a promoted instance's loader was forwarded when the instance
                // was scanned, so it is never a candidate.
                let old_objects = old_gen.walk_objects();
                let rescued = loaders_of_old_instances(
                    unreached_loaders,
                    old_objects.iter().map(|&(optr, _)| {
                        // SAFETY: `walk_objects` yields the start of each
                        // allocated old-gen object, so `optr` is a readable
                        // `ObjectHeader`; old gen is not relocated by a young
                        // cycle.
                        unsafe { (*(optr as *const ObjectHeader)).class_id.as_u32() }
                    }),
                );
                drop(old_objects);
                for &loader in &rescued {
                    Self::forward_object(
                        &young_from,
                        &young_object_starts,
                        &mut young_to,
                        &mut old_gen,
                        loader as *mut u8,
                        &mut objects_copied,
                        &mut pointer_map,
                        &mut promoted_worklist,
                        copy_policy,
                    );
                }
                if !rescued.is_empty() {
                    Self::redrain_serial_closure(
                        &young_from,
                        &young_object_starts,
                        &mut young_to,
                        &mut old_gen,
                        &mut objects_copied,
                        &mut pointer_map,
                        &mut promoted_worklist,
                        copy_policy,
                        &mut scan_cursor,
                        &mut scanned_promoted,
                        &mut deferred_dirty_cards,
                        loader_pin_on,
                    );
                }
                if mv_phase_on {
                    moving_phase_count_push("loader_old_instance_candidates", candidates as u128);
                    moving_phase_count_push("loader_old_instance_rescued", rescued.len() as u128);
                }
            }
        }
        mv_phase!("loader_old_instance");

        // gci i9/yl — the licensed moving `System.gc()` follows the
        // `mirror_pin` / `metadata_pin` rows of every YOUNG owner the closure
        // forwarded (copied, promoted or kept in place: a pinned survivor is
        // an identity entry of the map), to a fix-point: a followed value can
        // be another row's owner (loader A's static names loader B). After
        // the loader rescue, whose loaders own rows too, and BEFORE Phase
        // 2.5, whose "already forwarded?" test must see everything a live
        // owner keeps. Serial, like the rescue: it runs only on a licensed
        // cycle, over a handful of rows, and each row is followed once.
        // Empty `side_table_young_rows` (every other cycle): nothing runs.
        if !side_table_young_rows.is_empty() {
            let mut followed = 0usize;
            loop {
                let due = side_table_values_due(
                    &side_table_young_rows,
                    &mut side_table_rows_done,
                    |a| young_from.contains(a as *mut u8),
                    |a| pointer_map.contains_key(&a),
                );
                if due.is_empty() {
                    break;
                }
                followed += due.len();
                for &value in &due {
                    Self::forward_object(
                        &young_from,
                        &young_object_starts,
                        &mut young_to,
                        &mut old_gen,
                        value as *mut u8,
                        &mut objects_copied,
                        &mut pointer_map,
                        &mut promoted_worklist,
                        copy_policy,
                    );
                }
                Self::redrain_serial_closure(
                    &young_from,
                    &young_object_starts,
                    &mut young_to,
                    &mut old_gen,
                    &mut objects_copied,
                    &mut pointer_map,
                    &mut promoted_worklist,
                    copy_policy,
                    &mut scan_cursor,
                    &mut scanned_promoted,
                    &mut deferred_dirty_cards,
                    loader_pin_on,
                );
            }
            if mv_phase_on {
                moving_phase_count_push("side_table_rows", side_table_young_rows.len() as u128);
                moving_phase_count_push("side_table_values_followed", followed as u128);
            }
        }

        // Phase 2.5: Resurrect dead finalizable objects — forward any
        // unreachable finalizable objects so finalize() can access them.
        let mut dead_finalizers = Vec::new();
        // gc-common w36-d — the depth-2 set (`take_resurrection_closure`):
        // everything this phase copies lands in to-space above this mark, or is
        // promoted and popped from `promoted_worklist` in Phase 2.5b (the strong
        // closure drained both before this point). Their `pointer_map` KEYS are
        // the pre-collection addresses the reference processor asks about.
        let resurrect_to_mark = young_to.used();
        let mut resurrect_promoted: Vec<usize> = Vec::new();
        // gci i40/gn (`CRATONVM_GEN_IN_PLACE_RESURRECTION_CLOSURE`, default
        // ON): on a pinned in-place cycle the resurrection drain's young
        // objects land on the in-place scan stack, not in to-space; they are
        // recorded as they are popped in Phase 2.5b (destination addresses,
        // mapped back to keys below). Only when that stack is EMPTY here --
        // the strong closure drained it, as it drains to-space -- so nothing
        // the strong closure reached can be recorded; otherwise (never seen,
        // a guard) the cycle records as before.
        let in_place_drained_at_mark = promoted_worklist
            .in_place
            .as_deref()
            .is_some_and(|ip| ip.scan.is_empty());
        let mut resurrect_in_place: Vec<usize> = Vec::new();
        if fin_child_census {
            fin_child_census_lines(
                finalizer_addrs,
                &young_from,
                &old_gen,
                &pointer_map,
                promoted_worklist.in_place.as_deref(),
                &fin_census_seeds,
                roots,
            );
        }
        for &old_addr in finalizer_addrs {
            if pointer_map.contains_key(&old_addr) {
                continue; // already reachable — skip
            }
            let old_ptr = old_addr as *mut u8;
            if !young_from.contains(old_ptr) {
                continue; // not in young gen (e.g. old gen or invalid)
            }
            let new_ptr = Self::forward_object(
                &young_from,
                &young_object_starts,
                &mut young_to,
                &mut old_gen,
                old_ptr,
                &mut objects_copied,
                &mut pointer_map,
                &mut promoted_worklist,
                copy_policy,
            );
            dead_finalizers.push(new_ptr as usize);
        }

        // Phase 2.5b: Continue Cheney scan for resurrected objects and their refs
        if !dead_finalizers.is_empty() {
            // gci i40/gn: the switch is read once, on a cycle that resurrects.
            let record_in_place =
                in_place_drained_at_mark;
            loop {
                let mut made_progress = false;
                while scan_cursor < young_to.used() {
                    made_progress = true;
                    // SAFETY: `scan_cursor` is within `young_to.used()`; points to a valid copied object header.
                    let obj_ptr = unsafe { young_to.base_ptr_mut().add(scan_cursor) };
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    let total_size = gen_object_total_size(header);
                    // HIB-CV-24: class id captured before forwarding (as in the
                    // main scan above).
                    let cid = header.class_id.as_u32();
                    // Scan/forward ref slots (arrays + compact objects: 8-byte
                    // pointers; legacy objects: 16-byte Value cells).
                    // SAFETY: `obj_ptr`/`header` are a valid copied object in young_to.
                    unsafe {
                        forward_ref_slots(obj_ptr, header, |ref_ptr| {
                            if young_from.contains(ref_ptr) {
                                Some(Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                ))
                            } else {
                                None
                            }
                        });
                    }
                    // HIB-CV-24, the instance→loader edge (gengc-round4-move,
                    // 2026-09-23). The main serial scan and the parallel
                    // `scan_object` both follow it; this resurrection re-drain
                    // did not, so a young loader reachable only through a
                    // resurrected finalizable instance was left in from-space
                    // and reset with it — while the instance's `finalize()`,
                    // about to run, can reach it through its class.
                    if loader_pin_on {
                        if let Some(loader_old) = young_loader_pin(
                            cid,
                            &young_from,
                            promoted_worklist.loader_pins.as_ref(),
                        ) {
                            let lp = loader_old as *mut u8;
                            if young_from.contains(lp) {
                                Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    lp,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                );
                            }
                        }
                    }
                    scan_cursor += total_size;
                }
                // gen r4w5/pinned5: resurrected objects the pinned in-place
                // cycle copied into from-space free spans (or kept).
                while let Some(obj_ptr) = promoted_worklist.pop_in_place() {
                    made_progress = true;
                    // gci i40/gn: the depth-2 set's in-place half.
                    if record_in_place {
                        resurrect_in_place.push(obj_ptr as usize);
                    }
                    Self::scan_in_place_young(
                        obj_ptr,
                        &young_from,
                        &young_object_starts,
                        &mut young_to,
                        &mut old_gen,
                        &mut objects_copied,
                        &mut pointer_map,
                        &mut promoted_worklist,
                        copy_policy,
                        loader_pin_on,
                    );
                }
                // Also scan promoted objects from resurrection — drain the
                // shared worklist (see CRIT-P2 note above on the main scan).
                while let Some(obj_ptr) = promoted_worklist.pop() {
                    if !scanned_promoted.insert(obj_ptr as usize) {
                        continue;
                    }
                    made_progress = true;
                    resurrect_promoted.push(obj_ptr as usize);
                    // SAFETY: `obj_ptr` is a promoted old-gen object with a valid copied header.
                    let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
                    let promoted_cid = header.class_id.as_u32();
                    // Scan/forward ref slots (arrays + compact objects: 8-byte
                    // pointers; legacy objects: 16-byte cells). A forwarded ref
                    // that stays young is an old→young edge — defer-mark its card
                    // (re-applied after clear_all; a direct mark would be wiped),
                    // for every layout (the ServiceLoader cryptoProvider bug).
                    // Once per holder, as in the main scan above (per element
                    // card for a reference array on an element-precise table,
                    // gcd d2/h, `HolderCardDefer`).
                    let mut card_defer = HolderCardDefer::new(
                        obj_ptr as usize,
                        header,
                        promoted_worklist.element_card_base,
                    );
                    // SAFETY: `obj_ptr`/`header` are a valid promoted old-gen object.
                    unsafe {
                        forward_ref_slots_at(obj_ptr, header, |off, ref_ptr| {
                            if young_from.contains(ref_ptr) {
                                let new_ref_ptr = Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    ref_ptr,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                );
                                if !old_gen.contains(new_ref_ptr) {
                                    card_defer.slot_stays_young(off, &mut deferred_dirty_cards);
                                }
                                Some(new_ref_ptr)
                            } else {
                                None
                            }
                        });
                    }
                    // The instance→loader edge, as above; a loader that stays
                    // young is an old→young edge from this promoted holder.
                    if loader_pin_on {
                        if let Some(loader_old) = young_loader_pin(
                            promoted_cid,
                            &young_from,
                            promoted_worklist.loader_pins.as_ref(),
                        ) {
                            let lp = loader_old as *mut u8;
                            if young_from.contains(lp) {
                                let new_lp = Self::forward_object(
                                    &young_from,
                                    &young_object_starts,
                                    &mut young_to,
                                    &mut old_gen,
                                    lp,
                                    &mut objects_copied,
                                    &mut pointer_map,
                                    &mut promoted_worklist,
                                    copy_policy,
                                );
                                if !old_gen.contains(new_lp) {
                                    card_defer.whole_object();
                                }
                            }
                        }
                    }
                    card_defer.finish(&mut deferred_dirty_cards);
                }
                // gci i9/yl: a resurrected young owner's side-table rows (a
                // finalizable loader keeps its statics and mirrors for its
                // `finalize()`), forwarded here so this loop drains them and
                // the depth-2 set below records them. Each row once; empty off
                // the licensed moving `System.gc()`.
                if !side_table_young_rows.is_empty() {
                    let due = side_table_values_due(
                        &side_table_young_rows,
                        &mut side_table_rows_done,
                        |a| young_from.contains(a as *mut u8),
                        |a| pointer_map.contains_key(&a),
                    );
                    for value in due {
                        made_progress = true;
                        Self::forward_object(
                            &young_from,
                            &young_object_starts,
                            &mut young_to,
                            &mut old_gen,
                            value as *mut u8,
                            &mut objects_copied,
                            &mut pointer_map,
                            &mut promoted_worklist,
                            copy_policy,
                        );
                    }
                }
                if !made_progress {
                    break;
                }
            }
            // gc-common w36-d — record the depth-2 set: every pre-collection
            // address this phase copied (to-space above the mark) or promoted
            // (popped above). The resurrected candidates are among them.
            // Paid only on a cycle with a dead young finalizable.
            // gci i40/gn: plus, on a pinned in-place cycle, every object the
            // drain queued for an in-place scan (`resurrect_in_place`; empty
            // with `CRATONVM_GEN_IN_PLACE_RESURRECTION_CLOSURE=0` or off a
            // pinned cycle, which is the w36-d record unchanged).
            let to_base = young_to.base_ptr() as usize;
            let to_lo = to_base + resurrect_to_mark;
            let to_hi = to_base + young_to.used();
            let promoted: FxHashSet<usize> = resurrect_promoted.iter().copied().collect();
            let in_place_set: FxHashSet<usize> = resurrect_in_place.iter().copied().collect();
            let closure =
                resurrection_closure_keys(&pointer_map, to_lo, to_hi, &promoted, &in_place_set);
            self.note_resurrection_closure(&closure);
        }
        // gen r4w4/young4: the loader rescue and Phase 2.5 may have carved a
        // second serial promotion buffer; its tail goes back before the
        // verifier's `old_gen.walk_objects()` below and before Phase 5.
        promoted_worklist.retire_plab(&mut old_gen);

        // To-space occupancy, which is what "bytes copied" has always meant
        // here. After a PARALLEL copy phase it also includes the fillers over
        // retired per-worker buffer tails — real bytes of this arena, and the
        // reason `gen_evac::par_evac_census()` publishes `filler_bytes`
        // separately rather than leaving the difference to be inferred.
        //
        // gen r4w5/pinned5: a pinned in-place cycle leaves to-space empty; its
        // young occupancy after the cycle is every object that stays in
        // from-space (pinned, copied into a free span, kept).
        let bytes_copied = match promoted_worklist.in_place.as_deref() {
            Some(ip) => ip.live_bytes(),
            None => young_to.used(),
        };

        // ----- THE PAIRING: does a frozen peer hold an address we moved? -----
        //
        // §10.11 established that 84 % of relocating cycles run with a peer in
        // `CompiledUninterruptible`, whose `RelocationRule` is `Forbidden`
        // because "registers and JIT spill slots are not rewritable". That is
        // an ASSOCIATION. This is the demonstration it asked for: the peer's
        // whole GPR block was captured while it was frozen, and `pointer_map`
        // is exactly the set of from-space addresses this cycle RELOCATED, so
        // an intersection is a register that will be read after resume and no
        // longer names its object.
        //
        // A register is not heap, not frame-band memory, and no pass in this
        // collector rewrites one — which is why the heap verifier below reads
        // zero on the very cycles that corrupt (§6).
        if cratonvm_gc_core::gc_quiescence::peer_reg_pairing_enabled() {
            let captured = cratonvm_gc_core::gc_quiescence::take_peer_reg_capture();
            let mut hits = 0usize;
            let mut reported = 0usize;
            // THE INTERIOR CENSUS, and it is the one class of stale peer word
            // no conservative write-back can repair.
            //
            // `record_peer_stack_slot`'s capture and the blocked-wake
            // write-back both key on an OBJECT BASE, because `is_object_address`
            // is the only predicate a conservative scan is allowed to trust. A
            // word naming the INTERIOR of a live object -- a byte-array data
            // pointer, a `DirectByteBuffer` address, any derived pointer -- is
            // therefore neither marked, nor captured, nor rewritten, while the
            // object it points into is kept alive by some other root and
            // RELOCATED. The peer resumes and dereferences a vacated interior
            // address, which is why the observed faults read at UNALIGNED
            // addresses inside a decommitted span.
            //
            // That population is exactly what `unrewritable_conservative_jit_roots`
            // exists to refuse -- its own comment says the registry "drops
            // interior/derived pointers that name no base at all" -- so this
            // counter is the measurement behind that refusal rather than an
            // argument for it. (gen r4w6/pinstale6: "only reachable with
            // `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`" stopped being true in wave 5
            // — a pinned in-place cycle relocates with the term fired, and a
            // Cheney cycle relocates under blocked peers whenever no compiled
            // frame is live. `interior=` also counts interior words into DEAD
            // objects and free space; `interior_relocated=` below is the
            // subset that names a relocated object.)
            let from_lo = young_from.base_ptr() as usize;
            let from_hi = from_lo + young_from.used();
            let mut interior = 0usize;
            let mut dead_base = 0usize;
            // gen r4w5/pinned5: an interior word into a PINNED object is the
            // population the pinned copy exists to make safe — it still names
            // its object after the cycle — so it is counted apart.
            let mut interior_pinned = 0usize;
            for (_tid, _reg, val) in captured.iter().copied() {
                if val < from_lo || val >= from_hi || pointer_map.contains_key(&val) {
                    continue;
                }
                if promoted_worklist
                    .in_place
                    .as_deref()
                    .is_some_and(|ip| ip.covers_pinned(val))
                {
                    interior_pinned += 1;
                    continue;
                }
                if young_object_starts.contains(val) {
                    // A base that nothing relocated: the object died this
                    // cycle. Harmless — the peer's word names garbage, and
                    // garbage is what it named before the collection too.
                    dead_base += 1;
                } else {
                    interior += 1;
                }
            }
            // gen r4w6/pinstale6: THE CLASSIFICATION the default flip is gated
            // on. `stale=` above counts every captured word naming a relocated
            // base, whatever its population; most of what it counted on the
            // wave-5 probe run cannot be told from this line alone. Each word
            // is now split by source (`PeerWordSource`) and fate
            // (`PeerWordFate`), and `stale_live=` counts only the words no
            // channel rewrites and no pin keeps (`peer_word_stale_live`):
            //
            // * a helper-window stack BASE is rewritten at wake by the
            //   blocked-wake fold (`stale_helper_stack_remapped=`) — unless
            //   `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP` turned it off;
            // * a register word, a take-over stack word, and any word INTO a
            //   relocated object (`interior_relocated=`) have no channel.
            //
            // On a pinned in-place cycle (`cycle=pinned`) every one of those is
            // pinned — register words through the pause's ledger, blocked-peer
            // non-base stack words through the plan — and a take-over cannot
            // coexist with the cycle, so `stale_live=0` there is structural; a
            // non-zero reading on a pinned cycle is a defect. On an ordinary
            // Cheney cycle (`cycle=cheney`) nothing is pinned, with or without
            // the flag, and a non-zero reading is the pre-existing exposure of
            // the default moving path.
            let remap_on = cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled();
            let mut stale_register = 0usize;
            let mut stale_takeover_stack = 0usize;
            let mut stale_helper_stack_remapped = 0usize;
            let mut interior_relocated = 0usize;
            let mut stale_live = 0usize;
            for (tid, reg, val) in captured.iter().copied() {
                let source = PeerWordSource::of(reg);
                let fate = peer_word_fate(
                    val,
                    (from_lo, from_hi),
                    |a| pointer_map.get(&a).copied(),
                    |a| young_object_starts.contains(a),
                    |a| young_object_starts.nearest_start_at_or_below(a),
                    // SAFETY: `n` is a `pointer_map` value that differs from
                    // its key, i.e. the address this cycle COPIED an object to
                    // (to-space, old gen, or an in-place destination); the copy
                    // and its header are complete by now, and every mutator is
                    // stopped.
                    |n| gen_object_total_size(unsafe { &*(n as *const ObjectHeader) }),
                );
                let live = peer_word_stale_live(source, fate, remap_on);
                if live {
                    stale_live += 1;
                }
                match fate {
                    PeerWordFate::Untouched => continue,
                    PeerWordFate::RelocatedInterior => {
                        interior_relocated += 1;
                        continue;
                    }
                    PeerWordFate::RelocatedBase => {}
                }
                match source {
                    PeerWordSource::Register => stale_register += 1,
                    PeerWordSource::TakeoverStack => stale_takeover_stack += 1,
                    PeerWordSource::HelperStack if !live => stale_helper_stack_remapped += 1,
                    PeerWordSource::HelperStack => {}
                }
                let Some(&new_addr) = pointer_map.get(&val) else {
                    continue;
                };
                hits += 1;
                if reported < 12 {
                    reported += 1;
                    // Name the object by its NEW header: the old one is a
                    // forwarding word by now.
                    let cls = {
                        // SAFETY: `new_addr` is the address this cycle copied
                        // the object at `val` to (`RelocatedBase`: a map value
                        // that differs from its key); the copy is complete and
                        // the world is stopped.
                        let h = unsafe { &*(new_addr as *const ObjectHeader) };
                        cratonvm_gc_core::gc::resolve_class_info(h.class_id.as_u32())
                            .map(|(name, _)| name)
                            .unwrap_or_else(|| format!("cid#{}", h.class_id.as_u32()))
                    };
                    let src = match source {
                        PeerWordSource::Register => "register",
                        PeerWordSource::TakeoverStack => "takeover-stack",
                        PeerWordSource::HelperStack => "helper-stack",
                    };
                    eprintln!(
                        "[peer-reg-stale] os_tid={tid} reg=r{reg} src={src} live={live} holds 0x{val:x} \
                         -> RELOCATED to 0x{new_addr:x} ({cls}) -- this thread resumes with a vacated \
                         address{}",
                        if live { "" } else { " unless the blocked-wake fold rewrites it (it will)" },
                    );
                }
            }
            if hits > 0 {
                cratonvm_gc_core::gc_quiescence::PEER_REG_STALE.fetch_add(hits as u64, Ordering::Relaxed);
            }
            if stale_live > 0 {
                cratonvm_gc_core::gc_quiescence::PEER_REG_STALE_LIVE
                    .fetch_add(stale_live as u64, Ordering::Relaxed);
            }
            eprintln!(
                "[peer-reg-stale] cycle summary: captured_words={} stale={} interior={} dead_base={} interior_pinned={} pointer_map={} \
                 cycle={} stale_register={} stale_takeover_stack={} stale_helper_stack_remapped={} \
                 interior_relocated={} helper_stack_remap={} stale_live={}",
                captured.len(),
                hits,
                interior,
                dead_base,
                interior_pinned,
                pointer_map.len(),
                if in_place_cycle { "pinned" } else { "cheney" },
                stale_register,
                stale_takeover_stack,
                stale_helper_stack_remapped,
                interior_relocated,
                if remap_on { "on" } else { "off" },
                stale_live,
            );
        }

        if moving_young_dangling_verify_enabled() {
            let mut missed_young = 0usize;
            let mut missed_old = 0usize;
            let mut reported = 0usize;
            let cap = 40usize;
            // THE SCAN FRONTIER, captured before the walk below borrows the
            // arena again.
            //
            // Cheney's contract is that every object in to-space is eventually
            // scanned, which holds exactly while `scan_cursor` reaches
            // `young_to.used()`. It does not hold by construction here: the
            // main drain runs to the used() of ITS moment, then phase 2.5
            // resurrects finalizable objects -- copying MORE into to-space --
            // and the re-drain that follows is guarded by
            // `if !dead_finalizers.is_empty()`. Any other late copy, on a cycle
            // with no dead finalizers, leaves a tail nothing scans.
            //
            // So report the frontier next to the misses. `unscanned_tail > 0`
            // with every missed referrer at or above the cursor is the
            // difference between "the collector lost a pointer somewhere" and
            // "these exact bytes were never looked at".
            let final_scan_cursor = scan_cursor;
            let to_used_at_verify = young_to.used();
            let to_base_at_verify = young_to.base_ptr() as usize;
            let mut scan_obj = |space: &str, obj: *mut u8, header: &ObjectHeader| -> usize {
                let mut n = 0usize;
                // SAFETY: caller passes live objects from young_to/old_gen walks while STW.
                unsafe {
                    for_each_ref_slot(obj, header, |raw, slot_id| {
                        let t = raw as usize;
                        if !young_from.contains(raw as *const u8) {
                            return;
                        }
                        let th = &*(raw as *const ObjectHeader);
                        if !th.is_forwarded() {
                            return;
                        }
                        n += 1;
                        if reported < cap {
                            reported += 1;
                            let referrer = cratonvm_gc_core::gc::resolve_class_info(header.class_id.as_u32())
                                .map(|(name, _)| name)
                                .unwrap_or_else(|| format!("cid#{}", header.class_id.as_u32()));
                            let target = cratonvm_gc_core::gc::resolve_class_info(th.class_id.as_u32())
                                .map(|(name, _)| name)
                                .unwrap_or_else(|| format!("cid#{}", th.class_id.as_u32()));
                            let expected = pointer_map
                                .get(&t)
                                .copied()
                                .unwrap_or_else(|| th.forwarding_address() as usize);
                            let off = (obj as usize).saturating_sub(to_base_at_verify);
                            let unscanned = space == "YOUNG" && off >= final_scan_cursor;
                            eprintln!(
                                "[moving-young-verify] MISSED-HEAP-REWRITE {} {}@0x{:x} off=0x{:x} \
                                 scan_cursor=0x{:x} past_cursor={} slot={} -> forwarded {} \
                                 old=0x{:x} new=0x{:x}",
                                space,
                                referrer,
                                obj as usize,
                                off,
                                final_scan_cursor,
                                unscanned,
                                slot_id,
                                target,
                                t,
                                expected,
                            );
                        }
                    });
                }
                n
            };

            // Speculative copies the parallel evacuator abandoned after
            // losing a forwarding CAS. They are complete objects that nothing
            // references and nothing scanned, so every slot still names
            // from-space -- a WHOLE-OBJECT miss, which is the shape that
            // produced (and then sank) this instrument's only two historical
            // non-zero readings. Excluded here so a non-zero count means what
            // the instrument's contract says it means.
            let abandoned = take_abandoned_evac_copies();
            let mut skipped_abandoned = 0usize;

            let mut cursor = 0usize;
            let young_used = young_to.used();
            // gen r5w1/young5: the grid rules are `probe_grid_at`'s. To-space
            // has no free list (`next_free` is `None`), `Parse` is this walk's
            // zero-run behaviour, and every anomaly still ends the walk.
            //
            // gengc-round4-move (2026-09-23): the 8-byte GAP-filler sentinel a
            // parallel cycle stamps over a retired buffer tail too small for
            // an `int[]` (`gen_evac::install_gap_filler`) is strided.
            // `gen_object_total_size` cannot size it, so this walk used to stop
            // at the first one and silently verify nothing past it — the same
            // Bug-D shape every other young walk already strides.
            let verify_rules = GridRules {
                base: young_to.base_ptr() as usize,
                used: young_used,
                zero_run_hi: young_used,
                side_sorted: &[],
                zero_runs: ZeroRunRule::Parse,
                bad_gap_filler_is_anomaly: true,
                phantom: false,
            };
            let mut no_phantom_probe = 0usize;
            while cursor < young_used {
                // SAFETY: young_to is bump-allocated; cursor advances by object size.
                let obj = unsafe { young_to.base_ptr_mut().add(cursor) };
                let size = match probe_grid_at(cursor, None, &verify_rules, &mut no_phantom_probe) {
                    GridProbe::Gap { len } => {
                        cursor += len;
                        continue;
                    }
                    GridProbe::Object { size } => size,
                    GridProbe::BadGapFiller
                    | GridProbe::EmptyRun { .. }
                    | GridProbe::ZeroSpan { .. }
                    | GridProbe::BadSize { .. }
                    | GridProbe::CrossesFreeBlock { .. }
                    | GridProbe::Phantom { .. } => break,
                };
                // SAFETY: the probe parsed a plausible header at this in-bounds
                // to-space offset.
                let header = unsafe { &*(obj as *const ObjectHeader) };
                if abandoned.contains(&(obj as usize)) {
                    skipped_abandoned += 1;
                } else {
                    missed_young += scan_obj("YOUNG", obj, header);
                }
                cursor += size;
            }
            // gen r4w5/pinned5: a pinned in-place cycle's survivors are in
            // FROM-space — pinned objects (identity-forwarded: their headers
            // are not forwarding words, so a slot naming one is never counted),
            // copies into free spans and kept survivors. Walked by their list,
            // since from-space is not linearly walkable until the rebuild.
            if let Some(ip) = promoted_worklist.in_place.as_deref() {
                for &(addr, _) in &ip.live {
                    // gcj j2/pn2: a pinned object the rebuild is about to
                    // reclaim (`pinned_droppable`: unreached under the drop
                    // mode) is dead; its slots were never rewritten, so
                    // scanning it reported false `MISSED-HEAP-REWRITE`s.
                    if ip
                        .pinned_index(addr)
                        .is_some_and(|i| ip.pinned_droppable(i))
                    {
                        continue;
                    }
                    // SAFETY: every `live` entry is a whole object that stays
                    // in from-space this cycle, not yet zeroed or rebuilt.
                    let header = unsafe { &*(addr as *const ObjectHeader) };
                    missed_young += scan_obj("YOUNG-INPLACE", addr as *mut u8, header);
                }
            }
            for (obj, _size) in old_gen.walk_objects() {
                // SAFETY: walk_objects yields live old-gen object starts.
                let header = unsafe { &*(obj as *const ObjectHeader) };
                if abandoned.contains(&(obj as usize)) {
                    skipped_abandoned += 1;
                } else {
                    missed_old += scan_obj("OLD", obj, header);
                }
            }
            eprintln!(
                "[moving-young-verify] forwarded_heap_refs_remaining young={} old={} pointer_map={} abandoned_cas_loser_copies_skipped={}",
                missed_young,
                missed_old,
                pointer_map.len(),
                skipped_abandoned,
            );
            eprintln!(
                "[moving-young-verify] scan frontier: scan_cursor=0x{:x} to_used=0x{:x} \
                 unscanned_tail=0x{:x} dead_finalizers={}",
                final_scan_cursor,
                to_used_at_verify,
                to_used_at_verify.saturating_sub(final_scan_cursor),
                dead_finalizers.len(),
            );
        }

        // bc math-ec 0x4 seed-phase bisect: count `0x4` slots in the Cheney
        // to-space survivors and in old gen AFTER the minor (Cheney +
        // promotion) collection, BEFORE the possible major GC. A jump above
        // `sh_entry_old` here means the MINOR collector seeded the `0x4`.
        let (sh_cheney_young, sh_cheney_old): (usize, usize) = if seedhunt_enabled() {
            let cy = seedhunt_scan_young(
                young_to.base_ptr(),
                young_to.used(),
                "cheney",
                &mut sh_printed,
                sh_cap,
            );
            let mut co = 0usize;
            for (op, _) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let h = unsafe { &*(op as *const ObjectHeader) };
                co += seedhunt_scan_obj(op, h, "cheney", "O", &mut sh_printed, sh_cap);
            }
            (cy, co)
        } else {
            (0, 0)
        };

        mv_phase!("finalizer_resurrect");
        // Phase H (RH.1): compute promotion / young-copy stats from the
        // pointer_map BEFORE major_gc appends its own entries below.
        // Every entry at this point is a minor-GC forward: new_addr is
        // either in old_gen (promoted) or in young_to-space (copied).
        // Walking the map once avoids an expensive counter plumbed
        // through `forward_object`'s 12 call sites.
        let [bytes_promoted_cycle, objects_promoted_cycle, bytes_copied_young_cycle, objects_copied_young_cycle, fwd_reencounters_cycle, fwd_copies_cycle] =
            copy_tally_take();
        if mv_phase_on {
            // How many `forward_object` calls were pure lookups on an already
            // forwarded object. Each one is a hash probe of a map with as many
            // entries as there are survivors, so if this dwarfs the copy count
            // then `cheney_drain` is a lookup cost, not a copying cost.
            moving_phase_count_push("fwd_reencounters", fwd_reencounters_cycle as u128);
            moving_phase_count_push("fwd_copies", fwd_copies_cycle as u128);
        }
        // The walk below no longer produces those counters — `copy_tally_add`
        // does, at the copy site. What is left of it is the BUG-Z forward
        // validation, which is a diagnostic and does not need to run on every
        // collection: it cost 29 ms of a 424 ms median pause, ~756 000 random
        // destination-header dereferences deep inside stop-the-world.
        let mut bad_forward_count: u64 = 0;
        let mut bad_forward_sample: (usize, usize) = (0, 0);
        let fwd_walk = if fwd_walk_enabled() { usize::MAX } else { 0 };
        for (&old_addr, &new_addr) in pointer_map.iter().take(fwd_walk) {
            let new_ptr = new_addr as *const u8;
            let in_old = old_gen.contains(new_ptr);
            // gen r4w5/pinned5: a pinned in-place cycle's young destinations
            // (and its identity forwards) are in from-space.
            let in_young =
                young_to.contains(new_ptr) || (in_place_cycle && young_from.contains(new_ptr));
            // BUG-Z safety: every `pointer_map` value must be a forwarding
            // address inside young_to or old_gen. Under heavy multi-threaded
            // churn (TestFileStoreConcurrency) `forward_object` has been observed
            // to record a *garbage* value (e.g. a small integer) for a valid
            // young_from source — a heap-corruption bug tracked in BUG-Z.
            // Dereferencing such a value here SIGSEGVs in the GC's post-copy
            // stats walk. Skip it (the stats are advisory counters) and record a
            // sample so a single summary can be logged, rather than crashing or
            // spamming a line per entry. NOTE: this does not repair the bad
            // forward — `update_all_roots` still remaps through `pointer_map`;
            // see BUG-Z for the underlying fix.
            if !in_old && !in_young {
                bad_forward_count += 1;
                if bad_forward_sample == (0, 0) {
                    bad_forward_sample = (old_addr, new_addr);
                }
                continue;
            }
        }
        if bad_forward_count > 0 {
            // BUG-Z: surface the corruption once per cycle (not per entry).
            eprintln!(
                "[gc] WARNING: {} pointer_map forward(s) pointed outside the heap \
                 (corruption — see BUG-Z); skipped in stats. sample old=0x{:x} -> new=0x{:x}",
                bad_forward_count, bad_forward_sample.0, bad_forward_sample.1,
            );
        }

        mv_phase!("promotion_stats");
        // Phase 3: Clear card table and reset young from-space
        // The watched address's fate on THIS cycle, read after the copy phase
        // and before anything clears state. `Some` says the collector relocated
        // it and every holder must have been rewritten through the map; `None`
        // says it was never copied, so whatever still names it is dangling from
        // here on. Those are the two halves the `[forward-refused]` census
        // could not tell apart, because a refusal is only visible on the cycles
        // that happen to walk that semispace.
        if cratonvm_gc_core::tlab::watched() != 0 {
            let w = cratonvm_gc_core::tlab::watched();
            cratonvm_gc_core::tlab::watch_note(format_args!(
                "POST-COPY fate: pointer_map={} (None means it was NOT copied, so every reference to it is dangling from this cycle on)",
                match pointer_map.get(&w) {
                    Some(&to) => format!("Some(0x{to:x})"),
                    None => "None".to_string(),
                },
            ));
        }
        card_table.clear_all();
        // t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`): the
        // card clear on its own row, so `cardclear+young_reset` below is the
        // young reset alone.
        mv_phase!("card_clear");
        // Re-mark cards for promoted objects that still reference young gen.
        // These old→young cross-gen references were established during the
        // Phase 2 promoted-object scan and must be visible to the next GC.
        //
        // The actual `mark_dirty_bulk` is DEFERRED until after the possible
        // major GC below: with `CRATONVM_OLDGEN_COMPACT` a major GC
        // mark-compacts the old gen, relocating the very referrer objects
        // whose cards we are about to dirty (the default in-place mark-sweep
        // moves nothing and its `compact_map` holds identities only, so the
        // remap below is then a no-op). Marking here
        // (pre-compaction) would leave the remembered-set cards pointing at the
        // stale pre-compaction addresses; the next minor GC's dirty-card scan
        // would then look in the wrong place, miss the old→young edge, and free
        // a still-live young referent (intermittent stale Locale/ClassLoader
        // corruption). We remap each referrer address through `compact_map`
        // first when a major GC runs (see below).
        //
        // CRATONVM_DBG_STALE_OBJREF: instead of resetting (zeroing) the
        // just-evacuated `young_from` immediately, route it through the
        // quarantine ring for `quarantine_cycles()` extra cycles so a stale
        // native `ObjectRef` held into it still resolves to a forwarded
        // header (caught by `get_header`) rather than reading back all-zero.
        // The ring is ordered oldest-first: once it is full, the FRONT arena
        // (whose grace period has elapsed) is popped, reset (and grown, if
        // `young_from` has since expanded), and reused as the fresh arena;
        // the just-evacuated `young_from` is pushed onto the BACK to start
        // its own grace period. With the default single-cycle ring the final
        // external effect — which arena ends up serving as
        // `young_from`/`young_to` for the NEXT cycle — is identical to the
        // plain `young_from.reset()` this replaces; only the mechanics of
        // clearing memory differ.
        // ---- CRATONVM_DBG_SWEEP_LIVENESS, MOVING path (H2-CID0) ------------
        //
        // Everything this flag checked lived in `sweep_young_non_moving`, so
        // the whole assertion was silent on a cycle that actually COPIED. That
        // is a real hole, because the family's reader-side verdict has now
        // named a copying-cycle address:
        //
        //   NoSuchMethodError method="java/lang/Object.put(...)"
        //        caller="org/h2/engine/ConnectionInfo.readProperties @pc=95"
        //   gc::guard: receiver points into RECLAIMED memory …
        //        location=young TO-space (the inactive semispace)
        //
        // `pc=95` is `this.prop.put(key, value)` — the receiver came out of a
        // HEAP FIELD via `getfield`, and the address it holds is in the
        // semispace a moving collection evacuated and is about to wipe.
        // Neither the root-side nor the dead-span-side invariant can see that:
        // the object SURVIVED (some other reference kept it), it is the
        // referring FIELD that was never rewritten.
        //
        // Ask it directly, at the one moment the answer still exists — after
        // every fixup this cycle will do, before the arena is reset: does any
        // OLD-generation slot still point into the from-space we just
        // evacuated? A hit is an old->young edge the card table did not
        // deliver, and it names the referrer's class and slot.
        //
        // Costs a full old-gen walk per MOVING young collection, which is why
        // it is behind the flag. It uses the old-gen guard this collector call
        // already holds: `parking_lot::Mutex` is not reentrant, so re-locking
        // here deadlocks the collector at a safepoint with every mutator
        // stopped — the first version's `try_lock` reported SKIPPED on every
        // cycle, which is how that was found.
        if gc_flags().dbg_sweep_liveness {
            let lo = young_from.base_ptr() as usize;
            let hi = lo + young_from.used();
            let mut hits = 0usize;
            let mut old_scanned = 0usize;
            let mut first: Option<(usize, u32, usize, usize, bool)> = None;
            for (obj_ptr, _size) in old_gen.walk_objects() {
                old_scanned += 1;
                // SAFETY: `walk_objects` yields valid old-gen object starts.
                let h = unsafe { &*(obj_ptr as *const ObjectHeader) };
                // SAFETY: a walked old-gen object has a valid header and an
                // in-bounds body — `for_each_ref_slot`'s contract.
                unsafe {
                    for_each_ref_slot(obj_ptr, h, |ref_ptr, slot| {
                        let victim = ref_ptr as usize;
                        // gen r4w5/pinned5: after a pinned in-place cycle
                        // from-space still holds survivors, so an old slot into
                        // it is stale only when it names a FORWARDED header.
                        // SAFETY: `victim` is inside the mapped, not-yet-rebuilt
                        // from-space arena whenever the range test holds.
                        let stale = victim >= lo
                            && victim < hi
                            && (!in_place_cycle
                                || (*(victim as *const ObjectHeader)).is_forwarded());
                        if stale {
                            hits += 1;
                            if first.is_none() {
                                // SAFETY: `victim` is inside the mapped,
                                // not-yet-reset from-space arena.
                                let vh = &*(victim as *const ObjectHeader);
                                first = Some((
                                    victim,
                                    h.class_id.as_u32(),
                                    obj_ptr as usize,
                                    slot,
                                    vh.is_forwarded(),
                                ));
                            }
                        }
                    });
                }
            }
            eprintln!(
                "[SWEEP-LIVENESS moving] hits={hits} old_gen_scanned={old_scanned} \
                 from_space=0x{lo:x}..0x{hi:x}",
            );
            if let Some((victim, cid, referrer, slot, fwd)) = first {
                eprintln!(
                    "[SWEEP-LIVENESS moving]   an OLD-GEN slot still points into the evacuated \
                     from-space: victim=0x{victim:x} forwarded={fwd} <- referrer=0x{referrer:x} \
                     class_id={cid} slot={slot}. The arena is wiped next, so this field will \
                     read an all-zero header.",
                );
            }
        }
        let mut deferred_wipe: Option<cratonvm_gc_core::arena::DeferredWipe> = None;
        if let Some(ip) = promoted_worklist.in_place.take() {
            // gen r4w5/pinned5: a pinned in-place cycle does NOT reset
            // from-space and does not swap: from-space keeps its survivors
            // (pinned objects, copies into its free spans, kept survivors) and
            // stays the allocation space. What the Cheney path's reset + wipe
            // does to the whole arena is done here to its dead bytes only —
            // zeroed inside the pause, as the non-moving sweep zeroes its dead
            // spans, because this arena is allocated from the moment the pause
            // ends — and the arena is rebuilt as a sweep leaves it. The
            // quarantine ring, the synchronous wipe and the deferred wipe all
            // concern an EVACUATED semi-space, and there is none this cycle.
            self.note_pinned_young_cycle(&ip);
            let cycle = self.stats.minor_gc_count.load(Ordering::Relaxed) + 1;
            // gcf f1/y: the dead spans are zeroed on this heap's persistent
            // pool, as the sweep zeroes its own (`zero_spans_parallel_on`
            // with `mark_pool`); no pool dispatch is open here.
            let zero_threads = cratonvm_gc_core::young_mark::young_gc_threads(ip.used_at_start);
            let zero_pool = (zero_threads > 1).then(|| self.evac_pool(zero_threads));
            // JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_IN_PLACE_LAZY_ZERO`):
            // the object-start bitmap this cycle's walk built names every
            // header in the dead spans, so they can be poisoned one by one
            // instead of zeroing every dead byte inside the pause.
            //
            // Wave 4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`): a lazy bitmap
            // has walked only the chunks the evacuation asked about; the
            // poisoning needs the dead ones too, so they are walked here, on
            // the pool, before the first header store. Sound because a chunk
            // is unwalked only if nothing in it was forwarded (every forward
            // asks `contains` first), so its chain is still the one the
            // allocator wrote: in-place destinations land in skip blocks or
            // above `used`, never inside a chunk's chain. A chunk that fails
            // is memset whole by `poison_dead_object_headers`.
            //
            // Wave 4b: not with the dead-span mirror (`gen_in_place_dead_mirror_only`,
            // on whenever the lazy grid is): the dead chunks then need no
            // headers at all, and the pause stays O(chunks the cycle touched).
            // (Should the arena have no mirror, `poison_dead_object_headers`
            // still proves each span it poisons, serially.)
            // Wave 4c: forwarding is over; an unprovable chunk from here on is
            // a dead span the poisoning memsets whole, not a fatal stop.
            young_object_starts.seal_lazy(false);
            if young_object_starts.is_lazy()
                && gen_in_place_lazy_zero_enabled()
                && !gen_in_place_dead_mirror_only()
            {
                lazy_young_starts_prove_all(&young_object_starts, zero_threads, zero_pool);
            }
            let vacated = Self::finish_in_place_young_cycle(
                &mut young_from,
                *ip,
                cycle,
                zero_threads,
                zero_pool,
                Some(&young_object_starts),
            );
            // Wave 4b: publish the lazy-dirty mirror now, not only at the
            // cycle's closing bounds publish. The first lazy cycle CREATES it
            // (`note_lazy_dirty`), and under the mirror-only rule the dead
            // headers are not poisoned, so a validator call later in this
            // pause (monitor remap, finalizer bookkeeping) must already see it.
            // Republishing an unchanged mirror is harmless (retire-then-drop).
            self.store_lazy_dirty_mirrors_locked(
                young_from.lazy_dirty_mirror(),
                young_to.lazy_dirty_mirror(),
            );
            *self.young_in_place_vacated.lock() = vacated;
            mv_phase!("in_place_finish");
        } else if cratonvm_gc_core::stale_objref_debug::enabled() {
            let cycles = cratonvm_gc_core::stale_objref_debug::quarantine_cycles();
            // No `expect` (gengc-round4-move, 2026-09-23): `pop_front` on a
            // ring the length test says is full cannot be `None` while
            // `quarantine_cycles() >= 1` (the flag parser enforces it), but a
            // panic here would fire inside the pause with the young lock held,
            // and an empty ring's honest answer is simply "no arena to reuse".
            let oldest = if quarantine.len() >= cycles {
                quarantine.pop_front()
            } else {
                None
            };
            let mut reuse = match oldest {
                Some(mut oldest) => {
                    oldest.reset();
                    oldest
                }
                None => Arena::new(0),
            };
            if reuse.capacity() < young_from.capacity() {
                reuse.grow(young_from.capacity());
            }
            std::mem::swap(&mut *young_from, &mut reuse);
            // `reuse` now holds this cycle's just-evacuated from-space.
            quarantine.push_back(reuse);
        } else if sync_young_wipe_enabled() {
            young_from.reset();
        } else {
            // gen-gc-five: the metadata reset now, the memset after the pause
            // (`spawn_evacuated_wipe`, at the end of this cycle once a
            // possible `grow` has settled where the backing lives).
            deferred_wipe = Some(young_from.reset_deferring_zero());
        }

        // CRIT-P2 (2026-08-07): the conversion this used to do is gone.
        // `GcResult.pointer_map` and `MonitorCleanup::remap_after_gc` now name
        // `cratonvm_types::PointerMap`, i.e. the same `FxHashMap` the Cheney
        // scan builds, so the map is moved rather than rehashed. The old
        // comment called the rebuild "a single O(N) walk — cheap compared to N
        // SipHash operations across the Cheney scan"; it was N SipHash
        // operations, just moved to the end of the cycle, and it measured
        // 43 ms median / 98 ms max of a 424 ms pause on the OAuth2 6-lane
        // repro.
        mv_phase!("cardclear+young_reset");
        // Wave 4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`): what the lazy grid
        // walked this cycle. `objstart_lazy_failed` above zero after the
        // pre-forward proofs passed is a chunk first reached through a
        // precise reference that read as "not an object start".
        if gen_lazy_young_starts_dense_backoff_enabled() {
            if let Some((_, _, _, bytes)) = young_object_starts.lazy_census() {
                self.lazy_starts_backoff.note_lazy_cycle(bytes, young_used);
            }
        }
        if mv_phase_on {
            if let Some((_, walked, failed, bytes)) = young_object_starts.lazy_census() {
                moving_phase_count_push("objstart_lazy_walked", walked as u128);
                moving_phase_count_push("objstart_lazy_failed", failed as u128);
                moving_phase_count_push("objstart_lazy_bytes", bytes as u128);
                moving_phase_count_push(
                    "objstart_lazy_merged",
                    young_object_starts.lazy_merged_walks() as u128,
                );
            }
        }
        // (The `let mut pointer_map = pointer_map;` rebind that used to sit
        // here was the residue of that removed conversion — `pointer_map` was
        // already `mut` — and clippy's `redundant_locals` fires on it under
        // `-D warnings`, which is a gate this file has to keep green.)

        mv_phase!("pointer_map_rebuild");
        // Phase 4: Swap young spaces (monitor remap deferred until after a
        // possible major GC so we can pass the composed pointer_map).
        //
        // gen r4w5/pinned5: not on a pinned in-place cycle — its survivors are
        // in from-space, and from-space stays the allocation space.
        if !in_place_cycle {
            std::mem::swap(&mut *young_from, &mut *young_to);
        }
        // A2 breadcrumb (CRATONVM_DBG_A2): the swap relocates every live young
        // object to a fresh from-space, so all recorded absolute addresses are now
        // stale. Clear so cross-epoch lookups don't lie (keeps the breadcrumb
        // reliable within the next non-moving epoch, where A2's desync occurs).
        cratonvm_gc_core::a2dbg::clear();

        mv_phase!("young_swap");
        // Phase 5: Check if old gen is getting full — trigger a major GC. Since
        // 2026-10-13 (`CRATONVM_OLDGEN_COMPACT` default ON, WARNING
        // gci-user-flip-20261013) it is a sliding mark-compact unless
        // `moving_major_compaction_plan` gives the major to P-C (in place) or
        // `phase5_major_no_compact` vetoes the slide; before that it was an
        // IN-PLACE mark-sweep by default. (gci i20/gn docs pass; gen r4w2/youngpolicy, 2026-09-23:
        // this line, the debug message below and several comments in this
        // function said "mark-compact" unconditionally.)
        //
        // `take_major_gc_request` is ALWAYS evaluated (not short-circuited by
        // `||`) so an explicit `System.gc()` request is consumed exactly once
        // per cycle even when the occupancy threshold was independently also
        // crossed — otherwise the request would leak into and force a LATER,
        // unrelated allocation-triggered minor GC into a major cycle it never
        // asked for. See `gc_quiescence`'s doc comment for the full rationale.
        let major_requested = cratonvm_gc_core::gc_quiescence::take_major_gc_request();
        // gen r4w3/young2 (2026-09-23): the trigger is asked ONCE, here, and
        // both the debug line and the branch below read that answer.
        // `major_trigger_decision` counts every decision (and, under
        // `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS`, may say no above 75 %), so the
        // debug line used to recompute the bare `used >= capacity * 75 / 100`
        // expression and could print `will_run_major=true` for a cycle whose
        // major the hysteresis then skipped.
        let will_run_major = self.major_trigger_decision(&mut old_gen, major_requested);
        if gc_flags().dbg_mirrorpin {
            eprintln!(
                "[DBG_MIRRORPIN] Phase5 old_gen_used={} old_gen_cap={} major_requested={} will_run_major={}",
                old_gen.used(),
                old_gen.capacity(),
                major_requested,
                will_run_major
            );
        }
        // gcd d4/n (`gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words-FIXED-20260928`,
        // and lane m's d4 cross-lane request 2): the COMPACTION-ONLY veto of
        // this pause's major. The major still runs (the request, if any, is
        // consumed by `major_gc_finalizing` as before) but reclaims in place
        // when either
        // * compiled frames were live this pause (`has_conservative_roots`):
        //   the root slice the compaction rewrites does not hold a compiled
        //   loop's derived cursor into an old array (no oop map names it, a
        //   peer whose coverage proof held suppressed its conservative scan,
        //   and the band verifier checks only YOUNG words), nor a parked
        //   peer's native-band or register words. Default ON;
        //   `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD=0` restores the slide; or
        // * the pause's young pin ledger licensed this cycle (the pinned copy,
        //   default ON since the user's flip of 2026-10-13; term 4's arm or the
        //   opt-in take-over arm): the ledger holds young words only, so nothing keeps still
        //   an old object an unrewritable word names (lane m reverted its d3
        //   consumption of the request, which also suppressed the old sweep).
        // The veto outranks the debug switch `CRATONVM_OLDGEN_COMPACT` too.
        // gci i11/tk: the pinned-copy take-over arm relies on the first half
        // (`takeover_other_divert_terms` no longer diverts on OLDGEN_COMPACT).
        let major_no_compact = phase5_major_no_compact(
            pinned_young_words.is_some() && forced_pin_words.is_none(),
            has_conservative_roots,
            moving_major_jit_guard_enabled,
        );
        // gengc-round1-core (2026-09-20): the occupancy clause is guarded on a
        // NON-ZERO old-gen capacity, matching `run_non_moving_young_cycle`'s
        // `old_capacity > 0 && ...`. With a zero-capacity old generation —
        // `GenerationalHeap::with_sizes(young, 0)`, which the explicit-size
        // constructors permit — `used() >= 0 * 75 / 100` is trivially TRUE, so
        // every young cycle also ran a full mark-compact of an empty old gen,
        // and the occupancy line below divided by that same zero. An explicit
        // `System.gc()` still forces the cycle, because that is a request
        // rather than a threshold.
        let old_capacity = old_gen.capacity();
        let major_ran = if will_run_major {
            // gcg g6/yg: the young copy of this pause is done; an unrequested
            // major starts here (the `Copy` bean books the half before it).
            self.note_escalation_mark(major_requested);
            let old_occupancy_pct = if old_capacity > 0 {
                old_gen.used() * 100 / old_capacity
            } else {
                0
            };
            tracing::debug!(
                "Old gen at {}% (or explicit System.gc() request) — running major GC ({})",
                old_occupancy_pct,
                // gci i20/gn: the switch, not the verdict (`moving_major_compaction_plan`
                // below may still give the major to P-C, in place, or veto the slide).
                if oldgen_compact_enabled() {
                    "mark-compact unless P-C takes it or the slide is vetoed"
                } else {
                    "in-place mark-sweep"
                },
            );
            let old_used_before = old_gen.used();
            // gch h2/og -- P-C of `gengc-core-system-gc-forces-non-moving`
            // (`gcd-d9a` item 2, the moving half of `gcd-d4j`),
            // `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS` (default ON since 2026-10-13): this major seeds its old
            // mark from the TRUE roots over the post-swap young grid instead of
            // from every young survivor. Armed only on a plain moving cycle
            // (not the pinned in-place one, whose from-space holds the
            // survivors next to their sources, and not under live compiled
            // frames, whose words the first version does not argue); see
            // `moving_major_true_root_gate`.
            // A major that will compact keeps the legacy seed
            // (`old_gen_gc_inner` declines an ask under `compact` and would
            // count it as a `pinned_plan` fallback). `major_gc_finalizing`
            // decides the same way, from the same two inputs.
            // gch h4/pc (h3/mj review item 7): read once per moving major (it
            // was two declared-flag lookups, here and in the census below).
            let true_roots_on = full_gc_true_roots_enabled();
            // gci i8/pa (`gci-i8rv-oldgen-compact-default-disarms-moving-true-roots-FIXED-20261017`):
            // with `CRATONVM_OLDGEN_COMPACT` default ON (user flip) every
            // moving major without compiled frames compacted and so never took
            // P-C (default ON since i7). Now a major P-C can arm takes P-C and
            // reclaims in place; `OLDGEN_COMPACT` compacts every major P-C
            // cannot arm (compiled frames, the pinned in-place cycle, P-C or
            // the true-root seed killed), and a pending fragmentation request
            // (the OOM ladder's relief, the proactive trigger) still compacts
            // whatever P-C could do. See `moving_major_compaction_plan`.
            // t1 lane F (gch-h3mj step 3, `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_JIT`,
            // opt-in): the switch admits a pause with live compiled frames;
            // see `moving_major_gate_frames_term` and `jit_old_words` below.
            // t1 wave 2 lane W (`CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_IN_PLACE`):
            // the switch admits the pinned in-place cycle; see
            // `moving_major_gate_in_place_term` and `pc_in_place_roots` below.
            let pc_could_arm = moving_major_true_root_gate(
                true_roots_on,
                moving_major_gate_in_place_term(in_place_cycle, || pc_in_place_admitted),
                moving_major_gate_frames_term(
                    has_conservative_roots,
                    // A saturated capture buffer is an incomplete class 2b
                    // set: keep the legacy seed then. t1 wave 8 lane H3MJ: so
                    // is an EMPTY one under the remap's kill
                    // (`CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1` records
                    // nothing and never saturates).
                    || {
                        moving_major_jit_class2b_complete(
                                cratonvm_gc_core::gc_quiescence::blocked_peer_stack_remap_enabled,
                                cratonvm_gc_core::gc_quiescence::peer_stack_slots_saturated,
                            )
                    },
                ),
                moving_major_true_roots_enabled,
            );
            let (major_compacts, pc_in_place) = moving_major_compaction_plan(
                major_no_compact,
                oldgen_compact_enabled(),
                old_gen.fragmentation_compaction_pending(),
                pc_could_arm,
            );
            // `moving_major_true_root_gate(true_roots_on && !major_compacts, ..)`,
            // with the gate's own terms already folded into `pc_could_arm`.
            let moving_true_roots = pc_could_arm && !major_compacts;
            // gcd d9/a: a REQUESTED major that reached this moving cycle
            // (`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`) seeds its old mark from
            // every young survivor: the true-root seed exists on the
            // non-moving cycle only. Counted, so a census names it.
            // (gch h2/og: unless P-C armed it, above. An armed major is
            // counted by `old_gen_gc_inner`: a true-root major, or a fallback
            // by its reason -- a walk gap. A major that compacts is never
            // armed, so it is counted here as before.)
            if !moving_true_roots && major_requested && true_roots_on {
                note_true_root_fallback(
                    &mut old_gen,
                    crate::old_gen::TrueRootFallback::MovingCycle,
                    0,
                );
            } else if !moving_true_roots && !major_requested {
                // gch h1/jo: an OCCUPANCY major on a moving cycle that P-C did
                // not take (P-C is built and default ON since gch h2/og + the i7
                // flip; it is declined here by compiled frames, the pinned
                // in-place cycle, a compaction or its kill) keeps the legacy
                // seed. Not a
                // fallback -- it never asked for the true-root seed -- but
                // counted (`oldsz_moving_legacy_seed_majors`), so a row can say
                // how often the moving half of `gcd-d4j` runs.
                let _ = old_gen.note_moving_legacy_seed_major();
            }
            // The major GC reclaims old-gen storage — the in-place sweep
            // returns dead blocks to the free list, the sliding compaction
            // (default ON since 2026-10-13) moves survivors and zeroes the freed tail — so an
            // old-gen address stops being a survival proof from here on. Both
            // arms emit an identity entry for every watched object that stayed
            // put, so `compact_map` membership is the exact replacement proof.
            cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(true);
            // Publish THIS cycle's young relocation to the external-root
            // providers BEFORE the major GC consults them.
            //
            // Phase 1a above forwarded every overlay-held young object, but it
            // deliberately left the side tables themselves pointing at the
            // PRE-copy addresses — the VM's post-GC `remap_external_roots`
            // repoints them once the collector returns. That is too late when a
            // major GC runs in the SAME collector call, because `old_gen_gc`
            // seeds its mark worklist from exactly those side tables:
            //
            //   * `external_roots_for_matching_owners(|o| young_from.contains(o))`
            //     tests a pre-copy address against the POST-swap from-space, so
            //     it matches nothing and that owner's refs are never seeded;
            //   * `external_roots_for_owner(..)` in the BFS hands back pre-copy
            //     addresses for an object this cycle just PROMOTED, so the
            //     promoted copy is never marked and the compaction frees it
            //     while the overlay still holds the only reference to it.
            //
            // The reclaimed collection then reads its own state back through
            // the relocation-invariant identity-hash key and gets a zeroed
            // header (`ClassId(0)`, `num_slots == 0`) — surfacing as an
            // out-of-bounds field read on a `java/lang/Object` receiver, a
            // "Stale pointer detected in invokevirtual receiver" fallback, or
            // an `old-gen mark: rejecting external-overlay(BFS owner)` warning
            // when the stale address happens to land inside old gen's range.
            //
            // `run_non_moving_young_cycle` already publishes at this same
            // boundary for this same reason; this is that call on the moving
            // path. Idempotent with the VM's later pass: `pointer_map` here
            // holds only young→to-space / young→old-gen entries, whose keys and
            // values live in disjoint arenas, so a ref this remaps can never be
            // remapped a second time. (gen r4w5/pinned5: on a pinned in-place
            // cycle keys and values share from-space, and the property still
            // holds: every destination was a FREE byte when the cycle began, so
            // no value is any key, and a pinned object maps to itself.)
            cratonvm_gc_core::external_roots::remap_external_roots(&pointer_map);
            // H2-CID0: same reserved-TLAB-tail skip list the young sweep
            // uses; `major_gc`'s young walks parse from-space too.
            let young_skips = {
                let base = young_from.base_ptr() as usize;
                self.jit_tlab_skip_offsets(base, base + young_from.used())
            };
            // RESURRECTED FINALIZABLES ARE ROOTS OF THIS MAJOR (gengc-round4-move,
            // 2026-09-23).
            //
            // Phase 2.5 kept every dead finalizable object alive so its
            // `finalize()` can run, and `dead_finalizers` is the list the VM
            // hands the finalizer thread. One that was PROMOTED in doing so
            // (it was old enough, or promote-on-pressure was armed) now lives
            // in old gen referenced by nothing: not a root, not a young
            // survivor's field, not a marked old object's field. So the
            // same-cycle major below never marked it, and its in-place sweep
            // (the default: compaction is off, `oldgen_compact_enabled`) put it
            // straight back on the free list — along with everything reachable
            // only through it. The VM then enqueued a freed block for
            // finalization, and the next promotion that reuses it runs
            // `finalize()` on an unrelated object. With compaction on it was
            // worse in a second way: the address was never remapped through
            // `compact_map`, so even a survivor would have been reported at
            // its pre-compaction address.
            //
            // So: seed them as roots (the old-gen ones only — a young survivor
            // is already a major root through `mark_young_to_old_refs`), and
            // read their post-compaction addresses back out of the same slice.
            // The copy of `roots` is paid only on a cycle that promoted a dead
            // finalizable into a major; every other cycle passes `roots`
            // through exactly as before. (gcj j3/x2: that is the `=0` arm of
            // `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`; by default they are
            // the major's deferred seeds, below, marked in its resurrection
            // round, and the post-compaction remap of `dead_finalizers` is the
            // `compact_map` rewrite further down.)
            let finalizers_in_old: Vec<usize> = dead_finalizers
                .iter()
                .copied()
                .filter(|&a| old_gen.contains(a as *const u8))
                .collect();
            // gc-common w36-d: and the finalizables that died OLD, which no
            // young phase judges (Phase 2.5 skips every non-young candidate).
            // The major's own resurrection round keeps them for `finalize()`
            // (`OldGenFinalizerPass`); they join `dead_finalizers` below,
            // BEFORE its compaction-map rewrite.
            let mut old_fin_pass = OldGenFinalizerPass::new(finalizer_addrs);
            // gcj j3/rf3 (ec47, the legacy seed): Phase 2.5's resurrection set
            // at its post-copy addresses, for a legacy-seeded major to defer.
            old_fin_pass.young_resurrected = self.young_resurrection_members(&pointer_map);
            // gcj j2/rf2 (ec47, `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`):
            // the promoted resurrected finalizables are marked by the major's
            // resurrection round (`OldGenFinalizerPass::deferred_seeds`), not
            // rooted, so the old objects only they reach are the depth-2 set.
            // gcj j3/x2 R3: the pass's one read of the switch.
            let defer_fin_seeds = old_fin_pass.defers_seeds;
            let finalizers_in_old: Vec<usize> = if defer_fin_seeds {
                old_fin_pass.deferred_seeds.extend_from_slice(&finalizers_in_old);
                Vec::new()
            } else {
                finalizers_in_old
            };
            // gce e1/c (`gcd-d9a-moving-major-misses-side-table-rows-of-moved-owners-20260928`):
            // the `mirror_pin` / `metadata_pin` rows of every owner THIS
            // cycle moved. The rows are keyed by the owner's PRE-copy address
            // (the VM remaps them only after the collector returns), and the
            // root gatherer defers an OLD value to its owner, so the major's
            // BFS -- which looks a row up by the address it marked -- and its
            // legacy young seed -- which matches owners against the POST-swap
            // from-space -- both missed them: an old mirror or metadata value
            // of a young or just-promoted user loader was swept under it.
            // Every `pointer_map` source is a survivor of this cycle (the
            // legacy seed keeps every young survivor), so its rows' values
            // are roots. Two snapshots, taken only when a registry is
            // non-empty (`NON_EMPTY` latch): nothing on the common path.
            // gci i5/og (`gch-h3mj-...-review-findings` item 6, its
            // `metadata_pin` half; P-C only): under the moving true-root seed
            // "every source is a survivor" is false -- the young phase copies
            // a dead old holder's young child too -- so rooting a moved
            // owner's `metadata_pin` values kept them whatever the true roots
            // said (a never-started `Thread`'s inherited values under
            // `--compatible`). The P-C ask carries the rows RE-KEYED by the
            // owner's post-copy address instead (values mapped the same way,
            // `moving_metadata_rows_rekeyed`), and `old_gen_gc_inner` follows
            // them only for an owner its trace reaches, as the non-moving
            // true-root major does for an owner that did not move. The
            // `mirror_pin` half (owners are loaders, which the P-C seeds root
            // anyway) is unchanged.
            let metadata_rows = cratonvm_types::metadata_pin::snapshot();
            // gci i8/pa (`gch-h3mj-...-review-findings` item 6, its `mirror_pin`
            // and `loader_pin` thirds; P-C only): under
            // `CRATONVM_GC_MOVING_MAJOR_LOADER_REKEY` (opt-in), and always on a
            // licensed moving `System.gc()` (`side_table_licence`, whose young
            // mirrors were deferred to their loaders and are no root here), the
            // ask carries both registries RE-KEYED by post-copy address
            // (`moving_metadata_rows_rekeyed` for the mirror rows,
            // `moving_loader_rows_rekeyed` for the class -> loader rows), and the
            // moving cycle stops seeding every moved pinned loader and every
            // moved loader's mirrors: the major follows them only from a loader
            // or an instance its trace reaches. A fallback inside the call (the
            // legacy seed) roots this pause's promoted loaders instead
            // (`TrueRootAsk::fallback_loaders`), as before.
            let pc_rekey_loaders = moving_true_roots
                && (side_table_licence || moving_major_loader_rekey_enabled());
            let mirror_rows = cratonvm_types::mirror_pin::snapshot();
            let mut moved_owner_values = side_table_values_of_moved_owners(
                |owner| pointer_map.contains_key(&owner),
                mirror_rows.as_ref().filter(|_| !pc_rekey_loaders),
                metadata_rows.as_ref().filter(|_| !moving_true_roots),
            );
            let mut pc_metadata_rows: Option<FxHashMap<usize, Vec<usize>>> = None;
            let mut pc_metadata_moved_owners = 0usize;
            if moving_true_roots {
                if let Some(rows) = metadata_rows.as_ref() {
                    let (rekeyed, moved) = moving_metadata_rows_rekeyed(rows, &pointer_map);
                    pc_metadata_rows = Some(rekeyed);
                    pc_metadata_moved_owners = moved;
                }
            }
            drop(metadata_rows);
            let mut pc_mirror_rows: Option<FxHashMap<usize, Vec<usize>>> = None;
            let mut pc_loader_rows: Option<FxHashMap<u32, usize>> = None;
            if pc_rekey_loaders {
                if let Some(rows) = mirror_rows.as_ref() {
                    pc_mirror_rows = Some(moving_metadata_rows_rekeyed(rows, &pointer_map).0);
                }
                if cratonvm_types::loader_pin::loader_pinning_enabled() {
                    // Per class id, the row this heap owns when VMs share the
                    // id: one this pause moved, or an old one.
                    pc_loader_rows = cratonvm_types::loader_pin::snapshot_where(|a| {
                        pointer_map.contains_key(&a) || old_gen.contains(a as *const u8)
                    })
                    .map(|rows| moving_loader_rows_rekeyed(&rows, &pointer_map));
                }
            }
            drop(mirror_rows);
            let mut pc_fallback_loaders: Vec<usize> = Vec::new();
            // And the `loader_pin` VALUES this cycle promoted: the BFS looks
            // an old instance's loader up with `old_gen.contains`, which a
            // row still naming the loader's pre-copy YOUNG address fails, so a
            // user loader promoted in this pause and kept only by its old
            // instances was marked by nobody (a young survivor is retained by
            // the legacy seed; a promoted one is not). Over-retention: a
            // promoted loader survives this one major, as the young phase
            // that promoted it already judged it live.
            if cratonvm_types::loader_pin::loader_pinning_enabled() {
                let promoted_loaders = promoted_pinned_loaders(
                    &cratonvm_types::loader_pin::all_pinned_loaders(),
                    &pointer_map,
                    |a| old_gen.contains(a as *const u8),
                );
                // gci i8/pa: under the re-keyed rows the BFS finds a promoted
                // loader at its destination; only the legacy seed of a
                // fallback (whose young walk reads the registry's pre-copy
                // rows) still needs them as roots.
                if pc_rekey_loaders {
                    pc_fallback_loaders = promoted_loaders;
                } else {
                    moved_owner_values.extend(promoted_loaders);
                }
            }
            // gcf f1/c (`gcf-f1c-moving-major-frees-this-pauses-promotions-under-a-live-pointer-map-entry-20261001`):
            // THIS pause's promotion destinations are roots of the major, as
            // they are on the non-moving path (`sweep_old_gen_non_moving_impl`'s
            // `root_shadow`). The legacy seed below keeps every young
            // SURVIVOR's old referents, but a survivor the young phase PROMOTED
            // is no young survivor any more: one that only a dead old object
            // named (through its dirty card) was copied by the young phase,
            // entered `pointer_map` as `source -> destination`, and was then
            // swept by this major. The returned map still said it survived,
            // so a weak referent's `get()` (`watched_pre_gc_addr_survived`), a
            // finalizer row and every other source-keyed table were re-keyed
            // onto a freed block the next allocation reuses. Seeding them is
            // the legacy seed's own rule (a young survivor's subgraph is live
            // for this major), applied to the survivors it missed.
            // `CRATONVM_GC_MOVING_MAJOR_PROMOTION_SEED=0` restores the old seed.
            // gci i2/og: timed under `CRATONVM_DBG_GCPHASE` with the P-C seeds
            // below (both arms; outside `old_gen_gc_inner`'s phase lines).
            let t_prep = gc_flags().dbg_gcphase.then(std::time::Instant::now);
            let promotion_dests: Vec<usize> = {
                promotion_destinations_in(&pointer_map, |a| old_gen.contains(a as *const u8))
            };
            // gch h2/og (P-C): under the moving true-root seed the destinations
            // are NOT plain roots. They go in a tail the true-root seed skips
            // (`TrueRootAsk::promo_tail`), so a destination nothing reaches is
            // freed and leaves the map below (`drop_dead_moving_promotions`);
            // on a fallback inside `old_gen_gc_inner` (a walk gap, a
            // compaction) the tail is rooted exactly as without the switch.
            // The P-C seeds (`moving_true_root_extra_seeds`) are what the
            // legacy seed kept implicitly and a source-keyed table hands back
            // after the pause: every finalizable, every watched address and
            // every pinned loader this cycle moved, at their post-copy
            // addresses. Over-retention for those objects only, as on the
            // non-moving path. (gcj j3/x2: the finalizables only under
            // `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS=0`; by default their
            // copies are the major's deferred seeds -- see the hunk below.)
            // gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`, default ON since the i7 flip, `=0` kills; with
            // `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WATCHED_DEAD` at its default ON):
            // the watched set is NOT seeded; its young copies ride in the ask
            // (`TrueRootAsk::watched_post_copy`) and the ones the trace leaves
            // out are judged dead after the major (below). `pc_watched_copies`
            // is `(post-copy, pre-copy)`, sorted by post-copy.
            let mut pc_watched_copies: Vec<(usize, usize)> = Vec::new();
            // gci i3/og (`gci-i2og-...-review-findings` item 6): the one
            // watched snapshot of a P-C major, carried to `old_gen_gc_inner`
            // in the ask (`TrueRootAsk::watched_snapshot`) instead of cloned a
            // second time there.
            let mut pc_watched_snapshot: Option<Option<cratonvm_gc_core::gc_quiescence::WatchedSnapshot>> = None;
            let pc_seeds: Vec<usize> = if moving_true_roots {
                // gcj j3/rf3: with the seed-only row (`CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG`).
                let watched =
                    cratonvm_gc_core::gc_quiescence::watched_major_snapshot_for(self.watched_set_key());
                let watched_dead = watched.is_some()
                    && full_gc_true_roots_watched_dead_enabled();
                if watched_dead {
                    if let Some(w) = watched.as_ref() {
                        pc_watched_copies = moving_watched_young_copies(&pointer_map, w, |a| {
                            !old_gen.contains(a as *const u8)
                        });
                    }
                }
                // gci i8/pa: not under the re-keyed loader rows (the major
                // follows each loader from what reaches it).
                let loaders = if !pc_rekey_loaders
                    && cratonvm_types::loader_pin::loader_pinning_enabled()
                {
                    cratonvm_types::loader_pin::all_pinned_loaders()
                } else {
                    Vec::new()
                };
                // gcj j2/rf2 (ec47): the finalizables' copies are the major's
                // deferred seeds, marked in its resurrection round; a copy that
                // is also watched or a pinned loader stays a seed here.
                let seeds = moving_true_root_extra_seeds(
                    &pointer_map,
                    if defer_fin_seeds { &[] } else { finalizer_addrs },
                    watched.as_ref().filter(|_| !watched_dead),
                    &loaders,
                );
                if defer_fin_seeds {
                    old_fin_pass.deferred_seeds.extend(moving_true_root_extra_seeds(
                        &pointer_map,
                        finalizer_addrs,
                        None,
                        &[],
                    ));
                }
                pc_watched_snapshot = Some(watched);
                seeds
            } else if defer_fin_seeds {
                // gcj j3/x2 R3: on the legacy seed the promotion seed (gcf
                // f1/c) rooted EVERY destination -- the promoted resurrected
                // finalizables among them, and the promoted young objects only
                // they reach -- so ec47's deferral of `finalizers_in_old` was
                // undone here and their old children were strongly marked. A
                // destination in this pause's resurrection set is a deferred
                // seed instead: the round marks it (so it is kept, as the seed
                // kept it) and what only it reaches is the depth-2 set.
                let mut held = old_fin_pass.deferred_seeds.clone();
                if legacy_seed_defers_resurrected_enabled() {
                    held.extend_from_slice(&old_fin_pass.young_resurrected);
                }
                held.sort_unstable();
                held.dedup();
                for &dest in &promotion_dests {
                    if held.binary_search(&dest).is_ok() {
                        old_fin_pass.deferred_seeds.push(dest);
                    } else {
                        moved_owner_values.push(dest);
                    }
                }
                Vec::new()
            } else {
                moved_owner_values.extend_from_slice(&promotion_dests);
                Vec::new()
            };
            // gci i3/og: the ask's copy list (ascending, deduplicated).
            let mut pc_watched_post: Vec<usize> =
                pc_watched_copies.iter().map(|&(post, _)| post).collect();
            pc_watched_post.dedup();
            let pc_tail: &[usize] = if moving_true_roots {
                &promotion_dests[..]
            } else {
                &[]
            };
            if let Some(t) = t_prep {
                // gci i5/og: `md_rows=` / `md_moved=` -- the `metadata_pin`
                // owners the P-C ask carries re-keyed, and how many of them
                // this pause moved (0 / 0 off P-C or with no row).
                eprintln!(
                    "[gcphase] old-gen moving-major-prep: part={}us dests={} pc={} ip={} cf={} cmp={} md_rows={} md_moved={} rekey={} mr_rows={} ld_rows={} lic={}",
                    t.elapsed().as_micros(),
                    promotion_dests.len(),
                    u8::from(moving_true_roots),
                    // t1 lane F (gch-h3mj): the gate terms that can refuse P-C --
                    // the pinned in-place cycle, live compiled frames, a
                    // compacting major.
                    u8::from(in_place_cycle),
                    u8::from(has_conservative_roots),
                    u8::from(major_compacts),
                    pc_metadata_rows.as_ref().map_or(0, |m| m.len()),
                    pc_metadata_moved_owners,
                    // gci i8/pa: the loader / mirror re-key and the P-A
                    // licence of this pause (0 / 0 / 0 / 0 off both).
                    u8::from(pc_rekey_loaders),
                    pc_mirror_rows.as_ref().map_or(0, |m| m.len()),
                    pc_loader_rows.as_ref().map_or(0, |m| m.len()),
                    u8::from(side_table_licence),
                );
            }
            if moving_true_roots {
                TRUE_ROOT_MARK_OUT.with(|c| *c.borrow_mut() = TrueRootMarkOut::default());
            }
            // gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the parked
            // continuations the VM's split gather left out of `roots` because
            // none of their words is young. The young phase above needed none
            // of them (nothing old moved); this major marks (and may move) old
            // space, so they are TRUE roots of it, ahead of the skipped tail.
            // Empty unless the switch staged a section this pause.
            let parked_old_only = self.take_parked_old_only_roots();
            // t1 lane F (gch-h3mj step 3): a P-C major admitted under live
            // compiled frames (`CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_JIT`) roots
            // every captured word of this pause that names old space -- a
            // blocked peer's stack-slot captures (pure-native bands included,
            // which the helper-window pass records but does not append to
            // `roots`: class 2b) and the conservative deposits (class 2a) --
            // ahead of the skipped tail. The legacy seed kept such an old
            // object only when a young survivor also named it; the old mark's
            // root loop resolves an interior word to its object (H2-CID0).
            // Empty on every other pause.
            // t1 wave 2 lane W: and the captured words naming an object this
            // pause MOVED (an exact pre-copy base, `pointer_map` key), at its
            // post-copy address: the blocked-wake fold rewrites such a word
            // only after the collector returns, so the trace would otherwise
            // see a pre-copy address no grid holds (a pinned object maps to
            // itself). Over-retention only (`class2_words_mapped`).
            let jit_old_words: Vec<usize> = if moving_true_roots && has_conservative_roots {
                let mut w: Vec<usize> = Vec::new();
                cratonvm_gc_core::gc_quiescence::peer_stack_slot_values_into(&mut w, |_| true);
                cratonvm_gc_core::gc_quiescence::conservative_deposit_words_into(&mut w, |_| true);
                let w = class2_words_mapped(
                    w,
                    |v| old_gen.contains(v as *const u8),
                    |v| pointer_map.get(&v).copied(),
                );
                if gc_flags().dbg_gcphase {
                    eprintln!("[gcphase] old-gen moving-major-jit: words={}", w.len());                }
                w            } else {
                Vec::new()
            };
            // t1 wave 2 lane W (`CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_IN_PLACE`):
            // a P-C major on the pinned in-place cycle roots the plan's seeds
            // -- the pinned objects the young phase rooted from the pin
            // ledger, at their unchanged addresses -- ahead of the skipped
            // tail (`moving_major_gate_in_place_term`, consumer 3). Empty on
            // every other major.
            let pc_in_place_roots: &[usize] = if moving_true_roots && in_place_cycle {
                &pc_in_place_seeds[..]
            } else {
                &[]
            };
            if gc_flags().dbg_gcphase && moving_true_roots && in_place_cycle {
                eprintln!(
                    "[gcphase] old-gen moving-major-in-place: seeds={}",
                    pc_in_place_roots.len()
                );
            }
            let compact_map = if finalizers_in_old.is_empty()
                && moved_owner_values.is_empty()
                && pc_seeds.is_empty()
                && pc_tail.is_empty()
                && parked_old_only.is_empty()
                && jit_old_words.is_empty()
                && pc_in_place_roots.is_empty()
            {
                // gch h2/og: the ask, for the one `old_gen_gc_inner` call
                // `major_gc_finalizing` makes (it takes the slot).
                if moving_true_roots {
                    let mut ask = moving_true_root_ask(roots.len());
                    ask.watched_post_copy = std::mem::take(&mut pc_watched_post);
                    ask.watched_snapshot = pc_watched_snapshot.take();
                    ask.metadata_rows = pc_metadata_rows.take();
                    // gci i8/pa: the re-keyed loader / mirror rows (None off
                    // the re-key) and the fallback's promoted loaders.
                    ask.mirror_rows = pc_mirror_rows.take();
                    ask.loader_rows = pc_loader_rows.take();
                    ask.fallback_loaders = std::mem::take(&mut pc_fallback_loaders);
                    TRUE_ROOT_MARK_THIS_CALL.with(|c| *c.borrow_mut() = Some(ask));
                }
                // gci i8/pa: `pc_in_place` -- P-C won this major over
                // `CRATONVM_OLDGEN_COMPACT` (not a veto).
                Self::major_gc_finalizing_with(
                    roots,
                    &young_from,
                    &mut old_gen,
                    &young_skips,
                    Some(&mut old_fin_pass),
                    major_no_compact,
                    pc_in_place,
                )
            } else {
                let n = roots.len();
                let mut major_roots: Vec<ObjectRef> = Vec::with_capacity(
                    n + parked_old_only.len()
                        + finalizers_in_old.len()
                        + moved_owner_values.len()
                        + pc_seeds.len()
                        + pc_tail.len(),
                );
                major_roots.extend_from_slice(roots);
                // gci i13/vg: the old-only section (see above).
                major_roots.extend(parked_old_only.iter().map(|&a| {
                    // SAFETY: a word of a parked thread's root snapshot, which
                    // held it as an `ObjectRef` (non-null); never dereferenced
                    // here -- the major's root loop screens every root against
                    // the walked old-gen grid, as it did when this word was in
                    // `roots`.
                    unsafe { ObjectRef::from_raw(a as *mut u8) }
                }));
                major_roots.extend(finalizers_in_old.iter().map(|&a| {
                    // SAFETY: `a` is the address `forward_object` just copied a
                    // live (resurrected) object to, inside old gen.
                    unsafe { ObjectRef::from_raw(a as *mut u8) }
                }));
                major_roots.extend(moved_owner_values.iter().chain(pc_seeds.iter()).map(|&v| {
                    // SAFETY: non-null and 8-aligned
                    // (`side_table_values_of_moved_owners` and
                    // `moving_true_root_extra_seeds` filter), which is
                    // `ObjectRef::from_raw`'s whole contract; never
                    // dereferenced here -- the root loop screens it against
                    // the walked old-gen grid first, and the true-root seed
                    // against the young grid.
                    unsafe { ObjectRef::from_raw(v as *mut u8) }
                }));
                // t1 lane F (gch-h3mj step 3): the captured old words (empty
                // unless `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_JIT` admitted
                // this pause), true roots ahead of the tail.
                major_roots.extend(jit_old_words.iter().map(|&v| {
                    // SAFETY: non-zero (`class2_old_words`), inside the old
                    // generation's range or (lane W) a `pointer_map` value of
                    // this pause; never dereferenced here -- the root loop
                    // resolves it against the walked old-gen grid, and the
                    // true-root seed against the young grid.
                    unsafe { ObjectRef::from_raw(v as *mut u8) }
                }));
                // t1 wave 2 lane W: the in-place plan's seeds (empty unless
                // `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS_IN_PLACE` admitted this
                // pause), true roots ahead of the tail.
                major_roots.extend(pc_in_place_roots.iter().map(|&v| {
                    // SAFETY: a plan object's base, an object start of the
                    // pre-GC walk that the in-place copy left where it was
                    // (non-null, 8-aligned); never dereferenced here -- the
                    // root loop screens it against the walked old-gen grid,
                    // and the true-root seed against the young grid.
                    unsafe { ObjectRef::from_raw(v as *mut u8) }
                }));
                // gch h2/og: the skipped tail (P-C only; empty otherwise).
                let promo_tail = major_roots.len();
                major_roots.extend(pc_tail.iter().map(|&v| {
                    // SAFETY: as above (`promotion_destinations_in` filters).
                    unsafe { ObjectRef::from_raw(v as *mut u8) }
                }));
                if moving_true_roots {
                    let mut ask = moving_true_root_ask(promo_tail);
                    ask.watched_post_copy = std::mem::take(&mut pc_watched_post);
                    ask.watched_snapshot = pc_watched_snapshot.take();
                    ask.metadata_rows = pc_metadata_rows.take();
                    // gci i8/pa: the re-keyed loader / mirror rows (None off
                    // the re-key) and the fallback's promoted loaders.
                    ask.mirror_rows = pc_mirror_rows.take();
                    ask.loader_rows = pc_loader_rows.take();
                    ask.fallback_loaders = std::mem::take(&mut pc_fallback_loaders);
                    TRUE_ROOT_MARK_THIS_CALL.with(|c| *c.borrow_mut() = Some(ask));
                }
                // gen r4w5/oomjit5: the census labels the appended
                // finalizables by their seeding site.
                let tail_label = if moving_true_roots {
                    // gch h2/og: the P-C seeds, then (unless the seed runs)
                    // the destinations.
                    "moving-true-root seeds+side-table(moved owner)+finalizer-old"
                } else if moved_owner_values.is_empty() {
                    "finalizer-old"
                } else if !promotion_dests.is_empty() {
                    // gcf f1/c: the destinations dominate this tail.
                    "promotion-dest(moving)+side-table(moved owner)+finalizer-old"
                } else if finalizers_in_old.is_empty() {
                    "side-table(moved owner)"
                } else {
                    "finalizer-old+side-table(moved owner)"
                };
                let _census_tail = oldmark_census::enabled()
                    .then(|| oldmark_census::TailScope::new(n, tail_label));
                // gci i8/pa: as above.
                let map = Self::major_gc_finalizing_with(
                    &mut major_roots,
                    &young_from,
                    &mut old_gen,
                    &young_skips,
                    Some(&mut old_fin_pass),
                    major_no_compact,
                    pc_in_place,
                );
                // The compaction's root fixup rewrote the slice in place;
                // hand the caller's half back.
                roots.copy_from_slice(&major_roots[..n]);
                map
            };
            // gch h2/og (P-C): the dead promotions. The ask is taken by the
            // call; clear the slot anyway so nothing can leak into a later
            // old sweep. When the seed RAN (no fallback inside the call: the
            // in-place arm, never a compaction), a destination it did not
            // reach was freed; its source must leave `pointer_map`, or every
            // source-keyed table (a weak referent's survival, a monitor, a
            // finalizer row, the String table) is re-keyed onto a freed block.
            // The moving twin of `TrueRootSweepOut::dead_sources`.
            if moving_true_roots {
                TRUE_ROOT_MARK_THIS_CALL.with(|c| *c.borrow_mut() = None);
                let mark_out = TRUE_ROOT_MARK_OUT.with(|c| std::mem::take(&mut *c.borrow_mut()));
                if mark_out.ran {
                    // gci i2/og: `pc_tail` is every old destination of the
                    // map (ascending, deduplicated) whenever the promotion
                    // seed built it, so the verdict is one merge walk and the
                    // map is touched only for a dead one
                    // (`drop_dead_moving_promotions_sorted`). Empty: the seed
                    // is off (`CRATONVM_GC_MOVING_MAJOR_PROMOTION_SEED=0`) or
                    // nothing was promoted -- the per-entry verdict, as before.
                    // Timed under `CRATONVM_DBG_GCPHASE` (it runs outside
                    // `old_gen_gc_inner`'s `[gcphase] old-gen` lines).
                    let t_judge = gc_flags().dbg_gcphase.then(std::time::Instant::now);
                    let (judged, dead) = if moving_true_roots && pc_tail.is_empty() {
                        (0, 0)
                    } else if pc_tail.is_empty() {
                        drop_dead_moving_promotions(
                            &mut pointer_map,
                            |a| old_gen.contains(a as *const u8),
                            |a| old_gen.is_allocated_addr(a as *const u8),
                        )
                    } else {
                        drop_dead_moving_promotions_sorted(&mut pointer_map, pc_tail, |d| {
                            old_gen.unallocated_among_ascending(d)
                        })
                    };
                    if let Some(t) = t_judge {
                        let us = t.elapsed().as_micros();
                        eprintln!(
                            "[gcphase] old-gen pc-judge: part={us}us judged={judged} dead={dead}"
                        );
                    }
                    old_gen.note_true_root_promotions(judged, dead);
                    // gci i3/og (`CRATONVM_GC_MOVING_MAJOR_WATCHED_DEAD`): the
                    // watched young copies the trace left out are dead. Their
                    // SOURCES leave the map (the moving path's survival proof)
                    // and become the heap's verdict until the next collection
                    // starts (`true_root_watched_left_out`), as the non-moving
                    // major installs its left-out identity entries.
                    if !mark_out.watched_left_out_post.is_empty() {
                        let sources = moving_watched_left_out_sources(
                            &pc_watched_copies,
                            &mark_out.watched_left_out_post,
                        );
                        let n = self.install_true_root_watched_left_out(sources, &mut pointer_map);
                        if watchref_dbg() {
                            eprintln!(
                                "[watchref] moving true-root major: {n} watched young survivor(s) left out by the trace, judged dead (total {})",
                                self.true_root_watched_left_out_total(),
                            );
                        }
                    }
                }
            }
            // gen r5w3/oldgen7: the resize's interior half (opt-in), and the
            // young budget an old generation that just grew borrowed (opt-in;
            // the region bounds are republished at this cycle's end, below).
            self.decommit_old_interior_if_enabled(&mut old_gen);
            self.cede_young_budget_to_old(old_gen.capacity());
            dead_finalizers.extend_from_slice(&old_fin_pass.resurrected);
            // gcj j4/rf (`CRATONVM_GEN_FIN_ROUND_RECORDS_YOUNG`): the young
            // objects the finalizer round reached, recorded at their post-copy
            // addresses (the P-C trace's grid), back to the pre-copy sources the
            // processor's rows hold.
            if !old_fin_pass.closure_young_post.is_empty() {
                let mut post = std::mem::take(&mut old_fin_pass.closure_young_post);
                post.sort_unstable();
                post.dedup();
                old_fin_pass.closure.extend(
                    pointer_map
                        .iter()
                        .filter(|&(_, p)| post.binary_search(p).is_ok())
                        .map(|(&pre, _)| pre),
                );
            }
            // gcj j3/x2 R2: `take_resurrection_closure` promises PRE-collection
            // addresses. On this moving cycle the round also pops this pause's
            // PROMOTION destinations (the promoted resurrected finalizables it
            // was handed as deferred seeds, and what a deferred young candidate
            // reaches after the copy), and pushed them as they were popped --
            // post-copy old addresses no reference-processor row holds (j4/rf
            // maps them back only under the P-C trace). Each is mapped back to
            // its young source here. Exact: a destination was free when the
            // cycle began, so it is never also an old object's pre-collection
            // address, and a source (young) is never an old destination.
            map_promotion_destinations_to_sources(&mut old_fin_pass.closure, &pointer_map, |a| {
                old_gen.contains(a as *const u8)
            });
            self.note_resurrection_closure(&old_fin_pass.closure);
            // Every reported finalizable at its FINAL address. The ones that
            // did not move are absent from the map and keep their address.
            for addr in dead_finalizers.iter_mut() {
                if let Some(&final_addr) = compact_map.get(addr) {
                    *addr = final_addr;
                }
            }
            // CRITICAL FIX (heavy binary-trees GC corruption):
            //
            // Compose `pointer_map` with `compact_map` BEFORE merging. If a
            // minor-GC entry says `young_addr → promoted_addr` AND the major
            // GC compacted `promoted_addr → compacted_addr`, a naive merge
            // leaves the minor entry pointing at the now-stale `promoted_addr`.
            // `update_all_roots` does a single-step lookup per slot — so a
            // frame local that originally held `young_addr` would be rewritten
            // to `promoted_addr`, dereferencing freed/overwritten memory on
            // the next field read (the visible symptom: Node objects come back
            // as `java/lang/Object class_id=0 num_slots=0`).
            //
            // Walk all existing entries and chain any value that appears as a
            // compact_map key through to its final destination, THEN merge the
            // raw compact_map so external roots (statics, JNI, etc.) that
            // pointed directly at an uncompacted old-gen object also get the
            // correct post-compaction target.
            for new_addr in pointer_map.values_mut() {
                if let Some(&final_addr) = compact_map.get(new_addr) {
                    *new_addr = final_addr;
                }
            }
            // Remembered-set fixup across compaction: the deferred old→young
            // referrer addresses were recorded pre-compaction. Remap each
            // through `compact_map` to its post-compaction location before
            // dirtying its card, so the next minor GC's dirty-card scan finds
            // the relocated referrer (and therefore its old→young edge).
            // Referrers that did not move are absent from `compact_map` and
            // keep their original address.
            // gen r5w2/roots6 (cards3 item 2): the HOLDER is remapped, then
            // the element offset re-added, so an element card follows its
            // array across the compaction.
            let remapped_cards: Vec<usize> = deferred_dirty_cards
                .iter()
                .map(|&(holder, offset)| {
                    compact_map.get(&holder).copied().unwrap_or(holder) + offset
                })
                .collect();
            card_table.mark_dirty_bulk(&remapped_cards);
            // Merge old-gen compaction relocations into the overall pointer map
            // so the VM can update external roots (statics, JNI, string pool, etc.)
            pointer_map.extend(compact_map);
            let old_used_after = old_gen.used();
            // `used` can rise after compaction if the compactor's metadata
            // overhead exceeds reclaimed garbage; clamp with saturating_sub.
            self.stats.bytes_freed_old.fetch_add(
                old_used_before.saturating_sub(old_used_after) as u64,
                Ordering::Relaxed,
            );
            true
        } else {
            // No major GC: old-gen referrers did not move, so dirty their
            // cards at the addresses recorded during this cycle.
            let cards: Vec<usize> = deferred_dirty_cards
                .iter()
                .map(|&(holder, offset)| holder + offset)
                .collect();
            card_table.mark_dirty_bulk(&cards);
            // gen r4w6/oldpin6: the resize a concurrent cycle's sweep left due.
            if Self::resize_old_gen_after_concurrent_sweep(&mut old_gen) {
                // gen r5w3/oldgen7: and its interior half (opt-in); gen
                // r5w6/old10: retried at the next pause when damped.
                self.decommit_old_interior_after_concurrent_resize(&mut old_gen);
            }
            // gcj j2/og2 B4: the free-ratio growth that sweep left due; the
            // region bounds are republished at this cycle's end, below.
            if self.grow_old_gen_after_concurrent_sweep(&mut old_gen) {
                self.cede_young_budget_to_old(old_gen.capacity());
            }
            false
        };

        // bc math-ec 0x4 seed-phase bisect: count `0x4` slots in the post-swap
        // young from-space (the just-collected survivors) and in old gen AFTER
        // the possible major GC. A jump in the OLD count from `sh_cheney_old`
        // to here means the MAJOR mark-compact (update_refs_in_object /
        // fixup_young_old_refs) seeded the `0x4` — see handoff §6.1.
        if seedhunt_enabled() {
            let py = seedhunt_scan_young(
                young_from.base_ptr(),
                young_from.used(),
                "post",
                &mut sh_printed,
                sh_cap,
            );
            let mut po = 0usize;
            for (op, _) in old_gen.walk_objects() {
                // SAFETY: `op` is a live old-gen object header from walk_objects.
                let h = unsafe { &*(op as *const ObjectHeader) };
                po += seedhunt_scan_obj(op, h, "post", "O", &mut sh_printed, sh_cap);
            }
            if sh_entry_old | sh_cheney_young | sh_cheney_old | py | po != 0 {
                eprintln!(
                    "[seedhunt] GC entry_old={} | post-cheney young={} old={} | major_ran={} | \
                     post-major young={} old={}",
                    sh_entry_old, sh_cheney_young, sh_cheney_old, major_ran, py, po,
                );
            }
        }

        mv_phase!("major_check");
        // Phase 4b (moved): remap monitors with the FINAL composed pointer_map.
        // Doing this after a possible major GC ensures monitor keys for
        // promoted-then-compacted objects are remapped to their final
        // post-compaction addresses, not the intermediate post-promotion ones.
        monitors.remap_after_gc(&pointer_map);

        mv_phase!("monitor_remap");
        let bytes_freed = bytes_before.saturating_sub(bytes_copied);

        // Phase 6: Adaptive heap expansion.
        // If GC didn't reclaim enough space (less than 25% of capacity freed),
        // expand the young gen to avoid GC thrashing.
        //
        // After swap: young_from has live data, young_to is empty (was reset).
        // We can only safely grow young_to (empty arena). This means the
        // expansion takes effect on the NEXT GC cycle — when live objects are
        // copied into the now-larger to-space, which then becomes from-space.
        // gengc-round4-move (2026-09-23): the survival the two policies below
        // act on is measured from the copy TALLY — bytes of survivors that
        // stayed young — not from `bytes_copied` (to-space occupancy). The two
        // are identical on a serial cycle, whose to-space is survivors packed
        // end to end. On a PARALLEL cycle to-space also holds the fillers over
        // retired per-worker buffer tails and any forwarding-CAS loser's
        // abandoned copy, none of which survived anything; measured at up to
        // 327 KiB of filler against 393 KiB of survivors on a small young
        // generation (`PLAB_LIVE_DIVISOR`'s note). Reading occupancy made the
        // parallel arm report a lower `freed_percent` than the serial arm on
        // the same heap, arming promote-on-pressure and young expansion on
        // cycles the serial arm would not — a behaviour difference between the
        // two evacuators that `CRATONVM_GC_PAR_EVAC=0` would then "bisect".
        // `bytes_freed` (the `GcStats` field) keeps its documented meaning.
        let survived_young = (bytes_copied_young_cycle as usize).min(bytes_copied);
        let freed_percent = if bytes_before > 0 {
            bytes_before.saturating_sub(survived_young) * 100 / bytes_before
        } else {
            100
        };
        // Promote-on-pressure: if survival was very high this cycle,
        // arm the flag so the NEXT minor GC promotes ALL survivors to
        // old gen regardless of age. Otherwise a long-lived working set
        // gets copied semi→semi forever (binary-trees death spiral).
        //
        // Only arm when the *from* itself is at high occupancy. The
        // freed_percent metric measures young_to.used vs young_from.used,
        // but a low percentage when bytes_before is tiny (e.g. a partial-
        // allocation cycle) doesn't indicate real pressure. Gating on
        // bytes_before >= 1/2 of the from capacity rules out those
        // false-positives. force_promote_all was already consumed (and
        // cleared) at the top of this function via `swap`; setting it
        // here arms the *next* cycle.
        // `young_to` post-swap is the arena we just *collected* — its
        // capacity is what `bytes_before` was measured against.
        // gen r4w5/pinned5: after a pinned in-place cycle nothing was swapped,
        // so the arena that was collected is still `young_from`.
        let from_cap_before = if in_place_cycle {
            young_from.capacity()
        } else {
            young_to.capacity()
        };
        self.arm_promote_pressure_on_high_survival(bytes_before, survived_young, from_cap_before);

        // gen r4w6/young6: ADAPTIVE TENURING. Recompute the next cycle's
        // threshold from this cycle's age table (HotSpot's
        // `compute_tenuring_threshold`), against a target tied to the young
        // trigger; see `gen_heap_tenuring.rs`. The age table is the graded
        // response to survivor volume; the severe arm just above stays the
        // one-cycle `T = 0` floor (the `force_promote_all` short-circuit wins
        // over any threshold in `should_tenure`).
        // gcf f2/y: against the CONFIGURED eden equivalent of the arena this
        // cycle collected (`tenuring_eden_bytes`), not the pause-goal-adapted
        // live trigger -- HotSpot Serial's survivor target moves only with
        // the heap's geometry.
        if promoted_worklist.age_census {
            let eden = self.tenuring_eden_for(from_cap_before); // gcf f7/c
            let decision = if tenuring_engaged {
                self.tenuring.adapt(&promoted_worklist.ages, eden)
            } else {
                self.tenuring
                    .fixed_decision(eden, PROMOTION_AGE.saturating_sub(1))
            };
            if tenuring_print {
                for line in tenuring::distribution_lines(&decision, &promoted_worklist.ages) {
                    eprintln!("{line}");
                }
            }
            // gcf f4/r: the `gc,age` lines (`-Xlog:gc+age`).
            if self.tenuring.xlog_requested() {
                self.tenuring.stash_xlog(&decision, &promoted_worklist.ages);
            }
        }

        // Only expand when the young arena was actually under pressure at GC
        // start. Without this gate, a FORCED or early collection — e.g.
        // `System.gc()` fired when young holds only a few KB — computes
        // `freed_percent` against a tiny live set, reads it as "low reclamation",
        // and doubles the arena. Repeated across frequent forced GCs the young
        // balloons to `max_young_semi_size` (multi-GB) even though gigabytes are
        // free; growing + zeroing that arena under a stop-the-world is the
        // multi-thread GC "hang"/OOM (6 threads each looping `System.gc()` —
        // scratch_churn/Churn.java). Mirrors the `high_survival` occupancy guard
        // above (same `from_cap_before / 2` threshold): a young collected at its
        // `YOUNG_GC_THRESHOLD_PERCENT` (50%) natural-GC trigger still expands
        // (`bytes_before >= cap/2` holds), while a tiny forced/early GC
        // (`System.gc` at well under 50% full) does not.
        // gen r4w5/pinned5: a pinned in-place cycle does not expand. Growing
        // the idle arena only pays once a swap makes it from-space, which this
        // cycle does not do, and the threshold update below would then name a
        // capacity from-space does not have. The next Cheney cycle expands.
        // gci i10/gy (pinned5 L3, REFUTED by reading): on every heap the VM
        // builds (`VmHeap` -> `with_capacity_and_young`, i.e. `with_capacity`
        // without young flags) `max_young_semi_size` IS the starting
        // semi-space, so this arm grows nothing on ANY cycle, Cheney or
        // pinned; it can grow only on the explicit-size constructors (`new` /
        // `with_sizes`: unit tests and embedders). `gci_i10gy_tests` pins
        // that; a constructor that leaves young room to grow reopens L3.
        if !in_place_cycle
            && freed_percent < GC_EXPANSION_THRESHOLD_PERCENT
            && bytes_before >= from_cap_before / 2
        {
            let current_cap = young_to.capacity();
            // Whole-heap budget (see `max_heap_bytes`). The steady state after
            // this growth has BOTH semi-spaces at `new_cap` — they swap roles
            // every cycle and a copying collection needs a to-space as large
            // as the from-space — so the admissible ceiling is
            // `(Xmx - old_capacity) / 2`, not `Xmx - old - other_semi`.
            // `usize::MAX` (the explicit-size constructors) leaves this
            // saturating to "no bound", reproducing the historical behaviour.
            let budget_ceiling = if self.max_heap_bytes == usize::MAX {
                usize::MAX
            } else {
                self.max_heap_bytes.saturating_sub(old_gen.capacity()) / 2
            };
            let new_cap = (current_cap * 2)
                .min(self.max_young_semi_size)
                .min(budget_ceiling.max(current_cap));
            if new_cap > current_cap {
                tracing::debug!(
                    "GC: low reclamation ({}% freed) — expanding young to-space {} → {} bytes",
                    freed_percent,
                    current_cap,
                    new_cap,
                );
                // young_to is empty after reset+swap, safe to grow
                young_to.grow(new_cap);
            }
            // gci i10/gy: the capacity young_to really reached. A refused
            // reservation leaves it as it was (`Arena::grow`), and `grow`
            // rounds the request down to 8, so the threshold is sized from
            // the arena, never from the request: a threshold for a capacity
            // neither semi has would trigger past what from-space can hold.
            let new_cap = young_to.capacity();
            if new_cap > current_cap {
                // Update threshold based on the upcoming larger from-space.
                //
                // When a pause goal is armed the threshold is NOT a function of
                // capacity any more — `adapt_young_trigger_to_pause` drove it
                // down from pauses it measured. Restoring the capacity
                // percentage here would undo that on every expansion, so take
                // the smaller of the two: the goal keeps its say, and the new
                // capacity still supplies the ceiling.
                let default_for_new_cap = self.configured_young_trigger(new_cap); // gcf f7/c
                                                                                  // STW: heap expansion runs inside the collection pause.
                let current = self.young_gc_threshold.load(Ordering::Relaxed);
                let next = if self.young_pause_goal_ms > 0 {
                    current.min(default_for_new_cap)
                } else {
                    default_for_new_cap
                };
                self.young_gc_threshold.store(next, Ordering::Relaxed);
            }
        }

        // Republish region bounds: the arenas were swapped (and to-space may
        // have been grown to a new backing) above, so the lock-free
        // `is_object_address` cache must reflect the post-collection
        // `[base, end)` before any mutator resumes. Guards still held.
        self.store_region_bounds_locked(&young_from, &young_to, &old_gen);

        mv_phase!("heap_expand");
        // gen-gc-five: hand the evacuated semi-space's zeroing to the wipe
        // thread. After the swap that arena is `young_to`, and the expansion
        // above is the last thing this cycle does to it, so its backing is
        // final here. The bytes are zero long before the next cycle needs
        // them, and `collect_garbage_inner` joins the thread before it looks.
        if let Some(wipe) = deferred_wipe {
            let mut spans = young_to.deferred_wipe_spans(&wipe);
            // GIVE BACK FIRST, THEN WIPE WHAT IS LEFT.
            //
            // `CRATONVM_GEN_UNCOMMIT`, default ON; see
            // `uncommit_evacuated_young` for what it costs and why the default
            // is a measurement rather than a judgement. This is the arena the
            // collection evacuated FROM, so nothing live is in it, and the very
            // next thing this cycle does to it is zero it -- which is the same
            // statement made to the bytes instead of to the OS.
            //
            // The order is load-bearing, and the first attempt got it wrong in
            // both available ways. Putting the give-back in an `else` of this
            // `if` made it STRUCTURALLY DEAD: `deferred_wipe` is `Some` on the
            // default path (it is the final `else` of the reset arm above), so
            // the arm never ran and the census read zero for a switch that was
            // on. Running both against overlapping spans is worse than dead --
            // a released granule FAULTS on write, and the wipe thread would be
            // writing into memory this call had returned.
            //
            // They are not interchangeable, which is why the wipe is not simply
            // skipped: a released granule comes back from the OS zeroed, but
            // `Arena::hand_out` does not zero, and the give-back only releases
            // WHOLE granules rounded inward. The partial granules at a span's
            // edges still hold the previous cycle's object bytes and still have
            // to be written over. `retain_committed_spans` is exactly that
            // remainder.
            // gce e1/y (opt-in `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS`): keep as
            // much of the next to-space committed as this cycle's young
            // survivors needed (+25 %), so the next copy does not fault it in
            // inside its pause. See `uncommit_evacuated_young_keeping`.
            let keep = uncommit_keep_for_survivors(
                young_from.used(),
                true,
            );
            let released = self.uncommit_evacuated_young_keeping(&mut young_to, keep);
            if released != 0 {
                spans = young_to.retain_committed_spans(spans);
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = released,
                    "gen: returned the evacuated young semi-space to the OS",
                );
            }
            if mv_phase_on {
                let bytes: usize = spans.iter().map(|&(_, l)| l).sum();
                moving_phase_count_push("wipe_deferred_bytes", bytes as u128);
            }
            self.spawn_evacuated_wipe(spans);
        } else if !in_place_cycle {
            // The synchronous-wipe and quarantine arms already zeroed (or
            // parked) the arena, so there is no wipe to coordinate with.
            //
            // gen r4w5/pinned5: a pinned in-place cycle evacuated no arena —
            // `young_to` is the idle semi-space the previous cycle already
            // wiped and gave back, and the pinned pages are in from-space,
            // where neither the wipe nor the give-back ever reaches.
            let _ = self.uncommit_evacuated_young(&mut young_to);
        }
        // gcd d9/c: a completed moving or pinned in-place cycle copied (or
        // promoted) its survivors, so young's drain was not blocked. Stored
        // before the count moves: the trigger samples its floor once per
        // count (see `young_drain_blocked`).
        self.last_cycle_drain_blocked
            .store(false, Ordering::Relaxed);
        // Phase H (RH.1): commit per-cycle counters to the lifetime
        // accumulator. Do this at the end so tests can observe GC
        // statistics after the call returns.
        let minor_after = self.stats.minor_gc_count.fetch_add(1, Ordering::Relaxed) + 1;
        if in_place_cycle {
            // gen r4w5/pinned5: like a non-moving cycle, a pinned in-place one
            // leaves `used` a high-water mark over a free list, so the
            // `CRATONVM_DBG_GC_STRESS` trigger must read live occupancy after
            // it (see `gc_stress_decision`), or it collects at every carve.
            self.young_stress_epochs[0].store(minor_after, Ordering::Relaxed);
        }
        self.stats
            .bytes_promoted
            .fetch_add(bytes_promoted_cycle, Ordering::Relaxed);
        self.stats
            .objects_promoted
            .fetch_add(objects_promoted_cycle, Ordering::Relaxed);
        self.stats
            .bytes_copied_young
            .fetch_add(bytes_copied_young_cycle, Ordering::Relaxed);
        self.stats
            .objects_copied_young
            .fetch_add(objects_copied_young_cycle, Ordering::Relaxed);
        if major_ran {
            self.stats.major_gc_count.fetch_add(1, Ordering::Relaxed);
        }
        // gengc-round2-core2 (2026-09-20): the last mark of the moving arm.
        //
        // Without it, everything from `heap_expand` to here — the evacuated
        // semi-space give-back (`uncommit_evacuated_young`, a `madvise`/
        // `VirtualFree` over up to a whole semi-space), the wipe hand-off, and
        // the counter commits — was charged to no phase at all and vanished
        // into the residual. `render_gcpause_line` would still have balanced
        // (that is what `other=` is for), but a named cost is better than an
        // anonymous one, and this stretch calls into the OS.
        mv_phase!("epilogue");

        (
            GcResult {
                stats: cratonvm_gc_core::gc::GcStats {
                    objects_copied,
                    bytes_copied,
                    bytes_freed,
                },
                pointer_map,
            },
            dead_finalizers,
        )
    }

}
