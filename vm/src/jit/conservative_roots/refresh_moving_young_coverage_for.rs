#[allow(unused_imports)]
use super::*;

/// Collection-authoritative moving-young coverage refresh.
///
/// [`refresh_moving_young_coverage_for_current_thread`] proves (or disproves)
/// the obligation for the frames THIS thread owns. It is called both by each
/// mutator's root-snapshot deposit — where per-thread scope is exactly right —
/// and by the collection's own root gather, where it is **not sufficient**:
/// a collection must account for every live JIT frame in the process, not just
/// the initiator's.
///
/// The cross-thread obligations are discharged elsewhere for the two cases that
/// have machinery: a cooperatively-parked peer publishes its shadow values into
/// its `root_snapshot` and remaps its own shadow stack on resume, and an
/// OS-frozen / helper-window peer is scanned conservatively and marks the cycle
/// incomplete.
///
/// Until 2026-08-23 neither mechanism gave the *initiator* a positive proof at
/// the moment it decides whether to relocate, so the rule was: **any** peer in
/// JIT makes the cycle unproven. That is the
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md` residual —
/// on a many-threaded workload it fires on nearly every cycle, and on ZGC,
/// where relocation is the only defragmentation there is, the consequence is an
/// `OutOfMemoryError` on a heap that is 97 % free.
///
/// [`publish_peer_jit_coverage_for_stw`] is the handshake that replaces it. A
/// peer proves its OWN frames rewritable, on its own thread, at its own park,
/// and deposits the proven depth; this function accepts the cycle only when the
/// deposits account for **every** peer JIT entry in the process. The
/// accounting is a depth comparison rather than a thread count because
/// `GLOBAL_JIT_DEPTH` is the only process-wide view available and it is a
/// depth: `peer = global - mine`, and `proven >= peer` is the acceptance test.
/// Everything the handshake cannot see — an OS-frozen peer, a peer blocked in a
/// native with compiled frames below it, a peer whose own proof failed —
/// deposits nothing, so the shortfall refuses the cycle exactly as before.
///
/// `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` restores the blanket refusal on the
/// same binary, which is what makes this an A/B rather than a rebuild.
/// Over-diverting costs compaction; under-diverting costs the heap.
pub fn refresh_moving_young_coverage_for_collection(pins_honoured: bool) -> bool {
    if !moving_young_enabled() {
        return true;
    }
    // gci i23/r6: the collector's own proof never reads the generation-keyed
    // memo (`frozen_band_memo_armed`); a no-op unless that switch is on.
    let mut complete = with_collector_proof(refresh_moving_young_coverage_for_current_thread);
    // gen r4w5/pinwords5: the INITIATOR's half of the young pin-word ledger.
    // Every peer has parked (and deposited) by now; the proof above has pruned
    // this thread's returned entries, so the depth deposited is the live one.
    // gcj j1pn ledger item 3: the initiator's deposit skips the pause's frames.
    deposit_pause_young_pin_words(true);
    let peer_depth = peer_jit_depth();
    if peer_depth == 0 {
        // The unguarded door. Everything below is the peer ledger; a zero skips
        // all of it, so a zero has to be recorded where it happens or it is
        // indistinguishable from an accounted cycle in every census.
        cratonvm_gc::gc_quiescence::note_peer_depth_zero(
            global_jit_depth_raw(),
            current_thread_jit_depth(),
        );
    }
    if peer_depth > 0 {
        let proven = cratonvm_gc::gc_quiescence::peer_proven_jit_depth();
        // Depth belonging to peers this cycle discharged by PINNING instead of
        // by proof (see `add_xt_cycle_pinned_jit_depth`). Two conditions, and
        // both are load-bearing:
        //
        //  - the credit is enabled, so the old behaviour is one flag away; and
        //  - EVERY helper window this cycle was pinned. A single unpinned
        //    window means some frozen peer's stack is neither proven nor
        //    immobile, and since the account is one process-wide subtraction it
        //    cannot exclude just that peer -- so it credits nothing at all.
        //
        // `xt_cycle_pinned_jit_depth` applies the third condition itself: it
        // returns 0 if any pinned peer's depth was unknown.
        //  - AND THE COLLECTOR ABOUT TO RUN HONOURS THE PIN. This one was
        //    missing, and it is the difference between a discharge and a lie.
        //
        //    The credit says "this peer's frames need not be rewritable,
        //    because the objects they name will not move". Only a collector
        //    that can WITHHOLD an object makes that true. G1 withholds the
        //    region and ZGC the page, and both consume
        //    `gc_quiescence::pinned_jit_roots_snapshot()` to do it. The
        //    generational young collector is Cheney copying: from-space is
        //    reclaimed wholesale, so every live object in it moves by
        //    construction, and `gen_heap.rs` and `gen_evac.rs` accordingly
        //    contain no reader of that snapshot. The pinned addresses reach
        //    that backend only as ROOTS -- which keeps them ALIVE, and says
        //    nothing whatever about keeping them PUT.
        //
        //    So on the generational collector this credit discharged a peer's
        //    coverage obligation against a pin nobody applied, the cycle
        //    relocated, and the peer resumed with a stale reference in a
        //    compiled frame. Reproducible in ten seconds on the H2 JDBC corpus
        //    with `CRATONVM_GEN_UNCOMMIT=1` as the detector -- 3/3 SIGSEGV, and
        //    0/3 with either half of this credit switched off
        //    (`CRATONVM_XT_PINNED_PEER_DEPTH=0`,
        //    `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY=1`). The accounting line
        //    immediately before the fault was
        //    `peer_depth=2 proven=0 pinned=2 accounted=true`; the run that did
        //    not crash had `pinned=0 accounted=false`.
        //
        //    `CRATONVM_XT_PINNED_PEER_UNPINNABLE=1` restores the old behaviour
        //    on the same binary, which is what makes this an A/B rather than a
        //    rebuild -- and, on a collector that cannot pin, a way to reproduce
        //    the defect deliberately.
        let pinned = if pinned_credit_admissible(
            xt_pinned_peer_depth_enabled(),
            xt_pinned_peer_publish_only(),
            pins_honoured,
            xt_pinned_peer_credit_when_unpinnable(),
            crate::jit::xt_root_scan::helper_windows_all_pinned_this_cycle(),
        ) {
            cratonvm_gc::gc_quiescence::xt_cycle_pinned_jit_depth()
        } else {
            0
        };
        let covered = proven.saturating_add(pinned);
        let accounted = xt_jit_coverage_handshake_enabled()
            && (peer_coverage_accounted(peer_depth, covered) || xt_jit_coverage_assume());
        cratonvm_gc::gc_quiescence::note_peer_coverage_verdict(accounted);
        if xt_coverage_dbg() {
            eprintln!(
                "[xt-coverage] peer_depth={peer_depth} proven={proven} pinned={pinned} \
pins_honoured={pins_honoured} accounted={accounted}"
            );
        }
        if !accounted {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::CROSS_THREAD_JIT_PEER,
            );
            complete = false;
        }
    }
    complete
}

/// May this cycle discharge a peer's coverage obligation by PINNING?
///
/// Split out for the same reason [`peer_coverage_accounted`] is: every input is
/// a process-global read at the call site, so the RULE is untestable there and
/// this is the part that has to be right.
///
/// `pins_honoured` is the condition that was missing until 2026-09-06, and it
/// is the one that makes the other four mean anything — see the call site for
/// the reproduction. `credit_when_unpinnable` is the deliberate override that
/// re-opens the defect, so the fix has a positive control.
#[inline]
pub(in crate::jit::conservative_roots) const fn pinned_credit_admissible(
    enabled: bool,
    publish_only: bool,
    pins_honoured: bool,
    credit_when_unpinnable: bool,
    all_windows_pinned: bool,
) -> bool {
    enabled && !publish_only && (pins_honoured || credit_when_unpinnable) && all_windows_pinned
}

/// The handshake's acceptance test, split out so it is testable without racing
/// the process-global counters.
///
/// `>=` and not `==`: the ledger is a sum of per-thread deposits taken at each
/// peer's park, and a peer that returned from a JIT frame between its deposit
/// and this read lowers `peer_depth` without lowering `proven`. That direction
/// is safe — the frames it proved are a superset of the frames still live. The
/// unsafe direction is `proven` running ahead of what was actually proven this
/// pause, which is why the ledger is cleared by `request_stw` and by
/// `begin_moving_young_coverage_cycle` rather than after use.
#[inline]
pub(in crate::jit::conservative_roots) const fn peer_coverage_accounted(peer_depth: usize, proven_depth: usize) -> bool {
    proven_depth >= peer_depth
}

/// JIT entries held by threads OTHER than this one, right now.
///
/// `saturating_sub` and not a plain subtraction: `GLOBAL_JIT_DEPTH` is a
/// striped counter and this thread's own chain is a thread-local, so a torn
/// read across the stripes can momentarily make the global look smaller than
/// the local. Zero is the safe reading of that — it means "no peer depth to
/// account for", and the caller's other obligations still stand.
#[inline]
pub(in crate::jit::conservative_roots) fn peer_jit_depth() -> usize {
    GLOBAL_JIT_DEPTH
        .get()
        .saturating_sub(current_thread_jit_depth())
}

/// Does any thread other than this one hold a JIT entry right now -- is any
/// compiled frame of any VM in the process on another thread's stack?
/// Interpreter round i1 wave 24, lane L6: a class redefinition skips its
/// loop-exit handshake when none does (`request_withdrawn_body_exits`).
///
/// Every transfer into compiled code pushes an entry ([`push_entry_full`])
/// before it runs a compiled instruction, and each stripe is exact when it
/// is read, so an entry pushed before this call is always counted; an entry
/// pushed after it can only enter a body through a door the caller has
/// already closed (a patched entry, an expired memo, the OSR door's own
/// withdrawal check).
pub(crate) fn peer_threads_hold_jit_entries() -> bool {
    peer_jit_depth() != 0
}

/// The striped process-wide JIT depth, unreduced.
///
/// `peer_jit_depth` folds this into a difference and clamps at zero, which
/// destroys the evidence for whether the zero was a quiet process or an
/// inconsistent read. The classifier needs the raw value.
#[inline]
pub(in crate::jit::conservative_roots) fn global_jit_depth_raw() -> usize {
    GLOBAL_JIT_DEPTH.get()
}

/// May the cross-thread coverage handshake discharge `CROSS_THREAD_JIT_PEER`?
///
/// Default ON, so a KILL SWITCH: `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` (or
/// `off`/`false`/`no`) restores the blanket "any peer in JIT means unproven"
/// refusal. Default-off would leave the many-threaded case exactly where the
/// H2 page found it, which is the defect and not a conservative posture.
///
/// Read per cycle rather than latched: it is consulted once per collection, and
/// a `OnceLock` would let the first test to touch it decide the answer for
/// every later test in the binary.
pub(in crate::jit::conservative_roots) fn xt_jit_coverage_handshake_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no")
        }
        None => true,
    }
}

pub(in crate::jit::conservative_roots) fn xt_coverage_dbg() -> bool {
    false
}

/// `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` -- accept the peer accounting whatever
/// the ledger says.
///
/// **A MEASUREMENT INSTRUMENT, and unsafe to run with.** It is how the question
/// "is the cross-thread handshake the only thing between this workload and
/// compaction?" is asked, in the same spirit as
/// `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY`. A peer that deposited nothing has
/// NOT proved its frames rewritable, and relocating under it strands its oops.
///
/// It exists because the shortfall is now specific enough to be worth pricing.
/// With the 2026-08-30 safepoint repairs every per-frame proof in a 300 s run
/// of `org.h2.test.jdbc.TestCachedQueryResults` succeeds
/// (`frame_cov=(no_slot=0 misaligned=0 no_map=0 incomplete=0 ok=601)`), and the
/// peers that still refuse the cycle are the ones that never reach
/// `publish_peer_jit_coverage_for_stw` at all -- threads blocked in a native
/// with compiled frames below them, which park through the blocked-region
/// protocol rather than the STW barrier. Giving them a deposit meant teaching
/// the blocked-region WAKE to remap JIT frames, which this instrument was
/// built to price. (That wake remap has since landed --
/// `apply_blocked_wake_jit_remap`, default-on since 2026-09-08,
/// `CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP` -- and the helper-window pin credit is
/// what now discharges those peers; this doc said "it currently remaps only
/// interpreter frames" until gc-common w1-c.)
pub(in crate::jit::conservative_roots) fn xt_jit_coverage_assume() -> bool {
    false
}

/// `CRATONVM_XT_PINNED_PEER_UNPINNABLE=1` -- credit the pinned-peer depth even
/// on a collector that cannot honour the pin.
///
/// Default OFF, which is the corrected behaviour. It exists because the defect
/// this closes is invisible without it: with it set, the generational
/// collector's ten-second H2 reproduction comes back, so the fix has a positive
/// control rather than only an absence of crashes.
pub(in crate::jit::conservative_roots) fn xt_pinned_peer_credit_when_unpinnable() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

/// gcj j1/yg item 1: may a peer whose coverage proof just failed deposit its
/// young pin words for the pinned copy's unproven-frame arm? Only under the
/// arm's switch, and only while every reason recorded this cycle is one the
/// arm accepts (`gen_heap::PINNED_COPY_UNPROVEN_REASONS`).
pub(in crate::jit::conservative_roots) fn peer_deposits_unproven_frames() -> bool {
    let mask = cratonvm_gc::gc_quiescence::moving_young_incomplete_reason_mask();
    mask != 0
        && mask & !cratonvm_gc::gen_heap::PINNED_COPY_UNPROVEN_REASONS == 0
        && cratonvm_gc::gen_heap::pinned_copy_takes_unproven_frames_enabled()
}

/// A peer thread's half of the cross-thread JIT coverage handshake.
///
/// Called by a mutator that is about to park at the STW barrier, immediately
/// before `arrive_and_wait`. It runs THIS thread's own per-thread coverage
/// proof — the only place in the process where that proof can be run for this
/// thread, because `JIT_ENTRY_CHAIN`, the cached top RBP and the shadow window
/// are all thread-local — and, if it holds, deposits this thread's JIT depth
/// into the pause's ledger.
///
/// Why a deposit here is a promise the thread can keep: a cooperatively-parked
/// peer resumes through `apply_pointer_map_to_thread`, which runs
/// `remap_active_jit_frames`, `remap_register_image_words` and
/// `shadow_stack.remap` over exactly the chain this proof just walked. The
/// proof says every live oop of those frames is reachable from a channel the
/// remap rewrites; the resume applies the rewrite. Nothing else in the process
/// has to touch the peer's registers.
///
/// Cheap when it cannot matter: a thread with no JIT entries contributes
/// nothing to the initiator's `peer_depth` arithmetic, so it returns before
/// paying for `refresh_moving_young_coverage_for_current_thread` (whose
/// `native_stack_has_jit_frame` band walk is the expensive part). That is the
/// overwhelmingly common shape at a safepoint park.
pub fn publish_peer_jit_coverage_for_stw() {
    if !moving_young_enabled() || !xt_jit_coverage_handshake_enabled() {
        return;
    }
    // gcd d3/o: the shadow-bail refusal is now decided per thread, so a peer
    // must ask it even with no chain entry. Before, the initiator's own proof
    // refused every cycle after any bail, which covered a bailed compiled
    // frame on a peer the chain does not register (the A5 shape); now only
    // this peer can see its own shadow stack. One atomic load unless some
    // push in the process has bailed.
    if current_thread_jit_depth() == 0 {
        if shadow_bail_refuses_moving() {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::SHADOW_OVERFLOW_BAIL,
            );
        }
        // Pruning inside the proof can only SHRINK the chain, so an
        // already-empty chain stays empty and this early-out cannot skip a
        // nonzero deposit.
        return;
    }
    // WHICH obligation the peer's own proof fails on, when it fails.
    //
    // `proven=false` is the shortfall that refuses the whole cycle, and a
    // count of them cannot be acted on: the proof has SIX ways to say no
    // (`JIT_RELOCATION_UNSUPPORTED`, `NO_PRECISE_MAP`, `MISSING_EXACT_RBP`,
    // `FOREIGN_INNERMOST_RBP`, `ACTIVE_FRAME_MAP`, `PARENT_FRAME_MAP`) and they
    // want completely different repairs. The per-reason counters are already
    // maintained process-wide, so a before/after snapshot around this one call
    // names the term without any new bookkeeping. Only taken when the debug
    // flag is on — it is two array reads either side of a proof that already
    // walks the stack.
    let before =
        xt_coverage_dbg().then(cratonvm_gc::gc_quiescence::moving_young_incomplete_reason_mask);
    let proven = refresh_moving_young_coverage_for_current_thread();
    // Read the depth AFTER the proof: it prunes returned entries, and the
    // deposit must not claim more than the proof covered.
    let depth = current_thread_jit_depth();
    if proven && depth > 0 {
        cratonvm_gc::gc_quiescence::add_peer_proven_jit_depth(depth);
        // gen r4w5/pinwords5: this peer's half of the young pin-word ledger,
        // at the same park and for the same pause stamp. An unproven peer
        // deposits nothing: the cycle is already diverted one term earlier, and
        // its missing depth keeps the ledger incomplete as well.
        deposit_pause_young_pin_words(false);
    } else if depth > 0 && peer_deposits_unproven_frames() {
        // gcj j1/yg item 1 (`CRATONVM_GEN_PINNED_COPY_TAKES_UNPROVEN_FRAMES`):
        // a peer whose proof failed, while every reason recorded this cycle
        // is one the pinned copy's unproven-frame arm accepts (an
        // unregistered or unpublished compiled frame: words its deposit's
        // sweep notes), deposits its words and its depth into the LEDGER
        // only -- never into the proven depth, so the coverage accounting
        // still refuses the Cheney copy. A later reason the arm does not
        // accept makes the arm decline whatever this deposit says.
        deposit_pause_young_pin_words(false);
    }
    if let Some(before) = before {
        let added = cratonvm_gc::gc_quiescence::moving_young_incomplete_reason_mask() & !before;
        let mut why = String::new();
        for i in 0..cratonvm_gc::gc_quiescence::incomplete_reason::COUNT {
            if added & (1usize << i) != 0 {
                if !why.is_empty() {
                    why.push(',');
                }
                why.push_str(cratonvm_gc::gc_quiescence::incomplete_reason::label(i));
            }
        }
        if why.is_empty() {
            // Distinguishable from a reason literally labelled "none": this
            // says the proof added no obligation to the mask at all, which for
            // a `proven=false` deposit means the reason was ALREADY recorded
            // this cycle (by this thread's earlier proof, or by a peer).
            why.push_str("<no-new-reason>");
        }
        eprintln!("[xt-coverage] peer deposit proven={proven} depth={depth} why={why}");
    }
}

// ===========================================================================
// gen r4w5/pinwords5 (2026-09-24) — THIS THREAD'S DEPOSIT INTO THE PER-PAUSE
// YOUNG PIN-WORD LEDGER (`gc_quiescence::pause_young_pin_words`)
// ===========================================================================
//
// Two callers, both on the depositing thread's own stack, neither in a signal
// or fault context:
//
// * a cooperatively parking peer, in `publish_peer_jit_coverage_for_stw`,
//   right after its coverage proof deposited its depth (the thread then calls
//   `arrive_and_wait` and does not run Java code until the pause ends);
// * the initiator, in `refresh_moving_young_coverage_for_collection`, which
//   `memory::roots::collect_roots` runs after every peer has parked.
//
// Every other place a conservative JIT scan runs is deliberately NOT a deposit
// site, and the ledger's completeness rule covers each of them:
//
// * a BLOCKED peer (`vm_exec::deposit_root_snapshot`, the blocking half of
//   `update_root_snapshot`) never reaches the park, deposits nothing here, and
//   — if it is inside compiled code — leaves a depth shortfall, so the ledger
//   reads incomplete. (The coverage accounting refuses the same cycle for the
//   same reason: its `proven` depth has the same hole.)
// * a peer the take-over FROZE or read through a helper window
//   (`xt_root_scan`): the reader fails closed on any non-`NONE` take-over
//   verdict.
// * a virtual-thread carrier is an OS thread like any other: its JIT chain is
//   its own and it deposits at its own park.
// * the root-snapshot scan a parking peer runs just before this deposit
//   (`update_root_snapshot`) reads the same stack; this deposit re-reads it
//   with a young-range screen instead of an object-base screen.
//
// WHAT A DEPOSIT HOLDS: every word in a published young region (see
// `gc_quiescence::YoungPinRange`: low slack, inclusive end) that is
//
// 1. in this thread's callee-saved GPRs at the deposit (a compiled frame's
//    callee-saved register can still be live in the register file), or
// 2. in the JIT band `[deposit SP, outermost entry SP)` OUTSIDE a compiled
//    frame whose layout this walk resolved: the Rust helper frames compiled
//    code called into (whose prologues saved the compiled frame's
//    callee-saved registers), frames with no usable layout, conservative
//    (non-precise) entries, or
// 3. in a resolved compiled frame's band, unless `young_pin_slot_excuses`
//    excuses it: an object base in a slot the active oop map names (the slot
//    `remap_one_jit_frame` rewrites), and -- ONLY while the ledger merely
//    feeds a census -- an object base in a dataflow-modelled slot the map
//    proves dead, or an operand-spill slot above the safepoint's live cursor.
//    Register images, the blind spill, the outgoing-argument reserve,
//    unmodelled slots the map does not name, and every NON-base (interior /
//    derived) word are counted.
//
//    gen r5w4/pin8: when the ledger LICENSES MOVES (the pinned in-place copy,
//    option B) a deadness claim excuses nothing. A word the map calls dead is
//    still READ as a marking root by the band scan (`scan_one_frame_filtered`
//    publishes every modelled word the map does not name), and no channel
//    rewrites it; so if the claim is wrong, a relocating cycle moves the
//    object -- which stays reachable through the word AND through any other
//    reference -- and the frame resumes on the vacated address, which the
//    pinned copy re-serves in the same pause. The non-moving sweep never
//    moved such an object, so a wrong claim there cost at most a lost ONLY
//    reference. Pinning is the one answer that is right whether the claim is.
//
// Words, not bases, and nothing is resolved: an interior word is deposited as
// itself. Anything the walk cannot read or classify is counted (the
// layout-free sweep) or marks the deposit incomplete — never skipped.

/// Most compiled-frame bands one deposit classifies. A frame beyond it is not
/// classified, so every young word in it is counted by the layout-free sweep:
/// an over-count, never a miss.
pub(in crate::jit::conservative_roots) const YOUNG_PIN_MAX_FRAMES: usize = 256;

/// Callee-saved GPRs a deposit captures (x86-64 uses 8, AArch64 11).
pub(in crate::jit::conservative_roots) const YOUNG_PIN_SAVED_GPRS: usize = 11;

/// The compiled-frame bands a deposit has already classified, so the
/// layout-free sweep skips them. Fixed capacity: no allocation.
pub(in crate::jit::conservative_roots) struct YoungPinBands {
    pub(in crate::jit::conservative_roots) bands: [(usize, usize); YOUNG_PIN_MAX_FRAMES],
    pub(in crate::jit::conservative_roots) len: usize,
}

impl YoungPinBands {
    pub(in crate::jit::conservative_roots) fn new() -> Self {
        Self {
            bands: [(0, 0); YOUNG_PIN_MAX_FRAMES],
            len: 0,
        }
    }

    pub(in crate::jit::conservative_roots) fn has_room(&self) -> bool {
        self.len < YOUNG_PIN_MAX_FRAMES
    }

    pub(in crate::jit::conservative_roots) fn push(&mut self, lo: usize, hi: usize) {
        if let Some(b) = self.bands.get_mut(self.len) {
            *b = (lo, hi);
            self.len += 1;
        }
    }

    pub(in crate::jit::conservative_roots) fn sorted(&mut self) -> &[(usize, usize)] {
        let active = &mut self.bands[..self.len];
        active.sort_unstable();
        active
    }

    /// The lowest classified band's low end, if any band was classified.
    pub(in crate::jit::conservative_roots) fn lowest(&self) -> Option<usize> {
        self.bands[..self.len].iter().map(|&(lo, _)| lo).min()
    }
}

/// Does the ledger EXCUSE a young word read from `[rbp - off]` of a compiled
/// frame whose layout resolved? `word_is_object_base` is asked only when the
/// answer depends on it (it reads a header).
///
/// Excused, and nothing else:
/// * an operand-spill slot above the safepoint's live cursor — nothing reads
///   it again (`spill_slot_is_dead_above_cursor`, the marking scan's own
///   deadness argument, off under `CRATONVM_GC_DEAD_SPILL_ROOTS=0`);
/// * an OBJECT BASE in a slot the active safepoint's oop map names —
///   `remap_one_jit_frame` rewrites it after a move;
/// * an OBJECT BASE in a dataflow-modelled slot (Java local, operand spill)
///   the resolved map does NOT name — dead at this pc, the claim
///   `band_slot_is_verifiable_with_map` already spends.
///
/// Never excused: the callee-saved / register-image region and everything
/// beyond it (the blind spill, the outgoing-argument reserve), an unmodelled
/// slot the map does not name (LICM hoist, scalar-replacement fields, the
/// reserved-locals tail: nothing rewrites the FRAME word there), any word
/// when no map resolved, and any word that is not an object base (an interior
/// or derived pointer is rewritten by nobody).
///
/// This is STRICTER than `band_slot_is_verifiable_with_map`'s split, on
/// purpose: "verifiable" means "the coverage proof inspects it for shadow
/// publication", not "a channel rewrites the frame word", and the ledger
/// needs the second.
///
/// `trust_deadness` (gen r5w4/pin8): whether the two DEADNESS excuses (the
/// spill above the live cursor, the modelled slot the map does not name) may
/// be spent. `false` whenever the ledger licenses a relocating cycle
/// ([`young_pin_ledger_licenses_moves`]): then only a word a channel
/// REWRITES is excused. See the block comment above for why.
pub(in crate::jit::conservative_roots) fn young_pin_slot_excuses(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    map_slots: Option<&[i16]>,
    trust_deadness: bool,
    word_is_object_base: impl FnOnce() -> bool,
) -> bool {
    if trust_deadness && spill_slot_is_dead_above_cursor(layout, off, live_hi) {
        return true;
    }
    let register_image = (!layout.callee_saved_shallow
        && layout.callee_saved_lo > 0
        && off >= layout.callee_saved_lo)
        || layout.is_register_image(off);
    if register_image {
        return false;
    }
    let Some(slots) = map_slots else {
        return false;
    };
    let named = slots.iter().any(|&s| i32::from(s) == off);
    let dead_by_map = trust_deadness
        && !named
        && band_map_liveness_enabled()
        && region_is_dataflow_modelled(layout, off);
    (named || dead_by_map) && word_is_object_base()
}

/// Deposit every unexcused young word of one compiled frame's band
/// `[rbp - frame_size, rbp)`. Predicate-injected so the classification is
/// unit-testable on a heap buffer standing in for a frame. `trust_deadness`
/// as in [`young_pin_slot_excuses`].
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn young_pin_frame_words(
    rbp: usize,
    frame_size: usize,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    map_slots: Option<&[i16]>,
    trust_deadness: bool,
    is_young: impl Fn(usize) -> bool,
    is_object_base: impl Fn(usize) -> bool,
    mut note: impl FnMut(usize),
) {
    if frame_size == 0 || frame_size > rbp {
        return;
    }
    let mut addr = (rbp - frame_size + 7) & !7usize;
    while addr + 8 <= rbp {
        // Cast: a compiled frame is at most 1 MiB (checked by the caller).
        let off = (rbp - addr) as i32;
        // SAFETY: aligned read inside a live compiled frame's own band on this
        // thread's stack (or, in the unit tests, inside a live heap buffer),
        // bounded by the frame size the caller validated -- the same words
        // `scan_one_compiled_frame_with_layout` reads.
        let w = unsafe { (addr as *const usize).read() };
        addr += 8;
        if is_young(w)
            && !young_pin_slot_excuses(off, layout, live_hi, map_slots, trust_deadness, || {
                is_object_base(w)
            })
        {
            note(w);
        }
    }
}

/// Classify the compiled frames of one precise chain entry, walking the RBP
/// chain exactly as `scan_compiled_frame_bands` does, and record each
/// classified band. Stops (leaving the rest to the layout-free sweep) at the
/// first frame it cannot resolve. `trust_deadness` as in
/// [`young_pin_slot_excuses`].
pub(in crate::jit::conservative_roots) fn young_pin_classify_compiled_frames(
    info: PreciseFrameInfo,
    scanner_sp: usize,
    range: &cratonvm_gc::gc_quiescence::YoungPinRange,
    dep: &mut cratonvm_gc::gc_quiescence::YoungPinDeposit,
    bands: &mut YoungPinBands,
    trust_deadness: bool,
) {
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    let entry_sp = info.frame_base;
    let mut rbp = info.exact_rbp;
    if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
        return;
    }
    // A foreign innermost frame (an unguarded JIT->JIT callee) has no method
    // here: leave it, and everything above it, to the layout-free sweep.
    let Some(mut cm_ptr) = innermost_frame_method(
        rbp,
        info.exact_cm_id,
        entry_sp,
        scanner_sp,
        info.compiled_method,
    ) else {
        return;
    };
    // gch h1/jw (r17): the entry band's guard. A cut (or a full `bands`)
    // leaves every frame above it to the layout-free sweep, which deposits
    // each young word it finds -- the safe direction.
    let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
    while bands.has_room() && walk.step() {
        // SAFETY: the chain entry's method is Arc-owned by the JIT cache while
        // any of its frames is live, and a method resolved from a live frame's
        // return address is kept alive by that frame -- the contract
        // `scan_compiled_frame_bands` relies on for the same pointers.
        let cm: &cratonvm_jit::CompiledMethod = unsafe { &*cm_ptr };
        let Ok(frame_size) = usize::try_from(cm.osr_frame_size) else {
            return;
        };
        if frame_size == 0
            || frame_size > MAX_COMPILED_FRAME_BYTES
            || frame_size > rbp
            || rbp - frame_size < scanner_sp
        {
            return;
        }
        let live_hi = moving_young_frame_live_hi(rbp, cm);
        let map_slots = active_map_slots(rbp, cm);
        young_pin_frame_words(
            rbp,
            frame_size,
            &cm.frame_layout,
            live_hi,
            map_slots.as_deref(),
            trust_deadness,
            |w| range.contains(w),
            // `range` admits the low slack and one-past-end, where the young
            // commit screen answers `true` without covering the page; a word
            // there is not screened (and so is kept, the safe direction).
            |w| range.contains_span(w, cratonvm_types::HEADER_SIZE) && band_word_is_an_object(w),
            |w| dep.note(w),
        );
        bands.push(rbp - frame_size, rbp);
        // SAFETY: `rbp` is an aligned frame base inside this thread's live JIT
        // band (`[scanner_sp, entry_sp)`, checked above and at every step);
        // the prologue stores the caller RBP at `[rbp]` and the return
        // address at `[rbp + 8]`.
        let parent_rbp = unsafe { (rbp as *const usize).read() };
        // SAFETY: as above.
        let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
        let Some(parent) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
            return;
        };
        if parent_rbp <= rbp
            || parent_rbp & 0x7 != 0
            || parent_rbp >= entry_sp
            || parent_rbp < scanner_sp
        {
            return;
        }
        cm_ptr = parent as *const cratonvm_jit::CompiledMethod;
        rbp = parent_rbp;
    }
}

/// Deposit every young word of `[lo, hi)` outside the classified compiled
/// bands. A band the thread's stack limits do not let it read in full marks
/// the deposit incomplete.
pub(in crate::jit::conservative_roots) fn young_pin_sweep_layout_free(
    lo: usize,
    hi: usize,
    range: &cratonvm_gc::gc_quiescence::YoungPinRange,
    dep: &mut cratonvm_gc::gc_quiescence::YoungPinDeposit,
    bands: &mut YoungPinBands,
) {
    // gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 7): an
    // incomplete deposit records no word (`YoungPinDeposit::push` drops every
    // one), so its sweep is pure cost.
    if hi <= lo || dep.is_incomplete() {
        return;
    }
    let aligned_lo = (lo + 7) & !7usize;
    let top = own_stack_scan_hi(aligned_lo, hi, current_stack_pointer(), own_stack_top());
    if top < hi {
        dep.mark_incomplete();
        return;
    }
    let bands = bands.sorted();
    let mut next = 0usize;
    let mut addr = aligned_lo;
    while addr + 8 <= top {
        while bands.get(next).is_some_and(|&(_, bhi)| bhi <= addr) {
            next += 1;
        }
        if let Some(&(blo, bhi)) = bands.get(next) {
            if addr >= blo {
                addr = (bhi + 7) & !7usize;
                continue;
            }
        }
        // SAFETY: aligned read inside this thread's own live stack, between
        // the deposit's stack pointer and the outermost JIT entry's SP, clamped
        // by `own_stack_scan_hi` exactly as `scan_one_frame` clamps the same
        // band.
        let w = unsafe { (addr as *const usize).read() };
        addr += 8;
        if range.contains(w) {
            dep.note_native(w);
        }
    }
}

/// Is the pause's young pin ledger what licenses a RELOCATING young cycle
/// under live compiled frames -- the pinned in-place copy
/// (`CRATONVM_GEN_PINNED_YOUNG_COPY`) or term 4's option B
/// (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`)? With both off the ledger only
/// feeds a census, and the default sweep never moves under a compiled frame.
/// JIT r20 wave 18, lane gcI: and precise-root promotion, whose sweep moves a
/// precise-only root value on the ledger's word
/// (`gc_quiescence::young_pin_ledger_licenses_moves`, the one definition).
pub(in crate::jit::conservative_roots) fn young_pin_ledger_licenses_moves() -> bool {
    cratonvm_gc::gc_quiescence::young_pin_ledger_licenses_moves()
}

