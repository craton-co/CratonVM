#[allow(unused_imports)]
use super::*;

/// `CRATONVM_GC_OUTGOING_ARG_ROOTS=0` -- keep marking the outgoing-argument
/// reserve. **Default ON**, i.e. it is skipped on frames whose safepoint proved
/// it staged no reference there.
///
/// The third bisect lever, beside `CRATONVM_GC_REG_OOP_MAPS` and
/// `CRATONVM_GC_DEAD_SPILL_ROOTS`. Separate because it rests on a third claim.
pub(in crate::jit::conservative_roots) fn gc_outgoing_arg_roots_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G,
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        },
    )
}

/// Is `off` an operand-spill slot the active safepoint has already RECLAIMED?
///
/// `OopMapEntry::live_frame_hi` is the operand-spill cursor at the moment the
/// safepoint was emitted, and its own doc states the consequence: slots at a
/// larger offset are "inside the `max_stack`-sized spill reserve but above the
/// live operand stack and the staged invoke-argument buffer, so their contents
/// are dead -- the spill cursor reclaims by moving, it does not clear." What is
/// in them is whatever the deepest earlier operand stack left there, and in
/// allocation-heavy code that is a stale object pointer.
///
/// The tree already spends this claim: [`band_slot_is_verifiable`] returns
/// `false` here, which is how the moving-young coverage proof excuses these
/// words from shadow-stack publication. Relocation is already unsound if the
/// claim is wrong, because nothing rewrites them.
///
/// This is a STRONGER use of the same claim, and the difference is worth
/// stating plainly: excusing a word from publication over-retains when the
/// claim is wrong, whereas dropping it from the root set FREES. That is why it
/// carries its own switch and why the oracle
/// (`CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`) checks these words alongside the
/// register-mask ones rather than trusting the argument.
///
/// The class test is `region_name`'s, not a bare `[spill_lo, spill_hi)` range:
/// that ladder puts LICM hoist slots, scalar-replacement fields and the
/// reserved-locals tail AHEAD of `operand-spill`, and those are not reclaimed
/// by the cursor.
#[inline]
pub(in crate::jit::conservative_roots) fn spill_slot_is_dead_above_cursor(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    live_hi: Option<i32>,
) -> bool {
    if !gc_dead_spill_roots_enabled() {
        return false;
    }
    let Some(hi) = live_hi else {
        return false;
    };
    off >= hi && layout_is_operand_spill(layout, off)}

/// gci i4/so: [`spill_slot_is_dead_above_cursor`] for the ONE optimizing-tier
/// spill word the published layout files under the register-image tail.
///
/// `ir_lower` places colour `c` at `[rbp - (first_spill + 8c)]` and refuses
/// only an offset ABOVE `spill_cap_off` (`Lowerer::planned_slot_off`), so a
/// frame with no alignment padding keeps its LAST colour in the word AT
/// `spill_cap_off` (`staging_liveness_words` says so: an offset not strictly
/// above it "would be a colour's home"). Every save lands strictly above it:
/// the prologue's and the OSR entry stubs' XMM and GPR saves
/// (`saved_xmm_regs` / `saved_gpr_regs`, `spill_cap_off + .. + 16*(i+1)` /
/// `8*(i+1)`), and the deopt image and the argument staging deeper still. The
/// layout publishes `spill_hi = callee_saved_lo = spill_cap_off`, which the
/// reader's half-open `[lo, hi)` convention reads as "the word at
/// `spill_cap_off` is the first word of the register tail", so
/// [`layout_is_operand_spill`] says no for it and the cursor claim never
/// drops it.
///
/// The emitter counts on that drop: `Lowerer::emit_dead_ref_slot_clears` skips
/// every dead home at or above the safepoint's watermark because "a word above
/// it is not BAND-scanned". So a reference the last colour held from an
/// earlier round of a loop stayed a root at every GC point emitted before the
/// colour's own definition: `SoftRefBeforeOome`'s referent in `main`'s OSR body
/// with inlining off (`vi2-oo` `oo2_soft_census_noinl`, every major from #2:
/// `off=232 region=callee-saved-gpr-image tier=ir sp=6 live_hi=232
/// in_map=false`, `jit-band-unrewritable`), which kept the soft referent
/// through the last-ditch clear.
///
/// Same claim, same switch (`CRATONVM_GC_DEAD_SPILL_ROOTS=0` turns both off),
/// one more word; optimizing-tier bodies only, because the single-pass
/// prologue stores its FIRST saved register at `callee_saved_lo`
/// ([`callee_saved_image_register`]). A padded frame's word there is padding,
/// which no store writes after the fresh-frame clear, so dropping it is moot.
#[inline]
pub(in crate::jit::conservative_roots) fn ir_last_colour_is_dead_above_cursor(
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    live_hi: Option<i32>,
) -> bool {
    let layout = &cm.frame_layout;
    if !cm.used_ir_backend
        || layout.spill_hi <= layout.spill_lo
        || off != layout.spill_hi
        || layout.callee_saved_lo != layout.spill_hi
    {
        return false;
    }
    gc_dead_spill_roots_enabled() && live_hi.is_some_and(|hi| off >= hi)
}

/// nio21 w15 L5: is `off` inside one of the three TYPED ranges
/// (`licm-ref-hoist`, `licm-arith`, `scalar-replaced-field`) that head
/// [`cratonvm_jit::FrameLayout::region_name`]'s ladder?
#[inline(always)]
pub(in crate::jit::conservative_roots) fn layout_in_typed_range(layout: &cratonvm_jit::FrameLayout, off: i32) -> bool {
    let hit = |lo: i32, hi: i32| hi > lo && off >= lo && off < hi;
    hit(layout.ref_hoist_lo, layout.ref_hoist_hi)
        || hit(layout.arith_lo, layout.arith_hi)
        || hit(layout.scalar_lo, layout.scalar_hi)
}

/// nio21 w15 L5: exactly `layout.region_name(off) == "java-local"`, without
/// running the name ladder and a string compare per band word.
///
/// `region_name` is a diagnostics routine; until wave 15 the two per-word
/// claim predicates ([`spill_slot_is_dead_above_cursor`],
/// [`SpLocalClaim::proves_dead`]) called it and compared the `&str`, which the
/// `Nio21UnsafeVolatile pipe` profile measured at 3.6% of the row (every
/// blocking native deposits a JIT band scan). This mirrors the ladder's first
/// four rungs in the same order; `layout_class_predicates_match_region_name`
/// pins the equivalence over every offset of several layouts.
#[inline]
pub(in crate::jit::conservative_roots) fn layout_is_java_local(layout: &cratonvm_jit::FrameLayout, off: i32) -> bool {
    !layout_in_typed_range(layout, off) && layout.java_locals_hi > 0 && off < layout.java_locals_hi
}

/// nio21 w15 L5: exactly `layout.region_name(off) == "operand-spill"`; see
/// [`layout_is_java_local`]. The ladder places the typed ranges, the java
/// locals and the reserved-locals tail ahead of the spill range, so a slot
/// that also falls inside one of those is NOT operand spill.
#[inline]
pub(in crate::jit::conservative_roots) fn layout_is_operand_spill(layout: &cratonvm_jit::FrameLayout, off: i32) -> bool {
    if layout_in_typed_range(layout, off) {
        return false;
    }
    if layout.java_locals_hi > 0 && off < layout.java_locals_hi {
        return false;
    }
    if layout.locals_hi > 0 && off < layout.locals_hi {
        return false;
    }
    layout.spill_hi > layout.spill_lo && off >= layout.spill_lo && off < layout.spill_hi
}

/// `CRATONVM_GC_DEAD_SPILL_ROOTS=0` -- keep marking operand-spill slots above
/// the safepoint's live cursor. **Default ON**, i.e. they are dropped.
///
/// The bisect lever for [`spill_slot_is_dead_above_cursor`], and the companion
/// of `CRATONVM_GC_REG_OOP_MAPS`. The two narrowings are independent switches
/// because they rest on DIFFERENT claims -- one on the compiler's register
/// model, one on the spill cursor -- and a regression has to be attributable to
/// one of them.
pub(in crate::jit::conservative_roots) fn gc_dead_spill_roots_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G,
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        },
    )
}

// gen r5w3/live7 -- the fourth band claim: the emitter's per-safepoint
// liveness table ([`cratonvm_jit::CompiledMethod::frame_liveness`]), spent in
// the band scan and the pin publication beside the register mask, the dead
// spill and the outgoing reserve, and like them a licence to DROP roots.
//
// **The gate is `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`, read by the
// optimizing tier per compile** (`jit/src/ir_lower.rs`, the section above
// `ir_precise_frame_liveness_enabled`; default ON since gch h3/fd, `=0` turns
// it off). A body compiled with it off carries an empty table, which claims
// nothing, so that scan is byte-for-byte the pre-claim one. The scan side
// reads no flag of its own: a table exists only because the compile-time
// switch asked for it, and a second, scan-side switch would be one more
// process-global cache (`vm/tests/per_vm_state_statics_ratchet.rs`).
// (gci i3/ir: this paragraph said `=1` / opt-in, stale since h3/fd.)
//
// Oracle: `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` -- every dropped word goes
// through `note_excluded_band_word` with `why = "live-set"` (reachability
// from the narrowed root set), and [`note_liveset_contradiction`] reports a
// dropped word the safepoint's own map still names.

/// The active safepoint id whose liveness claim the scan of the frame at
/// `rbp` may spend, or `None` (claim nothing): the body publishes no table,
/// or the frame's id cannot be read ([`active_safepoint_id`], which also
/// refuses a `frame_holds_no_references` body, whose id word is stale by
/// contract).
#[inline]
pub(in crate::jit::conservative_roots) fn frame_liveness_sp_id(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<u32> {
    if cm.frame_liveness.is_empty() {
        return None;
    }
    active_safepoint_id(rbp, cm)
}

/// Does the emitter's liveness claim prove the word at `[rbp - off]` dead at
/// safepoint `sp_id` ([`frame_liveness_sp_id`])? `false` whenever it cannot:
/// see [`cratonvm_jit::FrameLivenessTable::proves_dead`] for the fail-closed
/// rules (uncovered word, no record, incomplete record, a sibling record that
/// keeps the word live).
#[inline]
pub(in crate::jit::conservative_roots) fn is_dead_by_frame_liveness(
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    sp_id: Option<u32>,
) -> bool {
    let Some(id) = sp_id else {
        return false;
    };
    let Ok(off) = i16::try_from(off) else {
        return false;
    };
    cm.frame_liveness.proves_dead(id, off)}

/// `[band words the liveness claim excluded, dropped words the active map
/// nevertheless NAMES, callee-saved image words the caller's claim excluded]`
/// (see [`note_liveset_contradiction`], [`caller_register_claim`]). ONE static
/// for all three: a process-wide diagnostic tally, as its neighbours are, and
/// every new static line counts against `vm/tests/per_vm_state_statics_ratchet.rs`
/// (gen r5w6/oomjit10 grew the array rather than adding a line).
pub(in crate::jit::conservative_roots) static LIVESET_CENSUS: [AtomicUsize; 11] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
pub(crate) const LIVESET_WORDS_EXCLUDED: usize = 0;
pub(in crate::jit::conservative_roots) const LIVESET_CONTRADICTIONS: usize = 1;
pub(in crate::jit::conservative_roots) const IMAGE_CALLER_DEAD_EXCLUDED: usize = 2;
pub(in crate::jit::conservative_roots) const IR_PRIM_EXCLUDED: usize = 3;
/// gce e1/f: single-pass java-local words [`SpLocalClaim`] excluded.
pub(in crate::jit::conservative_roots) const SP_LOCAL_EXCLUDED: usize = 4;
/// gce e1/f: frame-deopt GPR image words [`frame_deopt_image_is_residue`]
/// excluded.
pub(in crate::jit::conservative_roots) const DEOPT_RESIDUE_EXCLUDED: usize = 5;
/// gcf f7/f: header-shaped interior band words proven
/// ([`note_frame_scan_interior_word`], under `CRATONVM_DBG=oldmark-root-census`).
/// The opt-in that dropped them from the roots
/// (`CRATONVM_GC_FRAME_SCAN_INTERIOR_FILTER`) was deleted by gch h3/yc: it
/// dropped 0 words in 6 runs of the family it was built for (vh1-fr block C).
pub(in crate::jit::conservative_roots) const FRAME_INTERIOR_SEEN: usize = 6;
/// gci i23/rf: reference-free compiled frames whose band the scan cut at
/// `callee_saved_lo` ([`reference_free_band_floor`]).
pub(in crate::jit::conservative_roots) const REF_FREE_FRAMES: usize = 7;
/// gci i23/rf: the words those frames' scans did not read (every word below
/// the floor that survived the reserved-tail test, object-shaped or not).
pub(in crate::jit::conservative_roots) const REF_FREE_WORDS_EXCLUDED: usize = 8;
/// JIT round 25 (item 3): frames whose band scan spent the per-body
/// `Prim`-colour claim where the per-map one abstained
/// ([`ir_prim_colour_claim_census`]).
pub(in crate::jit::conservative_roots) const IR_PRIM_BODY_CLAIM_FRAMES: usize = 9;
/// JIT round 25 wave 4: per-body claim words that were NOT object starts, so
/// the pre-wave-4 rule would have handed them to `g1_band_reject_pins` (a G1
/// region pin), and the whole-word rule ignored
/// ([`ir_body_claim_ignores_word`], [`ir_body_claim_unpinned_census`]).
pub(in crate::jit::conservative_roots) const IR_BODY_CLAIM_UNPINNED: usize = 10;

/// gcf f5/f: frame-deopt GPR image words the residue claim excluded
/// ([`frame_deopt_image_is_residue`]), single-pass and -- under
/// `CRATONVM_GC_IR_DEOPT_IMAGE_RESIDUE_ROOTS` (opt-in) -- optimizing bodies,
/// cumulative. The engagement counter of both switches.
pub fn deopt_image_residue_census() -> usize {
    LIVESET_CENSUS[DEOPT_RESIDUE_EXCLUDED].load(Ordering::Relaxed)
}

/// gcf f5/f: TRANSITIONAL. The f3/f IR ref-map claim this counted is removed;
/// `memory/roots.rs`'s `[regoop]` line still names this function until the
/// f5/f report's rename hunk (`ir_ref_map=` -> `deopt_residue=`,
/// [`deopt_image_residue_census`]) lands, so it answers that census meanwhile.
/// Delete it with the hunk.
pub fn ir_ref_map_census() -> usize {
    deopt_image_residue_census()
}

/// gen r5w6/oomjit10: `(callee-saved GPR image words the band scan excluded
/// because the frame that OWNS the register calls it dead
/// ([`caller_register_claim`]), optimizing-tier `Prim`-colour words it
/// excluded ([`ir_prim_slots`]))`, cumulative.
pub fn callee_saved_image_census() -> (usize, usize) {
    (
        LIVESET_CENSUS[IMAGE_CALLER_DEAD_EXCLUDED].load(Ordering::Relaxed),
        LIVESET_CENSUS[IR_PRIM_EXCLUDED].load(Ordering::Relaxed),
    )
}

// gen r5w6/oomjit10 -- the fifth band claim: a callee-saved GPR IMAGE word
// decided by the frame whose register it is.
//
// A compiled prologue saves its CALLER's callee-saved GPRs into its own frame
// (`callee-saved-gpr-image`); the census on the gen r5w5 staging build named
// such a word (`GenR5W3OsrHolderProbe.check` off=112) as a holder of dropped
// data. Until now the band scan read every one of them conservatively: the
// word's frame has no claim about it (the value is not its own), and the
// frame whose value it is was never asked. That frame is found by the walk
// the scan already trusts -- `[rbp]` is the caller's RBP and `[rbp + 8]` a
// return address into its code -- and it can answer:
//
// * a SINGLE-PASS caller that saves the register itself
//   (`CompiledMethod::osr_callee_saved_regs`) and publishes a register oop
//   mask at its active safepoint ([`active_map_facts`]): the mask is the
//   claim this scan already spends on that caller's OWN blind-spill image of
//   the same register at the same safepoint, and the callee's image holds
//   the same value (the caller's register at its call, which the callee's
//   prologue copied). A clear bit is "dead"; a set bit or no mask keeps it;
// * an OPTIMIZING caller that saves the register
//   (`CompiledMethod::ir_saved_gpr_slots`): that tier never keeps a
//   reference in a GP register (`regalloc::xmm_roles::IR_GP_LINEAR_SCAN`
//   refuses `IrType::Ref`; its homes are authoritative), so "dead";
// * a compiled caller that does NOT save the register never writes it, so
//   the value is ITS caller's: ask the next frame up, the same way;
// * anything else -- a return address outside compiled code (the VM's Rust
//   code at the interpreter->JIT boundary, which may hold a raw reference in
//   a callee-saved register across the call and is why
//   `remap_register_image_words` exists), a body that publishes no save list,
//   an OSR body whose saves the trampoline skipped, a parent link that fails
//   validation, or a chain longer than `CALLER_CLAIM_MAX_FRAMES` -- keeps the
//   word, exactly as before.
//
// When a save-owning caller calls the register dead, whatever the callee's
// image holds is either that caller's own dead value or a pass-through value
// that the caller's OWN image also holds (its prologue saved it) and that
// the same rule decides one frame up. Dropping the callee's copy therefore
// never unroots a value any frame still reads. `CRATONVM_GC_CALLEE_SAVED_IMAGE_LIVENESS=0`
// restores the conservative read (consume side, next collection). Census:
// `callee_saved_image_census`; oracle: the excluded words go through
// `note_excluded_band_word` with `why = "caller-dead"`.

/// How many compiled frames up [`caller_register_claim`] follows a register
/// no frame saves before it gives up (keeps the word).
pub(in crate::jit::conservative_roots) const CALLER_CLAIM_MAX_FRAMES: usize = 32;

/// `CRATONVM_GC_CALLEE_SAVED_IMAGE_LIVENESS` -- default ON, `=0` keeps every
/// callee-saved GPR image word a root again. Read once per chain walk
/// ([`BandClaimSwitches`]), no
/// static (`vm/tests/per_vm_state_statics_ratchet.rs`).
pub(in crate::jit::conservative_roots) fn callee_saved_image_liveness_enabled() -> bool {
    true
}

/// What the frame that owns a callee-saved register says about it at the
/// callee's image word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum CallerRegisterClaim {
    /// The owning compiled frame proves the register holds no live reference.
    Dead,
    /// gkr: [`Self::Dead`] from a SINGLE-PASS owner. Its callee-saved
    /// registers hold Java values only (local homes and operand-stack
    /// entries), never a derived pointer, so the word is not one either: it
    /// owes no G1 reject pin (`dead_word_rejects_owe_no_pin_enabled`).
    DeadValue,
    /// Keep the word: a live claim, no claim, or no compiled owner.
    Keep,
    /// gci i13/oj (oomjit10 item 1, opt-in `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`):
    /// the walk reached the Rust boundary of a PROMISING door's own chain
    /// segment, and the register is one the claim covers
    /// ([`DOOR_SHIM_OWNED_GPRS`], `jit_bridge::run_jit_body_promised`). The
    /// word is dead iff it is one of the reference arguments that door handed
    /// over ([`CallerRegisterClaim::excludes_word`]); any other word is kept.
    /// A function of the frames and the thread's published record only, so
    /// [`CallerClaimMemo`] stays exact; the per-word half is applied by the
    /// scan, which has the word.
    DoorOwned,
}

impl CallerRegisterClaim {
    /// Does this claim drop the image word `word`? `Dead` drops every word,
    /// `Keep` none, `DoorOwned` exactly the words the promising door published
    /// (gci i13/oj).
    pub(in crate::jit::conservative_roots) fn excludes_word(self, word: usize) -> bool {
        match self {
            CallerRegisterClaim::Dead | CallerRegisterClaim::DeadValue => true,
            CallerRegisterClaim::Keep => false,
            CallerRegisterClaim::DoorOwned => door_handed_word(word),
        }
    }
}

// gci i13/oj (2026-10-13) -- oomjit10 item 1, the Rust door's callee-saved
// argument word (`docs/feature-designs/concurrent-gc-maturation.md`).
//
// A callee-saved image word whose chain ends in Rust is kept (`Keep`): rustc
// may hold an object in that register across the call. A promising door
// (`jit_bridge::run_jit_body_promised` for `execute_jit_call` and, gci i14/od,
// `execute_jit_call_decoded`; `jit_bridge::run_jit_body_raw_promised` for
// `interpreter::execute`'s first-call door) publishes, for the length of the
// call, the reference argument words it handed over WEAKLY and the chain
// depth its `JitEntryGuard` is pushed at (`JvmThread::door_promise`);
// [`door_promise_claim`] answers `DoorOwned` only for a walk bounded by
// exactly that entry's `entry_sp` on the scanning thread's own chain (so a
// peer scan, a nested segment or another door's boundary keeps `Keep`), and
// the scan drops only a word EQUAL to a published argument. Such a word, in
// any Rust frame it could have come from, is a copy of an argument that door
// released to a weak handle: the door's extent (the shim, `run_jit_body` /
// `run_jit_body_raw`, `try_call*`) reads no argument after the call, the door
// reads its arguments only through the re-pinned handles off the `Returned`
// arm, and a frame above the door that relied on an unrooted copy of it is
// what the weak door already excludes. Every other word keeps `Keep`, and
// `remap_register_image_words` still rewrites a dropped word whose object
// moved, so only that word's contribution to LIVENESS goes.
//
// gci i14/od: i13/oj's shim also zeroed RSI/RDI/R12-R15 before the call, so
// rustc parked its callers' values of them in the shim's save area, neither
// scanned nor remapped: every non-argument reference a frame above kept there
// lost its image root for the call, which no reading of rustc's register
// allocation can rule out. The zeroing is gone; the claim decides by the word
// alone, so it needs no register zeroed and covers RBX as well.

/// The GPRs (x86-64 numbering) whose callee-saved image word a promising door
/// may claim at its own segment's boundary: every callee-saved GPR but RBP
/// (whose save is the frame link, not an image word): RBX, RSI, RDI
/// (callee-saved on Win64), R12-R15 (gci i13/oj; RBX since i14/od). The claim
/// decides by the word (`DoorHandedArgWords::handed`), not the register.
pub(crate) const DOOR_SHIM_OWNED_GPRS: [u8; 7] = [3, 6, 7, 12, 13, 14, 15];

/// The promising door's record of the installed thread, when one is
/// published (gci i13/oj). `None` with no installed thread (the scan of a
/// thread that never entered compiled code, unit tests) or no live door.
pub(in crate::jit::conservative_roots) fn current_door_promise() -> Option<crate::threading::jvm_thread::DoorHandedArgWords> {
    let t = crate::jit::helpers::current_jit_thread_ptr();
    if t.is_null() {
        return None;
    }
    // SAFETY: the installed thread is this OS thread's own `JvmThread`, live
    // for as long as it is installed (`set_jit_thread` / `restore_jit_thread`).
    // The scan runs on this thread, inside a call its owner is suspended in,
    // so nothing writes the field during this read; a plain `Copy` record is
    // copied out (the same raw read `shadow_window_at` makes of the thread).
    let state = unsafe { (*t).door_promise.as_deref() }?;
    state.record.active.then_some(state.record)
}

/// Is `word` one of the argument words the installed thread's promising door
/// handed over (gci i13/oj)?
pub(in crate::jit::conservative_roots) fn door_handed_word(word: usize) -> bool {
    current_door_promise().is_some_and(|rec| rec.handed(word))
}

/// The answer of [`caller_claim_step`]'s native arm (gci i13/oj): `DoorOwned`
/// when `reg` is a covered register and the walk is bounded by the
/// promising door's own chain entry on this thread; `Keep` otherwise, which is
/// the arm's answer whenever no door promises (the default).
pub(in crate::jit::conservative_roots) fn door_promise_claim(reg: u8, walk_hi: usize) -> CallerRegisterClaim {
    if !cfg!(target_arch = "x86_64") || !DOOR_SHIM_OWNED_GPRS.contains(&reg) {
        return CallerRegisterClaim::Keep;
    }
    let Some(rec) = current_door_promise() else {
        return CallerRegisterClaim::Keep;
    };
    let the_doors_segment = JIT_ENTRY_CHAIN
        .try_with(|c| {
            c.try_borrow()
                .ok()
                .and_then(|v| v.get(rec.chain_depth).map(|e| e.entry_sp == walk_hi))
        })
        .ok()
        .flatten()
        .unwrap_or(false);
    if the_doors_segment {
        CallerRegisterClaim::DoorOwned
    } else {
        CallerRegisterClaim::Keep
    }
}

/// The x86-64 GPR whose prologue save the word at `off` of `cm`'s frame is,
/// when the body publishes its save list; `None` otherwise (and on every
/// other architecture, whose register numbering this does not model).
pub(in crate::jit::conservative_roots) fn callee_saved_image_register(cm: &cratonvm_jit::CompiledMethod, off: i32) -> Option<u8> {
    if !cfg!(target_arch = "x86_64") || !is_callee_saved_gpr_image(off, &cm.frame_layout) {
        return None;
    }
    if let Some(slots) = cm.ir_saved_gpr_slots.as_ref() {
        return slots.iter().find(|&&(_, o)| o == off).map(|&(r, _)| r);
    }
    if cm.used_ir_backend {
        return None;
    }
    // The single-pass prologue stores `alloc_used_regs[i]` at
    // `callee_saved_base + 8*i`, and publishes both (`x64/driver.rs`).
    let regs = cm.osr_callee_saved_regs.as_ref()?;
    let rel = off.checked_sub(cm.frame_layout.callee_saved_lo)?;
    if rel < 0 || rel % 8 != 0 {
        return None;
    }
    regs.get(usize::try_from(rel / 8).ok()?).copied()
}

/// gkr: `CRATONVM_GC_DEAD_WORD_REJECTS_OWE_NO_PIN` (default ON, `=0` kills).
/// Two band claims prove a word STALE -- a register save whose single-pass
/// owner proves the register dead ([`CallerRegisterClaim::DeadValue`]), and a
/// residue deopt image block (`frame_deopt_image_is_residue`) -- and let go of
/// it only when it is an object start, so a word that is not one kept its G1
/// reject pin as if it could be a derived pointer. Neither can hold one: the
/// owner's register holds Java values only, and a residue block is never read
/// again. Under this switch such a word owes no pin either. A stale interior
/// word into a dropped 52 MiB array in `row`'s single-pass frame kept its
/// humongous span through every collection (`T1W13BigAfterChurn` on G1).
/// Read once per frame scan.
pub(in crate::jit::conservative_roots) fn dead_word_rejects_owe_no_pin_enabled() -> bool {
    true
}

/// Ask the compiled frame that owns `reg` about the callee-saved image of it
/// in the frame at `rbp` (see the section above). Walks parent links no
/// higher than `walk_hi`.
pub(in crate::jit::conservative_roots) fn caller_register_claim(rbp: usize, reg: u8, walk_hi: usize) -> CallerRegisterClaim {
    let bit = cratonvm_jit::x64::reg_oop_mask_bit(reg);
    if bit == 0 {
        return CallerRegisterClaim::Keep;
    }
    walk_caller_claim(rbp, |cur| caller_claim_step(cur, reg, bit, walk_hi))
}

/// One step of [`caller_register_claim`]'s walk: from the frame at `cur`,
/// either an answer or the parent frame to ask next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum ClaimStep {
    /// `reg` passes through the frame above `cur` unsaved: ask from there.
    Up(usize),
    /// The walk ends here with this answer.
    Done(CallerRegisterClaim),
}

/// The walk of [`caller_register_claim`] over a step function: at most
/// `CALLER_CLAIM_MAX_FRAMES` steps, `Keep` past them.
pub(in crate::jit::conservative_roots) fn walk_caller_claim(
    start: usize,
    mut step: impl FnMut(usize) -> ClaimStep,
) -> CallerRegisterClaim {
    let mut cur = start;
    for _ in 0..CALLER_CLAIM_MAX_FRAMES {
        match step(cur) {
            ClaimStep::Up(parent) => cur = parent,
            ClaimStep::Done(claim) => return claim,
        }
    }
    CallerRegisterClaim::Keep
}

/// One step of the walk from the compiled frame at `cur`. A pure function of
/// `(cur, reg, walk_hi)` while the stack is stopped (and, at the native arm,
/// of the thread's promising-door record, which is fixed for the whole walk:
/// gci i13/oj), which is what lets [`CallerClaimMemo`] reuse its answers.
pub(in crate::jit::conservative_roots) fn caller_claim_step(cur: usize, reg: u8, bit: u16, walk_hi: usize) -> ClaimStep {
    if cur == 0 || cur & 0x7 != 0 || cur.checked_add(16).is_none_or(|end| end > walk_hi) {
        return ClaimStep::Done(CallerRegisterClaim::Keep);
    }
    // SAFETY: `cur` is a compiled frame base inside this thread's live
    // stack interval below `walk_hi`: the caller validated the first one,
    // and every later one passed the parent-link checks below. The
    // prologue keeps the caller's RBP at `[cur]` and the return address at
    // `[cur + 8]`.
    let parent_rbp = unsafe { (cur as *const usize).read() };
    let ret = unsafe { ((cur + 8) as *const usize).read() };
    let Some(p_ptr) = cratonvm_jit::lookup_jit_code_range(ret) else {
        // The Rust boundary. `Keep`, unless the promising door's shim owns
        // the register here (gci i13/oj, [`door_promise_claim`]).
        let _ = door_promise_claim(reg, walk_hi);
        return ClaimStep::Done(CallerRegisterClaim::Dead);
    };
    if parent_rbp <= cur || parent_rbp & 0x7 != 0 || parent_rbp >= walk_hi {
        return ClaimStep::Done(CallerRegisterClaim::Keep);
    }
    // SAFETY: code ranges retain their CompiledMethod metadata for the
    // lifetime of an active frame (the same contract every walk here uses).
    let parent: &cratonvm_jit::CompiledMethod =
        unsafe { &*(p_ptr as *const cratonvm_jit::CompiledMethod) };
    if let Some(slots) = parent.ir_saved_gpr_slots.as_ref() {
        if slots.iter().any(|&(r, _)| r == reg) {
            return ClaimStep::Done(CallerRegisterClaim::Dead);
        }
        return ClaimStep::Up(parent_rbp);
    }
    if parent.used_ir_backend {
        return ClaimStep::Done(CallerRegisterClaim::Keep);
    }
    let Some(regs) = parent.osr_callee_saved_regs.as_ref() else {
        return ClaimStep::Done(CallerRegisterClaim::Keep);
    };
    if !regs.contains(&reg) {
        return ClaimStep::Up(parent_rbp);
    }
    if shadow_stack_enabled() && parent.shadow_off_in_thread != 0 {
        return ClaimStep::Done(CallerRegisterClaim::Dead);
    }
    ClaimStep::Done(match active_map_facts(parent_rbp, parent).reg_mask {
        Some(mask) if mask & bit == 0 => CallerRegisterClaim::DeadValue,
        _ => CallerRegisterClaim::Keep,
    })
}

// gcf f2/f (2026-10-01) -- `gcf-f1f-proposal-memoize-the-caller-register-claim-per-frame`.
//
// The band scan walks a chain's frames innermost first and asks
// [`caller_register_claim`] for every callee-saved image word that holds an
// object start. In a recursion through frames that do not save the register,
// frame `i`'s walk and frame `i+1`'s walk cross the same frames to the same
// owner, so a stack of depth D paid up to `CALLER_CLAIM_MAX_FRAMES` parent
// steps -- each a code-range lookup -- per word. [`CallerClaimMemo`] keeps,
// for every `(frame, reg)` a walk crossed, the answer the walk found and how
// many steps from that frame it took. A later walk that reaches a remembered
// frame after `k` steps answers at once: the remembered answer when it lies
// within the step budget (`k + n <= CALLER_CLAIM_MAX_FRAMES`), `Keep` -- the
// budget answer -- when it does not. Every step is a pure function of
// `(cur, reg, walk_hi)` while the stack is stopped, so the answer is the
// plain walk's by construction (unit test
// `the_memoized_caller_claim_answers_exactly_as_the_plain_walk`). A walk that
// ran out of budget remembers nothing: its answer is not a property of the
// frames it crossed. The memo lives on the scanner's stack for one chain
// walk (fixed `walk_hi`), never in a static.

/// Per-chain-walk memo of [`caller_register_claim`] (see above).
pub(in crate::jit::conservative_roots) struct CallerClaimMemo {
    /// The bound every walk this memo answers for was made under.
    pub(in crate::jit::conservative_roots) walk_hi: usize,
    // `(frame rbp, reg) -> (answer, steps from that frame to the answer)`.
    // addr-keyed: not a heap address -- (frame RBP, register) of one chain walk.
    pub(in crate::jit::conservative_roots) known: rustc_hash::FxHashMap<(usize, u8), (CallerRegisterClaim, usize)>,
    /// Scratch: the frames the current walk crossed, in order.
    pub(in crate::jit::conservative_roots) path: Vec<usize>,
}

impl CallerClaimMemo {
    pub(in crate::jit::conservative_roots) fn new(walk_hi: usize) -> Self {
        CallerClaimMemo {
            walk_hi,
            known: rustc_hash::FxHashMap::default(),
            path: Vec::new(),
        }
    }

    /// [`caller_register_claim`]`(rbp, reg, self.walk_hi)`, memoized.
    pub(in crate::jit::conservative_roots) fn claim(&mut self, rbp: usize, reg: u8) -> CallerRegisterClaim {
        let bit = cratonvm_jit::x64::reg_oop_mask_bit(reg);
        if bit == 0 {
            return CallerRegisterClaim::Keep;
        }
        let walk_hi = self.walk_hi;
        self.walk(rbp, reg, |cur| caller_claim_step(cur, reg, bit, walk_hi))
    }

    /// [`walk_caller_claim`] over `step`, answering from and recording into
    /// the memo. `step` must be the same pure function for every call with
    /// the same `reg`.
    pub(in crate::jit::conservative_roots) fn walk(
        &mut self,
        start: usize,
        reg: u8,
        mut step: impl FnMut(usize) -> ClaimStep,
    ) -> CallerRegisterClaim {
        self.path.clear();
        let mut cur = start;
        for k in 0..CALLER_CLAIM_MAX_FRAMES {
            if let Some(&(claim, n)) = self.known.get(&(cur, reg)) {
                self.remember(reg, claim, k + n);
                return if k + n <= CALLER_CLAIM_MAX_FRAMES {
                    claim
                } else {
                    CallerRegisterClaim::Keep
                };
            }
            self.path.push(cur);
            match step(cur) {
                ClaimStep::Up(parent) => cur = parent,
                ClaimStep::Done(claim) => {
                    self.remember(reg, claim, k + 1);
                    return claim;
                }
            }
        }
        CallerRegisterClaim::Keep
    }

    /// The walk crossed `self.path` and its answer lay `total` steps from
    /// `path[0]`: record each crossed frame's distance to it.
    pub(in crate::jit::conservative_roots) fn remember(&mut self, reg: u8, claim: CallerRegisterClaim, total: usize) {
        for (j, &frame) in self.path.iter().enumerate() {
            self.known.insert((frame, reg), (claim, total - j));
        }
    }
}

// gen r5w6/oomjit10 -- the sixth band claim: an optimizing-tier frame's
// PRIMITIVE colours (item 1 of
// `docs/feature-designs/jit-ir-optimizations.md`).
//
// The optimizing tier colours its spill band in two disjoint pools and never
// moves a colour between them (`ir_lower::SlotPlan::prim_colors`, re-derived by
// `verify_slot_colouring`), and publishes the `Prim` colours' offsets on every
// map as `OopMapEntry::non_oop_stack_slots`. A word there never holds a
// reference of THIS activation. What it can hold is a leftover of the frame
// that last used the same stack depth -- the prologue zeroes the `Ref` homes
// (`zero_ref_value_slots`) and not the `Prim` ones -- and after an
// `OutOfMemoryError` that is often the dropped structure's address, left by an
// OSR'd frame that filled the heap at that depth. The band scan rooted it.
//
// One exception is kept: a `long` that carries a minted object handle
// (`memory::smuggled_longs`), which the interpreter roots too; such a word
// stays a root. `CRATONVM_GC_IR_PRIM_SLOT_ROOTS=0` keeps every such word a
// root again. The claim is spent for an optimizing-tier body at a safepoint
// whose map is found; the single-pass tier's list (the current operand stack
// only) is spent through the same arm under its own opt-in,
// `CRATONVM_GC_SP_STACK_PRIM_ROOTS` (gch h4/jm, `sp_stack_prim_claim_enabled`).

/// `CRATONVM_GC_IR_PRIM_SLOT_ROOTS` -- default ON (the words are dropped),
/// `=0` keeps them. Read once per chain walk
/// ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn ir_prim_slot_claim_enabled() -> bool {
    true
}

/// `CRATONVM_GC_SP_STACK_PRIM_ROOTS=1` -- default on (gch h4/jm,
/// `gch-h3yc-proposal-single-pass-non-oop-operand-slots-are-not-roots-REFUTED-20261015`):
/// the claim above for a SINGLE-PASS frame, from the list its exact maps
/// publish (`OopMapEntry::non_oop_stack_slots`), folded fail-closed by
/// [`cratonvm_jit::SpStackPrimFold`] (every map of the id exact and
/// agreeing, no listed slot also named, ascending, all in `operand-spill`).
/// Read once per chain walk ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn sp_stack_prim_claim_enabled() -> bool {
    true
}

// The `Prim`-colour offsets of an optimizing-tier frame at its active
// safepoint, ascending when the producer sorted them (a lookup that misses
// keeps the word, the safe direction); `None` claims nothing.
// gcf f2/f: the scan asks `frame_claim_facts`; the tests hold it to this.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn ir_prim_slots(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<&[i16]> {
    if !cm.used_ir_backend || !ir_prim_slot_claim_enabled() {
        return None;
    }
    let sp_id = active_safepoint_id(rbp, cm)?;
    cm.oop_maps
        .iter()
        .find(|m| m.bytecode_pc == sp_id)
        .map(|m| m.non_oop_stack_slots.as_slice())
        .filter(|s| !s.is_empty())
}

// JIT round 25 (item 3 of the round-24 redesign; the page is
// `docs/feature-designs/real-jdk-mode.md`) -- the claim
// above for the WHOLE BODY, at every poll.
//
// The claim above is spent only at a safepoint whose map resolved. At the
// entry poll, and at any other point no safepoint id is current for (a lean
// frame, a stale or zero id), every claim abstains, so every object-start word
// of the band is a root -- which is why the optimizing tier's prologue had to
// zero the whole frame (`ir_lower.rs`, `emit_fresh_frame_clear_sse`, ~11 % of
// the executed instructions of `regex-groups`). But the `Prim`-colour list is
// a property of the body, not of the safepoint: it is one list, cloned onto
// every map (`Lowerer::prim_slot_offsets`), and no value of the activation
// ever stores a reference into a `Prim` colour, at any point. Under
// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` (default ON since R25-FLIP) the JIT publishes it once more,
// per body (`CompiledMethod::ir_prim_colour_claim`), and leaves those words out
// of the fresh-frame clear; the collector then spends it at every frame of
// that body on the registered chain, id or no id.
//
// Exactly the semantics of the per-map claim, nothing wider: only an object
// start that is not a minted handle is let go of (a derived-looking word still
// captures its G1 region pin), it is withdrawn with the per-map claim
// (`CRATONVM_GC_IR_PRIM_SLOT_ROOTS=0`), and a frame the A5 pass recovered by
// inspection does not get it (a misread frame's claim would hide another
// frame's words -- the reference-free floor's rule). The moving-young verifier
// (`band_has_unpublished_young_word`) and the two opt-in unnamed-word passes
// (`pin_unnamed_frame_refs`, `remap_unmapped_frame_dupes`) honour it too: a
// stale young word there is residue nobody reads, not an unpublished root, and
// a primitive is never to be rewritten.
//
// What still roots such a word: the layout-free paths (the whole-band
// fallback, a foreign innermost frame, the frozen-peer scan of
// `xt_root_scan.rs`, the plain arm under `CRATONVM_GC_DEAD_SPILL_ROOTS=0`) --
// all non-moving, so a stale word there is retention, as for every word the
// clear never covered (the Rust frames).

/// The per-body `Prim`-colour claim of `cm` (see the section above), when it
/// may be spent: the body carries one, `on` (the per-map claim's switch,
/// [`ir_prim_slot_claim_enabled`]), the frame is on the registered chain
/// (`on_chain`), and the list is one the band scan's binary search can read
/// (strictly ascending, positive offsets) -- anything else claims nothing,
/// the fail-closed answer.
pub(in crate::jit::conservative_roots) fn ir_prim_colour_claim(
    cm: &cratonvm_jit::CompiledMethod,
    on: bool,
    on_chain: bool,
) -> Option<&[i16]> {
    let claim: &[i16] = &cm.ir_prim_colour_claim;
    if claim.is_empty() || !on || !on_chain || !cm.used_ir_backend {
        return None;
    }
    if claim[0] <= 0 || !claim.windows(2).all(|w| w[0] < w[1]) {
        return None;
    }
    Some(claim)
}

/// [`ir_prim_colour_claim`] for a frame on the registered chain, reading the
/// switch only for a body that carries a claim (every other body costs one
/// length test).
pub(in crate::jit::conservative_roots) fn ir_prim_colour_claim_on_chain(cm: &cratonvm_jit::CompiledMethod) -> Option<&[i16]> {
    if cm.ir_prim_colour_claim.is_empty() {
        return None;
    }
    ir_prim_colour_claim(cm, ir_prim_slot_claim_enabled(), true)
}

/// Is `[rbp - off]` a word of `claim` ([`ir_prim_colour_claim`])?
#[inline]
pub(in crate::jit::conservative_roots) fn ir_prim_colour_claimed(claim: Option<&[i16]>, off: i64) -> bool {
    claim.is_some_and(|c| i16::try_from(off).is_ok_and(|o| c.binary_search(&o).is_ok()))
}

/// JIT round 25 (item 3): the frames whose band scan spent the per-body
/// `Prim`-colour claim where the per-map one abstained (no current safepoint
/// id, or no map for it), cumulative. The engagement counter of
/// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`: `0` with the switch on means no
/// such frame was scanned, and the run says nothing about the claim.
pub fn ir_prim_colour_claim_census() -> usize {
    LIVESET_CENSUS[IR_PRIM_BODY_CLAIM_FRAMES].load(Ordering::Relaxed)
}

// JIT round 25 wave 4 -- a per-body claim word is ignored WHOLE.
//
// Until wave 4 a claimed word was let go of only when it was an object start;
// any other word fell through to the root/pin half of the scan, so on G1 a
// derived-looking word -- one pointing INTO a region without being an object
// start -- still captured that region's pin (`g1_band_reject_pins`). That was
// harmless while the prologue zeroed every word it now leaves out: the word
// read 0. Under the per-body claim the clear leaves the claimed words alone,
// so they keep whatever an earlier, deeper activation left there -- among it
// the interior addresses of that activation's dead arrays. G1 then pinned the
// regions of those dead arrays at every collection the frame was live for,
// and `NativeGrowthReclaimProbe -Xmx128m -XX:+UseG1GC` read `alAdd=3/4 ...
// PROBE-FAIL (sink 1)` in 1-3 runs out of 15 with each wave-2/3 class on top of
// the claim (the claim alone: 20/20 OK).
//
// The per-body claim is the body's statement that NO word of the list ever
// holds a reference or a derived pointer of this activation at any point the
// collector can look (a `Prim` colour, a claimable `Pinned` colour, the deopt
// GPR image, the System V helper area: `ir_lower.rs`,
// `Lowerer::body_claim_offsets_with`). Compiled homes keep no derived pointer
// across a GC point. So such a word is neither a root nor a pin nor a
// publication, whatever its bits look like -- with ONE exception kept from the
// item-3 rule: a minted handle in a `long` stays a root
// (`smuggled_longs::is_minted`). The per-map list (`non_oop_stack_slots`) keeps
// the object-start-only rule: it is spent only at a resolved safepoint, where
// the prologue's clear (no body claim) still zeroed its words.
//
// Under the switches of the claim it rides on: the body carries one only under
// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`, and the scan spends it only under
// `CRATONVM_GC_IR_PRIM_SLOT_ROOTS` (default on). The moving-young verifier and
// the two opt-in unnamed-word passes already skipped every claimed word.
// What still sees such a word: the layout-free scans (`scan_one_frame`: the
// whole-band fallback, a foreign innermost frame, the plain arm) and the
// frozen- / blocked-peer helper windows, which root and reject-pin every word
// of the band they read -- non-moving retention, as for the Rust frames.

/// Is `list` -- the `ir_prims` list a frame's scan is spending -- `cm`'s own
/// per-body claim (the list the caller substitutes for the map's at every
/// poll), so its words are ignored whole? Compared by identity (address and
/// length), never by contents: a map's equal list keeps the map's rule.
pub(in crate::jit::conservative_roots) fn ir_body_claim_ignores_word(cm: &cratonvm_jit::CompiledMethod, list: &[i16]) -> bool {
    let body: &[i16] = &cm.ir_prim_colour_claim;
    !body.is_empty() && cm.used_ir_backend && std::ptr::eq(body, list)
}

/// JIT round 25 wave 4: the per-body claim words the scan ignored that the
/// pre-wave-4 rule would have reject-pinned (not object starts), cumulative.
/// The engagement counter of the whole-word rule.
pub fn ir_body_claim_unpinned_census() -> usize {
    LIVESET_CENSUS[IR_BODY_CLAIM_UNPINNED].load(Ordering::Relaxed)
}

// gce e1/f (2026-09-29) -- the seventh band claim: a SINGLE-PASS frame's
// java-local word that no map of its active safepoint names.
//
// Found on `NativeGrowthReclaimProbe` (item 3 of
// `docs/feature-designs/real-jdk-mode.md`,
// `docs/feature-designs/deopt-osr.md`).
// The census on the `adb9178bc` build named the holder `method=
// NativeGrowthReclaimProbe.main off=96 region=java-local tier=sp sp=258
// in_map=false`: local 11 of the OSR'd `main`, the PREVIOUS round's 30 %
// `char[]`, read by this scan at the next round's `fillAndDrop` call. At bci
// 258 local 11 is an `int` on the path in from the `alAdd` row and a `char[]`
// on the back edge, so the verifier merges it to `top` and no bytecode can
// read it before the `astore 11` at 268. The interpreter (by liveness) and
// HotSpot drop it; the map does not name it; only this conservative read kept
// it, and the round's allocation failed.
//
// The claim. A single-pass body homes local `k` at `[rbp - 8*(k+1)]`
// (`x64::Compiler::local_offset`, `FrameLayout::java_locals_hi`). Every map of
// a safepoint names in `frame_slot_offsets` the home of each local the
// must-be-an-oop dataflow proves a reference there (Stage 2 of
// `emit_oop_map_for_safepoint`), plus any operand-stack entry that aliases a
// home. `OopMapEntry::local_oop_mask` is `Some` only when that dataflow
// REACHED the pc and the method has at most 64 locals (one window), i.e. only
// when Stage 2 described every local. So when every map of the active id
// carries the oracle, a java-local word none of them names holds no reference
// the frame will read: its local is not definitely a reference here, and the
// verifier refuses an `aload` of such a local before it is stored again.
//
// The band verifier already spends the same statement
// (`band_slot_is_verifiable_with_map`: a modelled-region word the map does not
// name is dead, default on since 2026-08-29), so a moving collection already
// leaves such a word unrewritten. Dropping the root is the STRONGER use --
// wrong, it frees instead of leaving a stale word -- hence its own switch,
// `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0`, and the oracle
// (`CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`, `why = "sp-local-map"`).
//
// Who else reads the word. A deopt snapshot at a pc bytecode liveness covers
// describes a dead local as `Undefined` (`build_frame_state_at`), and a local
// that is live there is Ref-typed on every path, so the mask names it. The one
// arm that can still read an unnamed home as a reference is
// `typed_local_frame_value`'s whole-method `LocalKind::Ref` at a pc liveness
// does NOT cover; it would resume a dead local of the interpreter frame with
// the stale word. That residual is recorded on the page above.
//
// Refused (claim nothing): an optimizing-tier body (its homes are colours, not
// `8*(k+1)`, and it has its own claims), a frame with no readable safepoint id,
// an id with no map, and an id any of whose maps lacks the oracle.

/// `CRATONVM_GC_SP_LOCAL_MAP_ROOTS` -- default ON (unnamed single-pass
/// java-local words are dropped), `=0` keeps them roots. Read once per chain
/// walk ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn sp_local_map_claim_enabled() -> bool {
    true
}

/// What the active safepoint's maps say about a single-pass frame's java
/// locals: bit `k` of `named` = some map names local `k`'s home, and
/// `num_locals` bounds the locals the maps describe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct SpLocalClaim {
    pub(in crate::jit::conservative_roots) named: u64,
    pub(in crate::jit::conservative_roots) num_locals: u16,
}

impl SpLocalClaim {
    /// The claim for safepoint `sp_id` out of a body's maps, or `None` (claim
    /// nothing): no map carries the id, or one of them lacks the locals oracle
    /// or describes no locals, or more than 64.
    pub(in crate::jit::conservative_roots) fn from_maps(maps: &[cratonvm_jit::OopMapEntry], sp_id: u32) -> Option<Self> {
        let mut acc = SpLocalAcc::new();
        for m in maps.iter().filter(|m| m.bytecode_pc == sp_id) {
            acc.add(m);
            if acc.refused {
                return None;
            }
        }
        acc.finish()
    }

    /// Is the word at `[rbp - off]` a java-local home this claim proves dead?
    pub(in crate::jit::conservative_roots) fn proves_dead(&self, layout: &cratonvm_jit::FrameLayout, off: i32) -> bool {
        if !layout_is_java_local(layout, off) {
            return false;
        }
        let Some(k) = sp_java_local_index(off) else {
            return false;
        };
        k < u32::from(self.num_locals) && self.named & (1u64 << k) == 0
    }
}

/// The fold [`SpLocalClaim::from_maps`] runs over the maps of one id, as a
/// value, for [`frame_claim_facts`]'s single pass (gcf f2/f). Once `refused`,
/// the claim is `None` whatever later maps say.
pub(in crate::jit::conservative_roots) struct SpLocalAcc {
    pub(in crate::jit::conservative_roots) named: u64,
    pub(in crate::jit::conservative_roots) num_locals: u16,
    pub(in crate::jit::conservative_roots) any: bool,
    pub(in crate::jit::conservative_roots) refused: bool,
}

impl SpLocalAcc {
    pub(in crate::jit::conservative_roots) fn new() -> Self {
        SpLocalAcc {
            named: 0,
            num_locals: u16::MAX,
            any: false,
            refused: false,
        }
    }

    pub(in crate::jit::conservative_roots) fn add(&mut self, m: &cratonvm_jit::OopMapEntry) {
        if self.refused {
            return;
        }
        let Some(mask) = m.local_oop_mask else {
            self.refused = true;
            return;
        };
        if m.num_locals == 0 || m.num_locals > 64 {
            self.refused = true;
            return;
        }
        self.any = true;
        self.named |= mask;
        self.num_locals = self.num_locals.min(m.num_locals);
        for &off in &m.frame_slot_offsets {
            if let Some(k) = sp_java_local_index(i32::from(off)) {
                self.named |= 1u64 << k;
            }
        }
    }