/// `CRATONVM_DBG_YOUNG_PIN_SWEEP_ENTRY_ONLY` (gci i35/wy, diagnostic, UNSOUND):
/// see its use in `young_pin_scan_own_chain_from`. Read per deposit (once per
/// thread per pause; no process static -- the statics ratchet).
///
/// gcj-j1pn-young-pin-ledger-review-findings item 8: it does NOT mark the
/// deposit incomplete. A deposit cut at the outermost entry still reads as
/// complete, so with the ledger licensing moves it can clear option B (term 4
/// stands down) and feed the pinned copy a word set that misses every word
/// above the entry: a relocating cycle may then move an object one of those
/// words names. That is the point of the lever (it names the producer of a pin
/// word by making it disappear) and the reason it is a diagnostic only: never
/// set it on a run whose correctness is being judged.
pub(in crate::jit::conservative_roots) fn dbg_young_pin_sweep_entry_only() -> bool {
    false
}

/// The upper bound of a deposit's layout-free sweep (gen r5w2/roots6).
///
/// The sweep always covered `[deposit SP, outermost compiled entry SP)`: the
/// compiled frames and every Rust frame BETWEEN them (the helpers, the
/// collector's own entry). Above the outermost entry sit the Rust frames that
/// CALLED into compiled code -- the interpreter's invoke path, a native's
/// upcall -- and a raw object address such a frame holds across the call is
/// named by no oop map, no shadow slot and no typed root. Under the default
/// policy that never mattered: a young cycle with a compiled frame live is a
/// non-moving sweep (term 4), so no object moves under such a word. The
/// ledger is what now licenses MOVING under a live compiled frame, and an
/// unswept word above the entry is exactly a word "no precise channel
/// rewrites" that the ledger then fails to name -- the object moves, its old
/// bytes are zeroed and re-served by the in-place copy, and the caller
/// resumes on them. So when the ledger licenses moves, the sweep runs to the
/// thread's stack top (`stack_top`, clamped below the static TLS block; `0`
/// when unknown, and then the old bound stands). Pure: testable.
#[inline]
pub(in crate::jit::conservative_roots) fn young_pin_sweep_hi(band_hi: usize, stack_top: usize, licenses_moves: bool) -> usize {
    if licenses_moves && band_hi != 0 && stack_top > band_hi {
        stack_top
    } else {
        band_hi
    }
}

/// gcj j1/pn: does a deposit's sweep stop short of what a licensing ledger
/// must name? Only when the ledger licenses moves and the chain is non-empty:
/// no band at all (`band_hi == 0`: every entry without an SP and without a
/// precise frame, so nothing above `lo` is swept), or a stack top that is
/// unknown (`stack_top == 0`, from `own_stack_top`: a foreign SP or an
/// unsupported target), where [`young_pin_sweep_hi`] keeps the entry bound
/// and the Rust frames above the outermost entry go unswept. A top at or
/// below the band is not short (the band already reaches it). Pure: testable.
#[inline]
pub(in crate::jit::conservative_roots) fn young_pin_sweep_falls_short(
    chain_non_empty: bool,
    band_hi: usize,
    stack_top: usize,
    licenses_moves: bool,
) -> bool {
    licenses_moves && chain_non_empty && (band_hi == 0 || stack_top == 0)
}



/// Walk this thread's JIT chain: classify each precise entry's compiled
/// frames, then sweep the rest of `[lo, outermost entry SP)` -- or of `[lo,
/// stack top)` when the ledger licenses moves ([`young_pin_sweep_hi`]).
/// Returns the chain length walked, or `None` if the chain could not be
/// borrowed.
///
/// `entry_sp` (gcj j1pn ledger item 3): the pause door's
/// [`cratonvm_gc::gc_quiescence::CollectorEntry`] SP when this deposit runs
/// inside one; the layout-free sweep then starts there
/// ([`young_pin_sweep_lo`]), not at `lo`.
pub(in crate::jit::conservative_roots) fn young_pin_scan_own_chain_from(
    lo: usize,
    entry_sp: Option<usize>,
    range: &cratonvm_gc::gc_quiescence::YoungPinRange,
    dep: &mut cratonvm_gc::gc_quiescence::YoungPinDeposit,
) -> Option<usize> {
    JIT_ENTRY_CHAIN
        .try_with(|c| {
            {
                let mut chain = c.try_borrow_mut().ok()?;
                flush_top_rbp_cache_to_chain(chain.as_mut_slice());
            }
            let chain = c.try_borrow().ok()?;
            let mut bands = YoungPinBands::new();
            let mut band_hi = 0usize;
            // gen r5w4/pin8: a ledger that licenses moves spends no deadness
            // claim (`young_pin_slot_excuses`); the census-only ledger keeps
            // its historical excuses, so the default path is unchanged.
            let licenses_moves = young_pin_ledger_licenses_moves();
            for entry in chain.iter() {
                band_hi = band_hi.max(entry.entry_sp);
                if let Some(info) = entry.precise {
                    band_hi = band_hi.max(info.frame_base);
                    young_pin_classify_compiled_frames(
                        info,
                        lo,
                        range,
                        dep,
                        &mut bands,
                        !licenses_moves,
                    );
                }
            }
            // gen r5w2/roots6: past the outermost compiled entry too, up to
            // the thread's stack top, whenever the ledger is what licenses a
            // relocating cycle (the pinned in-place copy or option B). See
            // `young_pin_sweep_hi`.
            // gci i35/wy: `CRATONVM_DBG_YOUNG_PIN_SWEEP_ENTRY_ONLY=1` (UNSOUND,
            // diagnostic only: an unswept word above the entry is not pinned)
            // stops the sweep at the outermost compiled entry, as when the
            // ledger only fed a census. It names the producer of a pin word no
            // root holds: if the word disappears, it was a stale word in the
            // Rust frames ABOVE the entry (the suspended interpreter).
            let sweep_hi = if dbg_young_pin_sweep_entry_only() {
                band_hi
            } else {
                let stack_top = own_stack_top();
                // gcj j1/pn: a licensing ledger whose sweep cannot reach the
                // stack top (top unknown: a foreign SP, an unsupported target)
                // or has no band at all for a non-empty chain did NOT name
                // every word above the entry; say so, so the pause declines
                // (`nonmoving-pin-ledger-incomplete`) instead of moving under
                // an unswept word.
                if young_pin_sweep_falls_short(
                    !chain.is_empty(),
                    band_hi,
                    stack_top,
                    licenses_moves,
                )
                {
                    dep.mark_incomplete();
                }
                young_pin_sweep_hi(band_hi, stack_top, licenses_moves)
            };
            let sweep_lo = young_pin_sweep_lo(lo, entry_sp, bands.lowest());
            // A compiled band below the door's capture: the entry is not what
            // it claims, and the registers this deposit noted were the door's,
            // not this frame's. Fail closed.
            match entry_sp {
                Some(sp) if sweep_lo != sp => dep.mark_incomplete(),
                Some(sp) => cratonvm_gc::gc_quiescence::note_young_pin_collector_cut(sp - lo),
                None => {}
            }
            young_pin_sweep_layout_free(sweep_lo, sweep_hi, range, dep, &mut bands);
            Some(chain.len())
        })
        .ok()
        .flatten()
}

/// Store the calling frame's callee-saved GPRs into `regs` and return the
/// stack pointer at that instant: the deposit's layout-free sweep starts
/// THERE, so it covers the depositing function's own saved registers (its
/// prologue pushed the caller's values) but none of the helper frames below
/// it, which hold the ledger's own young-range bounds.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(in crate::jit::conservative_roots) fn young_pin_capture_registers(regs: &mut [usize; YOUNG_PIN_SAVED_GPRS]) -> Option<usize> {
    let sp: usize;
    // SAFETY: eight 8-byte stores into `regs[0..8]` (in bounds of the 11-word
    // array `p` points at) and one register read; no other memory is touched,
    // the stack is not used and the flags are preserved. A register the
    // compiler picked for `p` that is itself callee-saved has its caller value
    // saved by this function's prologue, i.e. inside the band the sweep reads.
    unsafe {
        core::arch::asm!(
            "mov qword ptr [{p}], rbx",
            "mov qword ptr [{p} + 8], rbp",
            "mov qword ptr [{p} + 16], rsi",
            "mov qword ptr [{p} + 24], rdi",
            "mov qword ptr [{p} + 32], r12",
            "mov qword ptr [{p} + 40], r13",
            "mov qword ptr [{p} + 48], r14",
            "mov qword ptr [{p} + 56], r15",
            "mov {sp}, rsp",
            p = in(reg) regs.as_mut_ptr(),
            sp = lateout(reg) sp,
            options(nostack, preserves_flags),
        );
    }
    Some(sp)
}

/// AArch64 counterpart of the x86-64 capture: x19..x28 and the frame pointer.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(in crate::jit::conservative_roots) fn young_pin_capture_registers(regs: &mut [usize; YOUNG_PIN_SAVED_GPRS]) -> Option<usize> {
    let sp: usize;
    // SAFETY: five pair stores and one single store into `regs[0..11]` (the
    // whole 88-byte array `p` points at) and one register read; no other
    // memory, no stack use, flags preserved. See the x86-64 variant for a
    // callee-saved `p`.
    unsafe {
        core::arch::asm!(
            "stp x19, x20, [{p}]",
            "stp x21, x22, [{p}, #16]",
            "stp x23, x24, [{p}, #32]",
            "stp x25, x26, [{p}, #48]",
            "stp x27, x28, [{p}, #64]",
            "str x29, [{p}, #80]",
            "mov {sp}, sp",
            p = in(reg) regs.as_mut_ptr(),
            sp = lateout(reg) sp,
            options(nostack, preserves_flags),
        );
    }
    Some(sp)
}

/// No capture on other targets: the deposit is then marked incomplete.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
pub(in crate::jit::conservative_roots) fn young_pin_capture_registers(_regs: &mut [usize; YOUNG_PIN_SAVED_GPRS]) -> Option<usize> {
    None
}

const _: () = assert!(YOUNG_PIN_SAVED_GPRS == cratonvm_gc::gc_quiescence::COLLECTOR_ENTRY_GPRS);

/// gcj-j1pn-young-pin-ledger-review-findings item 3: publish this thread's
/// [`cratonvm_gc::gc_quiescence::CollectorEntry`] for the life of the
/// returned guard -- the stack pointer and callee-saved registers of the
/// CALLING frame (`#[inline(always)]`: the capture runs in the caller), the
/// pause door `gc_and_alloc::run_collection_pause`. Every frame of the pause
/// is a callee of that frame and lies below the captured SP, so a young pin
/// deposit made inside the pause sweeps from there (`young_pin_deposit_from`).
#[inline(always)]
pub(crate) fn enter_collector_entry() -> cratonvm_gc::gc_quiescence::CollectorEntryScope {
    let mut regs = [0usize; YOUNG_PIN_SAVED_GPRS];
    let sp = young_pin_capture_registers(&mut regs);
    cratonvm_gc::gc_quiescence::CollectorEntryScope::enter(
        sp.map(|sp| cratonvm_gc::gc_quiescence::CollectorEntry { sp, regs }),
    )
}

/// May a deposit whose sweep starts at `lo` take the collector-entry cut at
/// `entry_sp`? Only when the entry lies strictly above the deposit (the
/// deposit runs inside the door) and below the stack top (`stack_top == 0`:
/// unknown, then the entry is still below every frame the sweep reaches).
/// Pure: testable.
#[inline]
pub(in crate::jit::conservative_roots) fn young_pin_collector_cut_applies(lo: usize, entry_sp: usize, stack_top: usize) -> bool {
    entry_sp > lo && (stack_top == 0 || entry_sp < stack_top)
}

/// The low bound of a deposit's layout-free sweep: the collector entry when
/// one applies and no classified compiled band lies below it (compiled code
/// never runs inside the pause door, so a band below it means the entry is
/// not what it claims; the sweep then starts at `lo`, as before). Pure.
#[inline]
pub(in crate::jit::conservative_roots) fn young_pin_sweep_lo(lo: usize, entry_sp: Option<usize>, lowest_band: Option<usize>) -> usize {
    match entry_sp {
        Some(sp) if sp > lo && !matches!(lowest_band, Some(b) if b < sp) => sp,
        _ => lo,
    }
}

/// Deposit this thread's young pin words for the current pause. See the block
/// comment above for what a deposit holds.
///
/// `#[inline(never)]` and minimal on purpose: the registers and the sweep's
/// low bound are captured FIRST, in this frame, before anything here or below
/// can put a young-range bound in a register or a stack slot the sweep would
/// then read back as a pin word.
///
/// `initiator` (gcj j1pn ledger item 3): the collecting thread's own deposit,
/// made inside its root gathering, i.e. inside the pause door with nothing
/// but the pause's frames below the door's capture. Only that deposit takes
/// the collector-entry cut: a losing initiator parks inside the door too, but
/// whatever its park runs (a handshake, an agent callback) is not the pause's
/// own machinery, so its deposit sweeps as every peer's does.
#[inline(never)]
pub(in crate::jit::conservative_roots) fn deposit_pause_young_pin_words(initiator: bool) {
    let mut regs = [0usize; YOUNG_PIN_SAVED_GPRS];
    let lo = young_pin_capture_registers(&mut regs);
    young_pin_deposit_from(lo, &regs, initiator);
}

#[inline(never)]
pub(in crate::jit::conservative_roots) fn young_pin_deposit_from(
    lo: Option<usize>,
    regs: &[usize; YOUNG_PIN_SAVED_GPRS],
    initiator: bool,
) {
    // Generational only: the only consumer is its term 4, and G1 / ZGC publish
    // no young regions (the screen would be vacuous there).
    if !cratonvm_gc::gen_heap::published_young_regions_are_live() {
        return;
    }
    let Some(mut dep) = cratonvm_gc::gc_quiescence::begin_self_young_pin_deposit() else {
        return;
    };
    let range = cratonvm_gc::gc_quiescence::young_pin_range();
    let Some(lo) = lo else {
        // An incomplete deposit reads as incomplete whatever depth it claims.
        dep.mark_incomplete();
        dep.commit(0);
        return;
    };
    // gcj j1pn ledger item 3: inside the pause door, the collector's own
    // frames are not swept and the door's registers stand in for this
    // frame's (`gc_quiescence::CollectorEntry`).
    let entry = cratonvm_gc::gc_quiescence::collector_entry()
        .filter(|_| initiator)
        .filter(|e| young_pin_collector_cut_applies(lo, e.sp, own_stack_top()))
        .filter(|_| cratonvm_gc::gc_quiescence::young_pin_skips_collector_frames_enabled());
    let regs: &[usize] = match entry.as_ref() {
        Some(e) => &e.regs,
        None => regs,
    };
    // gcj-j1pn-pinned-cycle-review-findings item 1: noted FIRST and as
    // registers, so the pinned plan can tell them from stack words.
    for &w in regs {
        if range.contains(w) {
            dep.note_register(w);
        }
    }
    match young_pin_scan_own_chain_from(lo, entry.map(|e| e.sp), &range, &mut dep) {
        Some(depth) => dep.commit(depth),
        None => {
            dep.mark_incomplete();
            dep.commit(0);
        }
    }
}

/// Returns true if any thread anywhere in the process is currently inside a
/// JIT call. Used by the GC to decide whether compaction is safe.
#[inline]
pub fn any_thread_in_jit() -> bool {
    !GLOBAL_JIT_DEPTH.is_zero()
}

/// A lazy native entry must publish a chain another thread can read while
/// the owner is held in C; a mutable borrow cannot establish that promise.
#[inline]
pub(crate) fn current_thread_jit_chain_is_readable() -> bool {
    JIT_ENTRY_CHAIN
        .try_with(|c| c.try_borrow().is_ok())
        .unwrap_or(false)
}

/// Returns the number of active JIT entries on the *current* thread.
/// Mostly useful for tests and assertions.
#[inline]
pub fn current_thread_jit_depth() -> usize {
    JIT_ENTRY_CHAIN.with(|c| c.borrow().len())
}