    pub(in crate::jit::conservative_roots) fn finish(self) -> Option<SpLocalClaim> {
        (!self.refused && self.any).then_some(SpLocalClaim {
            named: self.named,
            num_locals: self.num_locals,
        })
    }
}

/// The local index whose single-pass home is `[rbp - off]`, when `off` is one
/// of the first 64 homes (`8*(k+1)`).
pub(in crate::jit::conservative_roots) fn sp_java_local_index(off: i32) -> Option<u32> {
    if off <= 0 || off % 8 != 0 {
        return None;
    }
    let k = u32::try_from(off / 8 - 1).ok()?;
    (k < 64).then_some(k)
}

// [`SpLocalClaim`] for the single-pass frame at `rbp`, or `None`.
// gcf f2/f: the scan asks `frame_claim_facts`; the tests hold it to this.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn sp_local_claim(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<SpLocalClaim> {
    if cm.used_ir_backend || !sp_local_map_claim_enabled() {
        return None;
    }
    let sp_id = active_safepoint_id(rbp, cm)?;
    SpLocalClaim::from_maps(&cm.oop_maps, sp_id)
}

// gce e1/f (2026-09-29) -- the eighth band claim: a single-pass frame's
// frame-deopt `SavedRegisters` GPR image that THIS activation never wrote.
//
// The second holder of the same probe. With the java-local word above let go
// (measured with `CRATONVM_JIT_BAND_SKIP=java-local` on `adb9178bc`), the
// same 30 % `char[]` was next read from `method=NativeGrowthReclaimProbe.
// fillAndDrop off=672 region=deopt-saved-gpr-image` -- the innermost frame,
// which had never deoptimized. Skipping both classes gave
// `toCharArray=4/4` where the default run gives 1-2/4.
//
// The block is written by exactly one thing: the body's own deopt stub
// (`x64/deopt_stubs.rs`, `emit_saved_register_spill`, and the shared tail's
// `MOV [rbp - base], RAX` before it), which spills all sixteen GPRs at
// `[rbp - (base - 8r)]` and then calls the VM (`x64_deopt_entry`, or
// `jit_throw_aioobe` first for reason 11) with the block live. The prologue
// never initialises it. So in a frame whose stub has not run, the block holds
// whatever an earlier, deeper frame left at that stack depth -- here the
// frame of the `toCharArray` call, which had the new array in a register.
//
// The stub's spill includes `gpr[5]`, RBP, whose value inside the stub is
// this frame's own `rbp`. So a block whose RBP image is not `rbp` was not
// written by this activation, and none of its words is read again: they are
// not roots. A block whose RBP image IS `rbp` is kept whole -- that is every
// live block, and also a leftover from an earlier activation at the same
// depth, which merely stays conservative.
//
// Single-pass bodies only as written here (gce e1/f): the optimizing tier's
// block was believed to sit among its prologue saves. It does not; the
// optimizing tier's own arm is the gcf f5/f section below (default ON since
// gcf f6/f). A single-pass callee-saved image word is still excluded.
// `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0` keeps the whole block a root, on
// both tiers.

/// `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS` -- default ON (a block this
/// activation never wrote is not a root), `=0` keeps it. Read once per chain
/// walk ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn deopt_image_residue_claim_enabled() -> bool {
    true
}

/// The `[rbp - off]` offset of the RBP image (`gpr[5]`) in `layout`'s
/// frame-deopt GPR block, when the frame reserved one.
pub(in crate::jit::conservative_roots) fn deopt_image_rbp_slot(layout: &cratonvm_jit::FrameLayout) -> Option<i32> {
    if layout.deopt_gpr_hi <= layout.deopt_gpr_lo {
        return None;
    }
    // `deopt_gpr_hi = base + 8` (`gpr[0]` at `[rbp - base]`), `gpr[r]` at
    // `[rbp - (base - 8r)]`.
    let off = layout.deopt_gpr_hi - 8 - 5 * 8;
    layout.is_deopt_saved_gpr_image(off).then_some(off)
}

/// Does the single-pass frame at `rbp` hold a frame-deopt GPR block its own
/// stub never wrote (`rbp_image`, the word at [`deopt_image_rbp_slot`], is not
/// `rbp`)? Pure over the read word, for the tests.
pub(in crate::jit::conservative_roots) fn deopt_image_is_residue(
    cm_is_single_pass: bool,
    layout: &cratonvm_jit::FrameLayout,
    rbp: usize,
    rbp_image: Option<usize>,
) -> bool {
    cm_is_single_pass
        && deopt_image_rbp_slot(layout).is_some()
        && rbp_image.is_some_and(|w| w != rbp)
}

// [`deopt_image_is_residue`] for the live frame at `rbp` (reads its RBP
// image), gated by the switch.
// gcf f2/f: the scan asks `frame_claim_facts`; the tests hold it to this.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn frame_deopt_image_is_residue(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> bool {
    // gcg g1/f: and the never-roots claim, in the order `frame_claim_facts_with`
    // asks them.
    ir_deopt_image_is_dead_with(
        cm,
        deopt_image_residue_claim_enabled(),
        ir_deopt_image_never_roots_enabled(),
    ) || frame_deopt_image_is_residue_with(
        rbp,
        cm,
        deopt_image_residue_claim_enabled(),
        ir_deopt_image_residue_claim_enabled(),
    )
}

// gcf f5/f (2026-10-01) -- the eighth claim for the OPTIMIZING tier
// (`CRATONVM_GC_IR_DEOPT_IMAGE_RESIDUE_ROOTS=1`, opt-in).
//
// vf3 named the holder of `NativeGrowthReclaimProbe -Xmx128m
// -XX:NewRatio=2` (`gcf-f3c-ngr-nr2-...`): `java/util/ArrayList.add off=360
// region=callee-saved-gpr-image tier=ir in_map=false`, a 28.9 MB `[B`
// (+21 MB young) -- the `fillAndDrop` garbage of an earlier, deeper frame,
// read through a word of the `ArrayList.add` activation that `main` calls,
// with no `reg=` suffix, i.e. NOT one of the body's published prologue
// saves. On this backend `FrameLayout::callee_saved_*` spans the whole tail,
// so every tail word reports as `callee-saved-gpr-image`; the tail is, from
// shallow to deep: the XMM and GPR prologue save bands, the 256-byte deopt
// `SavedRegisters` block, the argument staging region (`ir_lower.rs`, the
// `spill_cap_off` arithmetic in `Lowerer::new`). The caller-register rule
// (`caller_register_claim`) speaks for the published save slots only, and
// the single-pass residue claim refused every optimizing body on the belief
// that its deopt block "sits inside its callee-saved band, where a word may
// be a live prologue save". It does not: the block is DEEPER than both save
// bands, and is written by exactly one thing, the body's own deopt stub,
// which spills all sixteen GPRs at `gpr[r] -> [rbp - (base - 8r)]`, RBP
// (`gpr[5]`) included, before calling the VM with the block live
// (`ir_lower.rs`, the `store_abi_reg(r, deopt_regs_base - 8r)` loop) -- the
// single-pass convention exactly. So the same test applies: a block whose
// RBP image is not this frame's `rbp` was never written by this activation
// and none of its words is read again. As a second fence a word that IS a
// published prologue save slot (`ir_saved_gpr_slots`) is never claimed.
//
// gcf f6/f (2026-10-01): DEFAULT ON, and OSR bodies included. The f5 form
// also refused every body that publishes no save list -- an OSR-capable body
// that saves registers (`ir_lower.rs`: `ir_saved_gpr_slots` is `None` exactly
// then -- gcg g1/f: wrong, that `None` keys on the single-pass
// `osr_pc_to_native`, which no optimizing body sets, so the list is published
// on every optimizing body) -- on the belief that such a body's block might hold a save. It
// cannot: the OSR entry stub saves the caller's registers into the SAME save
// bands the prologue uses (`emit_osr_entry_stubs`, "The caller's registers,
// exactly as `emit_prologue` saves them"), and those bands end where the
// block begins (`Lowerer::new`; `ir_tail_word` names the geometry and
// `gcf_f6f_the_ir_tail_words_are_named_by_the_layout_arithmetic` pins it). The
// refusal, like the f5 opt-in default, leaves this word of the f5 regression
// holder rooted when it is a deopt-image word:
// `GenR4W6JitOomRootProbe.caseOomeCompiledCallee:()V off=520
// region=callee-saved-gpr-image tier=ir in_map=false`, the OSR body of the
// case (every `case*` method of that probe is entered once and OSR'd). With
// its live cursor at 264 the word at 520 cannot be a staging or helper word
// (staging starts past `spill_cap + saves + 256 >= 520`), so it is a save or
// a deopt-image word; the save bands (the caller's registers at the entry)
// and an unwritten deopt block (stack residue) both hold
// only values from before the OSR entry, and the measured chain did
// not exist then -- the word NAMES a chain node because its stale address was
// reused by one (same-size `Node`s, promoted into freed old space), which is
// the conservative scan's false retention, varying with the allocation
// layout from run to run (0/20 on f3g, 7/20 on f5d). That last step is the
// likeliest reading, not a measurement; the census suffix (`ir_tail_detail`:
// `tail=deopt-gpr[..] img=other`, `img=self` or `tail=save`) says which word
// it is on the next failing run, and only the first is this claim's.
//
// f6/f flipped the default with the same argument the single-pass claim has
// been default ON on since gce e1/f, and the f6 battery reverted it: 20+20
// interleaved JitOomRoot runs failed 4/20 on and 3/20 off, and vf5's `_ird`
// arms differed from their `_def` twins in 5 of 7 groups (both ways). The
// argument: the block's only reader is `ir_deopt_entry`,
// called by this frame's own stub right after it spilled all sixteen GPRs,
// RBP included, so a block whose RBP image is not `rbp` is read by nothing
// before it is rewritten; dropping it drops its pin too, which is safe for a
// word nothing reads (the rule of `f5-f-report.md`, part 2).
// `CRATONVM_GC_IR_DEOPT_IMAGE_RESIDUE_ROOTS=1` opts in (OSR bodies included
// since f6); `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0` still turns both tiers'
// claim off.

/// `CRATONVM_GC_IR_DEOPT_IMAGE_RESIDUE_ROOTS` -- opt-in (default OFF; the f6
/// flip was reverted by its battery): the frame-deopt residue claim
/// for optimizing-tier bodies too. Read once per chain walk
/// ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn ir_deopt_image_residue_claim_enabled() -> bool {
    false
}

// gcg g1/f (2026-10-01) -- an optimizing-tier body's deopt register block is
// dead at EVERY scan of its frame, whatever its RBP image says
// (`CRATONVM_GC_IR_DEOPT_IMAGE_NEVER_ROOTS`; opt-in until gcg g4/jm's vg2/f
// verdict flipped it default ON, `=0` restores the residue claim alone).
//
// The residue claim above drops a block whose RBP image is not `rbp` and keeps
// one whose image IS `rbp` (`img=self`), because it could not tell this
// activation's live block from one an EARLIER activation at the same address
// wrote. That case is the common one for a callee a loop calls: every
// activation of `ArrayList.add` that `NativeGrowthReclaimProbe.main` makes sits
// at the same `rbp`, so one deopt of one of them stamps the block with that
// `rbp` for every later activation, and its sixteen GPR words -- `fillAndDrop`'s
// array among them -- stay roots (`gcf-f5f-ngr-nr2-...`: the claim engaged and
// the `ArrayList.add off=360/376` holder persisted, which is what `img=self`
// predicts; by the layout arithmetic of `Lowerer::new` both offsets fall in the
// GPR half for any spill cap near the census's `live_hi=136`).
//
// The per-activation stamp the f6/f page asked for is not needed, because the
// block has no live window a scan can see. It is written in exactly one place,
// the body's shared deopt stub (`ir_lower.rs`, `emit_deopt_stub`), which
// spills the sixteen GPRs and sixteen XMMs, makes ONE call --
// `ir_deopt_entry` -- and then tears the frame down and returns. Its one reader
// is `ir_deopt_entry` (`jit/src/deopt.rs`) during that call, which reads the
// block into a `ReconstructedFrame` (`reconstruct_frame_from_machine_state`),
// pushes it on a thread-local stash and returns: pure Rust over the jit crate,
// no poll, no allocation on the Java heap, no VM call, so no collection and
// no root snapshot can run while the block is live, on this thread or (a
// thread in Rust code is neither frozen in compiled code nor blocked) from
// another. Every scan that applies the band claims therefore sees the frame at
// a safepoint of its body, where the block is either never written or left
// over from a deopt that has finished. The references the deopt needs travel
// in the stash, which is rooted on its own (`for_each_stashed_deopt_object`).
// The two source pins in the tests hold `emit_deopt_stub` to its one call and
// `ir_deopt_entry` to its read-and-stash body.
//
// Fences, as for the residue claim: only an object start is let go of (a
// derived pointer keeps its G1 pin), a published prologue save slot is never
// claimed (`deopt_residue_word_may_be_a_save`), and
// `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0` turns it off with the rest. The
// single-pass block is not covered: its stub's resume contract is a different
// file, and its residue claim is default ON already.

/// `CRATONVM_GC_IR_DEOPT_IMAGE_NEVER_ROOTS` -- DEFAULT ON since gcg g4/jm's
/// vg2/f verdict (`=0` restores the residue claim alone). Pooled g1a + g2a
/// interleaves: `oome-compiled-callee` failed 5/20 on the default arm and 0/20
/// with the claim; no soundness row moved. Read once per chain walk
/// ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn ir_deopt_image_never_roots_enabled() -> bool {
    true
}

/// Does the never-roots claim drop `cm`'s deopt GPR block (see the section
/// above)? `enabled` is the umbrella residue switch, `never` this claim's.
/// Pure over the compiled method: no frame word is read.
pub(in crate::jit::conservative_roots) fn ir_deopt_image_is_dead_with(
    cm: &cratonvm_jit::CompiledMethod,
    enabled: bool,
    never: bool,
) -> bool {
    enabled && never && cm.used_ir_backend && deopt_image_rbp_slot(&cm.frame_layout).is_some()
}

/// May the frame-deopt GPR-image word at `off` of `cm`'s frame also be a
/// prologue save of a caller's register (and so be kept by the residue
/// claim)? Single-pass: whenever the layout's callee-saved band covers it,
/// as before. Optimizing: only a published save slot -- the block lies below
/// both save bands, so with no published list no block word is one (gcf
/// f6/f; see the section above). gcg g1/f correction: f6/f read the list as
/// missing on every OSR-capable body, but `ir_lower.rs` withholds it only when
/// `osr_pc_to_native` is set, which is the single-pass OSR table; an
/// optimizing body's OSR entries are `ir_osr_entries`, so every optimizing
/// body, OSR or not, publishes its list (an empty one when it saves nothing).
pub(in crate::jit::conservative_roots) fn deopt_residue_word_may_be_a_save(cm: &cratonvm_jit::CompiledMethod, off: i32) -> bool {
    if !cm.used_ir_backend {
        return is_callee_saved_gpr_image(off, &cm.frame_layout);
    }
    match cm.ir_saved_gpr_slots.as_ref() {
        Some(slots) => slots.iter().any(|&(_, o)| o == off),
        None => matches!(ir_tail_word(&cm.frame_layout, off), Some(IrTailWord::Save)),
    }
}

/// [`frame_deopt_image_is_residue`] with the switch already read
/// ([`BandClaimSwitches`]).
pub(in crate::jit::conservative_roots) fn frame_deopt_image_is_residue_with(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    enabled: bool,
    ir_enabled: bool,
) -> bool {
    // gcf f5/f: an optimizing body only under its own switch. gcf f6/f: with
    // or without a published save list (see `deopt_residue_word_may_be_a_save`).
    if !enabled || (cm.used_ir_backend && !ir_enabled) {
        return false;
    }
    let layout = &cm.frame_layout;
    let rbp_image = deopt_image_rbp_slot(layout).and_then(|off| {
        let frame_size = usize::try_from(cm.osr_frame_size).ok()?;
        let off = usize::try_from(off).ok()?;
        if off > frame_size || off > rbp {
            return None;
        }
        let addr = rbp - off;
        // SAFETY: an aligned word inside this thread's own live compiled
        // frame, `[rbp - frame_size, rbp)`, the band the scan itself reads.
        (addr & 7 == 0).then(|| unsafe { (addr as *const usize).read() })
    });
    deopt_image_is_residue(true, layout, rbp, rbp_image)
}

/// x86-64 GPR name by encoding number. Diagnostics only.
pub(in crate::jit::conservative_roots) fn gpr_name(reg: u8) -> &'static str {
    const NAMES: [&str; 16] = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    NAMES.get(usize::from(reg)).copied().unwrap_or("?")
}

/// gen r5w6/oomjit10: the provenance suffix of a register-image band word --
/// which register, and the claim the scan held about it -- so a census holder
/// line in `safepoint-gpr-spill-image` or `callee-saved-gpr-image` says what
/// kept it. Empty for every other region. Diagnostics only (called under
/// `record_provenance_for`).
pub(in crate::jit::conservative_roots) fn register_image_detail(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    reg_mask: Option<u16>,
    walk_hi: usize,
) -> String {
    if let Some(name) = cm.frame_layout.spill_image_register(off) {
        return match reg_mask {
            Some(m) => format!(" reg={name} mask=0x{m:x}"),
            None => format!(" reg={name} mask=none"),
        };
    }
    let Some(reg) = callee_saved_image_register(cm, off) else {
        // zwr: a save-area word no published save names -- say what the
        // body saves and where its save area lies.
        if is_callee_saved_gpr_image(off, &cm.frame_layout) {
            let l = &cm.frame_layout;
            return format!(
                " reg=none saved={:?} sp_saved={:?} cs=[{},{}) xmm=[{},{}) fs={}",
                cm.ir_saved_gpr_slots,
                cm.osr_callee_saved_regs,
                l.callee_saved_lo,
                l.callee_saved_hi,
                l.xmm_saved_lo,
                l.xmm_saved_hi,
                cm.osr_frame_size,
            );
        }
        return String::new();
    };
    // SAFETY: `rbp + 8` is the validated frame's return-address slot.
    let ret = unsafe { ((rbp + 8) as *const usize).read() };
    let caller = match cratonvm_jit::lookup_jit_code_range(ret) {
        Some(p) => {
            // SAFETY: as in `caller_register_claim`.
            let p: &cratonvm_jit::CompiledMethod =
                unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
            format!("jit:{}", p.method_label)
        }
        None => "native".to_string(),
    };
    let claim = if walk_hi > rbp {
        format!("{:?}", caller_register_claim(rbp, reg, walk_hi))
    } else {
        "no-walk".to_string()
    };
    format!(" reg={} caller={caller} claim={claim}", gpr_name(reg))
}

/// gcd d1/b: the provenance suffix every rooted band word carries -- which
/// tier compiled the frame (`tier=ir` optimizing, `tier=sp` single-pass), its
/// active safepoint id, the live cursor the scan used (`live_hi=`, `none` =
/// the whole band was read) and whether a map of that safepoint names the word
/// (`in_map=`). Both tiers have an `operand-spill` region, and the census could
/// not tell them apart: `GenR5W2OsrDeadSlotProbe`'s
/// `cleared off=160 region=operand-spill` holder was read as an optimizing-tier
/// home while the same method in `GenR5W3OsrHolderProbe` was single-pass
/// (`docs/feature-designs/deopt-osr.md`).
/// A word `in_map=true` is one the compiler itself calls live there; a word
/// `in_map=false` below the cursor is one only the conservative band read
/// keeps. Diagnostics only (called under `record_provenance_for`).
pub(in crate::jit::conservative_roots) fn band_word_context(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    live_hi: Option<i32>,
) -> String {
    let tier = if cm.used_ir_backend { "ir" } else { "sp" };
    let sp_id = active_safepoint_id(rbp, cm);
    let in_map = match (sp_id, i16::try_from(off)) {
        (Some(id), Ok(off16)) => cm
            .oop_maps
            .iter()
            .filter(|m| m.bytecode_pc == id)
            .any(|m| m.frame_slot_offsets.contains(&off16)),
        _ => false,
    };
    let sp = sp_id.map_or_else(|| "none".to_string(), |id| id.to_string());
    let hi = live_hi.map_or_else(|| "none".to_string(), |h| h.to_string());
    // gcd d7/u: WHICH activation, and who it returns to. A census line naming
    // a callee's frame after its exception was caught (the pinned-callee OOME
    // shape) cannot otherwise be told from the same method's frame during the
    // fill, and the frame's caller is what says whether it is still on the
    // path the thread will return through. The current stack pointer (`cur`)
    // is the scanner's, so `rbp - cur` says how deep below the scan the frame
    // sits.
    // SAFETY: `rbp + 8` is the validated frame's return-address slot, the same
    // read `register_image_detail` makes.
    let ret = unsafe { ((rbp + 8) as *const usize).read() };
    let caller = match cratonvm_jit::lookup_jit_code_range(ret) {
        Some(p) => {
            // SAFETY: as in `caller_register_claim`.
            let p: &cratonvm_jit::CompiledMethod =
                unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
            format!("jit:{}", p.method_label)
        }
        None => "native".to_string(),
    };
    let cur = current_stack_pointer();
    format!(
        " tier={tier} sp={sp} live_hi={hi} in_map={in_map} rbp=0x{rbp:x} above_scan=0x{:x} returns_to={caller}{}{}",
        rbp.wrapping_sub(cur),
        ir_tail_detail(rbp, cm, off),
        frame_liveness_detail(cm, sp_id, off),
    )
}

/// zwr: what the body's liveness table ([`is_dead_by_frame_liveness`]) says
/// about the word: ` live_tab=none` (no table), or whether the word is
/// covered and how the records for the active safepoint read it
/// (`recs=<n>` complete / incomplete / listing it live). Diagnostics only.
pub(in crate::jit::conservative_roots) fn frame_liveness_detail(
    cm: &cratonvm_jit::CompiledMethod,
    sp_id: Option<u32>,
    off: i32,
) -> String {
    let table = &cm.frame_liveness;
    if table.is_empty() {
        return " live_tab=none".to_string();
    }
    let Ok(off16) = i16::try_from(off) else {
        return " live_tab=off-range".to_string();
    };
    let covered = table.covered.binary_search(&off16).is_ok();
    let Some(id) = sp_id else {
        return format!(" live_tab=covered:{covered},no-sp");
    };
    let recs: Vec<_> = table.records.iter().filter(|r| r.sp_id == id).collect();
    let complete = recs.iter().filter(|r| r.complete).count();
    let listed = recs
        .iter()
        .filter(|r| r.live.binary_search(&off16).is_ok())
        .count();
    format!(
        " live_tab=covered:{covered},recs:{},complete:{complete},listed_live:{listed}",
        recs.len()
    )
}

// gcf f6/f (2026-10-01) -- WHICH word of an optimizing-tier frame's tail a
// census holder is.
//
// On this tier `FrameLayout::callee_saved_*` spans the whole tail, so every
// tail word reports as `region=callee-saved-gpr-image`, and the f5 regression
// holder (`GenR4W6JitOomRootProbe.caseOomeCompiledCallee:()V off=520
// region=callee-saved-gpr-image tier=ir in_map=false`, no `reg=`) could be a
// prologue save, a deopt register image word or an argument-staging word --
// three different writers and three different fixes. `Lowerer::new`
// (`jit/src/ir_lower.rs`) lays the tail out, shallow to deep:
//
//   (spill_cap, spill_cap + xmm + gpr]   the prologue save bands
//   [base - 248, base - 128]             deopt `SavedRegisters` XMM half
//   [base - 120, base]                   its GPR half, `gpr[r]` at `base - 8r`
//   (base, top]                          staging, `arg[i]` at `top - 8i`
//   (top, frame_size]                    the helpers' shadow space and stack
//                                        arguments (`shadow` 32 + `stack_arg_reserve` 16)
//
// with `callee_saved_lo = spill_cap`, `deopt_gpr_hi = base + 8` and
// `top = args_stage_top_off = frame_size - 48` (the abutment
// `deopt_regs_base == args_stage_top_off - args_stage_size` is a debug
// assertion there). A deopt GPR word also says whether the block's RBP image
// is this frame's `rbp` (`img=self`: its own stub wrote it, or a previous
// activation at the same address did) or not (`img=other`: residue the
// activation never wrote, the residue claim's case). Diagnostics only.

/// The helpers' shadow space plus their stack-argument reserve at the deep end
/// of an optimizing-tier frame (`ir_lower.rs`, `Lowerer::new`: `shadow = 32`,
/// `stack_arg_reserve = 16`).
pub(in crate::jit::conservative_roots) const IR_HELPER_ARG_AREA_BYTES: i32 = 48;

/// One word of an optimizing-tier frame's tail (see the section above).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum IrTailWord {
    /// The prologue's callee-saved XMM / GPR save bands.
    Save,
    /// The deopt `SavedRegisters` XMM half.
    DeoptXmm,
    /// The deopt `SavedRegisters` GPR half: `gpr[r]`.
    DeoptGpr(u8),
    /// Argument-staging word `arg[i]`.
    Staging(u32),
    /// The helpers' shadow space / stack-argument reserve.
    HelperArgs,
}

/// Which tail word `off` is in an optimizing-tier layout. `None` for a word
/// shallower than the tail (`off <= callee_saved_lo`), past the frame, or a
/// layout that reserved no deopt image (the tail's geometry is then not
/// recoverable from the layout).
pub(in crate::jit::conservative_roots) fn ir_tail_word(layout: &cratonvm_jit::FrameLayout, off: i32) -> Option<IrTailWord> {
    let fs = layout.frame_size;
    if layout.deopt_gpr_hi <= layout.deopt_gpr_lo || off <= layout.callee_saved_lo || off > fs {
        return None;
    }
    let base = layout.deopt_gpr_hi - 8;
    let top = fs - IR_HELPER_ARG_AREA_BYTES;
    if top < base {
        return None;
    }
    if off > top {
        return Some(IrTailWord::HelperArgs);
    }
    if off > base {
        // Cast: `top - off` is in `[0, top - base)`, a small positive offset.
        return Some(IrTailWord::Staging(((top - off) / 8) as u32));
    }
    if layout.is_deopt_saved_gpr_image(off) {
        // Cast: `base - off` is in `[0, 120]`, so the index is `0..=15`.
        return Some(IrTailWord::DeoptGpr(((base - off) / 8) as u8));
    }
    if layout.is_deopt_saved_xmm_image(off) {
        return Some(IrTailWord::DeoptXmm);
    }
    Some(IrTailWord::Save)
}

/// The provenance suffix naming an optimizing-tier tail word
/// ([`ir_tail_word`]): ` tail=save|deopt-xmm|deopt-gpr[reg] img=self|other|
/// stage[i]|helper-args fs=<frame_size>`, or empty for any other word or tier.
/// Diagnostics only (called under `record_provenance_for`).
pub(in crate::jit::conservative_roots) fn ir_tail_detail(rbp: usize, cm: &cratonvm_jit::CompiledMethod, off: i32) -> String {
    if !cm.used_ir_backend {
        return String::new();
    }
    let layout = &cm.frame_layout;
    let Some(word) = ir_tail_word(layout, off) else {
        return String::new();
    };
    let what = match word {
        IrTailWord::Save => "save".to_string(),
        IrTailWord::DeoptXmm => "deopt-xmm".to_string(),
        IrTailWord::DeoptGpr(r) => {
            // Cast: `deopt_image_rbp_slot` answers a positive in-frame offset.
            let img = match deopt_image_rbp_slot(layout).map(|slot| slot as usize) {
                Some(slot) if rbp >= slot => {
                    // SAFETY: the RBP image slot lies inside the validated
                    // frame's band (`deopt_image_rbp_slot` checked it against
                    // the layout), the word the band scan itself reads.
                    let w = unsafe { ((rbp - slot) as *const usize).read() };
                    if w == rbp {
                        "self"
                    } else {
                        "other"
                    }
                }
                _ => "none",
            };
            format!("deopt-gpr[{}] img={img}", gpr_name(r))
        }
        IrTailWord::Staging(i) => format!("stage[{i}]"),
        IrTailWord::HelperArgs => "helper-args".to_string(),
    };
    // gcg g1/f: an optimizing body publishes its OSR entries in
    // `ir_osr_entries`, never in `osr_pc_to_native` (the single-pass table),
    // so the marker read only the latter was absent on every IR OSR frame.
    let osr = if !cm.ir_osr_entries.is_empty() || cm.osr_pc_to_native.is_some() {
        " osr"
    } else {
        ""
    };
    format!(" tail={what} fs={}{osr}", layout.frame_size)
}

/// `(words excluded by the liveness claim, contradictions)`, cumulative. The
/// second must be ZERO: a non-zero count means the emitter published a word
/// as dead at a safepoint whose own map lists it as a reference slot, i.e. the
/// two halves of one compile disagree, and every drop by the claim is suspect.
pub fn frame_liveness_census() -> (usize, usize) {
    (
        LIVESET_CENSUS[LIVESET_WORDS_EXCLUDED].load(Ordering::Relaxed),
        LIVESET_CENSUS[LIVESET_CONTRADICTIONS].load(Ordering::Relaxed),
    )
}

/// The liveness claim's second oracle (under `CRATONVM_DBG_VERIFY_REG_OOP_MAPS`):
/// a word the claim dropped that ANY map sharing the safepoint id names in
/// `frame_slot_offsets` is a self-contradiction of the compile -- the emitter
/// leaves every word it proves dead out of the map it publishes at the same
/// safepoint. Counted and reported; the scan still drops the word, so the
/// oracle observes exactly the configuration under test.
#[cold]
pub(in crate::jit::conservative_roots) fn note_liveset_contradiction(cm: &cratonvm_jit::CompiledMethod, off: i32, sp_id: Option<u32>) {
    if !verify_reg_oop_maps() {
        return;
    }
    let (Some(id), Ok(off16)) = (sp_id, i16::try_from(off)) else {
        return;
    };
    let named = cm
        .oop_maps
        .iter()
        .filter(|m| m.bytecode_pc == id)
        .any(|m| m.frame_slot_offsets.contains(&off16));
    if !named {
        return;
    }
    let n = LIVESET_CENSUS[LIVESET_CONTRADICTIONS].fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 16 || n.is_power_of_two() {
        eprintln!(
            "[liveset] CONTRADICTION (#{n}): {} sp={id} off={off} is proven dead by the \
             frame-liveness table but named by the safepoint's own oop map",
            cm.method_label,
        );
    }
}

/// Does the mask permit a live reference in the blind-spill slot at `off`?
///
/// `true` for every offset OUTSIDE the spill image -- this decides one region
/// and must not answer for the rest of the band.
#[inline]
pub(in crate::jit::conservative_roots) fn spill_slot_may_hold_oop(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    reg_mask: Option<u16>,
) -> bool {
    let Some(mask) = reg_mask else {
        return true;
    };
    let Some(idx) = layout.spill_image_index(off) else {
        return true;
    };
    // A slot index the mask cannot represent is one this build does not model.
    // Scan it: the mask is a licence, and it does not extend past its width.
    idx >= 16 || mask & (1u16 << idx) != 0
}

/// Whether `exact_rbp` recorded for a chain entry actually belongs to a
/// DEEPER, unguarded compiled frame rather than to the entry's own method.
///
/// A chain entry is pushed at a Rust/interpreter -> JIT boundary, so the frame
/// it describes was called from non-JIT code and its return address does NOT
/// point into compiled code. Every compiled prologue, however, publishes its
/// own RBP into the innermost-RBP mirror, which is flushed into the top chain
/// entry before every GC walk — and two generated-code paths reach a compiled
/// callee with no guard at all: the inline MIC/PIC cascade and the hashed
/// megamorphic stub (`runtime_lowering::emit_hashed_vtable_stub`, which is not
/// gated by `direct_jit_callee_calls_enabled()`). After such a call the entry
/// still names the boundary method while `exact_rbp` names the callee's frame.
///
/// Detect it by the frame's own return address: if `[exact_rbp + 8]` points
/// into registered JIT code, the frame was entered FROM compiled code and this
/// entry cannot describe it. Using the entry's `compiled_method` for that RBP
/// would validate coverage against a safepoint id read out of an unrelated
/// frame slot and then rewrite `[rbp - off]` for the wrong method's oop map,
/// leaving the callee's real oops unrelocated across an evacuation.
///
/// The outward RBP-chain walk is unaffected: it resolves every ancestor from
/// its return address, which is correct regardless.
///
/// # The direct self-call exception
///
/// "Entered from compiled code" is not by itself a reason to give up: it only
/// matters when the frame belongs to a method the entry does not name. A
/// compiled method that recurses does so through a direct `E8 rel32` CALL back
/// to its OWN entry (`x64.rs`, `self_call_patches`), pushing no guard — so a
/// recursive workload leaves the mirror pointing at an inner activation of the
/// very method the entry names, and `info.compiled_method` describes that frame
/// exactly. Rejecting it costs every moving cycle for no safety gain: measured
/// on `BinTreesClassic`, whose `itemCheck`/`bottomUpTree` recursion made this
/// the reason for 100% of fallbacks (82/82 at depth 18, 59/59 at 16, 2/2 at 14)
/// once the exact-RBP defect above it was fixed.
///
/// The exception is recognised from the machine code, not inferred from which
/// direct-call features happen to be gated off: the return address must lie in
/// the entry's own body AND the five bytes ending there must be a direct CALL
/// whose target is that body's entry point. Anything else — an indirect call,
/// a call from a different method, bytes that cannot be read — stays foreign.
///
/// gen r4w2/youngmark (2026-09-23, O3 of
/// `docs/feature-designs/generative-gc-architecture.md`):
/// **this wrapper has no callers.** The chain scan asks
/// [`innermost_frame_method`] directly (`Some(cm)` = not foreign), so every
/// comment in this file that cites `chain_entry_rbp_is_foreign` as "the check"
/// means that call. It is kept only because its doc is where the self-call
/// exception is written down.
#[allow(dead_code)]
pub(in crate::jit::conservative_roots) fn chain_entry_rbp_is_foreign(
    exact_rbp: usize,
    exact_cm_id: u32,
    entry_sp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
) -> bool {
    innermost_frame_method(exact_rbp, exact_cm_id, entry_sp, scanner_sp, cm).is_none()
}

/// Whether the direct-call callee resolution below is enabled. Default ON;
/// opt out with `CRATONVM_GC_NO_CALLEE_RESOLVE=1`, which restores the
/// "any deeper frame is foreign" behaviour exactly.
pub(in crate::jit::conservative_roots) fn callee_resolve_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !false)
}

/// The [`cratonvm_jit::CompiledMethod`] that actually describes the frame
/// standing at `exact_rbp`, or `None` when it cannot be established — in which
/// case every caller must fail closed (no precise map, no relocation, and a
/// conservative band bounded by the scanner's own SP).
///
/// The entry's own `cm` is the answer in two cases: the frame was entered from
/// non-JIT code (the ordinary boundary frame this entry was pushed for), or it
/// was entered by a direct self-call (a recursive activation of the very method
/// the entry names — see the exception documented on
/// [`chain_entry_rbp_is_foreign`]).
///
/// # Resolving a different callee
///
/// Neither of those covers the shape that made this the reason for **100% of
/// bintrees fallbacks**: `binaryTrees` calls `bottomUpTree` and `itemCheck`, so
/// the mirror names a frame belonging to a method the entry does not, and the
/// old code gave up. But "the entry cannot describe this frame" is not the same
/// as "nothing can" — the frame's own return address says exactly which method
/// built it, provided the call was the direct form:
///
/// ```text
/// [exact_rbp + 8] = ret_addr
/// [ret_addr - 5]  = E8 rel32          <- direct CALL
/// ret_addr + rel32 == callee.entry_ptr()
/// ```
///
/// A callee resolved that way describes the frame exactly, because that frame
/// was built by the prologue at `entry_ptr`. The requirement that the target be
/// the registered **entry point** — not merely some address inside the method —
/// is what makes it exact: a call landing anywhere else (an OSR entry, a stub)
/// would not have run the prologue that establishes these slot offsets.
///
/// Everything else stays foreign, and that is the whole safety argument: an
/// indirect call (the inline MIC/PIC cascade, the hashed megamorphic stub) has
/// no decodable target, unreadable bytes decode to nothing, and a displacement
/// resolving into the middle of a method resolves to `None`. The failure
/// direction is a non-moving sweep, never a frame walked with the wrong map.
pub(in crate::jit::conservative_roots) fn innermost_frame_method(
    exact_rbp: usize,
    exact_cm_id: u32,
    entry_sp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
) -> Option<*const cratonvm_jit::CompiledMethod> {
    innermost_frame_method_with(
        exact_rbp,
        exact_cm_id,
        entry_sp,
        scanner_sp,
        cm,
        &mut WalkLookups::per_call(),
    )
}

/// [`innermost_frame_method`] resolving the saved return address through a
/// walk's [`WalkLookups`] (JIT round 16 wave 15, lane throwfill): the same
/// answer.
pub(in crate::jit::conservative_roots) fn innermost_frame_method_with(
    exact_rbp: usize,
    exact_cm_id: u32,
    entry_sp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
    lookups: &mut WalkLookups,
) -> Option<*const cratonvm_jit::CompiledMethod> {
    if exact_rbp == 0 || exact_rbp & 0x7 != 0 {
        return Some(cm);
    }
    if exact_rbp < scanner_sp || exact_rbp.saturating_add(16) > entry_sp {
        return Some(cm);
    }
    // SAFETY: aligned read of the saved return address inside this thread's own
    // live JIT stack band, bounded by `scanner_sp` / `entry_sp` exactly as the
    // neighbouring chain walks do.
    let ret_addr = unsafe { ((exact_rbp + 8) as *const usize).read() };
    // Interpreter round i1 wave 23, lane L6: the entry names a NOT ENTRANT
    // body. Its entry jumps to a stub that builds no frame and may forward the
    // call to ANOTHER body (`cratonvm_jit::NotEntrantRecord::forward_to`), so
    // neither "entered from non-JIT code" nor "a direct self-call" proves the
    // frame is `cm`'s. Only the identity the frame's own prologue published
    // does; otherwise fail closed.
    // SAFETY: `cm` is Arc-owned by the JIT cache while any of its frames is
    // live, which is the precondition for being on this chain at all.
    if unsafe { (*cm).is_not_entrant() } {
        if !callee_resolve_enabled() {
            return None;
        }
        return published_innermost_method(exact_cm_id);
    }
    let Some(caller_cm) = lookups.code_range(ret_addr) else {
        // A non-JIT caller is the ordinary boundary frame this entry was pushed
        // for.
        return Some(cm);
    };
    if caller_cm as *const cratonvm_jit::CompiledMethod == cm {
        // The caller is the entry's own method. That is a recursive activation
        // iff the call really was the direct self-call form.
        // SAFETY: `cm` is Arc-owned by the JIT cache while any of its frames is
        // live, which is the precondition for being on this chain at all.
        let entry_ptr = unsafe { (*cm).entry_ptr() } as usize;
        if returned_from_direct_self_call(ret_addr, entry_ptr) {
            return Some(cm);
        }
    }
    if !callee_resolve_enabled() {
        return None;
    }
    // The frame's own prologue named itself; no decode needed.
    if let Some(published) = published_innermost_method(exact_cm_id) {
        return Some(published);
    }
    direct_call_callee(ret_addr, caller_cm)
}

/// The method the innermost frame's own prologue named, or `None`.
///
/// [`direct_call_callee`] can only answer for a frame entered by a direct
/// `CALL rel32`. An INDIRECT JIT->JIT call — every inline-cache hit ends in
/// `CALL R11`, plus the megamorphic stub and the trampolines — encodes no
/// rel32, so it failed closed and diverted the whole young collection to the
/// non-moving sweep. That was 419 of 419 of `Log4J2LoggingSystemTests`'s
/// fallback cycles under `-XX:+UseGenerationalGC`, with no other obligation in
/// the set (`docs/known-issues/springboot/`, the moving-young page).
///
/// Codegen publishes the compile id beside the RBP it already stores, in the
/// same instruction pair, at every point that touches the mirror. `id` is that
/// value as captured by [`JitPreciseFrameInfo::exact_cm_id`] — snapshotted
/// together with the RBP it belongs to, NOT read here. Reading the mirror at
/// scan time instead cost a whole verification round: the owning thread has
/// moved on by then, and the scan often runs on the collector's thread whose
/// mirrors describe a different stack, so the id resolved in 1 of 786 cycles.
///
/// An unbound id (reserved but not yet published, or released on drop) answers
/// `None`, which returns the caller to the decode it always had.
pub(in crate::jit::conservative_roots) fn published_innermost_method(id: u32) -> Option<*const cratonvm_jit::CompiledMethod> {
    let cm_ptr = cratonvm_jit::lookup_compile_id(id)?;
    Some(cm_ptr as *const cratonvm_jit::CompiledMethod)
}

/// Read the compile-id mirror — the identity half of the inline frame record.
/// Windows reads the probed TEB slot directly (as the RBP mirror does); Linux
/// goes through the JIT crate's accessor for the same Rust TLS cell generated
/// code writes. `0` = nothing published.
#[inline]
pub(in crate::jit::conservative_roots) fn published_compile_id() -> u32 {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_cm_tls_disp();
        if disp != 0 {
            // SAFETY: `disp` was validated by the startup sentinel probe in
            // `inline_cm_tls_disp()` to be a live, 8-byte-aligned TEB TLS slot
            // present on every thread — the same earned safety as the RBP
            // mirror read directly above.
            return (unsafe { read_gs_qword(disp) } & 0xFFFF_FFFF) as u32;
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if let Some(id) = cratonvm_jit::x64::inline_cm_tls_mirror_read() {
            return id;
        }
    }
    0
}

/// The identity of the CURRENT thread's innermost compiled activation at its
/// published safepoint, read from the inline frame record alone (two TLS
/// mirror reads, the compile-id table and the frame's safepoint-id word):
/// `(compile id, safepoint id, &CompiledMethod)`. `None` when the record is
/// not active (no published compile id or RBP) or the method reserves no
/// safepoint-id slot; the caller then walks the frames
/// ([`active_compiled_frames`]).
///
/// For a caller running straight below compiled code on this thread (a thin
/// helper), whose activation is live while the reference is used. Interpreter
/// round i1 wave 50, lane L4: the key of the exact `VarHandle` stand-in site
/// memo (`jit::helpers::varhandle_stand_in_site_descriptor`).
pub(crate) fn innermost_compiled_site_key(
) -> Option<(u32, u32, *const cratonvm_jit::CompiledMethod)> {
    let id = published_compile_id();
    if id == 0 {
        return None;
    }
    let rbp = top_rbp_get();
    if rbp == 0 {
        return None;
    }
    let cm_ptr = published_innermost_method(id)?;
    // SAFETY: the compile-id table hands out only registered, live methods,
    // and the activation the mirrors name is on this thread's stack below
    // the caller, so the method cannot be reclaimed while it runs.
    let sp_id = safepoint_id_slot(rbp, unsafe { &*cm_ptr })?;
    Some((id, sp_id, cm_ptr))
}

/// Decode the direct `CALL` whose return address is `ret_addr` and resolve its
/// target to the registered method whose **entry point** it is.
///
/// `caller_cm` is the method the return address lies in; its entry bounds the
/// five-byte backward read, so the read stays inside that method's own
/// executable buffer. Fails closed on every ambiguity.
pub(in crate::jit::conservative_roots) fn direct_call_callee(
    ret_addr: usize,
    caller_cm: usize,
) -> Option<*const cratonvm_jit::CompiledMethod> {
    if caller_cm == 0 {
        return None;
    }
    // SAFETY: `caller_cm` is what `lookup_jit_code_range` resolved for a return
    // address in a LIVE frame, so the JIT cache still owns the `Arc` whose inner
    // value it addresses — the same keep-alive argument the neighbouring chain
    // walks rely on.
    let caller_entry =
        unsafe { (*(caller_cm as *const cratonvm_jit::CompiledMethod)).entry_ptr() } as usize;
    // JIT round 20 wave 19, lane yStash (STASH20-2): a caller one of whose
    // call sites was re-pointed from a not-entrant body at its successor
    // (`JitCache::rebind_stale_call_site`) may still have a frame of the OLD
    // body returning to that site, which the displacement no longer names.
    // Fail closed (the non-moving direction), as for a not-entrant callee.
    // SAFETY: as above.
    if unsafe { (*(caller_cm as *const cratonvm_jit::CompiledMethod)).has_rebound_call_sites() } {
        return None;
    }
    let target = direct_call_target(ret_addr, caller_entry)?;
    let callee =
        cratonvm_jit::lookup_jit_code_range(target)? as *const cratonvm_jit::CompiledMethod;
    // Exactness: only the registered entry point runs the prologue that
    // establishes the slot offsets every caller is about to read.
    // SAFETY: as above — `target` is a live code address, so its owner is
    // retained by the JIT cache for as long as a frame of it is on this stack.
    //
    // Interpreter round i1 wave 23, lane L6: and only while that entry still
    // runs that prologue. A NOT ENTRANT body's entry jumps to its stub, which
    // builds no frame and either calls into the VM or forwards the call to
    // ANOTHER body (`cratonvm_jit::NotEntrantRecord::forward_to`), whose frame
    // this would describe with the wrong map. Fail closed.
    (unsafe { (*callee).entry_ptr() } as usize == target && !unsafe { (*callee).is_not_entrant() })
        .then_some(callee)
}

/// `CRATONVM_JIT_TRACE_NOT_ENTRANT_INNERMOST` -- JIT round 21 wave 8, lane
/// splice (`docs/feature-designs/deopt-osr.md`);
/// trace defect, **default ON**, `=0` restores the dropped frame. Read only
/// when [`innermost_frame_method_with`] has already refused (a cold path), so
/// not cached: no new process static.
pub(in crate::jit::conservative_roots) fn trace_not_entrant_innermost_enabled() -> bool {
    true
}

/// A STACK WALK's second chance at the innermost frame of a chain entry,
/// after [`innermost_frame_method_with`] refused it (JIT round 21 wave 8,
/// lane splice).
///
/// The refusal is right for the collector, and it cost a trace a frame:
/// `R21Fz2SpliceDeep2`'s `late` pass printed `[base,d1..d6,main]` against
/// HotSpot's `[base,d1..d6,row,main]`. The single-pass `row` was the innermost
/// frame of the entry (suspended in the helper that precisely resumed its
/// trapped callee `d6` in the interpreter). `d6`'s eviction and the new `L3`
/// class made the bodies around it not entrant, its frame published no
/// compile id (`cm_id=0`), and `direct_call_callee` fails closed on any not
/// entrant callee, so the activation silently left the walk, leaving only the
/// parent `main` read off the RBP chain.
///
/// The answer here is the same decode `direct_call_callee` makes -- the
/// frame's return address lies in a registered compiled caller, whose bytes
/// before it are a direct `CALL` to the callee's registered ENTRY -- with one
/// relaxation: a NOT ENTRANT callee is accepted while its stub has never
/// forwarded a call ([`cratonvm_jit::CompiledMethod::not_entrant_stub_ever_forwarded`]).
/// The stub builds no frame, so a compiled frame entered through that entry
/// was built by this body's own prologue, before the patch; only a forward
/// could have put another body's frame there. HotSpot reports frames of
/// not-entrant nmethods the same way. A caller with a re-bound call site
/// still fails closed (its displacement no longer proves the callee).
///
/// Trace walks only: the collector's doors (`innermost_frame_method`) keep
/// the fail-closed answer, so no oop map is read through this.
pub(in crate::jit::conservative_roots) fn trace_innermost_by_caller_call(
    exact_rbp: usize,
    entry_sp: usize,
    scanner_sp: usize,
    lookups: &mut WalkLookups,
) -> Option<*const cratonvm_jit::CompiledMethod> {
    if !callee_resolve_enabled() || !trace_not_entrant_innermost_enabled() {
        return None;
    }
    if exact_rbp == 0
        || exact_rbp & 0x7 != 0
        || exact_rbp < scanner_sp
        || exact_rbp.saturating_add(16) > entry_sp
    {
        return None;
    }
    // SAFETY: aligned read of the saved return address inside this thread's
    // own live JIT stack band, bounded by `scanner_sp` / `entry_sp` exactly as
    // `innermost_frame_method_with` reads the same word.
    let ret_addr = unsafe { ((exact_rbp + 8) as *const usize).read() };
    let caller_ptr = lookups.code_range(ret_addr)?;
    // SAFETY: `caller_ptr` is what the code-range registry answered for a
    // return address in a live frame, so the JIT cache still owns it (the
    // keep-alive argument of `direct_call_callee`).
    let caller = unsafe { &*(caller_ptr as *const cratonvm_jit::CompiledMethod) };
    let target = direct_call_target(ret_addr, caller.entry_ptr() as usize)?;
    let callee_ptr = cratonvm_jit::lookup_jit_code_range(target)?;
    // SAFETY: `target` is a registered code address, so its owner is retained
    // by the JIT cache while a frame of it is on this stack.
    let callee = unsafe { &*(callee_ptr as *const cratonvm_jit::CompiledMethod) };
    let not_entrant = callee.is_not_entrant();
    trace_innermost_decode_admits(
        target,
        callee.entry_ptr() as usize,
        not_entrant,
        not_entrant && callee.not_entrant_stub_ever_forwarded(),
        caller.has_rebound_call_sites(),
    )
    .then_some(callee_ptr as *const cratonvm_jit::CompiledMethod)
}

/// The admission rule of [`trace_innermost_by_caller_call`], pure: the
/// decoded target is the callee's registered entry, the caller has no
/// re-bound call site, and a not-entrant callee's stub never forwarded.
pub(in crate::jit::conservative_roots) fn trace_innermost_decode_admits(
    target: usize,
    callee_entry: usize,
    callee_not_entrant: bool,
    callee_ever_forwarded: bool,
    caller_rebound: bool,
) -> bool {
    target != 0
        && callee_entry == target
        && !caller_rebound
        && (!callee_not_entrant || !callee_ever_forwarded)
}