// ---------------------------------------------------------------------------
// t1 wave 5 lane E2: `ChainSegment` -- stage 2 of
// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`
// (`CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`, opt-in)
// ---------------------------------------------------------------------------
//
// A JDK continuation's slice run on its carrier's OS thread while the carrier
// has compiled frames below `enterSpecial`. The carrier is GC-blocked for the
// whole slice with its deposit standing, exactly as while it waits for a
// runner, so the slice is sound when (i) every per-OS-thread record of "what
// is on this stack" starts from a runner's zeros and comes back to the
// carrier's values before the carrier runs again, and (ii) nothing that scans
// or rewrites "this OS thread's stack" during the slice reaches above the
// slice's base, the stack pointer at `enterSpecial` (the words above it are
// the carrier's, whose own wake rewrites them once). No new static: every
// record moved here already exists; the base itself lives in the gc crate
// (`shadow_stack::current_segment_top` / `publish_stack_segment`).

/// The carrier's per-OS-thread JIT records, set aside for one slice run on
/// its OS thread below its compiled frames, and put back on drop. Built inside
/// `vm::jdk_continuation::enter_special`'s carrier arm, after the carrier's
/// blocked region was entered and before the slice mounts; dropped after the
/// slice returned (a yield returns normally; a panic aborts in that arm), so
/// the drop always runs before the carrier leaves its blocked region.
///
/// Set aside (and why each is per OS thread but about the CARRIER's stack):
/// 1. `JIT_ENTRY_CHAIN`: taken whole, so the slice starts with an empty chain
///    -- `current_thread_jit_depth`, `pinned_reason`, `learn_compiled_pin`,
///    `active_compiled_frames`, the chain scans and `prune_returned_jit_entries`
///    see the slice's frames only. `GLOBAL_JIT_DEPTH` is untouched: the
///    carrier's frames still exist.
/// 2. The innermost-RBP and compile-id mirrors (the OS-TLS slots generated
///    code writes under inline frame-record): saved and zeroed, as a push of a
///    fresh chain zeroes them.
/// 3. `JIT_CHAIN_HISTORY` (residue mark, the frozen-band memo, the deposit /
///    band-at-pause / delivery flags): a fresh history for the slice. The
///    carrier's comes back with the slice's residue folded in (a returned slice
///    frame left words below the base) and its mutation count moved on, and
///    its frozen-band verdict dropped.
/// 4. `UNREG_JIT_MEMO`: reset on enter AND on drop -- its clean verdict is
///    over `[verified_lo, top)`, and the slice wrote below the base.
///    `UNREG_JIT_AUTHORITATIVE` and `EXC_STASH_FLOOR`: saved and restored.
/// 5. The published per-tid JIT depth (the initiator's pinned-peer credit):
///    `0` for the slice, the carrier's chain length back on drop.
/// 6. The own-stack top: the gc crate's segment top is set to the base, so
///    `current_thread_stack_limits` (and through it `own_stack_top`,
///    `sp_on_own_stack`, the A5 probe, `in_native_band_top`) and
///    `SlotBand::current_thread` (the remap store band) stop at it.
/// 7. The code-cache quiescence record's blocked window
///    (`cratonvm_jit::jit_thread_blocked_segment_enter`): held open over the
///    slice, so the slice's foreign blocked-region leave (its remount's) can
///    never close the CARRIER's window and let a later slice-only band scan
///    vouch for an OS thread whose carrier still returns into compiled code.
/// 8. The shadow-stack window published under this OS tid: the slice's first
///    JIT entry publishes the continuation's; the drop republishes the
///    carrier's `(addr, base, end)` (hazard 1 of the page: returning from
///    `enterSpecial` into carrier compiled code is not a JIT entry, so nothing
///    else would).
///
/// The deopt stashes, `RESUMED_COMPILED_FRAME_DEPTH`, `OSR_CONTINUATIONS` and
/// the JIT-dispatch depth live outside this file; `enter_special` sets them
/// aside beside this guard (`CarrierCompiledBase` there).
pub(crate) struct ChainSegment {
    pub(in crate::jit::conservative_roots) chain: Vec<JitFrameChainEntry>,
    pub(in crate::jit::conservative_roots) top_rbp: usize,
    pub(in crate::jit::conservative_roots) top_cm_id: u32,
    pub(in crate::jit::conservative_roots) history: ChainHistory,
    pub(in crate::jit::conservative_roots) unreg_authoritative: bool,
    pub(in crate::jit::conservative_roots) exc_stash_floor: usize,
    pub(in crate::jit::conservative_roots) prev_segment_top: usize,
    pub(in crate::jit::conservative_roots) quiescence_nesting: u32,
    pub(in crate::jit::conservative_roots) carrier_shadow: Option<(usize, usize, usize)>,
}

impl ChainSegment {
    /// Set the carrier's records aside for a slice whose base is `base_sp`
    /// (the stack pointer in `enter_special`, above every slice frame and
    /// below every carrier frame). `carrier_shadow`: the carrier's shadow
    /// stack `(address, base, end)` when shadow publication is on, else `None`.
    pub(crate) fn enter(base_sp: usize, carrier_shadow: Option<(usize, usize, usize)>) -> Self {
        note_jit_boundary();
        let chain = JIT_ENTRY_CHAIN.with(|c| std::mem::take(&mut *c.borrow_mut()));
        let top_rbp = top_rbp_get();
        let top_cm_id = top_cm_id_mirror_read();
        top_rbp_set(0);
        top_cm_id_mirror_write(0);
        let history = JIT_CHAIN_HISTORY.with(|c| {
            let old = c.get();
            c.set(ChainHistory {
                mutations: old.mutations.wrapping_add(1),
                ..ChainHistory::new()
            });
            old
        });
        UNREG_JIT_MEMO.with(|c| c.set(UnregMemo::new()));
        let unreg_authoritative = UNREG_JIT_AUTHORITATIVE.with(|c| c.replace(false));
        let exc_stash_floor = EXC_STASH_FLOOR
            .try_with(|c| c.replace(NO_EXC_STASH_FLOOR))
            .unwrap_or(NO_EXC_STASH_FLOOR);
        publish_self_jit_depth(0);
        let prev_segment_top = cratonvm_gc::shadow_stack::replace_current_segment_top(base_sp);
        let quiescence_nesting = cratonvm_jit::jit_thread_blocked_segment_enter();
        Self {
            chain,
            top_rbp,
            top_cm_id,
            history,
            unreg_authoritative,
            exc_stash_floor,
            prev_segment_top,
            quiescence_nesting,
            carrier_shadow,
        }
    }
}

impl Drop for ChainSegment {
    fn drop(&mut self) {
        note_jit_boundary();
        // The slice's own entries are popped by their guards; a leaked one
        // (a guard whose drop was bypassed) is retired here exactly as
        // `prune_returned_jit_entries` retires it, so the process counts and
        // the quiescence record stay balanced and none of it reaches the
        // carrier's chain.
        let carrier = std::mem::take(&mut self.chain);
        let leftover = JIT_ENTRY_CHAIN.with(|c| std::mem::replace(&mut *c.borrow_mut(), carrier));
        let restored_depth = JIT_ENTRY_CHAIN.with(|c| c.borrow().len());
        for e in leftover {
            note_jit_residue(e.entry_sp);
            settle_reclaimed_entry_counts(e.depth_stripe, e.exec_token, e.tally_skipped);
        }
        top_rbp_set(self.top_rbp);
        top_cm_id_mirror_write(self.top_cm_id);
        JIT_CHAIN_HISTORY.with(|c| {
            let slice = c.get();
            let mut back = self.history;
            back.residue_hi = back.residue_hi.max(slice.residue_hi);
            back.mutations = back
                .mutations
                .wrapping_add(slice.mutations)
                .wrapping_add(1);
            back.frozen = None;
            c.set(back);
        });
        UNREG_JIT_MEMO.with(|c| c.set(UnregMemo::new()));
        UNREG_JIT_AUTHORITATIVE.with(|c| c.set(self.unreg_authoritative));
        let _ = EXC_STASH_FLOOR.try_with(|c| c.set(self.exc_stash_floor));
        publish_self_jit_depth(restored_depth);
        cratonvm_jit::jit_thread_blocked_segment_leave(self.quiescence_nesting);
        // Hazard 1: the shadow window under this OS tid is the carrier's again
        // before any carrier compiled code runs (the first is the compiled
        // caller `enterSpecial` returns into, which is no JIT entry).
        if let Some((addr, base, end)) = self.carrier_shadow {
            publish_self_shadow_addr_once(addr, base, end);
        }
        let _ = cratonvm_gc::shadow_stack::replace_current_segment_top(self.prev_segment_top);
    }
}

/// The OS tid the gc crate's per-tid segment table and the helper-window
/// captures are keyed by, for `enter_special`'s segment publication.
pub(crate) fn segment_os_tid() -> u32 {
    self_os_tid()
}


/// Code reclamation: publish, at a transition into a blocked state, which
/// compiled bodies this thread's stack can return into while it stays blocked.
///
/// A thread parked inside compiled code (a pool worker in
/// `LinkedBlockingQueue.take()`) holds a JIT execution for as long as it is
/// parked, so the process-wide quiescence count never reaches zero. Every return
/// address into compiled code on this thread lies between the current frame and
/// the outermost JIT entry's captured SP, so that band is what
/// `cratonvm_jit::jit_thread_blocked_enter` scans. See
/// `docs/jit/code-cache-lifetime.md`.
///
/// `#[inline(never)]` so the stack probe lives in this frame, which stays live
/// for the whole scan. Called by every `GcBarrier` blocked-state entry; paired
/// with [`note_blocking_transition_leave`].
#[inline(never)]
pub(crate) fn note_blocking_transition_enter() {
    let lo = current_stack_pointer();
    // A thread holding no JIT entry passes an empty band: nothing to scan, but
    // the window still nests, so the matching leave stays balanced.
    let hi = outermost_jit_entry_sp().unwrap_or(lo).max(lo);
    // SAFETY: `[lo, hi)` is this thread's own stack between this live frame and
    // the outermost live compiled entry's SP, all of it mapped for the call.
    unsafe { cratonvm_jit::jit_thread_blocked_enter(lo, hi) };
}

/// End the blocked window [`note_blocking_transition_enter`] opened.
#[inline]
pub(crate) fn note_blocking_transition_leave() {
    cratonvm_jit::jit_thread_blocked_leave();
}

/// This thread's block-stamp slot (`gc_quiescence::register_self_block_stamp_slot`)
/// and its blocked-window nesting; removed from the registry at thread exit,
/// as [`JitDepthSlot`].
pub(in crate::jit::conservative_roots) struct BlockStampSlot {
    pub(in crate::jit::conservative_roots) os_tid: u32,
    pub(in crate::jit::conservative_roots) cell: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(in crate::jit::conservative_roots) nesting: u32,
}

impl Drop for BlockStampSlot {
    fn drop(&mut self) {
        cratonvm_gc::gc_quiescence::unregister_block_stamp_slot(self.os_tid);
    }
}

thread_local! {
    pub(in crate::jit::conservative_roots) static SELF_BLOCK_STAMP_SLOT: std::cell::RefCell<Option<BlockStampSlot>> =
        const { std::cell::RefCell::new(None) };
}

/// ngr: publish the region hand-out clock at this thread's OUTERMOST blocked
/// region entry (`enter`), and 0 at its matching leave, for G1's post-block
/// screen of blocked-peer stack words (`g1_peer_pins_skip_post_block_regions`).
/// Called by the `GcBarrier` blocked-region entries and exits only, beside
/// [`note_blocking_transition_enter`] / [`note_blocking_transition_leave`]
/// (which also open code-reclamation windows on a RUNNING thread).
///
/// A skipped publish (thread teardown, re-entrant borrow) leaves 0 or the
/// window's own entry stamp, never one newer than the thread's last heap read:
/// 0 keeps every pin, and an entry stamp is what the window had. The leave
/// stores 0 before the barrier waits out a pause, so a leaving thread is
/// screened by nothing.
#[inline]
pub(crate) fn publish_self_block_stamp(enter: bool) {
    let _ =SELF_BLOCK_STAMP_SLOT.try_with(|c| {
        let Ok(mut slot) = c.try_borrow_mut() else {
            return;
        };
        let held = slot.get_or_insert_with(|| {
            let os_tid = self_os_tid();
            BlockStampSlot {
                os_tid,
                cell: cratonvm_gc::gc_quiescence::register_self_block_stamp_slot(os_tid),
                nesting: 0,
            }
        });
        if enter {
            if held.nesting == 0 {
                held.cell.store(
                    cratonvm_gc::gc_quiescence::region_handout_clock_at_block(),
                    std::sync::atomic::Ordering::Release,
                );
            }
            held.nesting = held.nesting.saturating_add(1);
        } else {
            held.nesting = held.nesting.saturating_sub(1);
            if held.nesting == 0 {
                held.cell.store(0, std::sync::atomic::Ordering::Release);
            }
        }
    });
}

/// Highest `entry_sp` on this thread's JIT entry chain — the outermost compiled
/// entry — or `None` with no entry. `try_with`/`try_borrow` because a blocking
/// transition can run during thread teardown and must never panic.
pub(in crate::jit::conservative_roots) fn outermost_jit_entry_sp() -> Option<usize> {
    JIT_ENTRY_CHAIN
        .try_with(|chain| {
            chain
                .try_borrow()
                .ok()
                .and_then(|entries| entries.iter().map(|e| e.entry_sp).max())
        })
        .ok()
        .flatten()
}

/// Kill switch for the nested-activation walk in [`active_compiled_frames`].
///
/// Default ON. `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES=1` restores the historical
/// one-frame-per-chain-entry answer, so the frame-count difference is an A/B
/// inside ONE binary instead of a comparison across two builds.
pub(in crate::jit::conservative_roots) fn nested_trace_frames_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !false
    })
}

/// Whether [`active_compiled_frames`] dumps the chain it walked to stderr.
///
/// `CRATONVM_DBG_SWCHAIN=1`. Off by default and cached, because this runs on
/// every VM-raised throw. What it prints per chain entry — the recorded
/// `entry_sp` / `exact_rbp` / published compile id, the boundary method, how
/// the innermost frame resolved, and the walked activation list — is the dump
/// that identified the defect this walk had: a frame the mirror named
/// correctly but no decoder could resolve, because only one of the two
/// encodings of a direct call was ever decoded.
pub(in crate::jit::conservative_roots) fn dbg_swchain_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || false)}

/// Kill switch for the bytecode index carried on [`ActiveCompiledFrame`].
///
/// Default ON. `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` makes every compiled
/// frame report `bci: -1`, which is exactly the `byte_code_index: -1` /
/// `(Unknown Source)` answer every compiled frame gave before 2026-09-01. The
/// point of the switch is that a line that looks wrong in a warmed-up trace
/// can be attributed — one environment variable decides whether it came from
/// this recovery or from the method's own `LineNumberTable`, inside ONE
/// binary. It reverts the OSR line with it: `stackwalker`'s display-only
/// override is the bci of a compiled half that no longer has one.
pub(in crate::jit::conservative_roots) fn compiled_frame_bci_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !false
    })
}

/// Kill switch for the inlined-callee chain carried on
/// [`ActiveCompiledFrame`].
///
/// Default ON, and the SAME variable the emitter half reads
/// (`x64::inlining`'s `inline_frame_map_enabled`), because a half-switched
/// feature is worse than either state: with the map emitted and the walk
/// ignoring it the artifact pays for metadata nobody reads, and with the walk
/// reading a map that was never emitted every frame silently loses its
/// callees. One variable, both halves.
/// Kill switch for the OPTIMIZING tier's line numbers.
///
/// Default ON. `CRATONVM_JIT_NO_IR_FRAME_LINES=1` restores the blanket
/// `used_ir_backend` refusal, i.e. `(Unknown Source)` for every frame the IR
/// backend compiled, which is what every such frame reported before
/// `CompiledMethod::safepoint_bci_table` existed.
///
/// It is a separate name from `CRATONVM_JIT_NO_COMPILED_FRAME_LINES` because
/// the two isolate different things. That one reverts the whole recovery, on
/// both backends at once, so it cannot say whether a suspect line came from the
/// id-to-bci translation this tier needs or from the slot read both tiers
/// share. The translation is the new evidence and is the half worth being able
/// to switch alone; a wrong line under this switch OFF and a right one under it
/// ON is a one-binary attribution.
pub(in crate::jit::conservative_roots) fn ir_frame_bci_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !false)
}

pub(in crate::jit::conservative_roots) fn inline_frame_chains_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !false)
}

/// Is `recorded` a value the JVM spec permits as a bytecode index?
///
/// `Code.code_length` must be less than 65536 (JVMS 4.9.1), so every genuine
/// bci is below that. Both synthetic pcs the single-pass backend stamps into
/// this same field are far above it — `ENTRY_POLL_BC_PC` (`u32::MAX`, the
/// method-entry poll) and `SP_ID_UNSET_BC_PC` (`u32::MAX - 1`, what the
/// prologue writes into the safepoint-id slot so an unsafepointed frame cannot
/// match the bci-0 map). One spec-derived bound rejects both, and it rejects
/// them for a reason a reader can check rather than by listing two constants
/// this crate cannot see (they are `pub(in crate::jit)` inside the jit crate's x64
/// module).
pub(in crate::jit::conservative_roots) fn plausible_bci(recorded: u32) -> Option<u32> {
    const MAX_CODE_LENGTH: u32 = 65_536;
    (recorded < MAX_CODE_LENGTH).then_some(recorded)
}

/// One live compiled activation, as a stack capture needs it.
///
/// Replaces the `(depth, label, class_id, cm_ptr)` tuple this function used to
/// return. The tuple was the reason a compiled frame had no line number: it
/// carried no bytecode index, and `stackwalker::compiled_frame_entry` — the
/// only consumer that wants one — could therefore do nothing but hard-code
/// `LINE_NUMBER_UNKNOWN`. See
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`.
#[derive(Debug, Clone)]
pub struct ActiveCompiledFrame {
    /// Interpreter depth this activation's chain entry was pushed at; the
    /// splice point in `stackwalker::interleave_compiled_frames`.
    pub interp_depth: u32,
    /// Index, in this thread's JIT entry chain, of the entry this activation
    /// was expanded from (every nested activation of one entry shares it).
    /// A record kept OUTSIDE the chain that noted the chain's length `J` when
    /// it was made (a reflective call, a JNI native's JVMTI row) goes after
    /// the activations with `chain_index < J` at its depth and before those
    /// with `chain_index >= J` (`stackwalker::trace_anchor_position`;
    /// interpreter round i1 wave 44, lane L3,
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    pub chain_index: u32,
    /// `"class/Name.method:descriptor"`, from the artifact's `method_label`.
    /// Shared with the artifact (`CompiledMethod::method_label_shared`).
    pub label: std::sync::Arc<str>,
    /// `ObjectHeader` class id of the artifact's owner.
    pub owner_class_id: u32,
    /// The `CompiledMethod` this activation is running, as a raw address.
    /// Valid for exactly as long as the frame is live.
    pub cm_ptr: usize,
    /// Bytecode index this activation is stopped at, or `-1` when the frame
    /// published no usable safepoint id. NEVER a guess: see
    /// [`activation_bci`].
    pub bci: i32,
    /// The callees the JIT INLINED into this artifact at the program point
    /// this activation is standing at, INNERMOST FIRST, as
    /// `("class/Name.method:descriptor", bci)` pairs — the label in the same
    /// shape [`ActiveCompiledFrame::label`] uses, so one splitter parses both,
    /// and the bci in that callee's own code.
    ///
    /// It is HotSpot's `ScopeDesc` chain, and it is why a compiled frame is
    /// not one frame: an inlined callee pushes nothing and, before this,
    /// contributed nothing to a trace. Empty for every non-inlining method and
    /// for every refusal on the producer side, and an empty chain reproduces
    /// the historical one-entry-per-artifact answer exactly. See
    /// [`compiled_frame_inline_chain`].
    pub inline_chain: Vec<InlinedLevel>,
    /// JIT round 19 wave 2 (lane throws2, THROWS18-2;
    /// `CRATONVM_THROWABLE_SHARED_INLINE_CHAINS=1`): the same chain, SHARED
    /// with the artifact's inline-frame map (one reference count instead of a
    /// `Vec` and a label clone per level). Set only by a walk asked for shared
    /// chains ([`CompiledWalkOptions::shared_inline_chains`], which the
    /// throwable capture asks for under that switch), and then INSTEAD of
    /// [`Self::inline_chain`], which is left empty. `None` from every other
    /// walk. A reader that may see a shared-chain walk reads [`Self::chain`].
    pub inline_chain_shared: Option<SharedInlineChain>,
    /// Was [`Self::inline_chain`] obtained from the EXACT key (this
    /// activation's own return address), rather than the coarse safepoint-id
    /// one?
    ///
    /// Display does not care -- both keys are fail-closed for it. A CALLER
    /// ATTRIBUTION walk does: one `cur_bc_pc` covers a whole spliced region and
    /// is shared with the inline cache's MISS EDGE, where the spliced body did
    /// NOT run. A wrong frame in a trace is a wrong frame; a wrong frame in the
    /// JEP 403 gate is a fail-OPEN caller. So
    /// `stackwalker::frame_class_ids_with_compiled` expands only an exact
    /// chain, and this is the bit that says which it has.
    ///
    /// `true` on an EMPTY chain is meaningless and never read: there is nothing
    /// to expand.
    pub chain_exact: bool,
    /// Did [`Self::bci`] come from an NPE trap site the EMITTER described
    /// (`apply_npe_trap_site`), rather than from the safepoint-id slot?
    ///
    /// Both answers are good enough to print a LINE, which is why the walk has
    /// never had to distinguish them. Rebuilding a JEP 358 *message* from the
    /// bci is a stronger use: it reads the opcode at that index and names the
    /// field or method it references, so a bci that is merely the last
    /// safepoint this frame passed — rather than the program point that
    /// trapped — yields a fluent sentence about the wrong dereference. A trap
    /// site is recorded AT the null check it describes and cannot be stale.
    ///
    /// See `runtime::interpreter::jit_npe_message`, the only reader.
    pub trap_site_exact: bool,
    /// Is [`Self::bci`] a safepoint bci the artifact's inline-frame map
    /// POISONED (`InlineFrameMap::safepoint_bci_is_poisoned`) -- the entry of
    /// a spliced region with several calls, so the frame stands somewhere
    /// inside an inlined body the map cannot name? Only ever `true` for an
    /// innermost frame read by its safepoint id (key 2) with no chain.
    ///
    /// A LINE may still be printed at that bci, as it always was: the line of
    /// the call the splice replaced. A JEP 358 MESSAGE may not be rebuilt from
    /// it: the opcode there is that call, not the dereference that trapped
    /// (`jit_npe_message` declines; SBR wave 5, lane reflacc -- a hot
    /// `Field.get(null)` read `Cannot invoke "Field.checkAccess(..)" because
    /// "this" is null` where the null was `checkAccess`'s `obj.getClass()`).
    pub chain_poisoned: bool,
    /// The artifact's `CompiledMethod::compile_cp_stamp`: the class-redefinition
    /// count its compilation began at, so the class version whose bytecode --
    /// and line table -- this activation and the callees it inlined run. A
    /// retained Throwable trace resolves the line of such a frame against the
    /// table its class had then when the class was redefined since
    /// (`obsolete_frames::resolve_lines_as_captured`; interpreter round i1
    /// wave 38, lane L3). `None` without a registered source.
    pub compile_cp_stamp: Option<u64>,
    /// Does the artifact stand for the code HotSpot's C2 would be running
    /// (`runtime::exceptions::body_is_c2_equivalent`: an optimizing-tier body,
    /// or a single-pass one past HotSpot's tier-4 count)? Read at the walk
    /// because `cm_ptr` is valid only while the frame is live, and the
    /// `OmitStackTraceInFastThrow` hotness rule
    /// (`runtime::exceptions::fast_throw_site_of`) reads it at a drain that
    /// may run after the frame has unwound. `false` wherever no walk read it.
    pub fast_throw_c2_tier: bool,
    /// Artifact ID of the compiled method running this activation (monotonic per process),
    /// used by fast throw to verify if traps were recorded prior to compilation.
    pub artifact_id: u64,
    /// JIT round 19 proposal wave 7, lane gc7 (SYNC619-1): the frame pointer
    /// of this activation -- the RBP its line and inline chain were read from
    /// -- or `None` when the walk had none for it (the boundary fallback of an
    /// entry whose walk reported other activations, the kill-switch path).
    ///
    /// Valid for exactly as long as the frame is live, and only on the thread
    /// whose stack it is. Its one reader is [`compiled_frame_reference_word`],
    /// which reads a word of the frame only where the frame's ACTIVE oop map
    /// names it as a live reference -- the word the collector's precise half
    /// rewrites -- so the value read is current. Never dereferenced as an
    /// object by that reader's callers: a monitor attribution compares it with
    /// the lock stack and nothing else.
    pub frame_rbp: Option<usize>,
}

impl ActiveCompiledFrame {
    /// This activation's inline chain, innermost first, whichever field holds
    /// it: [`Self::inline_chain_shared`] when the walk shared it, else
    /// [`Self::inline_chain`] (THROWS18-2, round 19).
    #[inline]
    pub fn chain(&self) -> &[InlinedLevel] {
        match &self.inline_chain_shared {
            Some(shared) => shared,
            None => &self.inline_chain,
        }
    }

    /// The chain as an owned `Vec`: [`Self::inline_chain`] moved out, or the
    /// shared chain's levels copied (a label clone each). For a reader that
    /// keeps a chain past the walk in a `Vec` (the OSR dedupe's override).
    pub fn into_chain_vec(self) -> Vec<InlinedLevel> {
        match self.inline_chain_shared {
            Some(shared) => shared.to_vec(),
            None => self.inline_chain,
        }
    }
}

/// One level of a compiled frame's inline chain, as a stack walk consumes it.
///
/// Replaces the `(label, bci)` pair the chain used to carry. The third field is
/// the whole reason: `class_id` is recorded by the RESOLVER, at the moment it
/// looked the spliced body up, so a walk that must answer in `ClassId` --
/// `frame_class_ids_with_compiled`, feeding the JEP 403 deep-reflection gate
/// and `Class.forName`'s caller loader -- can see an inlined method without
/// resolving a JIT label BY NAME. That resolution is what made the blind spot
/// deliberate: a by-name answer in a security gate is a guess, and this is not
/// one.
///
/// Fields: `label` (`"class/Name.method:descriptor"`, the shape
/// [`ActiveCompiledFrame::label`] uses; the emitter's own shared `Arc`, so
/// copying a chain onto a capture is a reference-count bump per level rather
/// than an allocation), `bci` (bytecode index in `label`'s own method) and
/// `class_id` (`ClassId` of the class `label` names, or `0` for "the producer
/// supplied none" -- a consumer must REFUSE on `0`, never substitute).
///
/// JIT round 19 wave 2 (lane throws2, THROWS18-2): the JIT's own
/// [`cratonvm_jit::x64::InlineFrameLevel`] -- the same three fields, which
/// this type used to copy level by level -- so a chain can be SHARED with the
/// artifact's inline-frame map ([`ActiveCompiledFrame::inline_chain_shared`])
/// instead of copied. Every construction and field access reads as before.
pub type InlinedLevel = cratonvm_jit::x64::InlineFrameLevel;

/// An inline chain shared with the artifact's inline-frame map
/// ([`ActiveCompiledFrame::inline_chain_shared`]; THROWS18-2, round 19).
pub type SharedInlineChain = cratonvm_jit::x64::InlineChain;

// The bytecode index a live compiled activation is stopped at.
//
// Reads the safepoint id the emitter publishes into `[rbp - sp_id_slot_off]`
// before every GC-capable call — the same slot `moving_young_frame_live_hi`
// and the precise root walkers already key off — and then REQUIRES that the
// artifact actually recorded a safepoint with that id.
//
// That second half is what makes this safe to put on a stack trace. The slot
// is written before a call, so between calls it holds the id of the last
// safepoint rather than the current position; and an artifact that reserves no
// slot leaves whatever the frame's uninitialised memory held. Requiring the id
// to name one of THIS artifact's own recorded safepoints rejects both, at the
// cost of answering `None` for a frame stopped somewhere no safepoint covers.
// A frame with no line is what this page's own reasoning asked for over a
// frame with a wrong one, and it is what the caller falls back to.
//
// # Two more things it refuses
//
// * **An optimizing IR-backend id with no translation** (`used_ir_backend`).
//   There `OopMapEntry::bytecode_pc` is NOT a bci: `ir_lower` stores a
//   monotonic safepoint counter starting at 1 (its own doc says so, and the
//   single-pass backend's `SP_ID_UNSET_BC_PC` comment explains why THAT
//   backend cannot do the same). Those ids are small integers
//   indistinguishable from plausible bcis, AND the artifact's own table
//   records them, so the confirmation above passes and a real,
//   confidently-wrong line would come out if the id were used as one.
//
//   Until 2026-09-02 the whole backend was refused on that ground, and that
//   was the largest remaining population of line-less compiled frames. The
//   refusal is now exactly as wide as the hazard: `ir_lower` records the
//   `(id, bci)` pair for every safepoint it emits
//   (`CompiledMethod::safepoint_bci_table`), this reads the bci THROUGH it,
//   and an id the table does not name is still refused. What that leaves is
//   an artifact compiled before the table existed, or a safepoint whose bci
//   exceeded `u32` -- both `None`, neither a guess.
//   `CRATONVM_JIT_NO_IR_FRAME_LINES=1` restores the blanket refusal.
// * **Anything outside the spec's bci range** — see [`plausible_bci`]. The
//   two synthetic pcs the single-pass backend stamps are already rejected by
//   the `i32::try_from` below (both are within one of `u32::MAX`), but that
//   is an accident of their VALUES rather than a rule, and the rule is what
//   the next synthetic pc will be measured against.
//
// An artifact with no safepoint-id slot (`sp_id_slot_off == 0`) -- compiled
// without the precise gate, or aarch64, whose backend hard-codes
// `bytecode_pc: 0` and would otherwise resolve every compiled frame to the
// first line of its method -- is already refused by `active_safepoint_id`.
//
// # aarch64: the refusal is UNREACHABLE, not merely conservative (2026-09-02)
//
// The 2026-09-01 page listed "aarch64 compiled frames still report no line"
// as an open residual. It is not one, and the reason is worth writing down
// because it is cheaper than re-deriving it: **an aarch64 compiled activation
// cannot be on the stack while a Java-level stack capture runs.**
//
// A trace is captured either at a throw or from a caller-sensitive native,
// and both are reached by an `invoke*`. `Arm64Backend::emit_invoke` sets
// `self.failed` UNCONDITIONALLY -- there is no call-target resolution in that
// backend at all -- so a method containing any call bails and is interpreted
// (`jit::aarch64_backend::tests::invokestatic_arm_exists_but_always_bails`).
// `athrow` and every object-model opcode bail on the same rule
// (`object_model_opcodes_are_all_unsupported`), so such a body cannot raise
// either, and `label_for_pc` refuses every backward branch target, so it
// cannot loop. What compiles there is leaf, straight-line, exception-free
// arithmetic that runs to its `ret`.
//
// `Arm64CompileResult::pending_oop_maps` is empty for the same generation of
// reasons (`compiled_methods_carry_no_oop_maps`) — as of 2026-09-03 the map
// WRITER is correct and its publication path works, but there is still no
// safepoint of any kind to call it at, and so none to name a bci at.
// A line number is downstream of a safepoint mechanism that backend does not
// have; giving it one is a consequence of building that mechanism, not a
// separate task. Three tests pin the premises, so a change that makes an
// aarch64 frame reachable from a capture trips them first.
//
// # A frame that keeps no references (`frame_holds_no_references`)
//
// Such a frame stores its id only before a call that leaves compiled code,
// so the slot is current for the INNERMOST activation and may be stale for
// one suspended in a direct JIT-to-JIT call. `native_pc` -- the frame's own
// return address, `Some` exactly for a frame below the innermost -- is then
// the key, looked up in `CompiledMethod::return_site_bci_table`, and a miss
// is refused rather than answered from the slot.
// The walk calls `activation_bci_with` since JIT round 16 wave 15; this
// form stays as the one-frame entry the tests pin.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn activation_bci(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    native_pc: Option<usize>,
) -> Option<i32> {
    activation_bci_with(rbp, cm, native_pc, &mut WalkLookups::per_call())
}

/// [`activation_bci`] with a walk's [`WalkLookups`]: the same answer and the
/// same census verdict; a walk-scoped `lookups` tallies the verdict for the
/// walk's one flush and asks the artifact's safepoint-id index instead of
/// scanning its maps (JIT round 16 wave 15, lane throwfill).
pub(in crate::jit::conservative_roots) fn activation_bci_with(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    native_pc: Option<usize>,
    lookups: &mut WalkLookups,
) -> Option<i32> {
    use cratonvm_jit::{
        FRAME_LINE_ANSWERED_IR, FRAME_LINE_ANSWERED_SINGLE_PASS, FRAME_LINE_REFUSED_ID_UNRECORDED,
        FRAME_LINE_REFUSED_IR_UNTRANSLATED, FRAME_LINE_REFUSED_NO_SP_ID,
        FRAME_LINE_REFUSED_OUT_OF_RANGE, FRAME_LINE_REFUSED_SWITCHED_OFF,
    };
    let indexed = lookups.scoped;
    let mut note = |slot: usize| lookups.note(slot);
    if cm.frame_holds_no_references {
        if let Some(pc) = native_pc {
            if cm.used_ir_backend && !ir_frame_bci_enabled() {
                note(FRAME_LINE_REFUSED_SWITCHED_OFF);
                return None;
            }
            let Some(recorded) = return_site_bci(cm, pc) else {
                note(FRAME_LINE_REFUSED_ID_UNRECORDED);
                return None;
            };
            let Some(bci) = plausible_bci(recorded).and_then(|v| i32::try_from(v).ok()) else {
                note(FRAME_LINE_REFUSED_OUT_OF_RANGE);
                return None;
            };
            note(if cm.used_ir_backend {
                FRAME_LINE_ANSWERED_IR
            } else {
                FRAME_LINE_ANSWERED_SINGLE_PASS
            });
            return Some(bci);
        }
    }
    // Every exit below names a census column. That is not decoration: a trace
    // prints `(Unknown Source)` for all five refusals and there is no other
    // way to tell which population a line-less frame came from. See
    // `cratonvm_jit::COMPILED_FRAME_LINE_COUNTS`.
    //
    // The raw slot, not `active_safepoint_id`: that refuses every
    // `frame_holds_no_references` frame for the GC's sake, and the innermost
    // one's slot is current (see above). For every other frame the two agree.
    // Interpreter round i5 wave 102, lane L2: every refusal below first asks
    // the return-site row of a frame suspended in a call
    // ([`return_site_line_fallback`]); only a frame with none is refused.
    let Some(raw_id) = safepoint_id_slot(rbp, cm) else {
        return return_site_line_fallback(cm, native_pc, &mut note, FRAME_LINE_REFUSED_NO_SP_ID);
    };
    // Round 12 wave 6 (lane exc4): a single-pass post-CALL stub that runs a
    // Java-capable helper leaves the call's own id in trace-only form (no map
    // matches it, so the collector fails closed). Strip the tag here -- and
    // only here -- and confirm the id below like any other.
    let id = match trace_only_sp_id_payload(raw_id) {
        Some(call_id) if !cm.used_ir_backend => call_id,
        _ => raw_id,
    };
    // `find_oop_map_for_safepoint_id` is `&self` and is the artifact's own
    // record of which bytecode PCs it emitted a safepoint at. A walk-scoped
    // lookup asks the same question of the artifact's sorted id index.
    let recorded_id = if indexed {
        cm.has_oop_map_for_safepoint_id(id)
    } else {
        cm.find_oop_map_for_safepoint_id(id).is_some()
    };
    if !recorded_id {
        return return_site_line_fallback(cm, native_pc, &mut note, FRAME_LINE_REFUSED_ID_UNRECORDED);
    }
    // The optimizing tier's slot holds an ID, not a bci. Translate it through
    // the artifact's own table, and refuse when there is none: an untranslated
    // id is a small integer that looks exactly like a bci, which is the wrong
    // answer this refusal was written to avoid.
    let recorded = if cm.used_ir_backend {
        if !ir_frame_bci_enabled() {
            note(FRAME_LINE_REFUSED_SWITCHED_OFF);
            return None;
        }
        match cm.safepoint_bci(id) {
            Some(bci) => bci,
            None => {
                return return_site_line_fallback(
                    cm,
                    native_pc,
                    &mut note,
                    FRAME_LINE_REFUSED_IR_UNTRANSLATED,
                );
            }
        }
    } else if !cm.safepoint_bci_table.is_empty() {
        // A single-pass body whose bytecode loop rewriter ran
        // (`CRATONVM_JIT_BYTECODE_LOOP_XFORM`): its ids are OUTPUT pcs of the
        // rewritten image, shifted past every replicated loop, and the driver
        // publishes this table to map them back. Using the id as a bci there
        // named the wrong line (r11-x64loop-loop-rewriter-pc-keyed-tables-left-
        // unlifted). Every safepoint of such a body is in the table, so a miss
        // is refused rather than guessed.
        match cm.safepoint_bci(id) {
            Some(bci) => bci,
            None => {
                return return_site_line_fallback(
                    cm,
                    native_pc,
                    &mut note,
                    FRAME_LINE_REFUSED_IR_UNTRANSLATED,
                );
            }
        }
    } else if id == ENTRY_POLL_SP_ID {
        // The method-entry poll's synthetic id, which a self-locking body's
        // prologue monitor enter publishes too (`x64/frames.rs`
        // `emit_self_lock_enter`): the frame stands at its entry, bci 0, as
        // HotSpot lists a method blocked in `lock_method`. Before the i54-L6
        // retirement it was refused as out of range, and a thread blocked
        // entering a synchronized compiled body directly called by compiled
        // code listed that method at line -1
        // (`L2W58BlockedEntrantInCompiledCallers.java`,
        // `incBig` / `sincBig`).
        0
    } else {
        id
    };
    let Some(bci) = plausible_bci(recorded).and_then(|v| i32::try_from(v).ok()) else {
        return return_site_line_fallback(cm, native_pc, &mut note, FRAME_LINE_REFUSED_OUT_OF_RANGE);
    };
    note(if cm.used_ir_backend {
        FRAME_LINE_ANSWERED_IR
    } else {
        FRAME_LINE_ANSWERED_SINGLE_PASS
    });
    Some(bci)
}

/// Interpreter round i5 wave 102, lane L2 (page
/// `i101-L2-compiled-frame-without-a-safepoint-id-has-no-line`): the line of a
/// frame whose safepoint-id slot [`activation_bci_with`] refused, from the
/// row the optimizing tier recorded for the call the frame is suspended in
/// (`CompiledMethod::return_site_bci_table`, kept for every IR body under
/// `CRATONVM_JIT_RETURN_SITE_LINE_FALLBACK`). Exact or nothing: the row is
/// keyed by the very return address the frame holds, so a frame with no
/// return address (the innermost one) or no row is refused as before, under
/// `refusal`; an answer is counted as an answered frame. A reference-free
/// frame asked its row first already and is not asked twice.
pub(in crate::jit::conservative_roots) fn return_site_line_fallback(
    cm: &cratonvm_jit::CompiledMethod,
    native_pc: Option<usize>,
    note: &mut impl FnMut(usize),
    refusal: usize,
) -> Option<i32> {
    use cratonvm_jit::{FRAME_LINE_ANSWERED_IR, FRAME_LINE_ANSWERED_SINGLE_PASS};
    let answer = match native_pc {
        Some(pc)
            if !cm.frame_holds_no_references
                && !cm.return_site_bci_table.is_empty()
                && cratonvm_jit::return_site_line_fallback_enabled() =>
        {
            return_site_bci(cm, pc)
                .and_then(plausible_bci)
                .and_then(|v| i32::try_from(v).ok())
        }
        _ => None,
    };
    match answer {
        Some(bci) => {
            note(if cm.used_ir_backend {
                FRAME_LINE_ANSWERED_IR
            } else {
                FRAME_LINE_ANSWERED_SINGLE_PASS
            });
            Some(bci)
        }
        None => {
            note(refusal);
            None
        }
    }
}

/// Mirror of `jit/src/x64/safepoint.rs`'s `ENTRY_POLL_BC_PC` (`u32::MAX`), the
/// synthetic id a single-pass body publishes at its method-entry poll, its
/// self-tail poll and its self-locking prologue's monitor enter: each
/// describes the state at bci 0 ([`activation_bci_with`]).
pub(in crate::jit::conservative_roots) const ENTRY_POLL_SP_ID: u32 = u32::MAX;

/// Mirror of `jit/src/x64/deopt_stubs.rs`'s `SP_ID_TRACE_ONLY_TAG` (the crates
/// share no module both can name; that file's
/// `the_encoding_is_the_one_the_vm_decoder_mirrors` pins the same numbers).
///
/// The single-pass stubs `emit_inline_callee_deopt_check` and
/// `emit_local_handler_stubs` call a helper that can run Java -- the servicing
/// helper resumes a trapped callee in the interpreter, which is where the
/// callee's implicit exception is built -- after the call's own map has
/// stopped describing the frame. They used to store the bare
/// `SP_ID_UNSET_BC_PC`, so a `fillInStackTrace` inside the helper read this
/// frame as `-1` with no inline chain and every callee the backend had spliced
/// at the call vanished from the trace
/// (`r12w5-exc3-ir-spliced-frames-vanish-from-implicit-exception-traces`).
/// They now store `SP_ID_TRACE_ONLY_TAG | call_id`: bit 31 over a payload
/// below 2^24, a value no recorded map carries (single-pass ids are emitter
/// pcs; the synthetic ones are `0xFFFF_FFFx`), so every collector reader --
/// all of which match maps by EQUALITY -- still finds no map. Only
/// [`activation_bci`] decodes it.
pub(in crate::jit::conservative_roots) const SP_ID_TRACE_ONLY_TAG: u32 = 0x8000_0000;
/// The payload mask for [`SP_ID_TRACE_ONLY_TAG`] (ids below 2^24).
pub(in crate::jit::conservative_roots) const SP_ID_TRACE_ONLY_PAYLOAD: u32 = 0x00FF_FFFF;

/// The call id a trace-only safepoint-id word carries, or `None` for every
/// other word -- in particular `SP_ID_UNSET_BC_PC` (`0xFFFF_FFFE`) and
/// `ENTRY_POLL_BC_PC` (`0xFFFF_FFFF`), whose top byte is not `0x80`.
pub(in crate::jit::conservative_roots) fn trace_only_sp_id_payload(raw: u32) -> Option<u32> {
    ((raw & !SP_ID_TRACE_ONLY_PAYLOAD) == SP_ID_TRACE_ONLY_TAG)
        .then_some(raw & SP_ID_TRACE_ONLY_PAYLOAD)
}


/// `return_site_bci`: the bci `cm` recorded for the call whose return address
/// is `pc`, from `CompiledMethod::return_site_bci_table`. `None` for an
/// address outside the body or a call it recorded no row for.
pub(in crate::jit::conservative_roots) fn return_site_bci(cm: &cratonvm_jit::CompiledMethod, pc: usize) -> Option<u32> {
    let entry = cm.entry_ptr() as usize;
    let end = entry.saturating_add(cm.code_len());
    // A return address is strictly INSIDE the body (a call instruction
    // precedes it) and at most one past its last byte -- the bound
    // `compiled_frame_inline_chain` applies to the same key.
    if entry == 0 || pc <= entry || pc > end {
        return None;
    }
    let off = u32::try_from(pc - entry).ok()?;
    let table = &cm.return_site_bci_table;
    let i = table.binary_search_by_key(&off, |&(o, _)| o).ok()?;
    table.get(i).map(|&(_, bci)| bci)
}

/// The callees inlined into `cm` at the program point a live frame is standing
/// at, INNERMOST FIRST, as `(label, bci)` pairs — where `label` is
/// `"class/Name.method:descriptor"`, the shape
/// `CompiledMethod::method_label` already uses, and `bci` is that method's own
/// bytecode index.
///
/// The chain EXTENDS [`activation_bci`]'s answer and never replaces it: the
/// enclosing compiled method keeps the bci that function returns, and these
/// are the frames HotSpot would show *below* it, from `ScopeDesc`'s nested
/// scopes. An empty chain is the historical answer — one frame per artifact —
/// and is what every non-inlining method produces.
///
/// # The two keys, and why neither one falls back to the other
///
/// The emitter half lives in `jit/src/x64/inlining.rs`
/// (`begin_inline_frame_recording` / `finish_inline_frame_recording`) and
/// records, at every call it emits from inside a spliced body, a row holding
/// BOTH keys this walk can offer: `native_offset`, the buffer position
/// immediately after the `CALL` — which is exactly what a child frame's
/// RBP-chain walk reads out of `[rbp+8]` — and `safepoint_bci`, the
/// `cur_bc_pc` the emitter simultaneously stores into the frame's
/// safepoint-id slot.
///
/// Which key applies is decided by WHICH FRAME is asking, not by which lookup
/// happens to hit:
///
///   * a frame BELOW the innermost one owns a return address on this stack, so
///     `native_pc` is `Some` and the exact offset names one program point and
///     nothing else. That is the authoritative key and it is the only one
///     consulted;
///   * the INNERMOST frame owns no return address here (its caller's is the
///     one on the stack), so the safepoint-id slot — i.e. [`activation_bci`]'s
///     answer — is its only evidence.
///
/// **There is deliberately no fallback from the exact key to the coarse one.**
/// A bci is a property of the enclosing method and both keys answer with the
/// same `cur_bc_pc`. A CHAIN is not. One `cur_bc_pc` covers a whole spliced
/// region, and the calls emitted on the inline cache's MISS EDGE — the ones
/// that were not spliced — sit under that same bci while recording no row of
/// their own. A parent frame suspended on a miss edge would then be handed the
/// chain belonging to the splice beside it: a frame naming a method that never
/// ran, which is the one outcome this whole area refuses. A miss on the exact
/// key means "this program point recorded no chain", and that is the answer.
///
/// # The optimizing tier needs no extra refusal here
///
/// Key 2 is the safepoint-id slot read back as a bci, and since 2026-09-02
/// [`activation_bci`] can answer that for an `used_ir_backend` artifact too
/// (through `CompiledMethod::safepoint_bci_table`). Key 1 is a CODE LAYOUT
/// fact -- the byte offset of a return address in this artifact's own buffer
/// -- and carries no assumption about which backend emitted it. Neither needs
/// a backend refusal, and since 2026-09-08 neither gets one: an IR artifact
/// that spliced a body now arrives here with a POPULATED `inline_frame_map`
/// and its inlined callees are reported like any other.
///
/// Until then they were not. `record_inline_frame_row` is called only from the
/// single-pass splicer, so an IR artifact's map was empty and this function
/// returned on the `is_empty()` guard before either key was consulted -- and
/// IR-tier inlining contributed no frames at all. That was invisible while the
/// optimizing tier claimed few methods and became a live trace defect when it
/// claimed more: measured 2026-09-08 on `probes/StackTraceAfterOsr.java`,
/// `after_main_osr` printed `len=3 [leaf mid* outer* probe main]` against the
/// interpreter's `len=5`, the two starred frames being bodies the IR splicer
/// had inlined into `probe`.
///
/// The producer is `ir_lower`'s `note_inline_frame_return_site`, and it is
/// keyed off `ir::IrInlineFrameSites` -- the COMBINED-BUFFER pc -- not off
/// `InlineScopeTable` as this note used to predict (that table was retired in
/// round 10 wave 9, for the reason its own note in `jit/src/ir.rs` gives).
/// The reason is key 2's own
/// ambiguity: a spliced region is covered by the caller's snapshot at the
/// `invoke` pc, so every level of a NESTED splice reports one bci and
/// `from_rows` (correctly) poisons rows that disagree under it. Combined-pc
/// ranges are disjoint by construction, so they name one body and one nesting
/// exactly -- which is what key 1, the exact return address, then carries.
///
/// # The fail-closed rule
///
/// A missing or ambiguous chain must produce NO frame rather than a guessed
/// one. That is stricter than the rule for a line number, and for a stronger
/// reason: a wrong line is visibly a line, while an inlined frame naming the
/// wrong method is indistinguishable from a real one to the person reading the
/// trace. The emitter already refuses a row whose bci is not a spec-legal
/// bytecode index, and POISONS a safepoint id two program points disagree
/// about (`CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON=1` reverts that half alone)
/// — one bci covers a whole spliced region, so a splice containing two calls
/// with different chains cannot be told apart from the safepoint-id slot
/// alone, which is the innermost frame's only evidence. Both refusals arrive
/// here as an empty chain, which is exactly the pre-2026-09-01 behaviour.
pub(in crate::jit::conservative_roots) fn site_inline_chain_shared(
    cm: &cratonvm_jit::CompiledMethod,
    native_pc: Option<usize>,
    sp_id: Option<u32>,
    bci: i32,
) -> (Option<cratonvm_jit::x64::InlineChain>, bool) {
    if let Some(pc) = native_pc {
        let entry = cm.code_bytes().as_ptr() as usize;
        let end = entry.saturating_add(cm.code_len());
        let Some(off) = (entry != 0 && pc > entry && pc <= end).then(|| (pc - entry) as u32)
        else {
            return (None, false);
        };
        let site = cm.site_record(Some(off), None, None);
        return (site.inline_chain.cloned(), site.inline_chain_exact);
    }
    let bci_hint = (bci >= 0).then_some(bci as u32);
    let site = cm.site_record(None, sp_id, bci_hint);
    (site.inline_chain.cloned(), site.inline_chain_exact)
}

pub(crate) fn compiled_frame_inline_chain(
    cm: &cratonvm_jit::CompiledMethod,
    bci: i32,
    native_pc: Option<usize>,
    sp_id: Option<u32>,
) -> (Vec<InlinedLevel>, bool) {
    let (chain, exact) = site_inline_chain_shared(cm, native_pc, sp_id, bci);
    let levels = chain
        .map(|c| {
            c.iter()
                .map(|l| InlinedLevel {
                    label: l.label.clone(),
                    bci: l.bci,
                    class_id: l.class_id,
                    hidden: l.hidden,
                })
                .collect()
        })
        .unwrap_or_default();
    (levels, exact)
}