/// Does chain entry `info` stand for NO compiled activation at all: the call
/// it was pushed for was intercepted by its body's not-entrant stub before the
/// body's prologue ran, and `jit_not_entrant_entry` is re-running the method
/// through the interpreter right now?
///
/// The interpreter (or any Rust door) pushes the entry for the body it read
/// from the cache, then calls that body's entry. A background tier-up that
/// supersedes the body in between patches the entry to the stub, which builds
/// no frame; the helper re-dispatches the call by name, and that re-dispatch
/// pushes its own interpreter frame (or its own chain entry) for the same
/// activation. The walk's boundary fallback then reported the stale body as
/// well, beneath it, with no bci: one exception in a few runs of
/// `R25W7FzDiag.java` read `d3,d2,d1,fDeep,fDeep` where HotSpot
/// reads `d3,d2,d1,fDeep`
/// (`docs/feature-designs/real-frame-deopt.md`).
///
/// All three are required, so a body that ever built a frame keeps it:
///
///   * the body is not entrant;
///   * no prologue published a frame for this entry (`exact_rbp == 0`; a
///     prologue that ran, the body's own or a compiled caller's, records it);
///   * the helper's intercepted return address is published and lies in no
///     compiled body, i.e. the stub was called straight from the boundary
///     rather than by a compiled caller of this entry (whose frame is then
///     the innermost one and is walked as before).
///
/// Trace walks only (the collector does not come through this walk). Rides on
/// `CRATONVM_JIT_TRACE_NOT_ENTRANT_INNERMOST` (default ON; `=0` restores the
/// duplicated frame).
pub(in crate::jit::conservative_roots) fn not_entrant_entry_never_framed(info: &PreciseFrameInfo, lookups: &mut WalkLookups) -> bool {
    let ret_pc = info.not_entrant_ret_pc;
    if ret_pc == 0 || info.exact_rbp != 0 || info.compiled_method.is_null() {
        return false;
    }
    // SAFETY: the chain entry's body is Arc-owned by the JIT cache while the
    // entry is on the chain (`PreciseFrameInfo::compiled_method`).
    if !unsafe { (*info.compiled_method).is_not_entrant() } {
        return false;
    }
    trace_not_entrant_innermost_enabled() && lookups.code_range(ret_pc).is_none()
}

/// Decode the `E8 rel32` whose return address is `ret_addr` and return its
/// target, or `None` when the bytes are not a direct near CALL.
///
/// `caller_entry` bounds the five-byte backward read so it cannot run below the
/// caller's own executable buffer. Split out from [`direct_call_callee`] with
/// no pointer dereference of its own, which is what makes it testable — the
/// resolution half needs a registered code range, and a unit test that
/// fabricated a `CompiledMethod` pointer to reach this logic would be reading
/// a `[u8; N]` as one.
pub(in crate::jit::conservative_roots) fn direct_call_target(ret_addr: usize, caller_entry: usize) -> Option<usize> {
    if caller_entry == 0 {
        return None;
    }
    direct_call_target_rel32(ret_addr, caller_entry)
        .or_else(|| direct_call_target_abs64(ret_addr, caller_entry))
}

/// Length of the `MOVABS RAX, imm64` + `CALL RAX` sequence
/// [`direct_call_target_abs64`] decodes, in bytes.
pub(in crate::jit::conservative_roots) const ABS64_CALL_LEN: usize = 12;

/// `E8 rel32` — the near CALL the JIT emits whenever the target is within
/// +/-2GB of the emit-time buffer position.
pub(in crate::jit::conservative_roots) fn direct_call_target_rel32(ret_addr: usize, caller_entry: usize) -> Option<usize> {
    // The call instruction lies between the caller's entry and the return
    // address, so a return address within 5 bytes of it cannot be one.
    if ret_addr < caller_entry.saturating_add(5) {
        return None;
    }
    // SAFETY: `[ret_addr - 5, ret_addr)` lies inside the executable buffer of
    // the compiled method `lookup_jit_code_range(ret_addr)` resolved, at or
    // above its entry point, and that buffer is kept alive by the live frame
    // whose return address this is. Code pages are readable.
    if unsafe { ((ret_addr - 5) as *const u8).read() } != 0xE8 {
        return None;
    }
    let rel = unsafe { ((ret_addr - 4) as *const i32).read_unaligned() };
    // `E8 rel32` targets `next_instruction + rel32`, and `ret_addr` IS the next
    // instruction.
    Some(ret_addr.wrapping_add(rel as usize))
}

/// `48 B8 <imm64> FF D0` — `MOVABS RAX, imm64` followed by `CALL RAX`, the
/// OTHER form of the very same direct call.
///
/// `x64::emit::emit_call_absolute` picks between two encodings for one call to
/// one known address: `E8 rel32` when the target is within +/-2GB of the
/// emit-time buffer position, and this twelve-byte absolute sequence
/// (`emit_call_imm64_via_rax`) when it is not. Which one a given call site gets
/// therefore depends on where the staging buffer and the callee's code page
/// happened to be allocated — the same call, compiled twice, can come out
/// either way.
///
/// Decoding only the first form made the choice observable as a correctness
/// bug. `stackwalker_log4j_deep_repeated_walks_finish_under_jit` failed in
/// roughly 10 of 12 runs of the same binary: when `recurse`'s call to
/// `LoggerFactory.resolveCaller` came out as the absolute form,
/// [`innermost_frame_method`] could not name the frame it built, returned
/// `None`, and the whole `resolveCaller` frame vanished from the walk. Log4j2's
/// caller lookup then answered with `recurse`'s declaring class. Measured on a
/// failing run, the twelve bytes ending at that return address were
/// `48 b8 00 70 9c 5b ed 75 00 00 ff d0` — this sequence, targeting
/// `resolveCaller`'s registered entry point.
///
/// This is not a weaker decode than the rel32 one, it is a stronger one: the
/// target is a literal in the instruction stream rather than a displacement to
/// add. Every caller still requires the decoded address to equal a registered
/// method's ENTRY POINT ([`direct_call_callee`]), so an indirect call — the
/// inline-cache `CALL R11`, the megamorphic stub — decodes to nothing and stays
/// foreign exactly as before. A bare `FF D0` not preceded by the `MOVABS` half
/// is rejected here too.
pub(in crate::jit::conservative_roots) fn direct_call_target_abs64(ret_addr: usize, caller_entry: usize) -> Option<usize> {
    // As above: the twelve-byte sequence has to fit between the caller's entry
    // and the return address, which is what bounds the backward read.
    if ret_addr < caller_entry.saturating_add(ABS64_CALL_LEN) {
        return None;
    }
    let seq = ret_addr - ABS64_CALL_LEN;
    // SAFETY: `[seq, ret_addr)` lies inside the executable buffer of the
    // compiled method `lookup_jit_code_range(ret_addr)` resolved, at or above
    // its entry point, and that buffer is kept alive by the live frame whose
    // return address this is. Code pages are readable. Same argument, and the
    // same bound, as `direct_call_target_rel32` — only the span differs.
    unsafe {
        // REX.W + B8+rd with rd = RAX: `MOVABS RAX, imm64`.
        if (seq as *const u8).read() != 0x48 || ((seq + 1) as *const u8).read() != 0xB8 {
            return None;
        }
        // FF /2 with ModRM mod=11 r/m=RAX: `CALL RAX`.
        if ((ret_addr - 2) as *const u8).read() != 0xFF
            || ((ret_addr - 1) as *const u8).read() != 0xD0
        {
            return None;
        }
        Some(((seq + 2) as *const u64).read_unaligned() as usize)
    }
}

/// Whether the five bytes ending at `ret_addr` are `E8 rel32` with the target
/// `entry_ptr` — i.e. `ret_addr` is the return address of a direct self-call.
///
/// Fails closed: a return address too close to the method entry to hold the
/// instruction, an opcode that is not `E8`, or a displacement that resolves
/// anywhere other than `entry_ptr` all answer `false`.
pub(in crate::jit::conservative_roots) fn returned_from_direct_self_call(ret_addr: usize, entry_ptr: usize) -> bool {
    // The call instruction lies between the method entry and the return
    // address, so a return address within 5 bytes of the entry cannot be one.
    // A self-call's caller and callee are the same method, so the method's own
    // entry both bounds the backward read and IS the target to match. Both
    // encodings `emit_call_absolute` can produce count: a self-call is subject
    // to the same rel32-reach coin flip as any other direct call, and reading
    // only the `E8` form here would call a recursive activation foreign on the
    // runs that came out absolute (see [`direct_call_target_abs64`]).
    entry_ptr != 0 && direct_call_target(ret_addr, entry_ptr) == Some(entry_ptr)
}

pub(in crate::jit::conservative_roots) fn moving_young_frame_coverage_complete(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> bool {
    moving_young_frame_coverage_complete_at(rbp, cm, None)
}

/// As [`moving_young_frame_coverage_complete`], plus the return address the
/// rbp-chain walk resolved `cm` from — `None` for the innermost frame, whose
/// method comes from the chain entry rather than from a return address.
///
/// The extra argument exists only for the `no_map` diagnostic: the walk's
/// "is this a JIT frame" test is `lookup_jit_code_range(ret_addr).is_some()`,
/// which the A5 probe's own comment says is not sufficient — "cached function
/// pointers and JIT helper arguments that happen to point inside generated code
/// read as return PCs and fabricate a frame". `is_plausible_return_pc` is the
/// sharper test that already exists for exactly that, and reporting it here
/// says whether a `no_map` frame is a real frame at an unmapped call site or a
/// fabricated one, which are opposite repairs.
pub(in crate::jit::conservative_roots) fn moving_young_frame_coverage_complete_at(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    ret_addr: Option<usize>,
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    // Nothing in such a frame can be a root, so no map is needed to say that
    // the shadow push published every one of them -- and the slot a map would
    // be found through may hold a stale id by contract
    // (`CompiledMethod::frame_holds_no_references`). The caller's registers in
    // its callee-saved image are the caller's obligation, as for every frame.
    if cm.frame_holds_no_references {
        frame_coverage_reason::COMPLETE.fetch_add(1, Relaxed);
        return true;
    }
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        frame_coverage_reason::NO_SP_ID_SLOT.fetch_add(1, Relaxed);
        return false;
    }
    let id_addr = rbp.wrapping_sub(sp_id_off as usize);
    if id_addr & 0x7 != 0 {
        frame_coverage_reason::MISALIGNED_SLOT.fetch_add(1, Relaxed);
        return false;
    }
    // SAFETY: aligned safepoint-id slot in a live JIT frame on this thread.
    let sp_id = (unsafe { (id_addr as *const usize).read() }) as u32;
    let mut found = false;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        found = true;
        if !map.moving_young_coverage_complete {
            frame_coverage_reason::MAP_NOT_COMPLETE.fetch_add(1, Relaxed);
            return false;
        }
    }
    if found {
        frame_coverage_reason::COMPLETE.fetch_add(1, Relaxed);
    } else {
        frame_coverage_reason::NO_MAP_FOR_STORED_ID.fetch_add(1, Relaxed);
        // WHICH frame, and what id was standing in its slot. This is the last
        // obligation blocking `TestMVStoreTool`
        // (`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`):
        // `no_map=9 incomplete=0 ok=46`, so no map ever refuses on its own
        // claim — nine frames simply cannot be located. A count cannot say
        // whether that is a frame that has not reached a safepoint yet, a call
        // site that recorded no map, or a method mis-resolved from a return
        // address, and those are three different repairs.
        //
        // Rate-limited to the first 32: it fires per frame per collection.
        if dbg_jit_rootscan() {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if N.fetch_add(1, Relaxed) < 32 {
                // For the INNERMOST frame the walk hands no return address, so
                // read the frame's own saved one here (`[rbp + 8]`). Who the
                // CALLER is settles the remaining question: if the frame at
                // `rbp` really is `cm`'s, its return address lands inside a
                // method that calls `cm`. If the (rbp, compile-id) mirror is
                // STALE — published by a callee's prologue and not restored on
                // its return, since `emit_post_call_rbp_republish` has only six
                // call sites — the two will not correspond, and
                // `own_ret_ok=false` says the word is not a return address at
                // all.
                let own_ret = if rbp != 0 && rbp & 0x7 == 0 {
                    // SAFETY: the same aligned in-band read the chain walk makes
                    // a few lines further on, on this thread's own stack.
                    Some(unsafe { ((rbp + 8) as *const usize).read() })
                } else {
                    None
                };
                let own_caller_cm = own_ret.and_then(cratonvm_jit::lookup_jit_code_range);
                let own_caller = own_caller_cm.map(|p| {
                    // SAFETY: a resolved range is Arc-owned by the JIT cache
                    // for as long as any of its frames is live.
                    let c: &cratonvm_jit::CompiledMethod =
                        unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
                    c.method_label.clone()
                });
                // THE DISCRIMINATOR. `innermost_frame_method` prefers the
                // compile-id MIRROR (`published_innermost_method`), which
                // generated code writes in its prologue and restores after a
                // JIT->JIT call. Decoding the caller's `E8 rel32` instead
                // answers the same question from the STACK, which cannot go
                // stale. If the two disagree, the mirror is naming a frame that
                // has already returned; if they agree, these are real frames
                // live at a call site that recorded no map — opposite repairs.
                let decoded = match (own_ret, own_caller_cm) {
                    (Some(r), Some(c)) => direct_call_callee(r, c).map(|p| {
                        // SAFETY: as above.
                        let m: &cratonvm_jit::CompiledMethod = unsafe { &*p };
                        m.method_label.clone()
                    }),
                    _ => None,
                };
                eprintln!(
                    "[frame-cov] no map for stored id: sp_id={sp_id} (0x{sp_id:x}) \
                     method={} maps={} ids={:?} sp_id_slot_off={sp_id_off} rbp=0x{rbp:x} \
                     walk_ret={:?} walk_ret_ok={:?} own_ret={:?} own_ret_ok={:?} \
                     own_caller={own_caller:?} decoded_callee={decoded:?}",
                    cm.method_label,
                    cm.oop_maps.len(),
                    cm.oop_maps
                        .iter()
                        .map(|m| m.bytecode_pc)
                        .take(24)
                        .collect::<Vec<_>>(),
                    ret_addr.map(|r| format!("0x{r:x}")),
                    ret_addr.map(is_plausible_return_pc),
                    own_ret.map(|r| format!("0x{r:x}")),
                    own_ret.map(is_plausible_return_pc),
                );
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Moving-young coverage VERIFICATION (arch-2026-07-26
// `moving-young-corruption-rootcause`)
// ---------------------------------------------------------------------------
//
// `OopMapEntry::moving_young_coverage_complete` is the codegen's ASSERTION that
// the shadow push at a safepoint published every live oop of that frame. The
// assertion is computed by `x64::moving_young_safepoint_coverage_complete` from
// the abstract interpreter's model of the frame: `self.stack` (the operand
// stack) plus `local_oop_masks` (the Java locals). `collect_live_oop_homes`
// publishes exactly that same set.
//
// A compiled frame holds oops in storage the abstract model does not describe:
//
//   * SCALAR-REPLACED OBJECT FIELDS. Escape analysis explodes an object into
//     raw frame slots at `[rbp - (field_base_offset + k*8)]`
//     (`x64.rs` `ScalarReplacedObject`, used by the `new` / `getfield` /
//     `putfield` arms). A reference-typed field of such an object is a genuine
//     heap pointer that is neither a Java local nor an operand-stack entry.
//   * LICM HOIST SLOTS. `hoist_info` hoists a loop-invariant `aaload` into
//     `hoist_offsets[idx]`; for a reference array that slot holds an object
//     reference for the whole loop, again outside the local/operand model.
//   * THE FULL-GPR SAFEPOINT SPILL AREA (`reg_spill_base`, the SB-CRASH-04
//     spill). It exists precisely to make register-resident values visible to a
//     CONSERVATIVE scan; the shadow push does not enumerate it.
//
// On the non-moving path all three are covered, because the conservative frame
// scan reads every word of the frame. Under moving-young `memory/roots.rs`
// SUPPRESSES that scan when the coverage proof passes — so those oops are
// neither marked nor rewritten, and a Cheney copy strands or reclaims them.
// That is a silent, GC-timing-dependent under-count with the exact shape the
// known-issue records for bt18.
//
// This verifier removes the need to trust the assertion. It walks each live
// compiled frame's OWN spill band — `[rbp - osr_frame_size, rbp)`, the same
// bounded band `scan_precise_frames_and_bands` uses, so intervening interpreter and
// Rust frames are excluded — and checks every word that lands in a published
// YOUNG semispace against the thread's published shadow window `[base, top)`.
// A word that is young-resident and unpublished is, by definition, a reference
// this collection could relocate and could not rewrite.
//
// Direction of error: the band scan is CONSERVATIVE, so it can flag a non-oop
// `i64` that happens to look young-resident. The cost of a false positive is
// one non-moving collection (counted, labelled, logged); the cost of a false
// negative would be heap corruption. Fail-closed everywhere: an unbounded band,
// a missing frame size, or an unresolvable shadow window all report "not
// verified".

/// Locate this thread's published shadow window `[base, top)` from a live
/// compiled frame.
///
/// The frame caches its `*mut JvmThread` in `[rbp - cm.shadow_thread_slot_off]`
/// (written by the prologue, or by the OSR trampoline for an OSR entry), and
/// the `ShadowStack` sits at `thread + cm.shadow_off_in_thread` with the
/// `#[repr(C)]` layout asserted in `cratonvm_gc::shadow_stack`.
///
/// `None` means "cannot resolve" — including the legitimate null-thread case
/// (`maybe_nop_out_shadow_fetch` NOPs the prologue fetch for a method that
/// never emitted a push, leaving the slot null). The caller treats `None` as an
/// EMPTY published set rather than as an error: a method that published nothing
/// is fine exactly as long as its band contains no young words, which is what
/// the band scan then goes on to check.
pub(in crate::jit::conservative_roots) fn shadow_window_from_frame(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
) -> Option<(usize, usize)> {
    shadow_window_from_frame_why(rbp, cm)
        .or_else(|e| {
            if band_thread_window_disabled() {
                Err(e)
            } else {
                shadow_window_for_thread_why(cm)
            }
        })
        .ok()
}

/// `shadow_window_from_frame`, but NAMING the refusal instead of erasing it.
///
/// §25.4. The band reporter printed `published=0` on all 272 of a run's reports
/// and that read as "the shadow push declined to publish these oops" -- a
/// codegen fact, pointing at `collect_live_oop_homes`. It was not: the window
/// was never resolved, so `published_shadow_values` had nothing to read and
/// returned an empty set. `Option` folds ten distinct refusals and a genuinely
/// empty stack into one indistinguishable `None`, and those point at opposite
/// repairs -- a missing reserved slot is a codegen gap, a foreign thread
/// pointer is a frame mis-attribution, a size mismatch is a torn read.
///
/// The wrapper above is the only caller that discards the reason, so the
/// diagnostic cannot drift away from the path actually taken.
pub(in crate::jit::conservative_roots) fn shadow_window_from_frame_why(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
) -> Result<(usize, usize), &'static str> {
    let thr_off = cm.shadow_thread_slot_off;
    if thr_off <= 0 || cm.shadow_off_in_thread <= 0 || rbp < thr_off as usize {
        return Err("no-thread-slot");
    }
    let slot = rbp - thr_off as usize;
    if slot & 0x7 != 0 {
        return Err("frame-slot-misaligned");
    }
    // SAFETY: `slot` is an aligned address inside this thread's own live
    // compiled frame, bounded by the same frame-size check the caller applied
    // before calling here.
    let thread_ptr = unsafe { (slot as *const usize).read() };
    if thread_ptr == 0 || thread_ptr & 0x7 != 0 {
        return Err("thread-null-or-misaligned");
    }
    // That word is only a thread pointer when `cm` really describes the frame
    // at `rbp`. Callers are expected to have excluded the mis-attribution case
    // (`chain_entry_rbp_is_foreign`), but a wrong `cm` turns this into an
    // arbitrary aligned stack word and the two loads below would dereference
    // it — which is exactly how the band verifier SIGSEGV'd on a `base` of
    // `0x5555_0000_0004`. Every compiled frame on this thread caches THIS
    // thread's `JvmThread`, so reject anything else whenever a thread is
    // installed (it is null only in unit tests and on threads that never
    // entered JIT code, where there is no frame to mis-attribute).
    let current = crate::jit::helpers::current_jit_thread_ptr() as usize;
    if current != 0 && thread_ptr != current {
        return Err("foreign-thread");
    }
    shadow_window_at(thread_ptr, cm.shadow_off_in_thread)
}

/// The thread's shadow window WITHOUT going through a frame slot.
///
/// §25.4's repair. `shadow_window_from_frame_why` reads the cached thread
/// pointer out of `[rbp - shadow_thread_slot_off]`, and `ir_lower`'s
/// `finish_lazy_thread_fetch` ERASES the fetch that fills that slot whenever
/// the method publishes nothing (`!shadow_pushed_any`) -- so a leaf that has no
/// oops to push leaves the slot zero BY DESIGN. That is a correct optimisation;
/// the band verifier's use of it was not. It resolved the window from the
/// INNERMOST frame alone, and when that frame was such a leaf the window came
/// back `None`, `published_shadow_values` returned an empty set, and every oop
/// in every OUTER frame read as unpublished. Measured on `CoverageBench`: 228
/// of 229 pauses declined relocation with `compiled-frame-oop-not-published`
/// while `main` had pushed 3 of its 3 mapped slots and `alloc` -- the innermost
/// frame, whose map names nothing -- was the only reason the window was lost.
///
/// The shadow stack is per-THREAD, so the frame slot was only ever a cache.
/// When a thread is installed, ask it directly. The frame path stays for the
/// case that has no installed thread (unit tests, and threads that never
/// entered JIT code), which is also the only case where the two could disagree
/// -- `shadow_window_from_frame_why` refuses a `thread_ptr` that is not
/// `current` anyway.
pub(in crate::jit::conservative_roots) fn shadow_window_for_thread_why(
    cm: &cratonvm_jit::CompiledMethod,
) -> Result<(usize, usize), &'static str> {
    if cm.shadow_off_in_thread <= 0 {
        return Err("no-shadow-offset");
    }
    let current = crate::jit::helpers::current_jit_thread_ptr() as usize;
    if current == 0 {
        return Err("no-installed-thread");
    }
    if current & 0x7 != 0 || current < 0x1_0000 {
        return Err("thread-null-or-misaligned");
    }
    shadow_window_at(current, cm.shadow_off_in_thread)
}

/// The shared half: validate `thread_ptr + shadow_off` as a `ShadowStack` and
/// return its `[base, top)` window. Both resolvers end here, so a window
/// reached through the frame cache and one reached through the installed
/// thread are held to identical invariants.
pub(in crate::jit::conservative_roots) fn shadow_window_at(thread_ptr: usize, shadow_off: i32) -> Result<(usize, usize), &'static str> {
    let ss = thread_ptr
        .checked_add(shadow_off as usize)
        .ok_or("shadow-addr-overflow")?;
    if ss & 0x7 != 0 {
        return Err("shadow-addr-misaligned");
    }
    // `thread_ptr` came out of a raw frame slot and is trustworthy ONLY when
    // `cm` really describes the frame at `rbp`. It does not always: an
    // unguarded JIT->JIT call publishes the CALLEE's frame base into the chain
    // entry (`chain_entry_rbp_is_foreign`), so `[rbp - shadow_thread_slot_off]`
    // addresses an arbitrary word of a different frame -- a spilled `long`, a
    // `Value` discriminant, an interior pointer. Non-zero and 8-aligned (all
    // this used to require) is a bar such a word clears constantly, and the
    // three reads below then dereference it. That is a real SIGSEGV, not a
    // hypothetical: `ApplicationContextAotGeneratorTests
    // .processAheadOfTimeWithPropertySource` faulted here on `addr=0xefc`
    // roughly three minutes into every run, taking the whole class's `RESULT`
    // line with it.
    //
    // Read `end` as well and require the exact `#[repr(C)]` invariant
    // `ShadowStack::ensure_allocated` establishes and `set_top` maintains:
    // three 8-aligned addresses with `base < end`, `end - base` EXACTLY the
    // fixed buffer size, and `top` inside `[base, end]`. Garbage that
    // satisfies all of that would have to be an actual shadow stack.
    let read_word = |off: usize| -> Option<usize> {
        let at = ss.checked_add(off)?;
        // SAFETY: `ss` is the candidate `ShadowStack` address; the caller has
        // no way to prove it is mapped, which is exactly what the checks below
        // are for -- but the read itself must not fault. Reject anything that
        // is not a plausible userspace address before dereferencing.
        if at < 0x1_0000 || at & 0x7 != 0 {
            return None;
        }
        Some(unsafe { (at as *const usize).read() })
    };
    if thread_ptr < 0x1_0000 {
        return Err("thread-addr-low");
    }
    let top =
        read_word(cratonvm_gc::shadow_stack::ShadowStack::TOP_OFFSET).ok_or("top-unreadable")?;
    let end =
        read_word(cratonvm_gc::shadow_stack::ShadowStack::END_OFFSET).ok_or("end-unreadable")?;
    let base =
        read_word(cratonvm_gc::shadow_stack::ShadowStack::BASE_OFFSET).ok_or("base-unreadable")?;
    const SHADOW_BYTES: usize = cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS * 8;
    if base < 0x1_0000 || base & 0x7 != 0 || top & 0x7 != 0 || end & 0x7 != 0 {
        return Err("fields-misaligned");
    }
    if end.checked_sub(base) != Some(SHADOW_BYTES) {
        return Err("buffer-size-mismatch");
    }
    if top < base || top > end {
        return Err("top-out-of-range");
    }
    Ok((base, top))
}

/// Collect the values currently published on the shadow window `[base, top)`.
/// gce e2/j: what the band test asks of the published values -- membership.
/// A plain set answers directly; [`LazyPublished`] builds its set on the first
/// question.
pub(in crate::jit::conservative_roots) trait PublishedWords {
    fn has(&self, w: usize) -> bool;
}

// addr-keyed: not a table -- membership over a caller's call-local set.
impl<S: std::hash::BuildHasher> PublishedWords for std::collections::HashSet<usize, S> {
    #[inline]
    fn has(&self, w: usize) -> bool {
        self.contains(&w)
    }
}

/// gce e2/j: [`published_shadow_values`] of one window, built on the first
/// membership question. The band test asks only about a word that is movable
/// (`addr_is_movable`), so a band with none -- every JNI in-native deposit of a
/// loop whose frame holds no young reference -- never builds the set. Same
/// answers: the set, once built, is the one the eager build made.
pub(in crate::jit::conservative_roots) struct LazyPublished {
    pub(in crate::jit::conservative_roots) window: Option<(usize, usize)>,
    pub(in crate::jit::conservative_roots) set: std::cell::OnceCell<PublishedSet>,
}

impl LazyPublished {
    pub(in crate::jit::conservative_roots) fn new(window: Option<(usize, usize)>) -> Self {
        LazyPublished {
            window,
            set: std::cell::OnceCell::new(),
        }
    }

    pub(in crate::jit::conservative_roots) fn get(&self) -> &PublishedSet {
        self.set.get_or_init(|| {
            let mut clock = coverage_census_clock();
            let set = published_shadow_values(self.window);
            coverage_census_lap(crate::native::jni::JniPhase::CovBandShadowSet, &mut clock);
            set
        })
    }
}

impl PublishedWords for LazyPublished {
    #[inline]
    fn has(&self, w: usize) -> bool {
        self.get().contains(&w)
    }
}

// nio21 w15 L5: the published-word set is keyed by addresses this thread
// itself read off its own shadow window, so it needs no DoS-resistant hash;
// `RandomState` (SipHash-1-3) showed up as ~2% of the `Nio21UnsafeVolatile
// pipe` row (one set per blocking native that finds a movable band word).
// Membership answers are identical; only the hasher changes.
// addr-keyed: call-local -- built per band question, dropped before the scan returns.
pub(in crate::jit::conservative_roots) type PublishedSet = rustc_hash::FxHashSet<usize>;

pub(in crate::jit::conservative_roots) fn published_shadow_values(window: Option<(usize, usize)>) -> PublishedSet {
    let mut out = PublishedSet::default();
    let Some((base, top)) = window else {
        return out;
    };
    // A pathological `top` cannot widen this beyond the backing buffer, which
    // `ShadowStack::set_top` clamps; bound it anyway so a torn read cannot walk
    // off the allocation.
    const MAX_SLOTS: usize = cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS;
    let slots = ((top - base) / 8).min(MAX_SLOTS);
    // The window is THIS thread's (the verifier runs on the thread whose
    // compiled frames it checks), so an indirect entry is trusted only inside
    // this thread's live stack. An odd primitive a compiled frame published in
    // a reference home names no such slot and counts as null instead of being
    // dereferenced -- the Generational half of `ShadowOddLongProbe`'s crash
    // (gc-common w5-g).
    let band = cratonvm_gc::shadow_stack::SlotBand::current_thread();
    // rx27: through the one window walker, so a frame descriptor record's
    // homes count as published (`shadow_stack::walk_window`).
    // SAFETY: aligned words inside the thread's own shadow-stack buffer; a
    // `Frame` home below `top` inside `band` names a live frame's slot.
    unsafe {
        cratonvm_gc::shadow_stack::walk_window(base, base + slots * 8, Some(band), |home| {
            out.insert(cratonvm_gc::shadow_stack::read_home(home));
        });
    }
    out
}

/// Verify that every young-resident word in this thread's live compiled frame
/// bands was published on the shadow stack.
///
/// Returns `true` when at least one word was **not** published (or when the
/// bands could not be bounded), i.e. when moving-young must NOT relocate this
/// cycle. See the module block above for why the codegen's own
/// `moving_young_coverage_complete` bit is not sufficient.
///
/// `reason_out` receives the specific `incomplete_reason` code on a `true`
/// result so the fallback histogram can distinguish "an oop was missed" from
/// "the frame could not be inspected at all".
pub fn moving_young_unpublished_frame_oop_present(reason_out: &mut usize) -> bool {
    if !moving_young_enabled() || band_verify_disabled() {
        return false;
    }
    // The band scan's residency test asks "could a moving cycle relocate the
    // object at this address?", and until 2026-08-21 it asked that as
    // `gen_heap::addr_in_published_young_regions`, which reads
    // `JIT_REGION_BOUNDS`. That table has one writer and it is
    // generational-only — G1 deliberately keeps it empty, ZGC never filled it.
    // Where it is empty the test answers `false` for EVERY address, so the scan
    // below inspects every verifiable slot, classifies none of them as young,
    // and returns "nothing unpublished" without having verified anything.
    //
    // That vacuous pass is not a theoretical hazard: it is how `PolynomialTest`
    // got `incomplete=false` under `-XX:+UseG1GC`, which let `roots.rs`
    // suppress the conservative scan, which left G1's pin set empty, which let
    // the pause evacuate a region a live compiled frame still referenced.
    //
    // It now asks `gen_heap::addr_is_movable`, the union of that young table
    // with `MOVABLE_BOUNDS` — a third table a collector fills to say what its
    // relocating phase may move, precisely because filling `JIT_REGION_BOUNDS`
    // to fix this verifier would silently re-enable an inline reference STORE
    // fast path G1 and ZGC must not have. ZGC publishes its arena envelope
    // there; generational still answers through the young table, so its
    // behaviour is unchanged.
    //
    // Fail closed, exactly as the module block above says this verifier does
    // for an unbounded band or an unresolvable shadow window: an uninspectable
    // frame reports "not verified", never "verified clean". The gate is on the
    // UNION being live, so a collector that publishes neither table still gets
    // the refusal it had before rather than a quiet pass.
    //
    // The gate has TWO ways to fire and they are reported apart, because the
    // second one looks like success from every angle the first is checked
    // from. A table can be useless because it is empty (nobody published) or
    // because it is about somebody else: both tables are process-global and
    // discriminated by slot 0, so each describes exactly ONE heap, and a second
    // live heap leaves the loser's every address answering `false` to
    // `addr_is_movable` — published, fresh, and not about these frames. See
    // `gen_heap::RELOCATABLE_HEAPS_LIVE`.
    if bounds_guard_enabled() {
        if !cratonvm_gc::gen_heap::published_bounds_represent_every_live_heap() {
            *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::BOUNDS_NOT_REPRESENTATIVE;
            return true;
        }
        if !cratonvm_gc::gen_heap::movable_bounds_are_live() {
            *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::YOUNG_BOUNDS_UNPUBLISHED;
            return true;
        }
    }
    let scanner_sp = current_stack_pointer();
    let mut unverified = false;
    let mut unpublished = false;
    // P3 of `gengc-r4-mark-conservative-stack-scan-gaps` (gen r4w3/young2,
    // 2026-09-23): the published-value set is keyed by its shadow WINDOW and
    // rebuilt only when an entry resolves a different one. It used to be
    // rebuilt for every chain entry — up to `DEFAULT_SHADOW_SLOTS` inserts
    // each — although every entry of one thread resolves into the same
    // thread-owned buffer, and nothing in this loop writes to it, so an equal
    // window reads equal values. Same answer, one build per distinct window.
    let no_values = LazyPublished::new(None);
    // gce e2/j: the set is built only when a band word is movable at all
    // (`LazyPublished`); a frame holding no young-looking word never pays for
    // it.
    let mut published_memo: Option<(Option<(usize, usize)>, LazyPublished)> = None;
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            flush_top_rbp_cache_to_chain(chain.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            let Some(info) = entry.precise else {
                // A conservative (map-less) entry is already reported by the
                // NO_PRECISE_MAP obligation; nothing to verify here.
                continue;
            };
            let entry_sp = entry.entry_sp;
            let mut rbp = info.exact_rbp;
            if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
                // MISSING_EXACT_RBP already covers rbp == 0; an out-of-range
                // value means the band cannot be located at all.
                unverified = true;
                continue;
            }
            let Some(innermost_cm) = innermost_frame_method(
                rbp,
                info.exact_cm_id,
                entry_sp,
                scanner_sp,
                info.compiled_method,
            ) else {
                // Nothing describes the frame now standing at `exact_rbp` — the
                // inline MIC/PIC cascade or the hashed megamorphic stub reached
                // it through an indirect call, so its method cannot be recovered
                // from the return address. This is the case
                // `refresh_moving_young_coverage_for_current_thread` reports as
                // FOREIGN_INNERMOST_RBP. Every offset this loop reads
                // (`shadow_thread_slot_off`, `osr_frame_size`, the band bounds)
                // would belong to the wrong method, so nothing here can be
                // verified. `scan_one_frame_precise` already declines to publish
                // an oop map under the same condition; declining to VERIFY is the
                // fail-closed counterpart -- it forces the non-moving sweep for
                // this cycle instead of trusting a band read out of the wrong
                // frame.
                unverified = true;
                continue;
            };
            // SAFETY: same contract as `scan_precise_frames_and_bands` — the chain
            // entry's CompiledMethod is Arc-owned by the JIT cache while any of
            // its frames is live, and a resolved callee is kept alive by the
            // live frame whose return address resolved it.
            let mut cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
            // §25.4 -- keep the WINDOW, not just the values read out of it.
            // `published_shadow_values(None)` and a genuinely empty window both
            // return an empty set, so `published=0` alone cannot say whether
            // publication was declined or the window could not be resolved at
            // all. Those point at opposite repairs.
            let window = shadow_window_from_frame(rbp, cm);
            if !matches!(&published_memo, Some((w, _)) if *w == window) {
                published_memo = Some((window, LazyPublished::new(window)));
            }
            let published: &LazyPublished =
                published_memo.as_ref().map_or(&no_values, |(_, set)| set);
            // gch h1/jw (r17): bounded by the entry band, and a cut walk is
            // an unverified one -- the frames past the cut were never read.
            let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
            loop {
                if !walk.step() {
                    unverified = true;
                    break;
                }
                let frame_size = cm.osr_frame_size;
                if frame_size <= 0 {
                    unverified = true;
                    break;
                }
                let frame_size = frame_size as usize;
                const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
                if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
                    unverified = true;
                    break;
                }
                let live_hi = moving_young_frame_live_hi(rbp, cm);
                coverage_census_note(crate::native::jni::JniPhase::CovBandFrames);
                coverage_census_count(
                    crate::native::jni::JniPhase::CovBandWords,
                    (frame_size / 8) as u64,
                );
                if band_has_unpublished_young_word(rbp, frame_size, cm, live_hi, published) {
                    if band_dbg() {
                        report_unpublished_band_words(
                            rbp,
                            frame_size,
                            cm,
                            live_hi,
                            published.get(),
                            window,
                        );
                    }
                    unpublished = true;
                    break;
                }
                // `[rbp]` = saved caller RBP, `[rbp + 8]` = return PC into it.
                // SAFETY: `rbp` was validated to lie in this thread's live JIT
                // stack interval.
                let parent_rbp = unsafe { (rbp as *const usize).read() };
                let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
                let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    // A non-JIT parent ends the walk cleanly: it owns no
                    // compiled spill band.
                    break;
                };
                if parent_rbp <= rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp >= entry_sp
                    || parent_rbp < scanner_sp
                {
                    unverified = true;
                    break;
                }
                // SAFETY: code ranges retain their CompiledMethod metadata for
                // the lifetime of an active frame.
                cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
                rbp = parent_rbp;
            }
            if unpublished {
                break;
            }
        }
    });
    if unpublished {
        *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::UNPUBLISHED_FRAME_OOP;
        return true;
    }
    if unverified {
        *reason_out = cratonvm_gc::gc_quiescence::incomplete_reason::UNBOUNDED_FRAME_BAND;
        return true;
    }
    false
}