pub(crate) fn compiled_frame_inline_chain_shared(
    cm: &cratonvm_jit::CompiledMethod,
    bci: i32,
    native_pc: Option<usize>,
    sp_id: Option<u32>,
) -> (Option<cratonvm_jit::x64::InlineChain>, bool) {
    site_inline_chain_shared(cm, native_pc, sp_id, bci)
}

/// Give the INNERMOST frame of an NPE snapshot the line and the inlined
/// callees of the inline null check that raised.
///
/// An inline null check is not a GC-capable call, so it publishes no safepoint
/// id and [`activation_bci`] correctly refuses the stale one in the slot: the
/// frame that RAISED was the one frame in the snapshot with no line. The
/// emitter records what it knows at every such site
/// (`x64::inlining::record_npe_trap_site`) and the site's cold trampoline hands
/// its key to `jit_npe_with_action`; this is where the two meet.
///
/// `trap_key == 0` means the site was not described -- the feature switched
/// off, a bci out of spec range, a chain the emitter refused, or a helper that
/// signals an NPE without going through an inline check at all -- and leaves
/// the snapshot exactly as the walk built it.
///
/// Three things make a mismatched key harmless rather than wrong:
///
///   * keys are per-compile MONOTONIC ids, never indices, so one that belongs
///     to another artifact (or to a rewound splice) MISSES rather than landing
///     on a neighbouring site;
///   * the lookup is against the innermost frame's OWN artifact, which is the
///     one whose trampoline ran;
///   * both halves respect the switches the rest of this file does, so
///     `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` still produces `-1` here and
///     `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` still produces no callee frames.
///
/// `frames` is OUTERMOST-first (see [`active_compiled_frames`]), so the frame
/// that trapped is the last one.
pub fn apply_npe_trap_site(frames: &mut [ActiveCompiledFrame], trap_key: u32) {
    if trap_key == 0 {
        return;
    }
    let want_bci = compiled_frame_bci_enabled();
    let want_chains = inline_frame_chains_enabled();
    if !want_bci && !want_chains {
        return;
    }
    let Some(top) = frames.last_mut() else {
        return;
    };
    if top.cm_ptr == 0 {
        return;
    }
    // SAFETY: the same window and the same contract as `active_compiled_frames`
    // -- this runs inside the JIT helper that trapped, on the owning thread,
    // with the artifact still registered in the JIT cache and its chain entry
    // still pushed, and `cm_ptr` came from that walk moments ago.
    let cm = unsafe { &*(top.cm_ptr as *const cratonvm_jit::CompiledMethod) };
    let Some(site) = cm.npe_trap_map.get(trap_key) else {
        return;
    };
    if want_bci {
        if let Ok(bci) = i32::try_from(site.bci) {
            top.bci = bci;
            // The emitter recorded this bci AT the null check it describes, so
            // it names the trapping program point rather than the last
            // safepoint the frame passed. That is the difference
            // `jit_npe_message` needs before it will read the opcode there.
            top.trap_site_exact = true;
            cratonvm_jit::note_compiled_frame_line(cratonvm_jit::FRAME_LINE_ANSWERED_NPE_TRAP);
        }
    }
    if want_chains {
        top.inline_chain = site
            .chain
            .iter()
            .map(|l| InlinedLevel {
                label: l.label.clone(),
                bci: l.bci,
                class_id: l.class_id,
                // Wave 122 (L2): the resolver's `Method::is_hidden` stamp.
                hidden: l.hidden,
            })
            .collect();
        // The trap site is an EXACT program point -- the emitter recorded it at
        // the null check it is describing, not under a bci a whole spliced
        // region shares. So this chain is as authoritative as a return-address
        // one.
        top.chain_exact = true;
    }
}

/// Give the INNERMOST frame of a direct-throw snapshot (`/ by zero`, AIOOBE)
/// the bci its single-pass cold stub stamped.
///
/// The sibling of [`apply_npe_trap_site`] for the two direct-throw helpers,
/// and the fix for `r11w14-orch-implicit-exception-frame-reports-line-minus-one`:
/// `R11RtFibTraceLines` printed `lines -1 27 25 ...` in every JIT arm against
/// HotSpot's `22 27 25 ...`. The frame that RAISED is the one frame the walk
/// cannot place. A zero-divisor guard or a bounds check is not a GC-capable
/// call, so it publishes no safepoint id, and [`activation_bci`] reads the slot
/// as the frame left it: the prologue's `SP_ID_UNSET_BC_PC` in a frame that
/// has made no call yet (refused, `-1` -- the probe's innermost `rec(0, d)`),
/// or the id of the LAST call it made (a real, confidently wrong line). The
/// helpers' own comments claimed the walk "already has" the bci; it never did.
///
/// The stub knows the bci exactly: the reason-3 / reason-12 `/ by zero` stubs
/// stamp `orig_bci(site_pc)` through `jit_set_throw_bci` immediately before
/// `jit_throw_arithmetic`, and the AIOOBE pads pass the same value as
/// `jit_throw_aioobe`'s fourth argument. Both are the ENCLOSING method's own
/// interpreter bci: neither `emit_safe_idiv` nor `emit_bounds_check` is reachable
/// from the splice walk (`x64/arith.rs`, `x64/arrays.rs` say so and
/// `may_file_by_bci` is their insurance), so there is no inlined callee at that
/// program point and any chain the walk derived from a stale slot is dropped.
///
/// Refused (the snapshot is left exactly as the walk built it) unless every one
/// of these holds, because a bci put on the WRONG frame is a wrong line in a
/// different method, which is worse than `-1`:
///   * the value is a spec-legal bci (`-1` = nothing stamped);
///   * the innermost JIT chain entry is a PRECISE one and the top frame belongs
///     to it (`interp_depth`): a non-precise entry contributes no frame, so the
///     last frame would belong to an OUTER entry's method;
///   * the top frame's artifact is a single-pass one with a safepoint-id slot:
///     only the x64 single-pass stubs call these helpers, and `sp_id_slot_off !=
///     0` means it compiled under the precise gate, whose prologue publishes the
///     RBP mirror the walk found the top frame through. An optimizing-tier
///     artifact (whose zero-divisor and bounds guards deopt rather than call
///     here) or an aarch64 one (no slot) is a mismatch, not a frame to relabel.
///
/// `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` still produces `-1` here.
pub fn apply_direct_throw_trap_bci(frames: &mut [ActiveCompiledFrame], stamped_bci: i64) {
    if !compiled_frame_bci_enabled() {
        return;
    }
    // `try_borrow`: the same reason `active_compiled_frames` gives -- a capture
    // may be reached with the chain already borrowed, and refusing is safe.
    let innermost_precise_depth = JIT_ENTRY_CHAIN.with(|c| {
        let chain = c.try_borrow().ok()?;
        let innermost = chain.last()?;
        innermost
            .precise
            .is_some()
            .then_some(innermost.interp_depth)
    });
    let Some(depth) = innermost_precise_depth else {
        return;
    };
    let _ = stamp_direct_throw_bci(frames, stamped_bci, depth);}

/// [`apply_direct_throw_trap_bci`] for a compiled `athrow` on a null reference
/// (`jit_throw_exception`'s null arm; JIT round 20 wave 7, lane fuzzC; probe
/// `R20FuzzCAthrowNull1`): put the `athrow`'s own bci on the innermost frame.
///
/// Unlike the direct-throw helpers, BOTH tiers call `jit_throw_exception` with
/// the bci of the `athrow` they lower (the single-pass `0xbf` arm passes
/// `orig_bci(pc)`, the optimizing tier's `Op::Throw` its `resume_bci`), and
/// neither builds one inside a spliced body (the single-pass splice walk has no
/// `athrow` arm; the IR builder refuses one there), so the bci is the
/// enclosing artifact's own and an optimizing-tier frame is admitted too. An
/// `athrow` is not a published safepoint in either tier, so without this the
/// frame kept its LAST call's bci (a wrong line) and the JEP 358 message,
/// which reads the opcode at that bci, was dropped. The other refusals are
/// [`stamp_direct_throw_bci`]'s: an out-of-spec bci, a top frame of another
/// chain entry, an artifact with no safepoint-id slot (aarch64, non-precise).
pub fn apply_athrow_trap_bci(frames: &mut [ActiveCompiledFrame], athrow_bci: i64) {
    if !compiled_frame_bci_enabled() {
        return;
    }
    let innermost_precise_depth = JIT_ENTRY_CHAIN.with(|c| {
        let chain = c.try_borrow().ok()?;
        let innermost = chain.last()?;
        innermost
            .precise
            .is_some()
            .then_some(innermost.interp_depth)
    });
    let Some(depth) = innermost_precise_depth else {
        return;
    };
    let _ = stamp_throw_bci(frames, athrow_bci, depth, true);}

/// The pure half of [`apply_direct_throw_trap_bci`], split out for the unit
/// tests: `innermost_depth` is the innermost PRECISE chain entry's
/// `interp_depth`. Returns whether the top frame was relabelled.
pub(in crate::jit::conservative_roots) fn stamp_direct_throw_bci(
    frames: &mut [ActiveCompiledFrame],
    stamped_bci: i64,
    innermost_depth: u32,
) -> bool {
    stamp_throw_bci(frames, stamped_bci, innermost_depth, false)
}

/// The body of [`stamp_direct_throw_bci`]; `admit_ir` also accepts an
/// optimizing-tier artifact ([`apply_athrow_trap_bci`]).
pub(in crate::jit::conservative_roots) fn stamp_throw_bci(
    frames: &mut [ActiveCompiledFrame],
    stamped_bci: i64,
    innermost_depth: u32,
    admit_ir: bool,
) -> bool {
    let Some(bci) = u32::try_from(stamped_bci)
        .ok()
        .and_then(plausible_bci)
        .and_then(|v| i32::try_from(v).ok())
    else {
        return false;
    };
    let Some(top) = frames.last_mut() else {
        return false;
    };
    if top.cm_ptr == 0 || top.interp_depth != innermost_depth {
        return false;
    }
    // SAFETY: the same window and contract as `apply_npe_trap_site` -- this
    // runs inside the helper the trapping stub called, on the owning thread,
    // with the artifact registered and its chain entry pushed; `cm_ptr` came
    // from `active_compiled_frames` moments ago.
    let cm = unsafe { &*(top.cm_ptr as *const cratonvm_jit::CompiledMethod) };
    if (cm.used_ir_backend && !admit_ir) || cm.sp_id_slot_off == 0 {
        return false;
    }
    top.bci = bci;
    // The stub is in the method's own body (see above): no inlined callee
    // stands at this program point, whatever the stale slot suggested.
    top.inline_chain = Vec::new();
    top.chain_exact = false;
    // The "npe-trap" column counts every innermost frame placed from a trap
    // site the emitter described rather than from the safepoint-id slot.
    cratonvm_jit::note_compiled_frame_line(cratonvm_jit::FRAME_LINE_ANSWERED_NPE_TRAP);
    true
}


/// The active compiled frames of the CURRENT thread, outermost first, as
/// `(interpreter depth at entry, "class/Name.method:descriptor", owner class)`.
///
/// This is what makes a JIT-compiled method visible to `Thread.getStackTrace()`
/// and to every throwable's trace: compiled code pushes no interpreter frame,
/// so without it the Java-visible stack silently loses every method the JIT has
/// taken over. See [`JitFrameChainEntry::interp_depth`] for the H2 case that
/// found it.
///
/// Entries with no label are skipped rather than reported as an unnamed frame:
/// the only artifacts with an empty `method_label` are the legacy/test compile
/// wrapper's, and inventing a frame for one would be worse than omitting it.
pub fn active_compiled_frames() -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_from(0)
}

/// [`active_compiled_frames`] for the chain entries pushed at interpreter depth
/// `min_depth` or deeper: the same frames, in the same order, for exactly those
/// entries, and none of the walk (the RBP chain, the label and inline-chain
/// reads) for the shallower ones. `0` is the whole set.
///
/// SBR wave 6 (lane dispatch): the bounded caller walk
/// ([`crate::runtime::stackwalker::capture_tail_trace`]) reads the innermost few
/// logical frames of the stack and used to pay for every compiled activation
/// beneath them. An entry's frames depend on that entry alone (its own RBP
/// chain and safepoint slots), so dropping the outer entries drops exactly their
/// frames and changes no other.
pub fn active_compiled_frames_from(min_depth: u32) -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_bounded(min_depth, usize::MAX)
}

/// The compiled-frame walk's two switches, read once (JIT round 17 wave 2,
/// lane walk: one cache for both, so the second switch added no `static`).
#[derive(Clone, Copy)]
pub(in crate::jit::conservative_roots) struct CompiledWalkSwitches {
    /// `CRATONVM_CALLER_ACT_BOUND` (default on).
    pub(in crate::jit::conservative_roots) caller_act_bound: bool,
    /// `CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY` (default on).
    pub(in crate::jit::conservative_roots) whole_entry: bool,
    /// `CRATONVM_THROWABLE_WALK_TWO_WAY_MEMO` (default off; JIT round 19
    /// wave 1, lane throwfill, THROWS18-3): a walk-scoped walk remembers the
    /// last TWO return addresses it resolved, not one (`WalkLookups`).
    pub(in crate::jit::conservative_roots) two_way_return_memo: bool,
    /// JIT round 19 wave 2, lane throws2 (each default on; read here so the
    /// lane adds no static, `per_vm_state_statics_ratchet`; the readers are
    /// the `throws2_*` accessors below):
    /// `CRATONVM_THROWABLE_SHARED_INLINE_CHAINS` (THROWS18-2),
    pub(in crate::jit::conservative_roots) shared_inline_chains: bool,
    /// `CRATONVM_COMPILED_WALK_ACTIVATION_MEMO` (WALK17-4),
    pub(in crate::jit::conservative_roots) activation_memo: bool,
    /// `CRATONVM_THROWABLE_MEMO_MISS_FUSED` (THROWS19-8),
    pub(in crate::jit::conservative_roots) memo_miss_fused: bool,
    /// `CRATONVM_STACKWALK_LEAN_CAPTURE` (WALK17-3 step 1).
    pub(in crate::jit::conservative_roots) stackwalk_lean_capture: bool,
    /// JIT round 19 wave 4, lane throws4 (THROWS2-1, default off; additive,
    /// as the throws2 rows): `CRATONVM_WALK_SHARED_INLINE_CHAINS`.
    pub(in crate::jit::conservative_roots) walk_shared_inline_chains: bool,
    /// JIT round 20 wave 18, lane xWalk (WALK20-2, default ON, `=0` kill):
    /// `CRATONVM_STACKWALK_SHARED_ENTRY_NAMES`.
    pub(in crate::jit::conservative_roots) stackwalk_shared_entry_names: bool,
    /// JIT round 21 wave 1, lane walk (WALK21-8, default off):
    /// `CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE`.
    pub(in crate::jit::conservative_roots) method_slot_front_cache: bool,
    /// The stack-walker residual-costs page, item 3 (default ON, `=0` kill):
    /// `CRATONVM_STACKWALK_SCOPED_CODE_LOOKUPS`.
    pub(in crate::jit::conservative_roots) stackwalk_scoped_code_lookups: bool,
    /// The same page, item 3 (default ON, `=0` kill):
    /// `CRATONVM_STACKWALK_REPEAT_ENTRY_MEMO`.
    pub(in crate::jit::conservative_roots) stackwalk_repeat_entry_memo: bool,
}

pub(in crate::jit::conservative_roots) fn compiled_walk_switches() -> CompiledWalkSwitches {
    static ON: std::sync::OnceLock<CompiledWalkSwitches> = std::sync::OnceLock::new();
    *ON.get_or_init(|| CompiledWalkSwitches {
        caller_act_bound: true,
        whole_entry: true,
        two_way_return_memo: false,
        shared_inline_chains: true,
        activation_memo: false,
        memo_miss_fused: true,
        stackwalk_lean_capture: false,
        walk_shared_inline_chains: false,
        stackwalk_shared_entry_names: true,
        method_slot_front_cache: false,
        stackwalk_scoped_code_lookups: true,
        stackwalk_repeat_entry_memo: true,
    })
}

/// `CRATONVM_STACKWALK_SCOPED_CODE_LOOKUPS` (default ON, `=0` kill; the
/// stack-walker residual-costs page, item 3): the whole compiled walk of a
/// `StackWalker` / `Thread.getStackTrace` capture
/// (`stackwalker::whole_stack_compiled_frames`) resolves return addresses
/// through one [`cratonvm_jit::JitCodeRangeView`] and its last-address memo
/// ([`WalkLookups::walk_scoped`], what the throwable capture already uses),
/// instead of a registry lookup per activation. Every live activation's
/// return address lies in a body that stays registered while the frame is
/// live, so the view answers each exactly as the registry does. Read once per
/// process (no new static: this table).
pub(crate) fn stackwalk_scoped_code_lookups_enabled() -> bool {
    compiled_walk_switches().stackwalk_scoped_code_lookups
}

/// `CRATONVM_STACKWALK_REPEAT_ENTRY_MEMO` (default ON, `=0` kill; the
/// stack-walker residual-costs page, item 3): a capture builds a compiled
/// activation's entry once per run of activations of the same body at the
/// same bci (a compiled recursion), and clones it for the repeats
/// (`stackwalker::push_compiled_frames_repeating`). Read once per process.
pub(crate) fn stackwalk_repeat_entry_memo_enabled() -> bool {
    compiled_walk_switches().stackwalk_repeat_entry_memo
}

/// `CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE=1` (WALK21-8, JIT round 21 wave
/// 1, lane walk; opt-in, default OFF): `stackwalker::find_method_index_memoized`
/// asks a per-thread, lock-free front cache before the process-wide slot
/// memo's `RwLock`, so a capture's frames stop taking that shared lock (one
/// read acquisition per captured frame of every walk, `Thread.getStackTrace`
/// and throwable capture). Read once per process (no new static: this table).
pub(crate) fn r21_method_slot_front_cache_enabled() -> bool {
    compiled_walk_switches().method_slot_front_cache
}

/// `CRATONVM_STACKWALK_SHARED_ENTRY_NAMES` (WALK20-2, JIT round 20 wave 18,
/// lane xWalk; default ON, `=0` kill): a compiled activation's or an inlined
/// level's capture entry (`stackwalker::compiled_frame_entry` /
/// `inlined_frame_entry`) takes its class, method and descriptor names from
/// the class's own `Arc<str>`s (one reference count each) instead of copying
/// them into fresh allocations, and asks the method memo with the label's
/// slices instead of two more temporary copies. The same text.
pub(crate) fn r20_stackwalk_shared_entry_names_enabled() -> bool {
    compiled_walk_switches().stackwalk_shared_entry_names
}

/// `CRATONVM_WALK_SHARED_INLINE_CHAINS=1` (JIT round 19 wave 4, lane throws4,
/// THROWS2-1; default OFF): the whole walks of `Thread.getStackTrace` and
/// `StackWalker` (`stackwalker::whole_stack_compiled_frames`) ask for shared
/// inline chains ([`CompiledWalkOptions::shared_inline_chains`]), as the
/// throwable capture does under THROWS18-2.
pub(crate) fn throws4_walk_shared_inline_chains_enabled() -> bool {
    compiled_walk_switches().walk_shared_inline_chains
}

/// `CRATONVM_THROWABLE_SHARED_INLINE_CHAINS=1` (JIT round 19 wave 2, lane
/// throws2, THROWS18-2; default OFF): see [`CompiledWalkOptions::shared_inline_chains`].
pub(crate) fn throws2_shared_inline_chains_enabled() -> bool {
    compiled_walk_switches().shared_inline_chains
}

/// `CRATONVM_COMPILED_WALK_ACTIVATION_MEMO=1` (WALK17-4; default OFF): see
/// [`CompiledWalkOptions::activation_memo`].
pub(crate) fn throws2_activation_memo_enabled() -> bool {
    compiled_walk_switches().activation_memo
}

/// `CRATONVM_THROWABLE_MEMO_MISS_FUSED=1` (THROWS19-8; default OFF): read by
/// `stackwalker::memo_miss_fused_enabled`.
pub(crate) fn throws2_memo_miss_fused_enabled() -> bool {
    compiled_walk_switches().memo_miss_fused
}

/// `CRATONVM_STACKWALK_LEAN_CAPTURE=1` (WALK17-3 step 1; default OFF): read
/// by `stackwalker::stackwalk_lean_capture_enabled`.
pub(crate) fn throws2_stackwalk_lean_capture_enabled() -> bool {
    compiled_walk_switches().stackwalk_lean_capture
}

/// `CRATONVM_CALLER_ACT_BOUND` (default on): [`active_compiled_frames_bounded`]
/// honours its bound. `0`/`off` walks every activation, as before.
pub(in crate::jit::conservative_roots) fn caller_activation_bound_enabled() -> bool {
    compiled_walk_switches().caller_act_bound
}

/// The per-entry activation guard of the walk before JIT round 17 wave 2: the
/// RBP-chain loop stopped after this many parent steps.
pub(in crate::jit::conservative_roots) const LEGACY_WALK_ACTIVATION_GUARD: usize = 4096;

/// `CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY` (default ON; `=0` is the kill
/// switch). JIT round 17 wave 2, lane walk
/// (`r17-compiled-walk-4096-activation-guard-drops-middle-frames-FIXED-20261005.md`).
///
/// On, a chain entry's activations are read to the end of its RBP chain: the
/// walk is already bounded by the entry's own stack band (every step must
/// move strictly up, below `entry_sp`), so the fixed 4096-step guard only ever
/// cut a deep compiled recursion short -- silently, losing its middle frames
/// from `Thread.getStackTrace`, `StackWalker` and an uncapped throwable --
/// and then reported the boundary method with the INNERMOST activation's RBP,
/// i.e. a line read from another frame's safepoint-id slot. On, the loop's
/// guard is the band's capacity (one step per 8-byte word), a guard that fires
/// marks the entry truncated (no boundary pushed after a gap), and a boundary
/// method pushed after a walk that reported other activations gets no RBP
/// (its line is refused, `Unknown Source`, never read from a foreign slot).
///
/// Off, the walk is the pre-fix walk exactly: the 4096 guard, the boundary
/// with the innermost RBP.
pub(crate) fn compiled_walk_whole_entry_enabled() -> bool {
    compiled_walk_switches().whole_entry
}

/// The RBP-chain step guard for one chain entry whose band is
/// `[scanner_sp, entry_sp)`: the legacy 4096 when `whole_entry` is off; else
/// the most steps the band admits (each step moves strictly up to another
/// 8-aligned word inside it), plus one, which never fires before the chain
/// ends -- it only refuses a cycle, which that monotone test already does.
pub(in crate::jit::conservative_roots) fn compiled_walk_step_guard(whole_entry: bool, scanner_sp: usize, entry_sp: usize) -> usize {
    if whole_entry {
        entry_sp.saturating_sub(scanner_sp) / 8 + 1
    } else {
        LEGACY_WALK_ACTIVATION_GUARD
    }
}

/// The step guard of the COLLECTOR's own RBP-chain walks over one chain entry
/// (gch h1/jw, `r17-gc-compiled-chain-walks-stop-at-4096-activations-silently`):
/// the coverage proof (`refresh_moving_young_coverage_for_current_thread`), the
/// band verifier (`moving_young_unpublished_frame_oop_present`), the young pin
/// classification, the precise scan (`scan_precise_frames_and_bands`), both
/// remaps (`remap_active_jit_frames` Stage 5, `remap_register_image_words`) and
/// the three debug audits.
///
/// One chain entry covers every activation of a compiled recursion (compiled
/// code calling itself pushes no entry), and each of those walks used to stop
/// after a fixed 4096 parent steps WITHOUT A RECORD: the coverage proof and the
/// band verifier never looked at activation 4098 and beyond and still answered
/// "complete", and Stage 5 never rewrote those activations' oop slots. The
/// limit is now [`compiled_walk_step_guard`]'s -- the entry band's capacity,
/// which a chain whose every step moves strictly up inside the band cannot
/// exhaust -- under the same switch as the trace walk
/// (`CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY`, default on; `=0` keeps 4096).
///
/// Under either limit, [`Self::step`] answering `false` means the walk was CUT,
/// never that the chain ended: every caller that states coverage treats a cut
/// as incomplete (the fail-closed answer), and every caller that only scans
/// leaves the rest to its conservative fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct GcChainWalkGuard {
    pub(in crate::jit::conservative_roots) limit: usize,
    pub(in crate::jit::conservative_roots) steps: usize,
}

impl GcChainWalkGuard {
    /// The guard for one chain entry whose band is `[scanner_sp, entry_sp)`.
    pub(in crate::jit::conservative_roots) fn for_entry(scanner_sp: usize, entry_sp: usize) -> Self {
        Self::with_limit(compiled_walk_step_guard(
            compiled_walk_whole_entry_enabled(),
            scanner_sp,
            entry_sp,
        ))
    }

    pub(in crate::jit::conservative_roots) const fn with_limit(limit: usize) -> Self {
        Self { limit, steps: 0 }
    }

    /// Take one step. `false` once `limit` steps were taken: the walk is cut.
    #[inline]
    pub(in crate::jit::conservative_roots) fn step(&mut self) -> bool {
        if self.steps >= self.limit {
            return false;
        }
        self.steps += 1;
        true
    }

    /// The most steps this guard admits.
    #[inline]
    pub(in crate::jit::conservative_roots) const fn limit(&self) -> usize {
        self.limit
    }
}

/// Where a walk with a per-entry activation bound cut a chain entry short
/// (JIT round 17 wave 2, lane walk; [`active_compiled_frames_cut`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledWalkCut {
    /// The cut entry's `interp_depth` (its splice point).
    pub interp_depth: u32,
    /// The cut entry's index in the JIT entry chain.
    pub chain_index: u32,
    /// Index, in the walk's answer, of the cut entry's OUTERMOST reported
    /// activation: everything from here on in the answer (the entry's kept
    /// activations and every inner entry's) is exactly what an unbounded walk
    /// reports there; everything the cut removed lies outside it.
    pub first_reported: usize,
}

/// [`active_compiled_frames_bounded`] for a throwable capture's bounded walk
/// (JIT round 17 wave 2, lane walk, TF17-5; `CRATONVM_THROWABLE_BOUNDED_WALK=1`):
/// at most the innermost `max_activations` activations of each chain entry
/// (whatever `CRATONVM_CALLER_ACT_BOUND` says), with every entry the bound cut
/// short recorded in `cuts` (cleared first), outermost entry first. With no
/// cut the answer is [`active_compiled_frames`]' (or, `scoped`,
/// [`active_compiled_frames_walk_scoped`]'s) exactly. Exact only while
/// [`compiled_walk_whole_entry_enabled`]: the legacy guard cuts without a
/// record.
pub fn active_compiled_frames_cut(
    max_activations: usize,
    scoped: bool,
    cuts: &mut Vec<CompiledWalkCut>,
) -> Vec<ActiveCompiledFrame> {
    cuts.clear();
    active_compiled_frames_walk_with(0, max_activations.max(1), scoped, Some(cuts))
}


/// [`active_compiled_frames_from`] that reports at most the INNERMOST
/// `max_activations` compiled activations of each chain entry.
///
/// SBR round 2 (lane CALLER): compiled code calling itself pushes no further
/// chain entry, so one entry can hold every activation of a deep recursion, and
/// the caller-class walks -- which read only the innermost few logical frames --
/// paid the RBP walk, the safepoint-id read and the inline-chain lookup for all
/// of them (`Field.getInt` 3 us at depth 4, 10-25 us under a 20-deep compiled
/// recursion). Truncating drops the OUTERMOST activations of an entry, so the
/// entry's innermost frames -- the ones a tail wants -- are unchanged; a
/// truncated entry's boundary method (its outermost activation) is not pushed
/// back. A caller that needs more asks again with a larger bound (the tail
/// walks widen their window, and the bound with it, and end unbounded on the
/// whole stack).
pub fn active_compiled_frames_bounded(
    min_depth: u32,
    max_activations: usize,
) -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_walk(min_depth, max_activations, false)
}

/// [`active_compiled_frames`] with walk-scoped lookups (JIT round 16 wave 15,
/// lane throwfill; a throwable capture under
/// `CRATONVM_THROWABLE_WALK_SCOPED_LOOKUPS=1`): the same frames, from one
/// code-range snapshot for the whole walk, the previous return address's
/// answer reused, the artifacts' safepoint-id indexes, and the line census
/// written once at the end. See `WalkLookups`.
pub fn active_compiled_frames_walk_scoped() -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_walk(0, usize::MAX, true)
}

/// The per-activation lookups of one compiled-frame walk (JIT round 16 wave
/// 15, lane throwfill).
///
/// `per_call` is the walk as it always was: every return address is resolved
/// by its own [`cratonvm_jit::lookup_jit_code_range`] (an `arc_swap` load, two
/// binary searches, the release), every line verdict bumps the process-wide
/// census at once, and a frame's safepoint id is confirmed by scanning its
/// artifact's oop maps. `walk_scoped` answers the same questions the same way
/// for one walk of the calling thread's own stack:
///
/// * the code ranges come from ONE [`cratonvm_jit::JitCodeRangeView`] taken at
///   the start (sound for this walk: see that type), and an address equal to
///   the previous one asked -- every activation of a compiled recursion
///   returns to the same call site -- reuses its answer from the same view;
/// * the census is tallied here and written by [`Drop`], one update per
///   non-zero column ([`cratonvm_jit::note_compiled_frame_lines`]);
/// * the safepoint id is confirmed by
///   [`cratonvm_jit::CompiledMethod::has_oop_map_for_safepoint_id`], exactly
///   the scan's answer.
pub(in crate::jit::conservative_roots) struct WalkLookups {
    pub(in crate::jit::conservative_roots) scoped: bool,
    pub(in crate::jit::conservative_roots) view: Option<cratonvm_jit::JitCodeRangeView>,
    /// The last address resolved through `view`, and its answer.
    pub(in crate::jit::conservative_roots) last: Option<(usize, Option<usize>)>,
    /// JIT round 19 wave 1 (lane throwfill, THROWS18-3;
    /// `CRATONVM_THROWABLE_WALK_TWO_WAY_MEMO=1`): the address resolved before
    /// `last` (a different one), and its answer, so two return addresses
    /// taking turns -- two compiled methods calling each other down the
    /// stack -- are both answered from the memo. Unused (`None`) when off.
    pub(in crate::jit::conservative_roots) prev: Option<(usize, Option<usize>)>,
    /// Whether `prev` is consulted and kept (the switch, read at creation).
    pub(in crate::jit::conservative_roots) two_way: bool,
    /// The walk's census, written on drop.
    pub(in crate::jit::conservative_roots) census: [u64; cratonvm_jit::FRAME_LINE_SLOTS],
    /// JIT round 19 wave 2 (lane throws2, WALK17-4): the census column the
    /// last [`Self::note`] counted, so the per-walk activation memo can
    /// replay an activation's verdict on a hit ([`ActivationMemo`]).
    pub(in crate::jit::conservative_roots) last_noted: usize,
}

impl WalkLookups {
    pub(in crate::jit::conservative_roots) fn per_call() -> Self {
        Self {
            scoped: false,
            view: None,
            last: None,
            prev: None,
            two_way: false,
            census: [0; cratonvm_jit::FRAME_LINE_SLOTS],
            last_noted: 0,
        }
    }

    pub(in crate::jit::conservative_roots) fn walk_scoped() -> Self {
        Self::walk_scoped_with(compiled_walk_switches().two_way_return_memo)
    }

    /// [`Self::walk_scoped`] with the two-way memo decided by the caller
    /// (THROWS18-3; the tests drive both arms).
    pub(in crate::jit::conservative_roots) fn walk_scoped_with(two_way: bool) -> Self {
        Self {
            scoped: true,
            view: Some(cratonvm_jit::jit_code_range_view()),
            last: None,
            prev: None,
            two_way,
            census: [0; cratonvm_jit::FRAME_LINE_SLOTS],
            last_noted: 0,
        }
    }

    /// [`cratonvm_jit::lookup_jit_code_range`]'s answer for `addr`.
    ///
    /// Every memoised answer came from the same `view` the miss would ask, so
    /// a hit in either way is the view's answer for `addr` exactly.
    #[inline]
    pub(in crate::jit::conservative_roots) fn code_range(&mut self, addr: usize) -> Option<usize> {
        let Some(view) = self.view.as_ref() else {
            return cratonvm_jit::lookup_jit_code_range(addr);
        };
        if let Some((last, answer)) = self.last {
            if last == addr {
                return answer;
            }
        }
        if self.two_way {
            if let Some((prev, answer)) = self.prev {
                if prev == addr {
                    // Swap the ways so `last` is the most recent address:
                    // an alternation then hits `prev` on every frame.
                    self.prev = self.last;
                    self.last = Some((addr, answer));
                    return answer;
                }
            }
        }
        let answer = view.lookup(addr);
        if self.two_way {
            self.prev = self.last;
        }
        self.last = Some((addr, answer));
        answer
    }

    /// [`cratonvm_jit::note_compiled_frame_line`], now or at the walk's end.
    #[inline]
    pub(in crate::jit::conservative_roots) fn note(&mut self, slot: usize) {
        self.last_noted = slot;
        if !self.scoped {
            cratonvm_jit::note_compiled_frame_line(slot);
        } else if let Some(n) = self.census.get_mut(slot) {
            *n += 1;
        }
    }

    /// Count a census SUB-column (`cratonvm_jit::FRAME_LINE_INNERMOST_UNRECORDED`)
    /// beside the verdict [`Self::note`] already counted: the verdict a memo
    /// replays (`last_noted`) is unchanged. JIT round 19 proposal wave 7, lane
    /// gc7 (OSRFRAME18-1).
    #[inline]
    pub(in crate::jit::conservative_roots) fn note_sub(&mut self, slot: usize) {
        if !self.scoped {
            cratonvm_jit::note_compiled_frame_line(slot);
        } else if let Some(n) = self.census.get_mut(slot) {
            *n += 1;
        }
    }
}

/// OSRFRAME18-1 (JIT round 19 proposal wave 7, lane gc7): is a frame the walk
/// read through the innermost-frame MIRROR's RBP (it has an RBP and no return
/// address of its own: the innermost activation of a chain entry, or the
/// boundary fallback read from the same mirror) whose line was refused because
/// its safepoint-id slot names no safepoint of the artifact -- the signature of
/// a DEAD innermost frame (one an emitter tore down before reaching the VM)?
/// A live innermost frame's slot is current by contract; a non-innermost one
/// was placed by a return address and is a different population.
pub(in crate::jit::conservative_roots) fn innermost_unrecorded_refusal(
    frame_rbp: Option<usize>,
    native_pc: Option<usize>,
    census: usize,
) -> bool {
    frame_rbp.is_some()
        && native_pc.is_none()
        && census == cratonvm_jit::FRAME_LINE_REFUSED_ID_UNRECORDED
}

impl Drop for WalkLookups {
    fn drop(&mut self) {
        if self.scoped {
            cratonvm_jit::note_compiled_frame_lines(&self.census);
        }
    }
}

/// The body of [`active_compiled_frames_bounded`] and
/// [`active_compiled_frames_walk_scoped`]; `scoped` picks the
/// [`WalkLookups`].
pub(in crate::jit::conservative_roots) fn active_compiled_frames_walk(
    min_depth: u32,
    max_activations: usize,
    scoped: bool,
) -> Vec<ActiveCompiledFrame> {
    let max_activations = if caller_activation_bound_enabled() {
        max_activations.max(1)
    } else {
        usize::MAX
    };
    active_compiled_frames_walk_with(min_depth, max_activations, scoped, None)
}

/// [`active_compiled_frames_walk`] with the bound already decided, recording
/// each entry the bound cut short in `cuts` when asked (JIT round 17 wave 2,
/// lane walk; [`active_compiled_frames_cut`]).
pub(in crate::jit::conservative_roots) fn active_compiled_frames_walk_with(
    min_depth: u32,
    max_activations: usize,
    scoped: bool,
    cuts: Option<&mut Vec<CompiledWalkCut>>,
) -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_walk_recording(min_depth, max_activations, scoped, cuts, None)
}

/// [`active_compiled_frames_walk_with`], recording what the walk read into
/// `record` when asked ([`RawWalkKey`]).
pub(in crate::jit::conservative_roots) fn active_compiled_frames_walk_recording(
    min_depth: u32,
    max_activations: usize,
    scoped: bool,
    cuts: Option<&mut Vec<CompiledWalkCut>>,
    record: Option<&mut RawWalkKey>,
) -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_walk_recording_opts(
        min_depth,
        max_activations,
        scoped,
        cuts,
        record,
        CompiledWalkOptions::default(),
    )
}

/// [`active_compiled_frames_walk_recording`] with the round-19 wave-2 walk
/// options ([`CompiledWalkOptions`]).
pub(in crate::jit::conservative_roots) fn active_compiled_frames_walk_recording_opts(
    min_depth: u32,
    max_activations: usize,
    scoped: bool,
    cuts: Option<&mut Vec<CompiledWalkCut>>,
    record: Option<&mut RawWalkKey>,
    opts: CompiledWalkOptions,
) -> Vec<ActiveCompiledFrame> {
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        // The top entry's `exact_rbp` lives in the `TOP_RBP` mirror between
        // push/pop boundaries; the walk below needs the LIVE innermost RBP, so
        // flush it exactly as `remap_active_jit_frames` does. `try_borrow_mut`
        // rather than `borrow_mut`: a stack capture is reachable from paths
        // that may already hold the chain borrow, and a stale (higher)
        // `exact_rbp` only shortens the walk — it degrades this function to the
        // answer it gave before, and never walks past a bound.
        if nested_trace_frames_enabled() {
            if let Ok(mut v) = c.try_borrow_mut() {
                flush_top_rbp_cache_to_chain(v.as_mut_slice());
            }
        }
        let chain = c.borrow();
        let Some(record) = record else {
            return walk_jit_chain_opts(
                &chain,
                scanner_sp,
                min_depth,
                max_activations,
                scoped,
                cuts,
                None,
                opts,
            );
        };
        record.clear();
        // JIT round 21 wave 10 (lane splice): read BEFORE the walk. A
        // mutation the walk raced (a publication, or a not-entrant stub's
        // first forward, which the innermost fallback answers through) then
        // leaves the key on the older generation, and the replay misses. Read
        // after the walk, a bump between the walk's read and this one was
        // recorded as if the walk had seen it.
        let generation = cratonvm_jit::jit_cache_generation();
        let walk = walk_jit_chain_opts(
            &chain,
            scanner_sp,
            min_depth,
            max_activations,
            scoped,
            cuts,
            Some(&mut *record),
            opts,
        );
        record.generation = generation;
        record.hi = chain.iter().map(|e| e.entry_sp).max().unwrap_or(0);
        let RawWalkKey { words, plan, .. } = &mut *record;
        let recorded = plan.len() == chain.len()
            && raw_walk_pass(
                &chain,
                scanner_sp,
                plan,
                &mut |w| {
                    words.push(w);
                    true
                },
                &mut |_| true,
            );
        if !recorded {
            record.clear();
        }
        walk
    })
}

/// JIT 2026-10-07 (throwfill retirement): what one walk of the calling
/// thread's compiled frames READ, so that a later walk can be shown to read
/// the same words -- and so to answer the same frames -- without resolving a
/// single code range, safepoint id or inline chain
/// ([`raw_walk_key_matches`]). A throwable capture memo keys on it
/// (`stackwalker::ThrowableCaptureMemo`): a throw loop's hit skips the walk.
///
/// The walk's answer is a function of exactly these inputs:
/// * the thread's JIT entry chain (each entry's depth, entry SP and precise
///   frame record, and whether its boundary body is not entrant);
/// * the saved-RBP chain words and return addresses each entry's walk reads
///   ([`raw_walk_pass`] reads a superset: it follows the chain to its bounds,
///   past the first return address the walk resolves to no body);
/// * what each return address resolves to, each body's oop maps, safepoint
///   tables and inline-frame map -- all fixed while the process-wide JIT cache
///   generation is (every publication, replacement and withdrawal moves it);
/// * for an innermost frame the trace fallback decoded through a not-entrant
///   callee (`trace_innermost_by_caller_call`), whether that callee's stub
///   ever forwarded -- keyed through the generation too: a record's first
///   forward box moves it (`jit/src/not_entrant.rs`, `install_forward`);
/// * each activation's safepoint-id slot (`slots`: address and word).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawWalkKey {
    pub(in crate::jit::conservative_roots) generation: u64,
    pub(in crate::jit::conservative_roots) words: Vec<usize>,
    pub(in crate::jit::conservative_roots) slots: Vec<(usize, usize)>,
    /// Per chain entry: the RBP-chain reads the walk made, and whether the
    /// last one ended at a return address resolving to no compiled body --
    /// the interpreter / Rust boundary, whose return address varies with the
    /// call path into compiled code and is checked by one lookup, not keyed.
    pub(in crate::jit::conservative_roots) plan: Vec<(usize, bool)>,
    /// The highest entry SP of the chain: the bound of the slot replays.
    pub(in crate::jit::conservative_roots) hi: usize,
}

impl RawWalkKey {
    pub(in crate::jit::conservative_roots) fn clear(&mut self) {
        self.generation = 0;
        self.words.clear();
        self.slots.clear();
        self.plan.clear();
        self.hi = 0;
    }

    /// Did the recording walk record anything to compare?
    pub(crate) fn is_recorded(&self) -> bool {
        !self.words.is_empty()
    }
}

/// The address of `cm`'s safepoint-id slot in the frame at `rbp`, exactly
/// where [`safepoint_id_slot`] reads it, or `None` where it reads nothing.
pub(in crate::jit::conservative_roots) fn safepoint_id_slot_addr(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<usize> {
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        return None;
    }
    let id_addr = rbp.checked_sub(sp_id_off as usize)?;
    (id_addr & 0x7 == 0).then_some(id_addr)
}

/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-1): the reference word
/// `[frame.frame_rbp - rbp_off]` of a live compiled activation of the CALLING
/// thread (a walk this thread just made, its frames unchanged since), or
/// `None`.
///
/// Read ONLY where the frame's ACTIVE oop maps -- the maps of the id in its
/// safepoint-id slot, the very maps the collector's precise half reads and
/// rewrites ([`active_maps`]) -- ALL name `rbp_off` as a live reference. That
/// is the soundness argument: such a word is a precise root, so a moving
/// collection rewrote it in place and the value read is the object's current
/// address. Everything else refuses: no frame pointer, a frame that holds no
/// references, a trace-only or unset id, an id no map is recorded at, a map
/// that does not name the word, an offset that is not a positive aligned
/// frame offset, or an address below this thread's stack pointer (no word of
/// a live caller frame lies there). A null word answers `None`.
///
/// The answer is an address, never dereferenced here, and its callers only
/// COMPARE it (with the thread's lock stack, under the same no-safepoint
/// window the walk ran in): a stale or foreign word that slipped every check
/// can at most fail to match.
pub(crate) fn compiled_frame_reference_word(
    frame: &ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    rbp_off: i32,
) -> Option<usize> {
    compiled_frame_reference_word_by(frame, cm, rbp_off, FrameWordReader::Own)
}

/// Whose stack a compiled activation's frame words are read from, which
/// decides the lower bound of the read (JIT round 19 proposal wave 11, lane
/// gc11, GC920-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameWordReader {
    /// The CALLING thread's own activation (a walk it just made): the floor is
    /// the reader's stack pointer, as since wave 7.
    Own,
    /// An activation of ANOTHER thread that a frame-trace pause holds excused
    /// in a native it entered under compiled code
    /// (`gc_and_alloc::publish_excused_native_traces`, `NX_COMPILED`; walked
    /// from a copy of its chain, `JitChainSnapshot::of_held_thread`). The
    /// reader's stack pointer says nothing about that stack, so the floor is
    /// the activation's own band `[rbp - frame_size, rbp)`
    /// (`CompiledMethod::frame_layout`): the read stays inside the frame the
    /// map describes, which the held thread neither leaves nor writes while
    /// the pause holds it (it runs no Java and no compiled code). A body that
    /// published no frame size reads nothing.
    HeldThread,
}

/// [`compiled_frame_reference_word`] with the reader named
/// ([`FrameWordReader`]): the same checks (every active map names the word)
/// over the reader's floor.
pub(crate) fn compiled_frame_reference_word_by(
    frame: &ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    rbp_off: i32,
    reader: FrameWordReader,
) -> Option<usize> {
    let rbp = frame.frame_rbp?;
    let floor = match reader {
        FrameWordReader::Own => current_stack_pointer(),
        FrameWordReader::HeldThread => held_thread_frame_floor(rbp, cm, rbp_off)?,
    };
    reference_word_named_by_active_maps(rbp, cm, rbp_off, floor)
}

/// GC920-4 (round 19 wave 11, lane gc11): the lowest address a held thread's
/// frame word at `[rbp - rbp_off]` may be read from -- the bottom of the
/// activation's own band, `rbp - frame_size` -- or `None` when the body
/// published no frame size or the offset lies outside the band.
pub(in crate::jit::conservative_roots) fn held_thread_frame_floor(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    rbp_off: i32,
) -> Option<usize> {
    let size = cm.frame_layout.frame_size;
    if size <= 0 || rbp_off <= 0 || rbp_off > size {
        return None;
    }
    rbp.checked_sub(usize::try_from(size).ok()?)
}

/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-1): the lock objects of
/// the `synchronized` BLOCKS a live SINGLE-PASS activation of the calling
/// thread holds where it stands, OUTERMOST first, as addresses (compared,
/// never dereferenced, by the caller).
///
/// The blocks are the artifact's own description at the frame's active
/// safepoint id (`CompiledMethod::taken_monitor_locals_at`, every path agreeing
/// and every level a taken lock naming its local); each lock object is its
/// local's home `[rbp - 8 * (k + 1)]`, read through
/// [`compiled_frame_reference_word`] (so only where the active map names that
/// home). All or nothing: a local held in a register
/// (`osr_local_assignments`) or any one word refused answers an empty list.
///
/// JIT round 19 proposal wave 8, lane gc8 (GC719-1, `ir_homes`, the switch
/// `CRATONVM_JMX_IR_FRAME_MONITOR_HOMES` its caller read): an OPTIMIZING-tier
/// activation's blocks are the frame words its artifact published for the
/// active id (`CompiledMethod::ir_monitor_homes_at`, each lock object's
/// reference home), read through the same [`compiled_frame_reference_word`],
/// all or nothing ([`ir_block_lock_words_at`]). With `ir_homes` off, an
/// optimizing-tier body answers nothing, as in wave 7.
///
/// A home is current after a moving collection in BOTH IR publication modes
/// (GC819-1, round 19 wave 9, lane gc9, settled by reading, nothing built):
/// under a frame block the collector rewrites it through its indirect entry;
/// under the per-call shadow push (value copies) every remap path
/// (`memory/gc.rs` `update_all_roots`, `gc_and_alloc.rs`'s resuming-thread
/// remap, `vm_exec.rs`'s blocked-wake remap) runs [`remap_active_jit_frames`]
/// first, whose [`remap_one_jit_frame`] rewrites every word the active maps
/// name in place unless the shadow stack owns it BY ADDRESS, and a value copy
/// owns nothing; the copy-back after the call stores the same new address.
/// So no read through the shadow copy is needed.
pub(crate) fn compiled_frame_block_lock_words(
    frame: &ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    ir_homes: bool,
) -> Vec<usize> {
    compiled_frame_block_lock_words_by(frame, cm, ir_homes, FrameWordReader::Own)
}

/// [`compiled_frame_block_lock_words`] with the reader named (JIT round 19
/// proposal wave 11, lane gc11, GC920-4: a held thread's activation reads
/// within its own band, [`FrameWordReader::HeldThread`]).
pub(crate) fn compiled_frame_block_lock_words_by(
    frame: &ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    ir_homes: bool,
    reader: FrameWordReader,
) -> Vec<usize> {
    let ir = cm.used_ir_backend;
    let described = if ir {
        ir_homes && !cm.ir_monitor_homes.is_empty()
    } else {
        !cm.taken_monitor_slots.is_empty()
    };
    if !described {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[JMX_BLOCK_WORDS] {} ir={} undescribed (taken={} homes={})",
                frame.label,
                ir,
                cm.taken_monitor_slots.len(),
                cm.ir_monitor_homes.len()
            );
        }
        return Vec::new();
    }
    let Some(rbp) = frame.frame_rbp else {
        return Vec::new();
    };
    let active = active_maps(rbp, cm);
    let Some(sp_id) = active.sp_id.filter(|_| !active.trace_only) else {
        return Vec::new();
    };
    let read = |off| compiled_frame_reference_word_by(frame, cm, off, reader);
    let out = if ir {
        ir_block_lock_words_at(cm, sp_id, read)
    } else {
        block_lock_words_at(cm, sp_id, register_local_block_holds_enabled(), read)
    };
    if crate::runtime::env_cache::frame_trace() {
        gc9_dbg_block_lock_words(frame, cm, rbp, sp_id, &out);
    }
    out
}

/// Round 19 wave 9 (lane gc9): under `CRATONVM_FRAME_TRACE=1`, what
/// [`compiled_frame_block_lock_words`] read for one activation and why.
#[cold]
pub(in crate::jit::conservative_roots) fn gc9_dbg_block_lock_words(
    frame: &ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    rbp: usize,
    sp_id: u32,
    out: &[usize],
) {
    let active = active_maps(rbp, cm);
    let maps: Vec<&[i16]> = active
        .maps(cm)
        .map(|m| m.frame_slot_offsets.as_slice())
        .collect();
    eprintln!(
        "[JMX_BLOCK_WORDS] {} ir={} sp_id={sp_id} locals={:?} homes={:?} table={:?} maps={maps:?} regs={:?} out={out:x?}",
        frame.label,
        cm.used_ir_backend,
        cm.taken_monitor_locals_at(sp_id),
        cm.ir_monitor_homes_at(sp_id),
        cm.taken_monitor_slots.iter().map(|(p, l)| (*p, l.to_vec())).collect::<Vec<_>>(),
        cm.osr_local_assignments,
    );
}