/// `CRATONVM_MOVING_YOUNG_BAND_DBG` — dump the frame offset of every word the
/// band scan rejected, so the storage class responsible can be named instead of
/// guessed at. Latched once; the scan runs on every collection.
pub(in crate::jit::conservative_roots) fn band_dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// The safepoint id the frame at `rbp` is standing on, or `None` when the
/// method reserved no id slot or the slot is misaligned.
///
/// Same read as `moving_young_frame_coverage_complete` makes; split out so the
/// band reporter can name the ACTIVE map without duplicating the contract.
/// Diagnostic only — every caller is behind `CRATONVM_MOVING_YOUNG_BAND_DBG`.
pub(in crate::jit::conservative_roots) fn frame_active_sp_id(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<u32> {
    let off = cm.sp_id_slot_off;
    // See `active_safepoint_id`: such a frame's slot names no safepoint.
    if off == 0 || cm.frame_holds_no_references {
        return None;
    }
    let addr = rbp.checked_sub(off as usize)?;
    if addr & 0x7 != 0 {
        return None;
    }
    // SAFETY: aligned safepoint-id slot of a live JIT frame on this thread,
    // reached through the same bounds the caller already validated `rbp` with.
    let v = unsafe { (addr as *const usize).read() };
    Some(v as u32)
}

pub(in crate::jit::conservative_roots) fn report_unpublished_band_words(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    live_hi: Option<i32>,
    published: &PublishedSet,
    // The shadow window `published` was read out of, resolved from the
    // INNERMOST frame of this chain. `None` means `shadow_window_from_frame`
    // declined -- no reserved thread slot, a misaligned or foreign thread
    // pointer -- which is a different fact from a window that resolved and held
    // nothing, and the `published` set cannot tell the two apart.
    window: Option<(usize, usize)>,
) {
    let win = match window {
        None => format!(
            "unresolved(frame={} thread={})",
            shadow_window_from_frame_why(rbp, cm).err().unwrap_or("ok"),
            shadow_window_for_thread_why(cm).err().unwrap_or("ok"),
        ),
        Some((base, top)) => format!("0x{base:x}..0x{top:x}({})", top.saturating_sub(base) / 8),
    };
    let map_slots = frame_active_map_slots(rbp, cm);
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    let mut hits = 0usize;
    while addr + 8 <= rbp && hits < 32 {
        // SAFETY: same bounded, aligned walk as `band_has_unpublished_word_with`.
        let w = unsafe { (addr as *const usize).read() };
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if band_slot_is_verifiable_with_map(off, &cm.frame_layout, live_hi, map_slots.as_deref())
            && cratonvm_gc::gen_heap::addr_is_movable(w)
            && !published.contains(&w)
        {
            hits += 1;
            // THE FORK, decided per word. `in_map` asks whether the ACTIVE
            // safepoint's oop map already names this slot:
            //
            //   in_map=true  — the dataflow calls the slot a LIVE reference here
            //                  and the map names it, but the shadow push did not
            //                  publish it. The two channels disagree, and
            //                  `collect_live_oop_homes` is the side that is
            //                  wrong.
            //   in_map=false — the dataflow does NOT call it live. Declining to
            //                  publish is then CORRECT, the slot merely holds a
            //                  stale reference nothing will read, and the band
            //                  verifier is refusing on a dead word. The repair
            //                  belongs on the verifier (a liveness bound it can
            //                  consult), not on codegen.
            //
            // Without this the two are indistinguishable from the outside, which
            // is how `bug-h2-testkillprocess-zgc-oom-at-97-percent-free`'s
            // `MVStore.closeStore` residual sat unresolved: 64 of 83 unpublished
            // words are that method's java-locals, one object in three
            // consecutive slots.
            let sp_id = frame_active_sp_id(rbp, cm);
            // THREE answers, not two. `sp_id.map(..)` gave `Some(false)` both
            // when a map was found and did not name the slot AND when NO MAP
            // EXISTS for the stored id -- and those point at opposite repairs.
            //
            // Measured on `org.h2.test.jdbc.TestCachedQueryResults`
            // (2026-08-29): six of seven reported words came from one frame
            // whose stored id was `1729768472`. That is not a bytecode pc; it
            // is `0x671a2c18`, the low 32 bits of `0x200671a2c18` -- the heap
            // pointer this same scan reports two slots away in the same frame.
            // The frame's sp-id slot holds half an object pointer, so no map
            // can match it, and `live_hi=None` on the same line says the same
            // thing. Printed as `Some(false)` that read as "the dataflow calls
            // this slot dead", which sends a reader at the band verifier
            // instead of at the frame whose id is garbage.
            //
            // `no-map-for-id` is also the honest label for what the SCAN does
            // here: `frame_active_map_slots` returns `None`, so the dead-slot
            // relaxation deliberately does not fire and the word is reported.
            // The report now says which of the two it is.
            let in_map: &'static str = match sp_id {
                None => "no-sp-id",
                Some(id) => {
                    let mut any_map = false;
                    let mut names_slot = false;
                    for m in cm.oop_maps.iter().filter(|m| m.bytecode_pc == id) {
                        any_map = true;
                        if m.frame_slot_offsets.iter().any(|s| i32::from(*s) == off) {
                            names_slot = true;
                        }
                    }
                    if !any_map {
                        "no-map-for-id"
                    } else if names_slot {
                        "true"
                    } else {
                        "false"
                    }
                }
            };
            // §25.3's PROBE -- the safepoint KIND, printed as
            // `pushed/named` per map matching the resolved id.
            //
            // Four candidate explanations for an `in_map=true` word that the
            // shadow push did not publish were eliminated in §25.1-.2, and the
            // structural fact that survived is that publication is CALL-SCOPED:
            // `emit_shadow_push` runs on the path into a call, and
            // `emit_shadow_reload` pops the homes again at the top of
            // `emit_oop_map_for_safepoint`. A safepoint that is not call-shaped
            // -- a loop poll, a re-entry -- therefore carries a map naming N
            // slots while nothing was ever pushed for it. The band verifier's
            // obligation has no such scope: it asks about EVERY safepoint.
            //
            // `0/N` on the reporting safepoints is that answer, and it puts the
            // repair in the PAIRING (publish for poll-shaped safepoints too, or
            // let the verifier consult the map instead of the push) rather than
            // in either side alone. A nonzero left-hand number refutes it and
            // sends the search back to `collect_live_oop_homes`.
            let shadow_pushed: String = match sp_id {
                None => "n/a".to_string(),
                Some(id) => {
                    let v: Vec<String> = cm
                        .oop_maps
                        .iter()
                        .filter(|m| m.bytecode_pc == id)
                        .map(|m| format!("{}/{}", m.shadow_pushed, m.frame_slot_offsets.len()))
                        .collect();
                    if v.is_empty() {
                        "none".to_string()
                    } else {
                        v.join(",")
                    }
                }
            };
            // THE DISCRIMINATOR for a `no-map-for-id` frame, and the reason
            // this dump exists at all.
            //
            // A safepoint id that is not a bytecode pc has two opposite causes:
            // something STORED an oop into the reserved sp-id slot (a codegen
            // defect at that offset), or `rbp` is wrong for this frame so the
            // read lands on a NEIGHBOURING slot that legitimately holds one (a
            // frame-resolution defect, the family of the 2026-08-26
            // innermost-mirror repair). Printing the whole reserved-locals tail
            // beside `sp_id_off` separates them in one line: a pointer sitting
            // exactly at `sp_id_off` is the first; the whole tail reading like
            // some neighbouring frame's is the second.
            if in_map == "no-map-for-id" {
                let lo = cm.frame_layout.java_locals_hi.max(8);
                let hi = cm.frame_layout.locals_hi;
                let mut tail = String::new();
                let mut o = lo;
                while o <= hi && o - lo < 128 {
                    let a = rbp.wrapping_sub(o as usize);
                    if a >= rbp - frame_size && a + 8 <= rbp && a & 7 == 0 {
                        // SAFETY: the same bounded, aligned in-band read the
                        // walk above makes, on this thread's own frame.
                        let v = unsafe { (a as *const usize).read() };
                        tail.push_str(&format!(" [{o}]=0x{v:x}"));
                    }
                    o += 8;
                }
                eprintln!(
                    "[moving-young-band]   no-map-for-id sp_id_off={} tail({}..{}):{}",
                    cm.sp_id_slot_off, cm.frame_layout.java_locals_hi, hi, tail,
                );
            }
            eprintln!(
                "[moving-young-band] {} off={off} region={} value=0x{w:x} published={} window={win} \
                 sp_id={sp_id:?} sp_id_off={} in_map={in_map} shadow_pushed={shadow_pushed} \
                 live_hi={live_hi:?} layout={:?}",
                cm.method_label,
                cm.frame_layout.region_name(off),
                published.len(),
                cm.sp_id_slot_off,
                cm.frame_layout,
            );
        }
        addr += 8;
    }
}

/// `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD=1` — let the frame-band verifier run
/// against an unpublished young-bounds table, i.e. let it pass vacuously.
///
/// The pre-fix behaviour, kept as an A/B arm. Unsafe on any collector that does
/// not publish `JIT_REGION_BOUNDS`, which is every collector except the
/// generational one.
pub(in crate::jit::conservative_roots) fn bounds_guard_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !false
    })
}

/// `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` — drop the frame-band verification
/// entirely and take the codegen's `moving_young_coverage_complete` bit at its
/// word. A MEASUREMENT INSTRUMENT: it is how "does the codegen model actually
/// cover this workload?" is asked, and it is unsafe to run with if the answer
/// is no. Not a supported configuration.
///
/// (gen r5w1/young5: this doc sat on `bounds_guard_enabled`, stranded one item
/// early.)
pub(in crate::jit::conservative_roots) fn band_verify_disabled() -> bool {
    static OFF: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&OFF, || {
        false    })}

/// `CRATONVM_MOVING_YOUNG_NO_JIT` — re-arm the process-wide JIT relocation
/// boundary: refuse every moving young collection while ANY compiled code
/// exists, without consulting the per-frame coverage proof.
///
/// This was the hard-coded behaviour between `86e69e848` (2026-07-29) and the
/// default-on closeout; see [`refresh_moving_young_coverage_for_current_thread`]
/// for why the per-cycle proof is the authority instead. Kept as a knob because
/// it is the one setting that makes "is this a moving-young defect?" a
/// single-variable experiment, and because it is the correct emergency lever if
/// a workload ever exposes an obligation the verifier does not model.
pub(in crate::jit::conservative_roots) fn moving_young_no_jit() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

/// gcd d2/f (found by JIT round 13 wave 1, lane crash): a JIT shadow push
/// that would pass the buffer's `end` stores nothing, tags the saved base and
/// bumps [`cratonvm_jit::SHADOW_OVERFLOW_COUNT`]; its reload then skips the
/// value restore (`jit/src/x64/safepoint.rs` `emit_shadow_push`, and its twin
/// in `jit/src/ir_lower.rs`). That safepoint's register-resident oops, staged
/// arguments and frame homes are published NOWHERE for the duration of the
/// call, while its compile-time `moving_young_coverage_complete` claim is
/// unchanged. A moving young cycle that spent the claim relocated those
/// objects without rewriting the frame, which resumed with stale pointers.
///
/// The count is process-wide and never decreases, so it is only the cheap
/// pre-filter (gcd d3/o). Which cycles a bail can hurt is decided per thread,
/// on the thread that owns the frames (every caller of this function runs
/// there): a bail is live on this thread only while its shadow `top` is
/// within [`SHADOW_BAIL_MAX_PUSH_BYTES`] of `end` -- see
/// [`shadow_bail_may_be_live`] for the invariant. A thread that has never
/// installed a JIT thread cannot have pushed at all. So after a bail only the
/// cycles in which some proving thread's shadow stack is still near full take
/// the non-moving sweep (whose conservative band scan does see the unpublished
/// words); the rest move again, in every VM of the process.
///
/// `CRATONVM_JIT_SHADOW_BAIL_BLOCKS_MOVING=0` restores the old (unsound)
/// trust; `CRATONVM_JIT_SHADOW_BAIL_STICKY=1` restores gcd d2/f's
/// process-wide sticky refusal (every cycle after the first bail). Both are
/// read only once a bail has happened, and with no static
/// (`vm/tests/per_vm_state_statics_ratchet.rs`).
pub(in crate::jit::conservative_roots) fn shadow_bail_refuses_moving() -> bool {
    shadow_bail_verdict(
        cratonvm_jit::SHADOW_OVERFLOW_COUNT.load(Ordering::Relaxed),
        || true,
        || false,
        current_thread_shadow_extent,
    )
}

/// An upper bound on the bytes ONE compiled shadow push can ask for, which is
/// how close to `end` a bailed push's `top` must have been.
///
/// Single-pass (`x64::Compiler::collect_live_oop_homes`) pushes DISTINCT homes:
/// at most one per operand-stack entry (`max_stack <= 65535`), one per
/// reference local (`max_locals <= 65535`), the staged invoke arguments (at
/// most 255), at most 64 locals per live inline splice, the live LICM hoist
/// slots, and registers that alias one of those. That is 1 MiB plus a few
/// KiB for anything a class file can express. The IR tier pushes at most one
/// word per frame slot of a frame capped at `DEFAULT_MAX_FRAME_BYTES`
/// (32 KiB), and its frame block at most 32 words. 1.5 MiB covers all of it;
/// on a buffer no larger than this every bail reads as live (the gcd d2/f
/// refusal).
pub(in crate::jit::conservative_roots) const SHADOW_BAIL_MAX_PUSH_BYTES: usize = 1536 * 1024;

/// What [`shadow_bail_verdict`] learns about the calling thread's shadow
/// stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum ShadowBailExtent {
    /// No `JvmThread` is installed (`JIT_THREAD` is null). Every compiled
    /// frame caches the thread its prologue read from `JIT_THREAD`, and a push
    /// through a null cached thread is skipped whole, so no live frame of this
    /// thread can have bailed: `JIT_THREAD` returns to null only when the
    /// outermost `set_jit_thread` scope's call has returned.
    NoJitThread,
    /// The installed thread's shadow stack: `top` and `end`.
    Window { top: usize, end: usize },
    /// A thread is installed but its shadow stack does not validate. Refuse.
    Unresolved,
}

/// The calling thread's shadow stack, validated exactly as the band
/// verifier's window is (`shadow_window_at`: 8-aligned fields, `end - base`
/// the fixed buffer size, `top` inside `[base, end]`).
pub(in crate::jit::conservative_roots) fn current_thread_shadow_extent() -> ShadowBailExtent {
    let current = crate::jit::helpers::current_jit_thread_ptr() as usize;
    if current == 0 {
        return ShadowBailExtent::NoJitThread;
    }
    let Ok(off) = i32::try_from(crate::threading::JvmThread::shadow_stack_offset()) else {
        return ShadowBailExtent::Unresolved;
    };
    match shadow_window_at(current, off) {
        Ok((base, top)) => ShadowBailExtent::Window {
            top,
            end: base + cratonvm_gc::shadow_stack::DEFAULT_SHADOW_SLOTS * 8,
        },
        Err(_) => ShadowBailExtent::Unresolved,
    }
}

/// Could a push that bailed on THIS shadow stack still be in flight?
///
/// The invariant: a bail leaves `top` where the push found it (`t`), and it
/// bailed because `t + need > end`. While the bailed frame's call is in
/// flight nothing lowers `top` below `t`: every later push starts at or above
/// it and its reload restores `top` to its own saved base (itself `>= t`);
/// the JIT-exit heal (`restore_jit_thread`) restores a watermark captured
/// inside that call (so `>= t`); and an exception or deopt that retracts
/// `top` does so only for frames entered after `t` was reached. So a live
/// bail implies `top + need > end`, and `need <=`
/// [`SHADOW_BAIL_MAX_PUSH_BYTES`]. The converse does not hold (a deep but
/// clean stack also answers yes), which is the fail-closed direction.
pub(in crate::jit::conservative_roots) fn shadow_bail_may_be_live(top: usize, end: usize) -> bool {
    top.saturating_add(SHADOW_BAIL_MAX_PUSH_BYTES) > end
}

/// The pure half of [`shadow_bail_refuses_moving`]: refuse when some push has
/// bailed, the switch is on, and either the sticky mode is asked for or this
/// thread's shadow stack could still hold a live bail. The switches and the
/// extent are consulted only after a bail.
pub(in crate::jit::conservative_roots) fn shadow_bail_verdict(
    overflow_count: usize,
    blocks_moving: impl FnOnce() -> bool,
    sticky: impl FnOnce() -> bool,
    extent: impl FnOnce() -> ShadowBailExtent,
) -> bool {
    if overflow_count == 0 || !blocks_moving() {
        return false;
    }
    if sticky() {
        return true;
    }
    match extent() {
        ShadowBailExtent::NoJitThread => false,
        ShadowBailExtent::Window { top, end } => shadow_bail_may_be_live(top, end),
        ShadowBailExtent::Unresolved => true,
    }
}

/// Scan one compiled frame's band `[rbp - frame_size, rbp)` for a word that
/// lands in a published young semispace and is absent from `published`.
///
/// Slots that are only ever an IMAGE of a register are skipped
/// ([`cratonvm_jit::FrameLayout::is_register_image`]): the prologue's
/// callee-saved GPR/XMM save area holds the CALLER's values, and the
/// per-safepoint blind GPR spill is write-only. Neither is a place the owning
/// frame resumes from, and both are full of DEAD register values — scanning
/// them yields "an oop was missed" on every collection, which is a false
/// verdict that costs every moving cycle. The genuine storage classes (Java
/// locals, operand spills, LICM hoist slots, scalar-replacement fields,
/// outgoing stack args) are all still scanned.
pub(in crate::jit::conservative_roots) fn band_has_unpublished_young_word(
    rbp: usize,
    frame_size: usize,
    cm: &cratonvm_jit::CompiledMethod,
    live_hi: Option<i32>,
    published: &(impl PublishedWords + ?Sized),
) -> bool {
    // Every word such a frame's verifiable band holds is a primitive or dead
    // residue (`CompiledMethod::frame_holds_no_references`); a young-looking
    // one is a stale bit pattern a collection need not rewrite. Its register
    // images are outside the verifiable band for every frame alike.
    if cm.frame_holds_no_references {
        return false;
    }
    // gcf f1/j: the package's in-native deposit, envelope first. The scan
    // below reports a word only if `addr_is_movable` accepts it, so a band
    // with no such word answers `false` without the active map's slots (a
    // sorted `Vec` per frame), the safepoint id or the per-word test.
    if in_native_envelope_first()
        && frame_size <= rbp
        && !band_has_word(
            rbp - frame_size,
            rbp,
            cratonvm_gc::gen_heap::addr_is_movable,
        )
    {
        coverage_census_note(crate::native::jni::JniPhase::EnvFrameSkipped);
        return false;
    }
    let map_slots = frame_active_map_slots(rbp, cm);
    // The safepoint id is what makes the verifier's answer specific to THIS
    // point in the method; without it there is no liveness question to ask.
    let sp_id = active_safepoint_id(rbp, cm);
    let ask = sp_id.map(|bci| move |off: i32| verifier_local_verdict(cm, off, bci));
    let ask_ref: Option<&dyn Fn(i32) -> VerifierSlotVerdict> = ask.as_ref().map(|x| x as _);
    band_has_unpublished_word_with_map(
        rbp,
        frame_size,
        &cm.frame_layout,
        live_hi,
        published,
        map_slots.as_deref(),
        cratonvm_gc::gen_heap::addr_is_movable,
        // The production predicate, whose range is the committed prefix.
        true,
        ask_ref,
        // JIT round 25 (item 3): a claimed `Prim` colour holds a primitive or
        // residue the clear no longer zeroes, never an unpublished reference;
        // the band scan does not root it either (`ir_prim_colour_claim`).
        ir_prim_colour_claim_on_chain(cm),
    )
}

/// [`active_map_slots`] widened to `i32`, sorted and de-duplicated, for the
/// band scan's per-word membership test.
///
/// gen r5w1/young5 (P3, second half, of
/// `docs/feature-designs/generative-gc-architecture.md`):
/// a `Vec`, not a `HashSet<i32>` built per frame. A safepoint map names a
/// handful of slots, so a slice `contains` is as fast as a hash probe and the
/// per-frame hash-table allocation is gone. Same answers: the set of offsets
/// is identical.
///
/// `None` — no id, or no map for it — is the fail-closed answer: it leaves every
/// band word verifiable, because without a liveness statement the scan must not
/// assume anything is dead.
pub(in crate::jit::conservative_roots) fn frame_active_map_slots(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<Vec<i32>> {
    let mut slots: Vec<i32> = active_map_slots(rbp, cm)?
        .into_iter()
        .map(i32::from)
        .collect();
    slots.sort_unstable();
    slots.dedup();
    Some(slots)
}

/// Is `off` in a region the JIT's abstract interpreter MODELS?
///
/// Java locals and operand-spill slots are exactly what
/// `moving_young_coverage_complete` is computed from, so for those the
/// safepoint's oop map carries a definite liveness statement. Everything else
/// the band scan looks at — the arith hoist slots, scalar-replacement fields,
/// the reserved-locals tail, and the ref-hoist slots of a frame that does not
/// set `ref_hoist_mapped` — is precisely what the map has NO opinion about,
/// which is the gap the band verifier was built to close (see
/// `frame_band_scan_rejects_a_relocatable_word_the_shadow_stack_never_published`).
///
/// V6 (round 12 wave 4, `r12w4-vec2-ref-hoist-modelled-region-patch`): a frame
/// with `FrameLayout::ref_hoist_mapped` names every LIVE ref-hoist slot in the
/// map of each safepoint inside its loop, so there an unnamed ref-hoist word
/// is dead too. Like every other deadness claim here, it is spent only against
/// a map that RESOLVED for the frame's safepoint id.
pub(in crate::jit::conservative_roots) fn region_is_dataflow_modelled(layout: &cratonvm_jit::FrameLayout, off: i32) -> bool {
    if layout.java_locals_hi > 0 && off < layout.java_locals_hi {
        return true;
    }
    // V6: the JIT names every live ref-hoist slot in its in-loop maps
    // (`FrameLayout::ref_hoist_mapped`), so here too an unnamed word is dead.
    if layout.ref_hoist_mapped
        && layout.ref_hoist_hi > layout.ref_hoist_lo
        && off >= layout.ref_hoist_lo
        && off < layout.ref_hoist_hi
    {
        return true;
    }
    layout.spill_hi > layout.spill_lo && off >= layout.spill_lo && off < layout.spill_hi
}

/// gci i6/mv: is the band word at `off` one the ACTIVE map proves dead --
/// a region the dataflow models ([`region_is_dataflow_modelled`]) whose slot
/// the resolved map (`map_slots`) does not name? Exactly the dead-word arm of
/// [`band_slot_is_verifiable_with_map`] (and so of the movable claim in
/// [`publish_validated_band_word`]); ask it only of a word that is verifiable
/// without the map. `None` (no map resolved) is never dead, as there.
/// Read by the root-remap audit's census only.
pub(in crate::jit::conservative_roots) fn band_word_is_map_dead(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    map_slots: Option<&[i32]>,
) -> bool {
    band_map_liveness_enabled()
        && map_slots.is_some_and(|slots| {
            region_is_dataflow_modelled(layout, off) && !slots.contains(&off)
        })
}

/// `CRATONVM_GC_NO_BAND_MAP_LIVENESS=1` — make the band verifier inspect every
/// word in a modelled region again, whether or not the active safepoint map
/// names it.
///
/// The bisect lever for the dead-slot exemption, default-ON.
pub(in crate::jit::conservative_roots) fn band_map_liveness_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !false)
}

/// gcj j2/pn2 (`gcj-j1pn-young-pin-ledger-review-findings-FIXED-20261025` item 1):
/// does the dead-spill excuse of [`band_slot_is_verifiable_with_map`] skip the
/// word at `[rbp - off]`? The excuse is "an operand-spill slot at or above the
/// safepoint's live cursor `hi` holds a stale value nothing reads". Unbounded
/// (`bounded == false`, the default) it excused every offset at or above
/// `max(spill_lo, hi)` -- past `spill_hi`, the typed ranges (`licm-ref-hoist`,
/// `licm-arith`, `scalar-replaced-field`) and the locals tail included, which
/// hold live references the claim says nothing about. Bounded, it excuses
/// exactly [`layout_is_operand_spill`] words, the rule its sibling
/// [`spill_slot_is_dead_above_cursor`] already applies. Pure, for the tests.
#[inline]
pub(in crate::jit::conservative_roots) fn band_dead_spill_excuses(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    hi: i32,
    bounded: bool,
) -> bool {
    layout.spill_hi > layout.spill_lo
        && off >= layout.spill_lo
        && off >= hi
        && (!bounded || layout_is_operand_spill(layout, off))
}

/// `CRATONVM_GEN_BAND_DEAD_SPILL_BOUNDED` -- gcj j2/pn2, read per call (no
/// process latch: the statics ratchet) and asked only for a word the bounded
/// rule would refuse to excuse, and in effect on the GENERATIONAL backend
/// only (`published_young_regions_are_live`: G1 and ZGC never publish the
/// young table, so their coverage verifier keeps the unbounded excuse
/// whatever the switch says). See [`band_dead_spill_excuses`].
///
/// WARNING gcj-j1pn-user-flip-20261025: default ON since the gcj j1pn
/// retirement (`=0` restores the unbounded excuse). Its flip rows (block BS:
/// `GenR4W6JitOomRootProbe` x2, the stale-objref canary, the gauntlet, the
/// pinned-copy probe, G1, ZGC) were all SAME as HotSpot and clean, and the
/// `[GC] decision histogram` moved by no more than the default rows' own
/// spread (JOR `moving` 71-72 on vs 68-71 def; gauntlet 91 vs 97-98). Revert
/// here first if an application shows fewer moving young cycles.
pub(in crate::jit::conservative_roots) fn band_dead_spill_bounded() -> bool {
    cratonvm_gc::gen_heap::published_young_regions_are_live()
}

/// Whether the verifier must inspect the word at `[rbp - off]`.
///
/// Skipped:
///
///   * everything at or beyond `callee_saved_lo` — the prologue's callee-saved
///     GPR/XMM save area, the write-only per-safepoint blind GPR spill, the
///     frame-deopt `SavedRegisters` block, the ABI shadow space and the
///     outgoing stack-arg reserve. Register IMAGES and outgoing arguments, not
///     storage this frame resumes from, and full of dead register values;
///   * operand-spill slots above the safepoint's live cursor. The cursor
///     reclaims by moving, so those slots keep whatever the deepest earlier
///     operand stack left in them — in allocation-heavy code, a stale object
///     pointer. Nothing reads them again.
///
/// Scanned: Java locals, the reserved-locals tail, LICM hoist slots,
/// scalar-replacement fields and the live operand-spill slots — every place a
/// compiled frame actually keeps a reference it will use after the call.
///
/// (gen r5w1/young5: this doc sat on `frame_active_map_slots`, stranded one
/// item early, so that function's docs described this one.)
pub(in crate::jit::conservative_roots) fn band_slot_is_verifiable(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
) -> bool {
    band_slot_is_verifiable_with_map(off, layout, live_hi, None)
}

/// [`band_slot_is_verifiable`] plus the ACTIVE safepoint's mapped slot offsets,
/// when one resolved.
///
/// `map_slots = Some(set)` means the dataflow has stated, for this exact pc,
/// which slots hold live references. In a region it MODELS, a word it does not
/// name is DEAD: the slot holds whatever an earlier scope left there, and
/// nothing will read it again — so demanding that it be published on the shadow
/// stack refuses a collection over a word that needs no rewriting.
///
/// Measured on `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
/// **all 74** of one run's unpublished words reported `in_map=false`, 64 of them
/// in `MVStore.closeStore:(ZI)V` at `sp_id=174`, four slots (offsets 48/56/64/72
/// = locals 5..8) all holding the SAME object. `closeStore`'s
/// `LocalVariableTable` puts slot 5 (`map`) in scope 149..170, so at bci 174 it
/// is out of scope and 6..8 are javac's loop/finally scaffolding copies of it.
/// The map, the shadow push and the dataflow all agreed; only the band scan
/// objected, and it was objecting to dead words.
///
/// `None` — no map resolved for the frame's sp-id — keeps every word verifiable,
/// which is the fail-closed direction: without a liveness statement the scan
/// makes no assumption about what is dead.
pub(in crate::jit::conservative_roots) fn band_slot_is_verifiable_with_map(
    off: i32,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    map_slots: Option<&[i32]>,
) -> bool {
    // The half-line: on a frame whose callee-save area is DEEPEST (x86-64),
    // everything at or beyond it is a register image or past the frame. A
    // backend that puts that area next to the frame pointer instead says so,
    // and gets the range exclusion below rather than this -- otherwise the
    // half-line would swallow its whole spill area, which is precisely the
    // region the oop maps describe.
    if !layout.callee_saved_shallow && layout.callee_saved_lo > 0 && off >= layout.callee_saved_lo {
        return false;
    }
    if layout.is_register_image(off) {
        return false;
    }
    if let Some(hi) = live_hi {
        // gcj j2/pn2: the switch is asked only for a word the unbounded
        // excuse covers and the bounded one does not (no latch: the statics
        // ratchet), so an operand-spill word costs no flag read.
        if band_dead_spill_excuses(layout, off, hi, false)
            && (layout_is_operand_spill(layout, off) || !band_dead_spill_bounded())
        {
            return false;
        }
    }
    if band_map_liveness_enabled() {
        if let Some(slots) = map_slots {
            if region_is_dataflow_modelled(layout, off) && !slots.contains(&off) {
                return false;
            }
        }
    }
    // §22.3's experiment, opt-in: a slot the ACTIVE OOP MAP names is covered by
    // the precise path, so the shadow stack need not also publish it.
    //
    // This is the COMPLEMENT of the rule just above. That one says "modelled
    // region, absent from the map => dead, skip"; this one says "present in the
    // map => the collector already visits it, skip". Together they leave only
    // the unmodelled regions, which is where §22.2's dump puts the other 26 of
    // 93 words (`reserved-locals-tail`, `in_map=false`).
    //
    // The claim it rests on: `scan_active_oop_map_at_rbp` resolves the map for
    // this safepoint and iterates its `slot_offsets`, so the root scan visits
    // the slot and `remap_active_jit_frames` rewrites it after a move. If that
    // holds, the band test reporting the slot as "unpublished" is asking about
    // a mechanism (shadow-stack publication) that is not the one covering it.
    //
    // OPT-IN, and REFUTED — see §24 and the flag's own doc. The claim above is
    // false in practice: with this on, the stale-after-remap detector reports
    // 260-545 stale words per run against 2-18 with it off, 54-140 of them in
    // java locals. Whatever `in_map` guarantees, it is not "the precise path
    // rewrote this slot", and the shadow-publication requirement is not
    // redundant for such words. The switch survives as the lever that measured
    // that, and it is fail-OPEN, so off is safe.
    if band_skip_in_map_enabled() {
        if let Some(slots) = map_slots {
            if slots.contains(&off) {
                return false;
            }
        }
    }
    true
}

/// `CRATONVM_MOVING_YOUNG_BAND_SKIP_IN_MAP=1` — §22.3's experiment, and
/// **REFUTED**. Do not enable it outside that experiment.
///
/// The claim was that a slot the active oop map names is rewritten by
/// `remap_active_jit_frames`, so the shadow stack need not also publish it.
/// The stale-after-remap detector says otherwise (§24): with this on,
/// `CoverageBench` leaves 260-545 stale words per run against 2-18 with it off,
/// 54-140 of them in `region=java-local` against 0-10. Those are references the
/// collector moved and nothing rewrote — precisely the corruption the
/// shadow-publication requirement exists to prevent.
///
/// Kept, rather than deleted, because it is the lever that produced that
/// answer and the one that would re-test it if the underlying mechanism ever
/// changes. It is fail-OPEN, so leaving it off is leaving it safe.
pub(in crate::jit::conservative_roots) fn band_skip_in_map_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// Predicate-injected core of [`band_has_unpublished_young_word`], so the scan
/// itself is unit-testable without mutating the process-global published
/// region-bounds table (which parallel tests share).
pub(in crate::jit::conservative_roots) fn band_has_unpublished_word_with(
    rbp: usize,
    frame_size: usize,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    published: &(impl PublishedWords + ?Sized),
    is_relocatable: impl Fn(usize) -> bool,
) -> bool {
    band_has_unpublished_word_with_map(
        rbp,
        frame_size,
        layout,
        live_hi,
        published,
        None,
        is_relocatable,
        // An arbitrary caller-supplied predicate: its words are not known to be
        // dereferenceable, so no header screen.
        false,
        // No compiled method in scope, so no liveness question can be asked —
        // and per the note in the word test, that keeps every report.
        None,
        // Nor a per-body `Prim`-colour claim.
        None,
    )
}

/// The object screen for the band test: is the word an actual object HEADER, or
/// merely a number that lands in the heap's address range?
///
/// `is_relocatable` answers the second question and the band test used to stop
/// there, so any stack word whose bit pattern fell inside the arena counted as
/// an unpublished oop. Every sibling instrument in this file screens with
/// `is_object_address` first; audit §16-§18 measured what skipping it costs
/// (hundreds to thousands of flagged words, `verifier_oop=0` on all of them).
///
/// Safe to read the header only because §18 bounded `MOVABLE_BOUNDS` to the
/// COMMITTED prefix — under the previous reservation-wide envelope this
/// dereference could touch a page that was never mapped.
///
/// `CRATONVM_MOVING_YOUNG_NO_BAND_OBJECT_SCREEN=1` restores the range-only
/// test. It is the fail-OPEN direction (more words flagged, more cycles
/// refusing to move), so it is the safe lever to reach for if a missed root is
/// ever suspected here.
#[inline]
pub(in crate::jit::conservative_roots) fn band_word_is_an_object(w: usize) -> bool {
    if band_object_screen_disabled() {
        return true;
    }
    if w == 0 || w & 0x7 != 0 {
        return false;
    }
    // The committed-prefix argument above stopped being enough once the young
    // generation started handing an evacuated semi-space's middle and free
    // blocks back to the OS (`uncommit_evacuated_young`, default ON): a stale
    // frame word can point into that half, inside the published span but on an
    // unmapped page. Nothing on a decommitted page is an object, so the answer
    // there is "no" without reading it (gen r4w4, 2026-09-24: SIGSEGV at
    // `[w + 8]` in `GenR4CardStorePathsProbe` under gc-stress).
    //
    // Furthermore, when called from young-gen pinning (`young_pin_frame_words`),
    // `YoungPinRange` is inclusive of the region end and low slack, so `w` can
    // sit at or near the region boundary (`w + HEADER_SIZE > end`). A word
    // whose header span crosses past the arena boundary or into an unmapped
    // guard page cannot be an object. `span_is_movable_and_committed` validates
    // both containment of the entire header span in published regions and page
    // commitment before any dereference.
    if !cratonvm_gc::gen_heap::span_is_movable_and_committed(w, cratonvm_types::HEADER_SIZE) {
        return false;
    }
    // gen r5w1/young5 (2026-09-26), item 2 of
    // `docs/feature-designs/generative-gc-architecture.md`:
    // the young screen above answers `true` for every word outside the
    // generational young tables, and `addr_is_movable` also admits
    // `MOVABLE_BOUNDS`, which ZGC fills with its whole arena envelope — parts
    // of which can be decommitted. Ask the movable envelope's own commit
    // screen too. `None` (no backend published one, today's state) keeps the
    // read below exactly as before; `Some(false)` is a word on an unmapped
    // granule, which is not an object.
    if cratonvm_gc::gen_heap::movable_range_committed(w, cratonvm_types::HEADER_SIZE) == Some(false)
    {
        return false;
    }
    // SAFETY: `w` is inside the published movable range, its header span does
    // not cross past the arena end, and its span is committed (checked just
    // above). Alignment is checked above too.
    unsafe { cratonvm_types::plausible_object_header_at(w as *const u8) }
}

/// `CRATONVM_MOVING_YOUNG_NO_BAND_LIVENESS_SCREEN=1` — stop consulting the
/// class file's type maps about a band word, so every word the object screen
/// admits is reported.
///
/// The FAIL-OPEN direction, like the object screen's own switch: more words
/// flagged means more cycles refusing to move, so this is the lever to reach
/// for if a missed root is ever suspected here. It is also what makes the
/// screen's worth measurable in ONE binary, which is the whole reason these
/// switches exist in this file.
///
/// The screen answers only while the verifier oracle is armed
/// (`CRATONVM_DBG_VERIFY_OOP_MAPS` present): unarmed, no type-map store is
/// bound ([`OracleTypeMapsScope::bind`]), every verdict is `Unknown`, and the
/// screen discards nothing, so in production this switch changes nothing.
/// That is deliberate, not an oversight (i8-L4, decided in wave 9): g1-audit
/// §22 measured the screen ARMED on `CoverageBench` and found no effect (median
/// incomplete rate 98.49 % off vs 98.46 % on), §27 then closed the residual it
/// was aimed at (0.00 % incomplete), and arming it in production would permit
/// moves on a verifier `NotOop` for java-local slots that no one has shown
/// every compiled tier leaves free of a live reference (an IR value's frame
/// home is not known to respect the bytecode local's scope).
pub(in crate::jit::conservative_roots) fn band_liveness_screen_disabled() -> bool {
    static OFF: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&OFF, || {
        false
    })
}

/// Kill switch for §25.4's thread-pointer fallback, so its effect is one
/// binary's A/B rather than two builds.
///
/// Off restores the pre-fix behaviour: the shadow window is resolved from the
/// innermost frame's cached thread pointer alone, and a no-publish leaf there
/// costs the whole chain its coverage proof.
pub(in crate::jit::conservative_roots) fn band_thread_window_disabled() -> bool {
    static OFF: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&OFF, || {
        false
    })
}

pub(in crate::jit::conservative_roots) fn band_object_screen_disabled() -> bool {
    static OFF: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&OFF, || {
        false
    })
}

/// [`band_has_unpublished_word_with`] plus the active map's live slots. See
/// [`band_slot_is_verifiable_with_map`].
///
/// gen r5w1/young5: this doc and the `allow` below used to sit on
/// [`band_word_is_an_object`], stranded two items early.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn band_has_unpublished_word_with_map(
    rbp: usize,
    frame_size: usize,
    layout: &cratonvm_jit::FrameLayout,
    live_hi: Option<i32>,
    published: &(impl PublishedWords + ?Sized),
    map_slots: Option<&[i32]>,
    is_relocatable: impl Fn(usize) -> bool,
    // Whether a word `is_relocatable` accepts may be DEREFERENCED.
    //
    // Only the caller knows. In production `is_relocatable` is
    // `gen_heap::addr_is_movable`, whose range §18 bounds by the COMMITTED
    // prefix, so the header words are mapped. But this function takes the
    // predicate as a parameter, and callers that pass an arbitrary closure —
    // every unit test here does — accept words that are not addresses at all.
    // Reading a header from one of those is a segfault, which is exactly what
    // the §19 object screen did until this flag existed: it inherited a safety
    // argument that held for one of its two callers.
    readable: bool,
    // The liveness oracle for a JAVA-LOCAL slot, when the caller has the
    // compiled method and safepoint id needed to consult it. `None` at a call
    // site means "cannot ask", which is not the same as "asked and got no".
    liveness: Option<&dyn Fn(i32) -> VerifierSlotVerdict>,
    // JIT round 25 (item 3): the body's per-body `Prim`-colour claim
    // ([`ir_prim_colour_claim`]); a word it names is never reported. `None`
    // claims nothing.
    prim_claim: Option<&[i16]>,
) -> bool {
    if frame_size == 0 || frame_size > rbp {
        return false;
    }
    let lo = rbp - frame_size;
    let mut addr = (lo + 7) & !7usize;
    // Same guard as `scan_one_frame`: a stale bound must never walk into
    // unmapped pages. A compiled frame is orders of magnitude smaller than
    // this, so the clamp is unreachable in practice.
    const MAX_SCAN_BYTES: usize = 1024 * 1024;
    let hi = rbp.min(addr.saturating_add(MAX_SCAN_BYTES));
    while addr + 8 <= hi {
        // Cast: a compiled frame is far smaller than i32::MAX bytes.
        let off = (rbp - addr) as i32;
        if !band_slot_is_verifiable_with_map(off, layout, live_hi, map_slots)
            || ir_prim_colour_claimed(prim_claim, i64::from(off))
        {
            addr += 8;
            continue;
        }
        // SAFETY: aligned read inside the calling thread's own live compiled
        // frame, bounded by the frame size recorded at compile time.
        let w = unsafe { (addr as *const usize).read() };
        if is_relocatable(w) && !published.has(w) && (!readable || band_word_is_an_object(w)) {
            // The LIVENESS screen. §19 established the survivors of the object
            // screen are header-shaped, so shape cannot separate a real missed
            // root from a dead slot still pointing at a live object. The class
            // file's own type maps can, for the java-locals band.
            //
            // ONE DIRECTION ONLY, and it is the whole safety argument here:
            // reporting an unpublished oop makes the cycle refuse to move, so
            // DISCARDING a report is the direction that permits movement. A
            // word is therefore discarded only on a positive `NotOop` — the
            // verifier saying this local definitely holds no reference at this
            // bci. `Unknown` (an inlined frame, a slot outside the locals band,
            // no type maps for the method) keeps the report, because "could not
            // ask" must never read as "answered no". That is the same
            // asymmetry §16 relies on, pointed the other way, because here the
            // consequence of being wrong is a missed root rather than a missed
            // refutation.
            let dead_by_verifier = !band_liveness_screen_disabled()
                && liveness
                    .map(|ask| ask(off) == VerifierSlotVerdict::NotOop)
                    .unwrap_or(false);
            if !dead_by_verifier {
                return true;
            }
        }
        addr += 8;
    }
    false
}