/// `CRATONVM_JMX_REGISTER_LOCAL_BLOCK_HOLDS`, **default ON** (JIT round 19
/// proposal wave 9, lane gc9; a HotSpot-parity fix found by the orchestrator's
/// `R19Sync6LockedDepths` run, `=0` is the kill switch): a single-pass
/// activation's `synchronized` block whose lock local lives in a register is
/// read from the local's canonical slot ([`block_lock_words_at`]). Off: such
/// a block names nothing and its monitor reports at the innermost frame, as
/// in waves 7-8. Read per activation on the cold JMX/JVMTI paths.
pub(crate) fn register_local_block_holds_enabled() -> bool {
    true
}

/// GC719-1 (round 19 wave 8, lane gc8): [`compiled_frame_block_lock_words`]
/// of an OPTIMIZING-tier body over a resolved safepoint id: each published
/// home (`CompiledMethod::ir_monitor_homes_at`, outermost first) through the
/// frame-word reader `read` (a unit test passes a heap image's). All or
/// nothing: one refused or null word answers an empty list.
pub(in crate::jit::conservative_roots) fn ir_block_lock_words_at(
    cm: &cratonvm_jit::CompiledMethod,
    sp_id: u32,
    read: impl Fn(i32) -> Option<usize>,
) -> Vec<usize> {
    let homes = cm.ir_monitor_homes_at(sp_id);
    let mut out = Vec::with_capacity(homes.len());
    for &off in homes {
        match read(off) {
            Some(word) => out.push(word),
            None => return Vec::new(),
        }
    }
    out
}

/// [`compiled_frame_block_lock_words`] over a resolved safepoint id, with the
/// frame-word reader `read` a parameter (a unit test passes a heap image's).
///
/// `register_homes` ([`register_local_block_holds_enabled`], JIT round 19
/// wave 9, lane gc9): a lock local the single-pass allocator homes in a
/// REGISTER is read from its canonical slot too. Sound for the same reason
/// the precise collector may rewrite that slot: every safepoint's pre-call
/// spill (`x64/safepoint.rs` `emit_pre_safepoint_spill_impl`, "Publish
/// register-homed locals to their canonical slots") stores each
/// register-homed reference local there before the call, and the map names
/// the slot only then (`read` still requires every active map to name it).
/// Off, such a local refuses the whole list, as in waves 7-8 -- which on the
/// probe's shapes was EVERY single-pass block hold (`coldBlock`,
/// `nestedBlocks`, `thisBlock`, `tCold`: their lock locals all live in
/// callee-saved registers), so the monitor fell to the innermost frame.
pub(in crate::jit::conservative_roots) fn block_lock_words_at(
    cm: &cratonvm_jit::CompiledMethod,
    sp_id: u32,
    register_homes: bool,
    read: impl Fn(i32) -> Option<usize>,
) -> Vec<usize> {
    let locals = cm.taken_monitor_locals_at(sp_id);
    let mut out = Vec::with_capacity(locals.len());
    for &k in locals {
        let in_register = !register_homes
            && cm
                .osr_local_assignments
                .as_ref()
                .is_some_and(|regs| regs.get(usize::from(k)).is_some_and(Option::is_some));
        let off = (i32::from(k) + 1) * 8;
        match (!in_register).then(|| read(off)).flatten() {
            Some(word) => out.push(word),
            None => return Vec::new(),
        }
    }
    out
}

/// Interpreter round i6 wave 121, lane L9: [`compiled_frame_reference_word`]'s
/// checks, but a NULL word the active maps name answers `Some(0)` instead of
/// `None`, so a reader can tell "a live reference slot holding null" from "a
/// word no map vouches for". For the debugger's home-slot locals
/// (`interpreter::read_compiled_homes`, `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`),
/// which DEREFERENCE a non-null answer, and must not list a null for a word
/// no map names (a stale home under a register-held local).
///
/// `reader` as in [`compiled_frame_reference_word_by`].
#[cfg(feature = "experimental-debug")]
pub(crate) fn compiled_frame_reference_slot_by(
    frame: &ActiveCompiledFrame,
    cm: &cratonvm_jit::CompiledMethod,
    rbp_off: i32,
    reader: FrameWordReader,
) -> Option<usize> {
    let rbp = frame.frame_rbp?;
    let floor = match reader {
        FrameWordReader::Own => current_stack_pointer(),
        FrameWordReader::HeldThread => held_thread_frame_floor(rbp, cm, rbp_off)?,
    };
    reference_slot_named_by_active_maps(rbp, cm, rbp_off, floor)
}

/// [`compiled_frame_reference_word`]'s checks and read, with the stack floor
/// `sp_floor` (the reader's stack pointer) a parameter so a unit test can run
/// it over a heap-backed frame image (floor `0`).
pub(in crate::jit::conservative_roots) fn reference_word_named_by_active_maps(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    rbp_off: i32,
    sp_floor: usize,
) -> Option<usize> {
    reference_slot_named_by_active_maps(rbp, cm, rbp_off, sp_floor).filter(|&word| word != 0)
}

/// [`reference_word_named_by_active_maps`] that answers a named null word as
/// `Some(0)` (wave 121, lane L9; [`compiled_frame_reference_slot_by`]).
pub(in crate::jit::conservative_roots) fn reference_slot_named_by_active_maps(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    rbp_off: i32,
    sp_floor: usize,
) -> Option<usize> {
    let off16 = i16::try_from(rbp_off)
        .ok()
        .filter(|&o| o > 0 && o % 8 == 0)?;
    if rbp & 0x7 != 0 {
        return None;
    }
    let addr = rbp.checked_sub(usize::try_from(off16).ok()?)?;
    if addr < sp_floor {
        return None;
    }
    let active = active_maps(rbp, cm);
    if active.trace_only || active.idx.is_empty() {
        return None;
    }
    if !active
        .maps(cm)
        .all(|map| map.frame_slot_offsets.contains(&off16))
    {
        return None;
    }
    // SAFETY: an aligned word of a live compiled frame (`rbp` came from a walk
    // bounded inside the owner's JIT stack band; `addr` lies between the
    // floor -- the reader's own stack pointer, or for a HELD thread's frame the
    // bottom of that frame's band, `FrameWordReader::HeldThread` -- and that
    // RBP), at an offset the frame's active map records as a live reference
    // slot.
    let word = unsafe { (addr as *const usize).read() };
    Some(word)
}

/// The raw words a walk of `chain` read, handed to `emit` in a fixed order;
/// `false` as soon as `emit` refuses one, or `not_code` one entry's last
/// return address, or the chain no longer allows a read the `plan` made.
/// `plan` (one row per chain entry, [`RawWalkKey::plan`]) bounds each
/// entry's reads to the walk's. Reads only this thread's own stack band,
/// under the walk's own bounds (aligned, inside `[scanner_sp, entry_sp)`,
/// strictly increasing RBPs).
pub(in crate::jit::conservative_roots) fn raw_walk_pass(
    chain: &[JitFrameChainEntry],
    scanner_sp: usize,
    plan: &[(usize, bool)],
    emit: &mut dyn FnMut(usize) -> bool,
    not_code: &mut dyn FnMut(usize) -> bool,
) -> bool {
    if plan.len() != chain.len() {
        return false;
    }
    for (e, &(reads, ended_not_code)) in chain.iter().zip(plan) {
        if !emit(e.interp_depth as usize) || !emit(e.entry_sp) {
            return false;
        }
        let Some(info) = e.precise else {
            if !emit(0) {
                return false;
            }
            continue;
        };
        // SAFETY: the chain entry's body is Arc-owned by the JIT cache while
        // the entry is on the chain (`PreciseFrameInfo::compiled_method`).
        let not_entrant =
            !info.compiled_method.is_null() && unsafe { (*info.compiled_method).is_not_entrant() };
        for w in [
            1,
            info.exact_rbp,
            info.exact_cm_id as usize,
            info.compiled_method as usize,
            info.not_entrant_ret_pc,
            usize::from(not_entrant),
        ] {
            if !emit(w) {
                return false;
            }
        }
        let entry_sp = e.entry_sp;
        let mut child_rbp = info.exact_rbp;
        for read in 0..reads {
            if child_rbp == 0
                || child_rbp & 0x7 != 0
                || child_rbp < scanner_sp
                || child_rbp >= entry_sp
            {
                return false;
            }
            // SAFETY: as in `walk_jit_chain`: an aligned address inside this
            // thread's own live JIT stack band, bounded by `scanner_sp` and
            // `entry_sp`.
            let parent_rbp = unsafe { (child_rbp as *const usize).read() };
            let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
            if !emit(parent_rbp) {
                return false;
            }
            if ended_not_code && read + 1 == reads {
                if !not_code(ret_addr) {
                    return false;
                }
            } else if !emit(ret_addr) {
                return false;
            }
            child_rbp = parent_rbp;
        }
        if !emit(!reads) {
            return false;
        }
    }
    emit(chain.len())
}

/// Would a compiled-frame walk of the calling thread now read exactly what
/// the one that recorded `key` read (and so answer the same frames)?
pub(crate) fn raw_walk_key_matches(key: &RawWalkKey) -> bool {
    if !key.is_recorded() || cratonvm_jit::jit_cache_generation() != key.generation {
        return false;
    }
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        if nested_trace_frames_enabled() {
            if let Ok(mut v) = c.try_borrow_mut() {
                flush_top_rbp_cache_to_chain(v.as_mut_slice());
            }
        }
        let chain = c.borrow();
        let mut recorded = key.words.iter().copied();
        raw_walk_pass(
            &chain,
            scanner_sp,
            &key.plan,
            &mut |w| recorded.next() == Some(w),
            &mut |ret| cratonvm_jit::lookup_jit_code_range(ret).is_none(),
        ) && recorded.next().is_none()
            && key.slots.iter().all(|&(addr, word)| {
                addr >= scanner_sp
                    && addr < key.hi
                    // SAFETY: an aligned slot inside this thread's live JIT
                    // band (bounded above), in the frame the equal chain
                    // words name.
                    && unsafe { (addr as *const usize).read() } == word
            })
    })
}

/// Why [`raw_walk_key_matches`] answers `false` (`None` when it answers
/// `true`): `gen`, `word N` (the first recorded word that differs, or the
/// count when the lengths do), `slot N`. For `CRATONVM_DBG_CAPTURE_MEMO`.
pub(crate) fn raw_walk_key_mismatch(key: &RawWalkKey) -> Option<String> {
    if !key.is_recorded() {
        return Some("unrecorded".to_string());
    }
    let generation = cratonvm_jit::jit_cache_generation();
    if generation != key.generation {
        return Some(format!("gen {} -> {generation}", key.generation));
    }
    let scanner_sp = current_stack_pointer();
    JIT_ENTRY_CHAIN.with(|c| {
        if nested_trace_frames_enabled() {
            if let Ok(mut v) = c.try_borrow_mut() {
                flush_top_rbp_cache_to_chain(v.as_mut_slice());
            }
        }
        let chain = c.borrow();
        let mut recorded = key.words.iter().copied();
        let mut index = 0usize;
        let mut differs: Option<(usize, usize)> = None;
        let mut boundary_is_code = None;
        let same = raw_walk_pass(
            &chain,
            scanner_sp,
            &key.plan,
            &mut |w| {
                let ok = recorded.next() == Some(w);
                if !ok {
                    differs = Some((index, w));
                }
                index += 1;
                ok
            },
            &mut |ret| {
                let not_code = cratonvm_jit::lookup_jit_code_range(ret).is_none();
                if !not_code {
                    boundary_is_code = Some(ret);
                }
                not_code
            },
        );
        if let Some(ret) = boundary_is_code {
            return Some(format!("boundary {ret:#x} is compiled code now"));
        }
        if !same {
            let (at, now) = differs.unwrap_or((index, 0));
            return Some(format!(
                "word {at} of {}: {:#x} -> {now:#x}",
                key.words.len(),
                key.words.get(at).copied().unwrap_or(0)
            ));
        }
        if recorded.next().is_some() {
            return Some(format!("words {index} of {}", key.words.len()));
        }
        key.slots
            .iter()
            .position(|&(addr, word)| {
                !(addr >= scanner_sp
                    && addr < key.hi
                    // SAFETY: an aligned slot inside this thread's live JIT
                    // band (bounded above), in the frame the equal chain
                    // words name.
                    && unsafe { (addr as *const usize).read() } == word)
            })
            .map(|i| format!("slot {i} of {}", key.slots.len()))
    })
}

/// [`active_compiled_frames_walk_scoped`] (or the per-call walk), recording
/// the walk's raw key into `record` ([`RawWalkKey`]; left unrecorded when the
/// walk could not be).
pub(crate) fn active_compiled_frames_walk_recorded(
    max_activations: usize,
    scoped: bool,
    record: &mut RawWalkKey,
) -> Vec<ActiveCompiledFrame> {
    // The bounded form is [`active_compiled_frames_cut`]'s walk (its cuts
    // unread by a throwable capture); unbounded, the plain walk's.
    let mut cuts = Vec::new();
    let cuts = (max_activations != usize::MAX).then_some(&mut cuts);
    active_compiled_frames_walk_recording(0, max_activations.max(1), scoped, cuts, Some(record))
}

/// JIT round 19 wave 2 (lane throws2): how a compiled-frame walk builds what
/// it reports, beyond the walk's own switches. The default is the walk as it
/// always was; each option answers the SAME frames (the same bcis, the same
/// chains, the same census verdicts) by a cheaper route.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompiledWalkOptions {
    /// THROWS18-2 (`CRATONVM_THROWABLE_SHARED_INLINE_CHAINS=1`, asked by the
    /// throwable capture only): each activation's inline chain is the
    /// artifact's own shared chain ([`ActiveCompiledFrame::inline_chain_shared`])
    /// instead of a copied `Vec` ([`ActiveCompiledFrame::inline_chain`] left
    /// empty). Only a reader that goes through [`ActiveCompiledFrame::chain`]
    /// may be handed such a walk.
    pub shared_inline_chains: bool,
    /// WALK17-4 (`CRATONVM_COMPILED_WALK_ACTIVATION_MEMO=1`): a per-walk
    /// two-way memo of `(artifact, return address, safepoint-id word) ->
    /// (bci, census verdict, chain)` for every activation below the innermost
    /// ([`ActivationMemo`]). Pure in its key: [`activation_bci_with`] and
    /// [`compiled_frame_inline_chain`] read nothing else for such a frame.
    pub activation_memo: bool,
}

/// [`active_compiled_frames_walk_recorded`] with [`CompiledWalkOptions`].
pub(crate) fn active_compiled_frames_walk_recorded_opts(
    max_activations: usize,
    scoped: bool,
    record: &mut RawWalkKey,
    opts: CompiledWalkOptions,
) -> Vec<ActiveCompiledFrame> {
    let mut cuts = Vec::new();
    let cuts = (max_activations != usize::MAX).then_some(&mut cuts);
    active_compiled_frames_walk_recording_opts(
        0,
        max_activations.max(1),
        scoped,
        cuts,
        Some(record),
        opts,
    )
}

/// [`active_compiled_frames_cut`] with [`CompiledWalkOptions`].
pub fn active_compiled_frames_cut_opts(
    max_activations: usize,
    scoped: bool,
    cuts: &mut Vec<CompiledWalkCut>,
    opts: CompiledWalkOptions,
) -> Vec<ActiveCompiledFrame> {
    cuts.clear();
    active_compiled_frames_walk_recording_opts(
        0,
        max_activations.max(1),
        scoped,
        Some(cuts),
        None,
        opts,
    )
}

/// The whole walk -- [`active_compiled_frames`] (`scoped == false`) or
/// [`active_compiled_frames_walk_scoped`] -- with [`CompiledWalkOptions`].
pub fn active_compiled_frames_whole_opts(
    scoped: bool,
    opts: CompiledWalkOptions,
) -> Vec<ActiveCompiledFrame> {
    active_compiled_frames_walk_recording_opts(0, usize::MAX, scoped, None, None, opts)
}

/// WALK17-4's memo key: the artifact, the activation's own return address
/// (the exact inline-chain key) and the word its safepoint-id slot held
/// (`None`: the frame has no readable slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct ActivationMemoKey {
    pub(in crate::jit::conservative_roots) cm: usize,
    pub(in crate::jit::conservative_roots) pc: usize,
    pub(in crate::jit::conservative_roots) sp_id: Option<u32>,
}

/// WALK17-4's memoised answer for one [`ActivationMemoKey`].
#[derive(Clone, Debug)]
pub(in crate::jit::conservative_roots) struct ActivationMemoEntry {
    pub(in crate::jit::conservative_roots) bci: i32,
    /// The census column [`activation_bci_with`] counted (replayed on a hit).
    pub(in crate::jit::conservative_roots) census: usize,
    /// The chain, shared with the artifact (`None` = empty).
    pub(in crate::jit::conservative_roots) chain: Option<cratonvm_jit::x64::InlineChain>,
    pub(in crate::jit::conservative_roots) chain_exact: bool,
}

/// WALK17-4 (round 19 wave 2): one walk's activation memo, two ways (a
/// recursion repeats one key, two methods calling each other alternate two).
///
/// Sound for one walk of one thread's own stack: every artifact a key names
/// is a live frame's for the whole walk (the walk's keep-alive contract), so
/// its address cannot be reused for another artifact while the memo exists,
/// and for an activation BELOW the innermost the answer is a function of the
/// key alone: [`activation_bci_with`] reads the artifact, the return address
/// and the slot word; [`compiled_frame_inline_chain`] with a return address
/// reads only the exact key (it never falls back to the safepoint id).
/// The innermost activation (no return address) is never memoised: its
/// chain lookup can count a poisoned-bci refusal, a side effect a replay
/// would lose.
#[derive(Default)]
pub(in crate::jit::conservative_roots) struct ActivationMemo {
    pub(in crate::jit::conservative_roots) ways: [Option<(ActivationMemoKey, ActivationMemoEntry)>; 2],
    pub(in crate::jit::conservative_roots) next: usize,
}

impl ActivationMemo {
    pub(in crate::jit::conservative_roots) fn get(&self, key: &ActivationMemoKey) -> Option<&ActivationMemoEntry> {
        self.ways
            .iter()
            .flatten()
            .find(|(k, _)| k == key)
            .map(|(_, entry)| entry)
    }

    pub(in crate::jit::conservative_roots) fn insert(&mut self, key: ActivationMemoKey, entry: ActivationMemoEntry) {
        self.ways[self.next] = Some((key, entry));
        self.next ^= 1;
    }
}

/// [`active_compiled_frames_walk`] over `chain` -- the calling thread's
/// entry chain, or a copy of another thread's ([`JitChainSnapshot`],
/// interpreter round i2 wave 58, lane L6) -- with `scanner_sp` the lowest
/// stack address the walk may read (the calling thread's stack pointer, or
/// the copied thread's when it took the copy). `max_activations` is the
/// bound already decided (`CRATONVM_CALLER_ACT_BOUND` applied by the
/// caller); `cuts` records each entry the bound cut short when asked (JIT
/// round 17 wave 2, lane walk; [`active_compiled_frames_cut`]).
pub(in crate::jit::conservative_roots) fn walk_jit_chain(
    chain: &[JitFrameChainEntry],
    scanner_sp: usize,
    min_depth: u32,
    max_activations: usize,
    scoped: bool,
    cuts: Option<&mut Vec<CompiledWalkCut>>,
    record: Option<&mut RawWalkKey>,
) -> Vec<ActiveCompiledFrame> {
    walk_jit_chain_opts(
        chain,
        scanner_sp,
        min_depth,
        max_activations,
        scoped,
        cuts,
        record,
        CompiledWalkOptions::default(),
    )
}