/// Refresh the current thread's moving-young coverage status for the active
/// JIT frames it owns. Returns `true` when every live frame reachable from this
/// thread has a complete active-safepoint proof; on `false`, the GC-side
/// per-cycle flag is marked so the collector diverts to the non-moving sweep.
pub fn refresh_moving_young_coverage_for_current_thread() -> bool {
    // gch h4/bp: a JNI in-native deposit that leaves its compiled band to the
    // pause proves nothing here: "unproven" sends the deposit to the chain
    // scan, which publishes nothing either (`scan_active_jit_frames`). No
    // ledger row is written: the pause that scans the band marks its own
    // cycle (`gc_and_alloc::append_in_native_band_roots`).
    //
    // gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in): unless
    // the deposit asked for the proof ([`with_band_at_pause_proof`]); then it
    // runs exactly as in a band-off deposit (a failure marks the rows as
    // there), and the deposit records the depth it proved.
    if band_at_pause_deposit_armed() && !band_at_pause_proof_armed() {
        return false;
    }
    if !moving_young_enabled() {
        return true;
    }
    // gce e1b/j: the JNI phase census's split of this proof (an in-native
    // deposit with `CRATONVM_DBG_JNI_PHASE=1` only; `None` otherwise).
    let mut census_clock = coverage_census_clock();

    // Process-wide JIT relocation boundary (added 2026-07-29, `86e69e848`).
    //
    // When armed, the mere EXISTENCE of compiled code — not a specific unproven
    // frame — forces every young collection onto the non-moving sweep. It was
    // added after a Hibernate corruption chase in which the verifier below
    // observed unregistered entries, unavailable exact frame bases, and live
    // oops outside the published map, on the argument that discovering a gap
    // mid-scan is too late to repair that cycle.
    //
    // That argument does not describe how this code is wired. This function is
    // the collection's PRE-cycle proof: `roots.rs::collect_roots` calls it (via
    // `refresh_moving_young_coverage_for_collection`) *before* the collector
    // picks a young path, and `gen_heap::collect_garbage_inner` diverts on the
    // published verdict. A gap found here has always been found in time. What
    // the blanket actually bought was insurance against the verifier itself
    // missing an obligation — at the price of making moving-young unreachable
    // in every process that ever compiles a method, i.e. every real workload:
    // measured on `BinTreesClassic 18` at `-Xmx512m`, 66 of 66 young cycles
    // fell back, all of them attributed to this branch and none to a real
    // obligation.
    //
    // So it is a policy knob rather than a hard-coded law, and the per-cycle
    // proof is the default authority. `CRATONVM_MOVING_YOUNG_NO_JIT=1` restores
    // the blanket for a bisect or an emergency — the same fail-closed
    // direction, no longer the only available setting.
    if moving_young_no_jit() && cratonvm_jit::jit_code_range_count() != 0 {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc::gc_quiescence::incomplete_reason::JIT_RELOCATION_UNSUPPORTED,
        );
        return false;
    }

    // gcd d2/f (JIT round 13): a JIT shadow push that bailed on the buffer's
    // `end` published none of its oops, so the bailed frame's coverage claim
    // is not backed while its call is in flight; see
    // `shadow_bail_refuses_moving` (per thread since gcd d3/o). Every moving
    // decision reaches this function: the initiator's through
    // `refresh_moving_young_coverage_for_collection`, each parked peer's
    // through `publish_peer_jit_coverage_for_stw` (a refused peer deposits no
    // depth; a depth-0 peer is asked there too), and both root-snapshot
    // deposits directly.
    if shadow_bail_refuses_moving() {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc::gc_quiescence::incomplete_reason::SHADOW_OVERFLOW_BAIL,
        );
        return false;
    }

    let dbg = moving_young_coverage_dbg();
    // GC719-3 (round 19 wave 8, lane gc8): opt-in, read once per proof.
    let mirror_check = innermost_mirror_check_enabled();
    // GC819-2 (round 19 wave 9, lane gc9): the anchored form, read once too.
    let anchor_tight = mirror_check && innermost_mirror_anchor_enabled();
    let scanner_sp = current_stack_pointer();
    if !dbg_no_prune() {
        let _ = prune_returned_jit_entries(scanner_sp);
    }

    let mut complete = true;
    coverage_census_lap(crate::native::jni::JniPhase::CovPrologue, &mut census_clock);

    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            if let Some(top) = chain.last_mut() {
                if let Some(info) = top.precise.as_mut() {
                    info.exact_rbp = top_rbp_get();
                info.exact_cm_id = published_compile_id();
                }
            }
        }

        let chain = c.borrow();
        if dbg {
            eprintln!(
                "[moving-young-coverage] chain_len={} top_rbp=0x{:x} scanner_sp=0x{:x}",
                chain.len(),
                top_rbp_get(),
                scanner_sp,
            );
        }
        for (entry_idx, entry) in chain.iter().enumerate() {
            let Some(info) = entry.precise else {
                if dbg {
                    eprintln!("[moving-young-coverage] incomplete: JIT entry has no precise map");
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::NO_PRECISE_MAP,
                );
                complete = false;
                continue;
            };
            // SAFETY: the compiled method pointer is the same Arc-stable pointer
            // used by the existing precise scan/remap paths.
            let cm: &cratonvm_jit::CompiledMethod =
                unsafe { &*(info.compiled_method as *const cratonvm_jit::CompiledMethod) };
            let exact_rbp = info.exact_rbp;
            if exact_rbp == 0 {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: missing exact rbp entry_idx={} scanner_sp=0x{:x} entry_sp=0x{:x} maps={} sp_id_off={}",
                        entry_idx,
                        scanner_sp,
                        entry.entry_sp,
                        cm.oop_maps.len(),
                        cm.sp_id_slot_off,
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::MISSING_EXACT_RBP,
                );
                complete = false;
                continue;
            }
            // gch h1/jw (review): an RBP that is not an aligned word of this
            // entry's band locates no frame. `innermost_frame_method` answers
            // the entry's own method for it, and the coverage check below then
            // read the safepoint-id slot at `exact_rbp - sp_id_slot_off` --
            // outside the band -- and could answer "complete" from whatever
            // stood there. The band verifier refuses the same RBP
            // (`UNBOUNDED_FRAME_BAND`), but only while it runs
            // (`CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` turns it off); refuse it
            // here too, before any read, with the verifier's reason.
            if exact_rbp & 0x7 != 0 || exact_rbp < scanner_sp || exact_rbp >= entry.entry_sp {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: exact rbp=0x{exact_rbp:x} outside the entry band [0x{scanner_sp:x}, 0x{:x})",
                        entry.entry_sp
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::UNBOUNDED_FRAME_BAND,
                );
                complete = false;
                continue;
            }
            let innermost =
                innermost_frame_method(exact_rbp, info.exact_cm_id, entry.entry_sp, scanner_sp, info.compiled_method);
            // The frame standing at the recorded RBP may belong to a method the
            // entry does not name (a JIT->JIT call published its own base
            // there). When the call was the direct form, the callee resolves
            // exactly and its own map is the one to check; when it was indirect
            // there is nothing to check it against, and relocating would strand
            // that frame's oops — take the non-moving sweep for this cycle.
            // SAFETY: a resolved method is kept alive by the live frame that
            // resolved it, the same contract as the chain entry's own pointer.
            let innermost_cm: Option<&cratonvm_jit::CompiledMethod> =
                innermost.map(|p| unsafe { &*p });
            if innermost_cm.is_none() {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: innermost rbp=0x{exact_rbp:x} belongs to a JIT callee reached indirectly"
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::FOREIGN_INNERMOST_RBP,
                );
                complete = false;
            } else if let Some(refusal) = innermost_cm
                .filter(|_| mirror_check)
                .and_then(|m| innermost_mirror_refusal(exact_rbp, scanner_sp, m, anchor_tight))
            {
                // GC719-3: the mirror names no frame that called out of its
                // body -- possibly a DEAD frame whose RBP now lies inside live
                // Rust frames -- or (GC819-2, opt-in) the return address it
                // found is not the one the frame's active safepoint names.
                // Fail closed: no frame word is rewritten through it this
                // cycle (the non-moving sweep). Counted under the
                // foreign-innermost reason, named here.
                eprintln!(
                    "{refusal} (entry {entry_idx}, entry_sp=0x{:x}, scanner_sp=0x{scanner_sp:x}): coverage refused",
                    entry.entry_sp,
                );
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::FOREIGN_INNERMOST_RBP,
                );
                complete = false;
            } else if !moving_young_frame_coverage_complete(
                exact_rbp,
                innermost_cm.unwrap_or(cm),
            ) {
                if dbg {
                    eprintln!(
                        "[moving-young-coverage] incomplete: active frame map at rbp=0x{:x}",
                        exact_rbp
                    );
                }
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::ACTIVE_FRAME_MAP,
                );
                complete = false;
            }

            let mut child_rbp = exact_rbp;
            // gch h1/jw (r17-gc-compiled-chain-walks-stop-at-4096-activations-
            // silently): this loop used to end after 4096 steps with nothing
            // marked, so the parents past the guard were never checked and the
            // verdict could still read "complete". The guard is now the entry
            // band's, and a walk it cuts is an incomplete proof.
            let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry.entry_sp);
            loop {
                if !walk.step() {
                    if dbg {
                        eprintln!(
                            "[moving-young-coverage] incomplete: parent walk cut after {} steps at rbp=0x{:x}",
                            walk.limit(),
                            child_rbp
                        );
                    }
                    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                        cratonvm_gc::gc_quiescence::incomplete_reason::UNBOUNDED_FRAME_BAND,
                    );
                    complete = false;
                    break;
                }
                if child_rbp == 0 || child_rbp & 0x7 != 0 {
                    break;
                }
                if child_rbp < scanner_sp || child_rbp >= entry.entry_sp {
                    break;
                }
                // SAFETY: `child_rbp` is an aligned address inside this live
                // thread's JIT stack band, same invariant as
                // `remap_active_jit_frames`.
                let parent_rbp = unsafe { (child_rbp as *const usize).read() };
                let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
                // A link read out of the stack is NOT yet a frame base. Both
                // helpers below dereference `[parent_rbp - sp_id_slot_off]`
                // (and `remap_one_jit_frame` WRITES every slot its oop map
                // names), so the link must be validated BEFORE it is used, not
                // after: a zero/garbage word faulted at `0 - sp_id_slot_off`
                // while walking a json-smart chain. Same order and same
                // predicate as the other rbp-chain walks in this file
                // (`scan_active_oop_map_at_rbp`'s walk and the two conservative
                // band walks).
                if parent_rbp <= child_rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp < scanner_sp
                    || parent_rbp >= entry.entry_sp
                {
                    break;
                }
                let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
                    break;
                };
                let cm: &cratonvm_jit::CompiledMethod =
                    unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                if !moving_young_frame_coverage_complete_at(parent_rbp, cm, Some(ret_addr)) {
                    if dbg {
                        eprintln!(
                            "[moving-young-coverage] incomplete: parent frame map at rbp=0x{:x}",
                            parent_rbp
                        );
                    }
                    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                        cratonvm_gc::gc_quiescence::incomplete_reason::PARENT_FRAME_MAP,
                    );
                    complete = false;
                }
                child_rbp = parent_rbp;
            }
        }
    });

    coverage_census_lap(
        crate::native::jni::JniPhase::CovChainWalk,
        &mut census_clock,
    );
    // A5 — unregistered JIT frame on the native stack.
    //
    // arch-2026-07-26 (`moving-young-precise-roots`): this probe used to be
    // SKIPPED entirely under moving-young, on the argument that "in precise
    // moving-young mode every actual compiled transition is registered by
    // JitEntryGuard and checked above". That argument is an assertion, not a
    // proof, and it was load-bearing for heap safety in exactly the direction
    // where being wrong is fatal:
    //
    //   * A JIT frame that is live WITHOUT a `JitEntryGuard` is not in the
    //     chain, so the loop above never examines it. It publishes no shadow
    //     homes and no per-safepoint oop map, so nothing can rewrite its
    //     register/spill slots.
    //   * With the probe skipped, `complete` stayed `true`, `roots.rs` took the
    //     precise-only branch (suppressing the conservative backstop that would
    //     at least have MARKED the frame's oops), and `gen_heap` ran the moving
    //     cycle — relocating objects out from under raw slots nobody rewrites.
    //     That is the canonical `main`-compiled bintrees corruption, re-armed.
    //   * The condition is known to be reachable: the whole A5 fix exists
    //     because it was observed (`Vm::invoke` → compiled app `main`, live
    //     while a clinit / interpreted callee triggers a GC).
    //
    // The probe therefore runs under moving-young as well, and a hit is treated
    // as an incomplete coverage proof (divert to the non-moving sweep) AND as
    // the A5 conservative-root condition, exactly as on the legacy path.
    //
    // The original skip was motivated by UTILITY, not soundness: the probe is a
    // raw-word scan, not a frame walk, so cached function pointers and JIT
    // helper arguments that happen to point inside generated code read as
    // return PCs and fabricate a frame. Over-detection costs moving cycles; it
    // never costs correctness. Reducing that false-positive rate (so
    // moving-young engages more often) is a real follow-up and is specified in
    // the design doc — it belongs in `native_stack_has_jit_frame` /
    // return-address validation, or in registering the entry-point transition,
    // NOT in suppressing the check.
    //
    // SB-LOADER-ZIPCONTENT (2026-08-04): that follow-up is now done, in the
    // place the paragraph above names — `is_plausible_return_pc` requires the
    // bytes before a candidate word to decode as the tail of a `call`. The
    // check itself is unchanged and still runs under moving-young.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if cratonvm_jit::jit_code_range_count() > 0 {
        let cover_hi = JIT_ENTRY_CHAIN
            .with(|c| c.borrow().iter().map(|e| e.entry_sp).max())
            .unwrap_or(scanner_sp);
        let search_lo = scanner_sp.max(cover_hi);
        let high = current_thread_stack_high();
        // OBSERVATION ONLY — ask the memo what it WOULD have said, without
        // acting on it. Routing this call site through the memo measured inert
        // (reverted); this says whether that is because the memo never engages
        // or because engaging saves nothing. A flat profile cannot tell those
        // apart, and four attempts were judged on a flat profile.
        if a5_engagement_enabled() {
            A5_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
            A5_PROBE_WORDS.fetch_add(high.saturating_sub(search_lo) / 8, Ordering::Relaxed);
            let code_ranges = cratonvm_jit::jit_code_range_count();
            let hiwater_on = unreg_memo_hiwater_enabled();
            let would = UNREG_JIT_MEMO.with(|c| {
                let mut m = c.get();
                let d = m.observe(search_lo, code_ranges, hiwater_on);
                // Do NOT store: this is an observer, and `observe` mutates the
                // hiwater mark. Put the memo back exactly as it was.
                let _ = m;
                d
            });
            match would {
                UnregScan::AlreadyClean => A5_PROBE_MEMO_CLEAN.fetch_add(1, Ordering::Relaxed),
                UnregScan::Detect { hi: Some(_) } => {
                    A5_PROBE_MEMO_BANDED.fetch_add(1, Ordering::Relaxed)
                }
                UnregScan::Detect { hi: None } => {
                    A5_PROBE_FULL_RESCAN.fetch_add(1, Ordering::Relaxed)
                }
            };
        }
        // gen r5w1/young5: the probe below reads `[search_lo, high)` of THIS
        // thread's stack, which is only this thread's stack if the scanner's
        // SP is on it. A Finalizer thread under
        // `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=1` faulted here, page-aligned
        // ~16 MiB above RSP: an SP (or a stack top) that did not describe the
        // stack the thread was running on. `own_stack_top` now caps such a
        // read at the fixed 8 MiB, and a capped probe that finds nothing is
        // not a proof, so an unverifiable band is an incomplete proof (the
        // scan is skipped), and the values are reported so the stale one can
        // be found.
        if !sp_on_own_stack(scanner_sp) {
            let n = OWN_STACK_FOREIGN_SP.fetch_add(1, Ordering::Relaxed);
            if n < 4 {
                let (low, top) = current_thread_stack_limits();
                let entries: Vec<usize> =
                    JIT_ENTRY_CHAIN.with(|c| c.borrow().iter().map(|e| e.entry_sp).collect());
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    "moving-young coverage: the scanner SP 0x{scanner_sp:x} is not on this \
                     thread's stack [0x{low:x}, 0x{top:x}); the A5 band [0x{search_lo:x}, \
                     0x{high:x}) is not readable and the cycle is taken as unproven \
                     (chain entry SPs: {entries:x?}, residue_hi=0x{:x})",
                    jit_residue_hi(),
                );
            }
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME,
            );
            complete = false;
        }
        // gce e1/j: inside a JNI in-native deposit, a clean verdict over this
        // band recorded at the same chain state still holds (see "The frozen
        // band of a JNI in-native deposit"); the verdict of a fresh, whole
        // (untruncated) probe is recorded below, after the classification, for
        // the next one. Outside one both calls are no-ops and the probe runs
        // exactly as before.
        let mut whole_band_probed = false;
        let hit = if high > search_lo && sp_on_own_stack(scanner_sp) {
            // A classified (residue-only) verdict serves only while this
            // site's residue filter is on, i.e. while a fresh probe would
            // discard that hit too (gce e1b/j).
            if frozen_band_clean(search_lo, high, !a5_residue_filter_enabled()) {
                // The audit `scan_active_jit_frames` runs over its memo, asked
                // of the band a live frame can be in (above the residue mark,
                // as the classification below reads a hit).
                if dbg_unreg_memo_audit() {
                    UNREG_MEMO_SHORTCIRCUITS.fetch_add(1, Ordering::Relaxed);
                    let live_lo = jit_residue_hi().max(search_lo);
                    if high > live_lo && native_stack_has_jit_frame(live_lo, high).is_some() {
                        UNREG_MEMO_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None
            } else {
                whole_band_probed = !own_stack_band_truncated(search_lo, high);
                native_stack_has_jit_frame(search_lo, high)
            }
        } else {
            None
        };
        let raw_clean = hit.is_none();
        // Price the frame-shape filter without branching on it: for every cycle
        // this probe diverts, say how many band words looked like JIT return
        // addresses and how many of those sat at a slot with real frame shape.
        // `shaped=0` on a hit means the filter would have converted THIS cycle.
        if hit.is_some() && a5_census_enabled() {
            let (total, shaped) = native_stack_jit_frame_census(search_lo, high);
            eprintln!(
                "[a5-census] hits={total} shaped={shaped} band=[0x{search_lo:x},0x{high:x}) \
                 band_bytes={}",
                high.saturating_sub(search_lo),
            );
        }
        // CLASSIFY THE ONE HIT, then decide. The residue test is a
        // thread-local read of a mark the probe already maintains, so it runs
        // on every hit; the shape test reaches the global code-range table and
        // runs only when something asks. Counting happens whether or not either
        // screen is armed, which is what lets a run say how much of its OWN
        // fallback population each screen would convert — the number the two
        // screens have never had.
        let hit = match hit {
            None => None,
            Some((slot, word)) => {
                let n = A5_FALLBACK_HITS.fetch_add(1, Ordering::Relaxed) + 1;
                let residue_hi = jit_residue_hi();
                let is_residue = residue_hi != 0 && slot < residue_hi;
                if is_residue {
                    A5_FALLBACK_RESIDUE.fetch_add(1, Ordering::Relaxed);
                }
                let want_shape = a5_shape_filter_enabled() || a5_fallback_dbg();
                let shapeless = want_shape && !a5_slot_has_frame_shape(slot, high);
                if shapeless {
                    A5_FALLBACK_SHAPELESS.fetch_add(1, Ordering::Relaxed);
                }
                let filtered = (is_residue && a5_residue_filter_enabled())
                    || (shapeless && a5_shape_filter_enabled());
                if a5_fallback_dbg() && (n <= 16 || n.is_power_of_two()) {
                    eprintln!(
                        "[a5-fallback] #{n} slot=0x{slot:x} word=0x{word:x} \
                         residue_hi=0x{residue_hi:x} is_residue={is_residue} \
                         shapeless={shapeless} filtered={filtered} \
                         band=[0x{search_lo:x},0x{high:x}) chain_len={}",
                        JIT_ENTRY_CHAIN.with(|c| c.borrow().len()),
                    );
                }
                if filtered {
                    A5_FALLBACK_FILTERED.fetch_add(1, Ordering::Relaxed);
                    None
                } else {
                    Some((slot, word))
                }
            }
        };
        // gce e1/j: the verdict, not the raw probe: a residue hit the filter
        // above discards is discarded the same way next time (the residue mark
        // moves only with a pop, which voids the memo).
        // gce e1b/j: a classified-clean verdict is recorded only when NO word
        // above the residue mark is a JIT return address (the filter above
        // reads the lowest hit only, and a residue hit can hide a live frame
        // above it), so it means exactly what `scan_active_jit_frames`' A5
        // block concludes when it declines without a rescue.
        if whole_band_probed && hit.is_none() {
            if raw_clean {
                note_frozen_band_clean(search_lo, high, true);
            } else {
                let live_lo = jit_residue_hi().max(search_lo);
                if live_lo >= high || native_stack_has_jit_frame(live_lo, high).is_none() {
                    note_frozen_band_clean(search_lo, high, false);
                }
            }
        }
        if let Some((slot, word)) = hit {
            if dbg {
                // Name the actual evidence: which slot, which word, how far
                // into which compiled body, and the bytes the return-address
                // filter accepted. "Some word somewhere looked like compiled
                // code" is not something the next reader can act on, and this
                // probe's whole cost is that it can be wrong.
                let (body, off, tail) = match cratonvm_jit::pin_jit_code_range_owner(word) {
                    Some(cm) => {
                        let entry = cm.entry_ptr() as usize;
                        let back = word.saturating_sub(entry).min(8);
                        let mut bytes = String::new();
                        for i in 0..back {
                            // SAFETY: inside the buffer `cm` holds alive.
                            let b = unsafe { ((word - back + i) as *const u8).read() };
                            bytes.push_str(&format!("{b:02x} "));
                        }
                        (entry, word - entry, bytes)
                    }
                    None => (0, 0, "<body reclaimed>".to_string()),
                };
                eprintln!(
                    "[moving-young-coverage] incomplete: unregistered JIT frame on native stack \
                     (stack slot 0x{slot:x} holds 0x{word:x} = body 0x{body:x}+0x{off:x}, \
                     preceding bytes [{tail}], search band [0x{search_lo:x}, 0x{high:x}), \
                     slot is 0x{depth:x} above search_lo)",
                    depth = slot.saturating_sub(search_lo),
                );
            }
            cratonvm_gc::gc_quiescence::set_unregistered_jit_frame_on_stack();
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME,
            );
            complete = false;
        }
    }

    coverage_census_lap(crate::native::jni::JniPhase::CovA5, &mut census_clock);
    // COVERAGE VERIFICATION, not coverage assertion (arch-2026-07-26
    // `moving-young-corruption-rootcause`). Everything above trusts
    // `OopMapEntry::moving_young_coverage_complete`, a codegen bit computed
    // from the abstract interpreter's local/operand model. That model does not
    // describe scalar-replacement field slots, LICM hoist slots, or the
    // full-GPR safepoint spill area — all of which can hold live oops that the
    // shadow push never publishes and that `roots.rs` no longer scans
    // conservatively once the proof "passes". Check the frames' actual bytes
    // instead of taking the bit's word for it.
    //
    // Deliberately placed HERE rather than at the `roots.rs` call site: this
    // function is also what `vm_exec::deposit_root_snapshot` and
    // `interpreter::update_root_snapshot` call before suppressing their own
    // conservative scans, so a parked or safepointed thread gets the same
    // verification without those (differently-owned) files changing.
    {
        let mut reason = cratonvm_gc::gc_quiescence::incomplete_reason::NONE;
        if moving_young_unpublished_frame_oop_present(&mut reason) {
            if dbg {
                eprintln!(
                    "[moving-young-coverage] incomplete: {}",
                    cratonvm_gc::gc_quiescence::incomplete_reason::label(reason),
                );
            }
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(reason);
            complete = false;
        }
    }

    coverage_census_lap(
        crate::native::jni::JniPhase::CovBandVerify,
        &mut census_clock,
    );
    if !complete {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete();
    }
    complete
}