/// [`walk_jit_chain`] with the round-19 wave-2 walk options
/// ([`CompiledWalkOptions`]; the default options are that walk exactly).
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn walk_jit_chain_opts(
    chain: &[JitFrameChainEntry],
    scanner_sp: usize,
    min_depth: u32,
    max_activations: usize,
    scoped: bool,
    mut cuts: Option<&mut Vec<CompiledWalkCut>>,
    mut record: Option<&mut RawWalkKey>,
    opts: CompiledWalkOptions,
) -> Vec<ActiveCompiledFrame> {
    // JIT round 17 wave 2 (lane walk): see `compiled_walk_whole_entry_enabled`.
    let whole_entry = compiled_walk_whole_entry_enabled();
    let nested_enabled = nested_trace_frames_enabled();
    let want_bci = compiled_frame_bci_enabled();
    let want_chains = inline_frame_chains_enabled();
    let dbg_chain = dbg_swchain_enabled();
    {
        if dbg_chain {
            eprintln!(
                "[swchain] scanner_sp=0x{scanner_sp:x} entries={} nested={nested_enabled}",
                chain.len()
            );
        }
        // JIT round 16 wave 15 (lane throwfill): an empty chain has nothing to
        // resolve, so it takes no snapshot.
        let mut lookups = if scoped && !chain.is_empty() {
            WalkLookups::walk_scoped()
        } else {
            WalkLookups::per_call()
        };
        let mut out: Vec<ActiveCompiledFrame> = Vec::with_capacity(chain.len());
        // WALK17-4 (round 19 wave 2): the per-walk activation memo, empty and
        // unread unless `opts.activation_memo`.
        let mut activation_memo = ActivationMemo::default();
        // JIT round 16 (lane throwpath, TP16-4 first step): one activation
        // buffer for the whole walk, cleared per chain entry, instead of a
        // fresh `Vec` grown push by push for every entry -- the allocator's
        // `grow_one` / `finish_grow` were ~19% of this function in a
        // throwable's fill (sampled). Its contents per entry are unchanged.
        let mut nested: Vec<(
            *const cratonvm_jit::CompiledMethod,
            Option<usize>,
            Option<usize>,
        )> = Vec::with_capacity(if chain.is_empty() { 0 } else { 16 });
        for (dbg_i, e) in chain.iter().enumerate() {
            if e.interp_depth < min_depth {
                if let Some(record) = record.as_deref_mut() {
                    record.plan.push((0, false));
                }
                continue;
            }
            let Some(info) = e.precise else {
                if let Some(record) = record.as_deref_mut() {
                    record.plan.push((0, false));
                }
                if dbg_chain {
                    eprintln!(
                        "[swchain] e{dbg_i} entry_sp=0x{:x} precise=none",
                        e.entry_sp
                    );
                }
                continue;
            };
            let entry_sp = e.entry_sp;
            if dbg_chain {
                // SAFETY: the same keep-alive contract as the reporting loop at
                // the end of this function.
                let b = unsafe { &*info.compiled_method }.method_label.clone();
                eprintln!(
                    "[swchain] e{dbg_i} entry_sp=0x{entry_sp:x} exact_rbp=0x{:x} cm_id={} depth={} boundary={b}",
                    info.exact_rbp, info.exact_cm_id, e.interp_depth
                );
            }
            // An entry whose body never built a frame: the not-entrant stub
            // intercepted the very call this entry was pushed for, and its
            // helper is re-running the method through the interpreter, whose
            // own frames are this activation (`not_entrant_entry_never_framed`).
            // Reporting the boundary below as well named the method twice.
            if not_entrant_entry_never_framed(&info, &mut lookups) {
                if let Some(record) = record.as_deref_mut() {
                    record.plan.push((0, false));
                }
                if dbg_chain {
                    eprintln!("[swchain] e{dbg_i} not-entrant re-dispatch: no frame of its own");
                }
                continue;
            }
            // One chain entry is one interpreter->JIT boundary, but the
            // compiled region behind it can be many ACTIVATIONS deep: compiled
            // code calling itself never re-enters from the interpreter, so it
            // pushes no further chain entry. Reporting only the boundary method
            // made 64 nested activations read as ONE frame to
            // `Throwable.getStackTrace()` and `StackWalker` alike — Log4j2's
            // caller lookup then walked past the frame it wanted and answered
            // with the enclosing class
            // (`stackwalker_log4j_deep_repeated_walks_finish_under_jit`).
            // Walk the saved-RBP chain the way `remap_active_jit_frames`'
            // Stage 5 already does, and report every activation.
            //
            // Each activation is carried with the RBP of ITS OWN frame, not
            // just its method: that is where the safepoint id lives, and it is
            // the only thing standing between a compiled frame and a line
            // number. The walk already computes every one of these addresses
            // to find the next frame — it used to drop them on the floor.
            //
            // The THIRD member is that frame's own RETURN ADDRESS, when the
            // RBP-chain walk read one. It is `None` for the innermost
            // activation (whose return address is not on this stack) and for
            // the boundary fallback, and it is the EXACT key the inline-frame
            // map is recorded under — see `compiled_frame_inline_chain`.
            //
            // The RBP is an `Option` because `innermost_frame_method` ALSO
            // answers `Some(cm)` when the RBP is unusable (it falls back to
            // the boundary method) and the caller cannot tell the two answers
            // apart from the outside. Reading `[rbp - sp_id_slot_off]` for an
            // out-of-band RBP would be a wild read, not merely a wrong line,
            // so the bound is re-checked here; the walk's own `parent_rbp`
            // values were bounds-checked in the loop below and need no second
            // test.
            let exact_rbp = (info.exact_rbp != 0
                && info.exact_rbp & 0x7 == 0
                && info.exact_rbp >= scanner_sp
                && info.exact_rbp.saturating_add(16) <= entry_sp)
                .then_some(info.exact_rbp);
            nested.clear();
            // Set when the bounded walk stopped before reaching the boundary.
            let mut truncated = false;
            // For a raw walk key: the RBP-chain reads this entry made, and
            // whether the last one ended the entry at a return address that
            // resolves to no compiled body (its value is then not keyed).
            let mut raw_reads = 0usize;
            let mut raw_ended_not_code = false;
            if nested_enabled {
                // JIT round 21 wave 8 (lane splice): a stack walk (never the
                // collector, whose doors keep `innermost_frame_method`'s
                // fail-closed answer) names an innermost frame of a body made
                // not entrant after the frame was built, rather than losing
                // it (`trace_innermost_by_caller_call`).
                let innermost = match innermost_frame_method_with(
                    info.exact_rbp,
                    info.exact_cm_id,
                    entry_sp,
                    scanner_sp,
                    info.compiled_method,
                    &mut lookups,
                ) {
                    Some(cm) => Some(cm),
                    None => trace_innermost_by_caller_call(
                        info.exact_rbp,
                        entry_sp,
                        scanner_sp,
                        &mut lookups,
                    ),
                };
                if let Some(innermost) = innermost {
                    // The innermost frame has no return address of its own
                    // on this stack, so only the safepoint-id slot can place
                    // it -- unless a not-entrant re-dispatch in flight names
                    // its resume pc; use that when it lies in that body.
                    // SAFETY: `innermost` is the live artifact the walk just
                    // resolved (same contract as the reporting loop below).
                    let pc = info.not_entrant_ret_pc;
                    let in_body = pc != 0 && {
                        let cm = unsafe { &*innermost };
                        let entry = cm.entry_ptr() as usize;
                        entry != 0 && pc > entry && pc <= entry.saturating_add(cm.code_len())
                    };
                    nested.push((innermost, exact_rbp, in_body.then_some(pc)));
                }
                // JIT frames use `push rbp; mov rbp,rsp`, so `[rbp]` is the
                // caller RBP and `[rbp+8]` the return address INTO that caller.
                // Same bound checks and the same order as the Stage 5 walk —
                // this one only READS, where that one rewrites oop slots.
                // JIT round 17 wave 2 (lane walk): the step guard is the
                // band's capacity, not Stage 5's 4096, unless
                // `CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY=0`; a guard that
                // fires anyway marks the entry truncated.
                let step_guard = compiled_walk_step_guard(whole_entry, scanner_sp, entry_sp);
                let mut child_rbp = info.exact_rbp;
                let mut guard = 0usize;
                loop {
                    if guard >= step_guard {
                        truncated |= whole_entry;
                        break;
                    }
                    if nested.len() >= max_activations {
                        truncated = true;
                        break;
                    }
                    guard += 1;
                    if child_rbp == 0 || child_rbp & 0x7 != 0 {
                        break;
                    }
                    if child_rbp < scanner_sp || child_rbp >= entry_sp {
                        break;
                    }
                    // SAFETY: `child_rbp` is an aligned address inside this
                    // thread's own live JIT stack region, bounded by
                    // `scanner_sp` (this frame) and `entry_sp` (the boundary
                    // that pushed the chain entry), and validated before use
                    // exactly as the other rbp-chain walks in this file do.
                    let parent_rbp = unsafe { (child_rbp as *const usize).read() };
                    let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
                    raw_reads += 1;
                    if parent_rbp <= child_rbp
                        || parent_rbp & 0x7 != 0
                        || parent_rbp < scanner_sp
                        || parent_rbp >= entry_sp
                    {
                        break;
                    }
                    match lookups.code_range(ret_addr) {
                        // `ret_addr` lies in the PARENT, so the frame this
                        // method is running in is the one at `parent_rbp`.
                        // `ret_addr` also lies in THIS method and is the
                        // instruction after the call it is suspended in — the
                        // exact key `InlineFrameMap` rows are recorded under.
                        Some(cm_ptr) => nested.push((
                            cm_ptr as *const cratonvm_jit::CompiledMethod,
                            Some(parent_rbp),
                            Some(ret_addr),
                        )),
                        // The parent is the interpreter / Rust boundary: this
                        // entry has no further compiled ancestors.
                        None => {
                            raw_ended_not_code = true;
                            break;
                        }
                    }
                    child_rbp = parent_rbp;
                }
            }
            if let Some(record) = record.as_deref_mut() {
                record.plan.push((raw_reads, raw_ended_not_code));
            }
            // Never report FEWER frames than the pre-walk answer. A walk cut
            // short by a bound, one that never started (`exact_rbp == 0`), and
            // the kill-switch path all still owe the boundary method the chain
            // entry was pushed for.
            //
            // JIT round 17 wave 2 (lane walk): `exact_rbp` is the INNERMOST
            // activation's frame. It is the boundary's own only when the walk
            // reported nothing else; after other activations it belongs to
            // one of them, and reading the boundary's safepoint id from it is
            // a line from a foreign slot. So the boundary then gets no RBP
            // (its line refused), unless the kill switch restores the old read.
            if !truncated && nested.last().map(|(p, _, _)| *p) != Some(info.compiled_method) {
                let boundary_rbp = if whole_entry && !nested.is_empty() {
                    None
                } else {
                    exact_rbp
                };
                nested.push((info.compiled_method, boundary_rbp, None));
            }
            // JIT round 17 wave 2 (lane walk): a cut entry, for a capture that
            // must know where the bounded answer stops being the whole one.
            // Its outermost reported activation is the next one `out` takes.
            if truncated {
                if let Some(cuts) = cuts.as_deref_mut() {
                    cuts.push(CompiledWalkCut {
                        interp_depth: e.interp_depth,
                        chain_index: u32::try_from(dbg_i).unwrap_or(u32::MAX),
                        first_reported: out.len(),
                    });
                }
            }
            if dbg_chain {
                let names: Vec<String> = nested
                    .iter()
                    // SAFETY: as the reporting loop below.
                    .map(|(p, _, _)| unsafe { &**p }.method_label.clone())
                    .collect();
                let mut runs: Vec<String> = Vec::new();
                for n in &names {
                    match runs.last_mut() {
                        Some(last) if last.starts_with(&format!("{n} x")) => {
                            let c: usize =
                                last.rsplit(" x").next().unwrap_or("1").parse().unwrap_or(1);
                            *last = format!("{n} x{}", c + 1);
                        }
                        Some(last) if last == n => *last = format!("{n} x2"),
                        _ => runs.push(n.clone()),
                    }
                }
                eprintln!(
                    "[swchain] e{dbg_i} activations={} [{}]",
                    names.len(),
                    runs.join(" | ")
                );
            }
            // `nested` is innermost-first; the splice in
            // `runtime::stackwalker::interleave_compiled_frames` wants
            // outermost-first, and entries sharing an `interp_depth` keep their
            // push order.
            out.reserve(nested.len());
            for (cm_ptr, frame_rbp, native_pc) in nested.iter().rev() {
                // SAFETY: exactly the contract documented on
                // `PreciseFrameInfo::compiled_method` — the JIT cache holds an
                // owning `Arc` for as long as the body is registered, and the
                // chain entry is popped the moment the call returns or unwinds,
                // so there is no stale-pointer window. This read happens on the
                // owning thread, from a Java-level stack capture, i.e. strictly
                // inside that window. Pointers added by the walk came from
                // `lookup_jit_code_range`, which only answers for a code range
                // still registered in the cache.
                let cm = unsafe { &**cm_ptr };
                if cm.method_label.is_empty() {
                    continue;
                }
                // The safepoint-id slot this activation's line and inline
                // chain are read from, for a raw walk key ([`RawWalkKey`]).
                if let (Some(record), Some(rbp)) = (record.as_deref_mut(), *frame_rbp) {
                    if let Some(addr) = safepoint_id_slot_addr(rbp, cm) {
                        // SAFETY: as `safepoint_id_slot`, which reads the same word.
                        record
                            .slots
                            .push((addr, unsafe { (addr as *const usize).read() }));
                    }
                }
                // WALK17-4 (round 19 wave 2; `CRATONVM_COMPILED_WALK_ACTIVATION_MEMO=1`):
                // a non-innermost activation with a usable RBP is answered from
                // the per-walk memo when its artifact, return address and
                // safepoint-id word repeat (a compiled recursion repeats one
                // triple per activation); its census verdict is replayed.
                let memo_key = if opts.activation_memo && want_bci {
                    match (frame_rbp, native_pc) {
                        (Some(rbp), Some(pc)) => Some(ActivationMemoKey {
                            cm: *cm_ptr as usize,
                            pc: *pc,
                            sp_id: safepoint_id_slot(*rbp, cm),
                        }),
                        _ => None,
                    }
                } else {
                    None
                };
                let memo_hit = memo_key
                    .as_ref()
                    .and_then(|key| activation_memo.get(key))
                    .cloned();
                let (bci, inline_chain, inline_chain_shared, chain_exact) = match memo_hit {
                    Some(hit) => {
                        lookups.note(hit.census);
                        let (owned, shared) = if opts.shared_inline_chains {
                            (Vec::new(), hit.chain)
                        } else {
                            (
                                hit.chain.as_deref().map_or_else(Vec::new, <[_]>::to_vec),
                                None,
                            )
                        };
                        (hit.bci, owned, shared, hit.chain_exact)
                    }
                    None => {
                        let bci = match frame_rbp {
                            Some(rbp) if want_bci => {
                                activation_bci_with(*rbp, cm, *native_pc, &mut lookups)
                                    .unwrap_or(-1)
                            }
                            _ => {
                                // Counted, so an all-`-1` trace under
                                // `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` is
                                // distinguishable from one where the recovery ran and
                                // refused. A kill switch that looks identical to a
                                // defect is how a switch left set in an environment
                                // becomes a bug report.
                                //
                                // Round 12 wave 5 (lane exc3): only the switch is
                                // `switched-off`. A frame the walk reached with NO
                                // usable RBP (the boundary fallback for an
                                // out-of-band or unpublished mirror) is the census's
                                // `no-sp-id` population, as its table says; counting
                                // it as a kill switch hid exactly the frames
                                // `r12w5-exc3-ir-spliced-frames-vanish-from-implicit-exception-traces`
                                // is about.
                                lookups.note(if want_bci {
                                    cratonvm_jit::FRAME_LINE_REFUSED_NO_SP_ID
                                } else {
                                    cratonvm_jit::FRAME_LINE_REFUSED_SWITCHED_OFF
                                });
                                -1
                            }
                        };
                        let census = lookups.last_noted;
                        // OSRFRAME18-1 (round 19 wave 7, lane gc7): a refused
                        // line of a mirror-read frame is counted apart, and
                        // named under `CRATONVM_DBG_SWCHAIN=1`.
                        if want_bci && innermost_unrecorded_refusal(*frame_rbp, *native_pc, census)
                        {
                            lookups.note_sub(cratonvm_jit::FRAME_LINE_INNERMOST_UNRECORDED);
                            if dbg_chain {
                                eprintln!(
                                    "[swchain] e{dbg_i}   innermost-unrecorded {} compile_id={} \
                                     slot_id={:?} (a dead innermost frame reads so)",
                                    cm.method_label,
                                    cm.compile_id,
                                    frame_rbp.and_then(|rbp| safepoint_id_slot(rbp, cm)),
                                );
                            }
                        }
                        let (owned, shared, chain_exact) = if !want_chains {
                            (Vec::new(), None, false)
                        } else if opts.shared_inline_chains || memo_key.is_some() {
                            // THROWS18-2: the artifact's own chain, shared.
                            let (shared, exact) = compiled_frame_inline_chain_shared(
                                cm,
                                bci,
                                *native_pc,
                                frame_rbp.and_then(|rbp| safepoint_id_slot(rbp, cm)),
                            );
                            if opts.shared_inline_chains {
                                (Vec::new(), shared, exact)
                            } else {
                                (
                                    shared.as_deref().map_or_else(Vec::new, <[_]>::to_vec),
                                    shared,
                                    exact,
                                )
                            }
                        } else {
                            let (owned, exact) = compiled_frame_inline_chain(
                                cm,
                                bci,
                                *native_pc,
                                frame_rbp.and_then(|rbp| safepoint_id_slot(rbp, cm)),
                            );
                            (owned, None, exact)
                        };
                        if let Some(key) = memo_key {
                            activation_memo.insert(
                                key,
                                ActivationMemoEntry {
                                    bci,
                                    census,
                                    chain: shared.clone(),
                                    chain_exact,
                                },
                            );
                        }
                        // A non-shared walk keeps only its owned copy.
                        let shared = if opts.shared_inline_chains {
                            shared
                        } else {
                            None
                        };
                        (bci, owned, shared, chain_exact)
                    }
                };
                let chain_len = inline_chain_shared
                    .as_ref()
                    .map_or(inline_chain.len(), |shared| shared.len());
                // Key 2 only (the innermost frame), and only when it found no
                // chain: see `ActiveCompiledFrame::chain_poisoned`.
                let chain_poisoned = want_chains
                    && native_pc.is_none()
                    && chain_len == 0
                    && u32::try_from(bci)
                        .is_ok_and(|b| cm.inline_frame_map.safepoint_bci_is_poisoned(b));
                if dbg_chain {
                    // Round 12 wave 5 (lane exc3): every input the line and
                    // the inline chain were decided from, per activation, so
                    // a frame printed `(Unknown Source)` with its inlined
                    // callees missing names its cause in one run -- no RBP,
                    // an id the artifact never recorded, or a return address
                    // with no inline-frame row.
                    let entry = cm.entry_ptr() as usize;
                    let ret_off = native_pc.map(|pc| pc.wrapping_sub(entry));
                    let slot_id = frame_rbp.and_then(|rbp| safepoint_id_slot(rbp, cm));
                    eprintln!(
                        "[swchain] e{dbg_i}   {} ir={} no_refs={} rbp={} ret_off={ret_off:?} \
                         slot_id={slot_id:?} bci={bci} chain={} exact={chain_exact} \
                         map_rows={} sp_bci_rows={}",
                        cm.method_label,
                        cm.used_ir_backend,
                        cm.frame_holds_no_references,
                        frame_rbp.is_some(),
                        chain_len,
                        cm.inline_frame_map.len(),
                        cm.safepoint_bci_table.len(),
                    );
                }
                out.push(ActiveCompiledFrame {
                    // The walk reads the safepoint-id slot; only
                    // `apply_npe_trap_site` can promise an exact trap site.
                    trap_site_exact: false,
                    interp_depth: e.interp_depth,
                    // Cast-free: a chain deeper than `u32::MAX` entries cannot
                    // exist (every entry is a native stack frame).
                    chain_index: u32::try_from(dbg_i).unwrap_or(u32::MAX),
                    label: cm.method_label_shared(),
                    // The method's holder, not the receiver class an inherited
                    // body was published under (`CompiledMethod::frame_class_id`).
                    owner_class_id: cm.frame_class_id(),
                    // The artifact itself, so the trace assembler can ask it
                    // whether an interpreter frame's pc is one of ITS OSR entry
                    // points. Valid for exactly as long as the frame is live,
                    // which is the same window this whole function reads in.
                    cm_ptr: *cm_ptr as usize,
                    bci,
                    // The callees this artifact spliced at that same program
                    // point. `Vec::new()` does not allocate, so a method that
                    // inlines nothing -- and the whole feature switched off --
                    // costs one branch.
                    inline_chain,
                    inline_chain_shared,
                    chain_exact,
                    chain_poisoned,
                    compile_cp_stamp: cm.compile_cp_stamp(),
                    fast_throw_c2_tier: crate::runtime::exceptions::body_is_c2_equivalent(cm),
                    artifact_id: cm.artifact_id,
                    frame_rbp: *frame_rbp,
                });
            }
        }
        out
    }
}

/// A copy of one thread's JIT entry chain and its stack pointer, taken by
/// that thread, for another thread to walk its compiled activations later
/// (interpreter round i2 wave 58, lane L6;
/// `docs/feature-designs/real-jdk-mode.md`):
/// the JDWP native frames window of a thread running a native under
/// compiled code records this as it enters the native
/// (`interpreter::open_native_frames_window`) instead of the whole compiled
/// view, and the reader builds the view only when a debugger reads.
#[derive(Default)]
pub(crate) struct JitChainSnapshot {
    pub(in crate::jit::conservative_roots) entries: Vec<JitFrameChainEntry>,
    pub(in crate::jit::conservative_roots) scanner_sp: usize,
}

#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
impl JitChainSnapshot {
    /// Copy the calling thread's chain (its top entry's live RBP flushed in,
    /// as [`active_compiled_frames`] does) and stack pointer into `self`,
    /// reusing its buffer.
    pub(crate) fn capture(&mut self) {
        self.entries.clear();
        self.scanner_sp = current_stack_pointer();
        JIT_ENTRY_CHAIN.with(|c| {
            if nested_trace_frames_enabled() {
                if let Ok(mut v) = c.try_borrow_mut() {
                    flush_top_rbp_cache_to_chain(v.as_mut_slice());
                }
            }
            if let Ok(chain) = c.try_borrow() {
                self.entries.extend_from_slice(&chain);
            }
        });
    }

    /// Forget the copy.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.scanner_sp = 0;
    }

    /// The copied thread's compiled activations, outermost first, as
    /// [`active_compiled_frames`] on that thread answered when it took the
    /// copy.
    ///
    /// # Safety
    ///
    /// The copied thread must not have returned into, or moved, any frame the
    /// copy names since it took it, and must not until this returns: the walk
    /// reads its stack between `scanner_sp` and each entry's `entry_sp`, and
    /// dereferences the compiled bodies the entries name (alive while their
    /// frames are). A thread inside a native it entered after taking the copy,
    /// held there by its native frames window, satisfies this.
    pub(crate) unsafe fn active_compiled_frames(&self) -> Vec<ActiveCompiledFrame> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        walk_jit_chain(
            &self.entries,
            self.scanner_sp,
            0,
            usize::MAX,
            false,
            None,
            None,
        )
    }

    /// A copy of ANOTHER thread's chain, read through the addresses that
    /// thread published once ([`current_thread_jit_cells`]), with its top
    /// entry's frame record flushed in from that thread's mirrors (interpreter
    /// round i2 wave 60, lane L6, `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`;
    /// `docs/feature-designs/real-jdk-mode.md`).
    /// The walk's lower bound is the innermost recorded RBP: every read the
    /// walk makes lies at or above it (the RBP chain only climbs, and the
    /// safepoint-id slot is inside the frame it names). `None` when the
    /// cells are unpublished or the chain is mutably borrowed.
    ///
    /// # Safety
    ///
    /// The other thread must be held where it cannot push, pop or reallocate
    /// its chain, write its frame-record mirrors, or leave any compiled frame
    /// the chain names, until the copy's walk is done: a thread excused by a
    /// handshake pause in a native it entered under compiled code
    /// (`jvm_thread::NX_COMPILED`) runs no Java and no compiled code until the
    /// pause releases it, which the pause does after its walk.
    pub(crate) unsafe fn of_held_thread(cells: JitChainCells) -> Option<Self> {
        if cells.chain == 0 {
            return None;
        }
        // SAFETY: `cells.chain` is the address of the other thread's
        // `JIT_ENTRY_CHAIN` cell, which lives as long as that thread; the
        // caller guarantees nothing writes it during the copy.
        let cell = unsafe { &*(cells.chain as *const RefCell<Vec<JitFrameChainEntry>>) };
        let chain = unsafe { cell.try_borrow_unguarded() }.ok()?;
        let mut entries: Vec<JitFrameChainEntry> = chain.to_vec();
        if nested_trace_frames_enabled() {
            if let Some(info) = entries.last_mut().and_then(|top| top.precise.as_mut()) {
                if cells.rbp_slot != 0 {
                    // SAFETY: the other thread's live mirror slot (published
                    // by it), read while it is held.
                    info.exact_rbp = unsafe { (cells.rbp_slot as *const usize).read_volatile() };
                }
                info.exact_cm_id = if cells.cm_slot != 0 {
                    // SAFETY: as above.
                    (unsafe { (cells.cm_slot as *const usize).read_volatile() } & 0xFFFF_FFFF)
                        as u32
                } else {
                    0
                };
            }
        }
        let scanner_sp = entries
            .iter()
            .filter_map(|e| e.precise.as_ref().map(|p| p.exact_rbp))
            .filter(|rbp| *rbp != 0)
            .min()
            .unwrap_or(usize::MAX);
        Some(Self {
            entries,
            scanner_sp,
        })
    }

    /// How many entries the copy holds.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The addresses another thread needs to copy this thread's JIT entry chain
/// while it is held ([`JitChainSnapshot::of_held_thread`]): the
/// `JIT_ENTRY_CHAIN` cell and the innermost frame record's two mirror slots
/// (the TEB / TCB slot generated code writes, or the Rust fallback cell).
/// Stable for the thread's life, so published once (interpreter round i2
/// wave 60, lane L6).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct JitChainCells {
    pub(crate) chain: usize,
    pub(crate) rbp_slot: usize,
    pub(crate) cm_slot: usize,
}

/// [`JitChainCells`] of the calling thread.
pub(crate) fn current_thread_jit_cells() -> JitChainCells {
    let chain = JIT_ENTRY_CHAIN.with(|c| c as *const RefCell<Vec<JitFrameChainEntry>> as usize);
    let (rbp_slot, cm_slot) = frame_record_slot_addrs();
    JitChainCells {
        chain,
        rbp_slot,
        cm_slot,
    }
}

/// The addresses of the calling thread's innermost-RBP and compile-id mirror
/// slots, as [`top_rbp_get`] and [`published_compile_id`] read them (0: no
/// compile-id slot).
pub(in crate::jit::conservative_roots) fn frame_record_slot_addrs() -> (usize, usize) {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_rbp_tls_disp();
        if disp != 0 {
            // SAFETY: `gs:[0x30]` is the TEB's self pointer (`NT_TIB.Self`)
            // on x86-64 Windows; `disp` is a validated TEB slot offset.
            let teb = unsafe { read_gs_qword(0x30) };
            let cm_disp = cratonvm_jit::x64::inline_cm_tls_disp();
            return (
                teb.wrapping_add(disp),
                if cm_disp != 0 {
                    teb.wrapping_add(cm_disp)
                } else {
                    0
                },
            );
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let disp = cratonvm_jit::x64::inline_rbp_tls_disp();
        if disp != 0 {
            // The probe proved `fs:[disp32]` (sign-extended) is the mirror
            // cell; `fs:[0]` is the TCB self pointer it was derived from.
            let fs_base = read_fs_base();
            let at = |d: usize| fs_base.wrapping_add(d as u32 as i32 as isize as usize);
            let cm_disp = cratonvm_jit::x64::inline_cm_tls_disp();
            return (at(disp), if cm_disp != 0 { at(cm_disp) } else { 0 });
        }
    }
    (TOP_RBP.with(|c| c.as_ptr() as usize), 0)
}

/// `fs:[0]`, the x86-64 Linux TCB self pointer.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(in crate::jit::conservative_roots) fn read_fs_base() -> usize {
    let val: usize;
    // SAFETY: one aligned load of the ABI-guaranteed TCB self pointer.
    unsafe {
        core::arch::asm!(
            "mov {out}, qword ptr fs:[0]",
            out = out(reg) val,
            options(nostack, preserves_flags, readonly),
        );
    }
    val
}

// ---------------------------------------------------------------------------
// Cross-thread JIT-root gap detector (multi-thread-in-JIT-under-STW)
// ---------------------------------------------------------------------------

/// Diagnostic counter: number of times [`scan_active_jit_frames`] observed the
/// unsupported "multi-thread-in-JIT" condition (this thread's chain is empty
/// while another thread holds live JIT frames). Exposed for tests / JFR.
pub static CROSS_THREAD_JIT_GAP_HITS: AtomicUsize = AtomicUsize::new(0);

/// Cached `CRATONVM_STRICT_JIT_ROOTS` gate. When set, the cross-thread-JIT-gap
/// detector PANICS instead of merely warning, so a CI / fuzzing run can make
/// the (otherwise silent) unsupported condition a hard, visible failure. Off by
/// default so production keeps the safe published-snapshot path. Cached because
/// the detector runs on the per-native-call hot path.
pub(in crate::jit::conservative_roots) fn strict_jit_roots() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

