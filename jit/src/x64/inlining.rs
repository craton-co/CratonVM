// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Callee inlining at the emitter level.
//!
//! `try_emit_inline` decides whether a call site chosen callee can be emitted
//! in line, and `try_emit_inline_body` walks that callee bytecode against this
//! frame. It is a second, smaller bytecode walk with a different set of rules
//! — the callee locals live in the caller spill area, its metadata is read
//! from the `InlineSite` rather than from this method tables, and anything it
//! cannot model is a refusal rather than a bail.
//!
//! Refusing here is always safe: the call site falls back to a real call.

use super::*;

/// Does this deopt point, published from inside a splice of `callee_key`,
/// describe a stack that never existed?
///
/// The postcondition in [`Compiler::try_emit_inline`] refuses a splice that
/// publishes any point this answers `true` for. THREE independent things have to
/// hold for a point published inside a splice to be well formed:
///
///  * **it says it came from inside one** — a caller chain, which
///    `push_inline_scope` produces; and
///  * **its innermost frame names a spliced CALLEE and not the enclosing
///    method** — which is the half the check was missing. Without this clause
///    a point naming `Caller.method:()V` at a bci belonging to `Callee.method`
///    passes. The VM's identity gate accepts that pair (the key names the
///    method being resumed) and resumes the caller's bytecode at a foreign bci
///    — a wrong-code bug, not a missed optimisation; and
///  * **its bci lies inside that callee's own code** (round 10 wave 7). The
///    identity clause alone admits the converse pair: an ENCLOSING-method pc
///    stamped with the callee's name. That is not hypothetical — every
///    publisher the splice walk actually contains
///    (`emit_post_invoke_exception_check` and
///    `emit_precise_null_check_field_store` — TWO, not the four an earlier
///    draft listed; see
///    `docs/internal/fixed-bugs/r10-bybci-splice-publisher-count-is-overstated-FIXED-20260922.md`)
///    keys on `Compiler::dbg_last_pc`, which the
///    OUTER walk owns and which therefore still names the caller's invoke for the
///    whole splice, while `current_bytecode_owner` stamps the callee. Those sites
///    are all gated on `precise_exception_frames`, which is refused inlining
///    three times over, so none of them can fire here today — this clause is the
///    backstop for the day one does.
///
///    It is an upper bound only, and says so: an enclosing pc that happens to be
///    smaller than the callee's code length still passes. A range test cannot
///    distinguish two bytecode spaces that overlap; only the producer naming the
///    owner can, which is what `current_bytecode_owner` does for the walk and
///    what those `dbg_last_pc` sites would have to do for themselves.
///
/// Kept as a free function so the property can be tested directly. The state it
/// guards is not reachable from a compile today, and a guard nobody can make
/// fire is a guard nobody has tested.
///
/// # What is now done, and what is still owed, before this can start admitting
///
/// **Done (N2).** The VM half. Every rebuild sink used to refuse a non-empty
/// caller chain outright, so the moment this check admitted a well-formed
/// point, the artifact carrying it would deopt into
/// `InternalError: precise deoptimization unavailable … refusing
/// side-effecting replay`. `vm/src/runtime/interpreter/deopt_resume.rs` now
/// offers `build_deopt_frame_chain`, and the two sinks that matter — the
/// frame-pushing `resume_real_ir_deopt` and the dispatch-helper
/// `try_resume_trapped_callee` — materialise and run a chain instead of
/// refusing it. A sink that still cannot is COUNTED as `inlined-caller-chain`
/// rather than assumed absent.
///
/// **Done (M1/N5), so this check can now admit.** `build_frame_state_at` no
/// longer stamps `self.method_key` unconditionally: `Compiler::inline_callee_scopes`
/// is pushed beside `inline_scope_stack` in `push_inline_scope`, and
/// `current_bytecode_owner` / `resume_bci_for` derive the identity AND the bci
/// space from it together — so a point published inside a splice names the
/// callee at a bci in the callee's own space, which is what this asks for.
///
/// **Done (round 10 wave 7): the FRAME CONTENTS.** M1 moved the identity and the
/// bci and left the locals behind, so a callee-identified point carried
/// `Compiler::num_locals` entries read out of the ROOT method's local homes and
/// described by the ROOT method's liveness/oop/kind analyses
/// (`r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`).
/// The claim below — "the FIRST edit to publish one produces well-formed
/// metadata" — was false on that axis, and on the monitor and operand-stack axes
/// with it. `build_frame_state_at` now builds a splice-published frame to the
/// callee's own geometry (`InlineCalleeScope`), with every slot
/// `FrameValue::Unsupported`: well formed, honest, and fail-closed, because one
/// `Unsupported` slot makes the frame unresumable and every VM sink then refuses
/// rather than reconstructing the callee from the caller's locals.
///
/// **Still true, and worth keeping in view:** nothing in the emitter publishes
/// a point from inside a splice yet (`emit_inline_invoke_into_rax` deliberately
/// omits `snapshot_pre_intrinsic_call`, and the `precise_exception_frames`
/// publishers the walk does reach cannot run in a compile that inlines), so the
/// population this admits is still empty. What changed is that the FIRST edit to
/// publish one produces metadata that is well formed on every axis this checks
/// and unresumable where it cannot be truthful, instead of plausible-looking and
/// wrong; and the refusal that remains is about a real defect rather than about
/// an unimplemented stamp.
///
/// **Do not relax this check instead.** It is the only thing keeping
/// caller-named/callee-bci'd metadata out of every artifact, and the VM's
/// identity gate `deopt_frame_matches_method` ACCEPTS that pair — the key does
/// name the method being resumed. The failure mode is resuming the caller's
/// bytecode at a bci belonging to the callee, silently.
fn splice_point_is_misidentified(
    point: &crate::deopt::DeoptimizationPoint,
    callee_key: &str,
    enclosing_owner_key: &str,
    callee_code_len: usize,
) -> bool {
    if point.frame_state.caller.is_none() {
        return true;
    }
    if point.frame_state.method_key.is_empty() {
        return true;
    }
    // This splice's OWN point. Its bci is published in the callee's bytecode
    // space (`resume_bci_for` hands a callee pc through verbatim), so it must
    // address the callee's code. `callee_code_len` is `InlineSite::callee_code_len`
    // — the same figure `x64/driver.rs` registers as this key's `code_len` with
    // the metadata verifier, so a point this admits cannot be reported
    // `BciOutOfRange` at install either, and one this refuses costs a splice
    // rather than the whole artifact.
    if point.frame_state.method_key == callee_key {
        return usize::try_from(point.frame_state.bci).unwrap_or(usize::MAX) >= callee_code_len;
    }
    // Anything else must not be the enclosing bytecode owner. A point published
    // by a NESTED splice inside this body legitimately names the INNER callee,
    // and was already held to this same rule by that splice's own postcondition
    // (against ITS code length), so refusing it here would make nesting and
    // publishing mutually exclusive for no safety gain.
    //
    // What must never appear is `enclosing_owner_key`, which is what
    // `build_frame_state_at` stamps one level out. That is the caller-named /
    // callee-bci'd pair this check exists to exclude. It is read from
    // `current_bytecode_owner` before the splice opens rather than assumed to be
    // `Compiler::method_key`, because for a nested splice the enclosing owner is
    // a CALLEE and the root's key would not catch it.
    point.frame_state.method_key == enclosing_owner_key
}

/// Record what a branch leaves at its target, or check it against what an
/// earlier branch to the same target already left.
///
/// Returns `false` on a disagreement, which refuses the splice. Verified
/// bytecode cannot produce one — the JVM's own verifier requires every path to
/// a merge to agree on stack depth and types — so this is a ratchet against the
/// walk having mis-modelled something, not a case that is expected to fire.
/// How many times the live-slot clamp below MOVED the cursor — i.e. how
/// many splices would have handed a caller-owned frame slot out twice. Zero
/// means the guard never engaged on this run, which is what a report of it has
/// to say next to any result that credits it.
static INLINE_LIVE_SLOT_CLAMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_inline_live_slot_clamp() {
    INLINE_LIVE_SLOT_CLAMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Engagement count for the inline live-slot clamp. Printed by
/// `jit-method-stats`.
pub fn inline_live_slot_clamps() -> u64 {
    INLINE_LIVE_SLOT_CLAMPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT=-inline-live-slot-clamp` — measurement-only escape hatch
/// that restores the pre-fix cursor rewind, so the fix can be A/B'd in ONE
/// binary. Turning it off reinstates a wrong-address store; it is not a
/// supported configuration.
fn inline_live_slot_clamp_disabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP")
}

/// Whether the single-pass splice walk lowers the `float`/`double` arithmetic,
/// conversion, negation and compare bytecodes (r9 wave 7, lane `review7a`).
///
/// **Default ON.** `CRATONVM_JIT_NO_INLINE_FP_ARITH=1` restores the old walk,
/// which bailed on the first such opcode and rolled the whole splice back to a
/// real call: `inline-rollback OsrWideProbe$Sq.area(J)D at pc=95: callee_pc=5
/// op=0x8a` -- a one-line `(double)(k & 7) * 1.5` getter, planned, walked and
/// thrown away at its `l2d`. Every FP-returning leaf the planner admitted met
/// the same end.
///
/// The arms reuse the OUTER walk's own lowering (`emit_float_binop`,
/// `emit_double_binop`, `emit_int_to_fp_xmm0`, `pop_fp_operand_to_xmm0`,
/// `emit_fp_to_int_nan_fixup`, `emit_fcmp`) verbatim, on the same operand-stack
/// model the splice already shares with it: `push_call_result` has always put
/// a nested `D`/`F` call result on the callee stack as `Xmm(0)`, so every other
/// arm here already reads an XMM-resident operand correctly, and the merge
/// spill (`spill_callee_stack_to_merge_slots`) and the return arm move one
/// through `load_slot_to_reg` like any other slot. Read once per splice.
fn inline_fp_arith_enabled() -> bool {
    !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_FP_ARITH")
}

/// How many reservations the open-inline-locals floor MOVED — i.e. how many
/// would have handed out a word an enclosing spliced callee's locals still own.
///
/// A SECOND counter rather than a second use of [`INLINE_LIVE_SLOT_CLAMPS`],
/// because the two guard different things: that one protects the caller's
/// OPERAND stack from a rewound cursor, this one protects a spliced callee's
/// LOCALS from every path that lowers the cursor. A single number could not say
/// which a result should be credited to — and this defect exists in the first
/// place because a single number (308 overlap reports) could not say which half
/// of it was the hazard.
static INLINE_LOCALS_FLOOR_BUMPS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(super) fn note_inline_locals_floor() {
    INLINE_LOCALS_FLOOR_BUMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Engagement count for the open-inline-locals floor. Printed by
/// `jit-method-stats` beside [`inline_live_slot_clamps`]. Zero means the guard
/// never engaged on this run, which is what any report crediting it has to say.
pub fn inline_locals_floor_bumps() -> u64 {
    INLINE_LOCALS_FLOOR_BUMPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1` — measurement-only escape hatch
/// restoring the pre-fix reservation start, so the fix can be A/B'd in ONE
/// binary.
///
/// Separate from `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP` on purpose: that
/// switch removes the operand-stack clamp, whose absence is a known miscompile
/// (bc-java `LEATest`), so an A/B through it would price two changes at once
/// and one of them is not this one. Turning THIS one off reinstates a store
/// onto a live enclosing local; it is not a supported configuration.
pub(super) fn inline_locals_floor_disabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR")
}

/// DIAGNOSTIC (`CRATONVM_DBG_JIT_LOCALS_FLOOR=1`): one line per reservation the
/// open-inline-locals floor considered, while a splice is nested.
///
/// It prints BOTH outcomes on purpose. `BUMPED` is the guard doing its job.
/// `KEPT` is a reservation the ONE-SIDED rule would have moved and the range
/// rule left alone -- the population that turned out to be a miscompile, so a
/// run can COUNT it instead of inferring it from a bug report.
/// `inline_locals_floor_bumps()` counts only the first kind.
pub(super) fn dbg_note_locals_floor(
    why: super::SpillReason,
    cursor: i32,
    slots: usize,
    start: i32,
    outcome: &str,
    compiler: &super::Compiler,
) {
    if !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_LOCALS_FLOOR") {
        return;
    }
    eprintln!(
        "[jit-locals-floor] {outcome} {why:?} cursor={cursor} slots={slots} start={start} one_sided_floor={} scopes={} in {}",
        compiler.open_inline_locals_floor(),
        compiler.inline_oop_scopes.len(),
        compiler.method_label,
    );
}

// ---------------------------------------------------------------------------
// The spliced direct call's oop map, keyed at the RETURN ADDRESS
// ---------------------------------------------------------------------------
//
// `emit_inline_direct_call` emits, in this order: the `CALL`, then
// `emit_post_call_rbp_republish`, then `emit_oop_map_for_safepoint`. That last
// one records `native_pc_offset = buf.pos()` at the moment it runs, so the map
// is stamped at `return_address + sizeof(republish)` -- NOT at the return
// address, which is the key every walker actually holds for a frame below the
// innermost one (it reads it out of `[rbp+8]`).
//
// THE SIZE OF THE GAP, and why it is not a constant a consumer could absorb.
// `emit_post_call_rbp_republish` has two shapes:
//
//   * the inline TLS mirror (`inline_rbp_tls_disp != 0`, which is the shape
//     taken whenever `precise_maps` is on): 9 bytes for
//     `MOV [tls:disp32], RBP`, plus 11 more for the
//     `MOV DWORD [tls:disp32], compile_id` half that must move with it, so 9
//     or 20;
//   * the `helpers.frame_record` fallback: `PUSH RAX` + `SUB RSP, imm8` +
//     `MOV ARG_REGS[0], RBP` + `CALL rel32` + `ADD RSP, imm8` + `POP RAX`,
//     which is 1 + 4 + 3 + 5 + 4 + 1 = 18 (25 if the helper is out of rel32
//     reach and `emit_call_imm64_via_rax` is used instead).
//
// It is never ZERO in any configuration where the discrepancy is observable:
// the republish returns early only when `!precise_maps`, and the one consumer
// that keys on this offset refuses outright when `sp_id_slot_off == 0`, which
// `x64.rs` sets from the same `precise_maps`. So the exact lookup does not
// merely usually miss -- it misses every time it is attempted.
//
// WHAT THIS IS, AND WHAT IT IS NOT. It is NOT a GC-correctness defect. That
// was settled from the source before anything here was written, because the
// answer decides how much risk the repair is worth:
//
//   * the root scan and the relocation walker key on the SAFEPOINT-ID slot,
//     i.e. `OopMapEntry::bytecode_pc`. `conservative_roots`' frame scan, its
//     `remap_one_jit_frame`, and `moving_young_frame_live_hi` all select with
//     `.filter(|m| m.bytecode_pc == sp_id)`, and the innermost frame's
//     evidence is `find_oop_map_for_safepoint_id`. Not one of them reads
//     `native_pc_offset`;
//   * `CompiledMethod::find_oop_map_for_pc` -- the exact binary search whose
//     own doc says the GC walker calls it with the frame's return PC -- has
//     ZERO non-test callers anywhere in the tree;
//   * the only non-test exact match on `native_pc_offset` is
//     `conservative_roots::compiled_frame_bci`'s first evidence source
//     (`cm.oop_maps.iter().find(|m| m.native_pc_offset == off)`), which
//     recovers a LINE NUMBER for a compiled stack frame;
//   * the remaining readers are compile-time: `bytecode_walk`'s duplicated-
//     region shift (it rewrites the field by a delta, it does not look one
//     up) and `ir_lower`'s assertion that IR-tier entries carry `0` there.
//
// So the cost today is a silently degraded line number, not a lost root. It
// is also not specific to splices: all five `emit_post_call_rbp_republish`
// sites in `bytecode_walk.rs` have the same call/republish/map order, so the
// exact path misses for every direct compiled-to-compiled call in this
// backend. Fixing it here fixes the spliced arm; the top-level arms live in
// another agent's file and are recorded in `.agent-requests/B5-wiring.txt`.
//
// WHY THE KEY IS RE-STAMPED RATHER THAN THE EMISSION REORDERED. Moving
// `emit_oop_map_for_safepoint` above the republish would put the map at the
// return address by construction and need no fixup afterwards. It is the
// WRONG repair, and the reason is that that function does not only record
// metadata -- it EMITS code: the shadow-stack reload (`emit_shadow_reload`,
// which writes each pushed oop's possibly-relocated value back into its home
// register or frame slot) and, after the map, the Stage-4 reload of oop
// locals from their canonical slots. The republish's `helpers.frame_record`
// shape contains a `CALL`, and `LOCAL_REGS` on SysV is
// `[R12, R13, R14, R15, RBX, RSI, RDI]` -- RSI and RDI are CALLER-saved
// there, and are `ARG_REGS[0]` and `ARG_REGS[1]` besides. Reordering would
// therefore restore a live oop into a register the next few instructions
// destroy, on the platform this VM is measured on. The frame the map
// DESCRIBES is valid at both points (nothing in the republish moves RBP, and
// the offsets are all `[rbp - off]`), so the description was never the
// problem -- only the number it is filed under is. Re-stamping leaves the
// emitted bytes BYTE-IDENTICAL, which is the smallest change that can be
// right, and is the one thing that cannot introduce a codegen bug.
//
// FAIL CLOSED. Every shape `restamp_call_oop_map_at_return` does not
// recognise leaves the entry exactly as `emit_oop_map_for_safepoint` wrote it
// and is counted as `refused`. A wrong key is worse than the miss it
// replaces: `find_oop_map_for_pc` binary-searches this table and
// `compiled_frame_bci` takes the FIRST `find` match, so a duplicate or
// out-of-order key answers with some other safepoint's bci -- a confidently
// wrong line, which `compiled_frame_bci`'s own doc calls the single worst
// outcome available to it.

/// `CRATONVM_JIT=-inline-call-map-at-return` -- measurement-only escape hatch
/// that restores the pre-fix key (return address + the republish's bytes), so
/// the change is A/B-able in ONE binary. A regression in anything that reads
/// `native_pc_offset` then bisects to this in one RUN rather than one BUILD,
/// which on a 32-core host shared with several other sessions is the whole
/// difference between a ten-minute answer and an hour-long one.
///
/// Cached, like `safepoint::oopmap_presence_only` and A18's map gate: the
/// question is asked once per spliced direct call emitted, which is a compile-
/// time path, but the answer cannot change within a process and re-reading the
/// environment per splice would be the only cost this fix has.
fn inline_call_map_at_return_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_CALL_MAP_AT_RETURN")
    })
}

/// Census of the re-stamp, index-parallel with
/// `INLINE_CALL_MAP_AT_RETURN_COUNTS`.
///
/// Five numbers rather than one, because "the fix is compiled in" and "the fix
/// did anything" are different readings and a single total cannot separate
/// them. A silent fallback is exactly the shape this project has been bitten
/// by before -- an instrument armed where it cannot fire -- and the defect
/// being repaired here went unseen for precisely that reason, so the counter
/// is written to make a ZERO readable rather than merely absent:
///
/// * `stamped-at-return` -- the key was moved back onto the return address.
///   This is the win. A zero here on a run that spliced direct calls means the
///   republish emitted nothing, i.e. `precise_maps` was off, in which case
///   `compiled_frame_bci` refuses the artifact anyway and there was nothing to
///   repair.
/// * `already-at-return` -- the map was already keyed correctly, so there was
///   no gap to close. Kept separate from the above so "the republish is inert
///   in this configuration" is distinguishable from "the fix engaged".
/// * `no-map` -- the safepoint pushed no entry at all: an empty map is skipped
///   off the precise path, and a failed compiler returns early. Not an error,
///   and the largest bucket on the default path.
/// * `refused` -- the ratchet. A shape that cannot be proven safe to re-key:
///   more than one entry pushed by one safepoint, a key that would collide
///   with or precede its neighbour, an offset that does not fit `u32`, or a
///   stamped offset EARLIER than the return address (which would mean the
///   buffer moved backwards between the call and the map). MUST be zero. A
///   non-zero reading is a real finding about the emitter, not about this
///   code, and the entry is left untouched when it happens.
/// * `reverted` -- `CRATONVM_JIT_NO_INLINE_CALL_MAP_AT_RETURN` was set.
pub const INLINE_CALL_MAP_AT_RETURN_NAMES: [&str; 5] = [
    "stamped-at-return",
    "already-at-return",
    "no-map",
    "refused",
    "reverted",
];

static INLINE_CALL_MAP_AT_RETURN_COUNTS: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Index into [`INLINE_CALL_MAP_AT_RETURN_NAMES`]: the key was corrected.
const INLINE_CALL_MAP_STAMPED: usize = 0;
/// Index: the key already named the return address.
const INLINE_CALL_MAP_ALREADY: usize = 1;
/// Index: the safepoint published no map to re-key.
const INLINE_CALL_MAP_NONE: usize = 2;
/// Index: the ratchet refused. Must stay zero.
const INLINE_CALL_MAP_REFUSED: usize = 3;
/// Index: the kill switch is set, so the pre-fix key stands.
const INLINE_CALL_MAP_REVERTED: usize = 4;

/// Read the census. Printed by `jit-method-stats` once the one-line re-export
/// in `jit/src/x64.rs` and the one-line format argument in `jit/src/tiered.rs`
/// land -- the exact same two-file wiring `inline_live_slot_clamps` above
/// already has. Both files were owned by another agent on 2026-09-01, so the
/// edits are written out in `.agent-requests/B5-wiring.txt` instead of made
/// here. Until then the census is readable in one run with
/// `CRATONVM_DBG_OOPCOV=1`, which prints a line per event below; that name is
/// reused deliberately rather than minted, so this adds exactly ONE new flag
/// to the surface the `types/` declaration guard checks.
pub fn inline_call_map_at_return_counts() -> [u64; 5] {
    let mut out = [0u64; 5];
    for (i, slot) in INLINE_CALL_MAP_AT_RETURN_COUNTS.iter().enumerate() {
        out[i] = slot.load(std::sync::atomic::Ordering::Relaxed);
    }
    out
}

/// Re-key the oop map `emit_oop_map_for_safepoint` has just pushed onto the
/// RETURN ADDRESS of the call it belongs to. See the block comment above for
/// why this is a re-stamp and not a reordering, and for the evidence that the
/// defect it repairs is a line-number one rather than a GC one.
///
/// `maps_before` is `oop_maps.len()` sampled immediately BEFORE the emission,
/// and `return_pc` the buffer position immediately after the `CALL` -- the
/// same value `record_inline_frame_row` is handed, so the oop map and A18's
/// inline-frame row are filed under ONE key rather than two. That agreement is
/// the point: the consumer half looks both of them up with the address it read
/// out of `[rbp+8]`, and a map keyed 20 bytes later would make the line number
/// and the inlined-frame chain disagree about which call the frame is in.
fn restamp_call_oop_map_at_return(
    oop_maps: &mut [crate::OopMapEntry],
    maps_before: usize,
    return_pc: usize,
) {
    let outcome = restamp_outcome(oop_maps, maps_before, return_pc);
    if let Some(slot) = INLINE_CALL_MAP_AT_RETURN_COUNTS.get(outcome) {
        slot.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // Under the existing oop-map coverage debug key, so the census is readable
    // today without the `jit-method-stats` wiring. One line per spliced direct
    // call is the same order of volume as the per-method `[oopcov]` lines
    // `driver.rs` already prints under this key.
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OOPCOV") {
        eprintln!(
            "[oopcov] inline-direct-call-map {} return_off={} census={:?}",
            INLINE_CALL_MAP_AT_RETURN_NAMES
                .get(outcome)
                .copied()
                .unwrap_or("?"),
            return_pc,
            inline_call_map_at_return_counts(),
        );
    }
}

/// The decision half of [`restamp_call_oop_map_at_return`], split out so every
/// path -- including the refusals -- flows through one counter bump and one
/// optional print rather than repeating both at seven `return` sites.
///
/// Returns the [`INLINE_CALL_MAP_AT_RETURN_NAMES`] index of what it did.
fn restamp_outcome(
    oop_maps: &mut [crate::OopMapEntry],
    maps_before: usize,
    return_pc: usize,
) -> usize {
    if inline_call_map_at_return_disabled() {
        return INLINE_CALL_MAP_REVERTED;
    }
    // `emit_oop_map_for_safepoint` pushes at most one entry, and pushes none
    // when the map came out empty off the precise path or when the compiler
    // was already failed. Zero is ordinary; anything other than zero or one is
    // a shape this function was not written against.
    if oop_maps.len() == maps_before {
        return INLINE_CALL_MAP_NONE;
    }
    if oop_maps.len() != maps_before + 1 {
        return INLINE_CALL_MAP_REFUSED;
    }
    let Ok(return_off) = u32::try_from(return_pc) else {
        // A code buffer past 4 GiB. Unreachable in this backend, and a
        // truncating cast here would name a byte in some other method.
        return INLINE_CALL_MAP_REFUSED;
    };
    let stamped = oop_maps[maps_before].native_pc_offset;
    if stamped == return_off {
        return INLINE_CALL_MAP_ALREADY;
    }
    if stamped < return_off {
        // The map is emitted strictly after the call, so its key can only be
        // LATER than the return address. An earlier one means the buffer moved
        // backwards between the two -- a rewind this does not model, and one
        // that would make the "still sorted" argument below unsound.
        return INLINE_CALL_MAP_REFUSED;
    }
    // The table must stay sorted AND single-keyed. `find_oop_map_for_pc`
    // binary-searches it (and `debug_assert`s the sortedness), and
    // `compiled_frame_bci` takes the first linear `find` match, so a duplicate
    // key resolves to whichever entry happens to come first. Lowering this
    // entry's key can only bring it closer to its predecessor, so the
    // predecessor is the only one that can be violated -- everything pushed
    // after this point is at a strictly later buffer position. In practice the
    // predecessor is at least the `CALL` instruction's own length below
    // `return_off`; the check is a ratchet against that ceasing to be true.
    if maps_before > 0 && oop_maps[maps_before - 1].native_pc_offset >= return_off {
        return INLINE_CALL_MAP_REFUSED;
    }
    oop_maps[maps_before].native_pc_offset = return_off;
    INLINE_CALL_MAP_STAMPED
}

// ---------------------------------------------------------------------------
// PC -> inline-chain map (the PRODUCER half)
// ---------------------------------------------------------------------------
//
// An inlined callee contributes NO stack-trace frame today, because
// `conservative_roots::active_compiled_frames_with_bci` is flat: one entry per
// compiled artifact. HotSpot's equivalent is a `ScopeDesc` CHAIN -- an inlined
// callee is a nested scope at the same PC -- which is what makes the inlined
// frames reappear in a warmed-up trace. See
// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`,
// defect (2).
//
// WHAT WAS RULED OUT, in the order it was checked, because each one looked
// like it should already be the answer:
//
//   * **`CompiledMethod::inlined_methods`.** It survives to runtime and names
//     every spliced callee -- but it is a flat SET keyed by nothing. It exists
//     for class-change invalidation, cannot say which callee a given PC is
//     inside, and cannot say at which bci. Naming a frame from it would be
//     guessing.
//   * **The deopt caller chain.** `DeoptimizationPoint::frame_state.caller` IS
//     a real chain, IS retained on the artifact, and IS PC-indexed (each
//     point records the `native_offset` it was taken at). Two facts kill it. First, every level carries
//     `method_key: self.method_key` -- the COMPILING method -- because
//     `build_frame_state_at` has no other identity to stamp; a nested level
//     would therefore name the outer method with an inner method's bci, which
//     is the malformed pair this whole exercise exists not to produce.
//     Second, and decisively, the invoke arm inside a splice deliberately
//     publishes no point at all (`emit_inline_invoke_into_rax`: "Deliberately
//     NO `snapshot_pre_intrinsic_call` here"), so the ONE native offset a
//     stack walk actually keys on -- the return address of the call the deeper
//     frame is suspended in -- has no deopt point under it. Making it publish
//     one would record a resume bci the enclosing method does not have, which
//     is exactly the `IndexOutOfBoundsException`-into-`InternalError`
//     regression of 2026-08-28.
//   * **`OopMapEntry`.** It is the right key and it does survive, but it has
//     no spare field, and `bytecode_pc` inside a splice holds the ENCLOSING
//     method's invoke bci (`cur_bc_pc` is not moved by the inline walk) --
//     that is the answer `compiled_frame_bci` already returns, and the thing
//     this map has to EXTEND rather than replace.
//
// So the chain has to be emitted. This is the emitter half: it records, per
// call emitted from inside a spliced body, the chain of (callee label, bci)
// pairs a stack walk arriving at that call must expand into frames.
//
// KEYS. Deliberately the same two `conservative_roots::compiled_frame_bci`
// already uses, and no third one:
//
//   1. `native_offset` -- the buffer position immediately after the `CALL`,
//      i.e. the return address a parent frame's RBP-chain walk reads, which is
//      the key `OopMapEntry::native_pc_offset` is recorded under.
//   2. `safepoint_bci` -- `cur_bc_pc` at the call, which is the value the
//      emitter stores into the frame's safepoint-id slot and the value
//      `compiled_frame_bci` recovers for the INNERMOST frame. It is the bci of
//      the enclosing compiled method, so a chain found under it appends
//      directly to the frame `compiled_frame_bci` already describes.
//
// FAIL CLOSED, in three places, because a fabricated frame is worse than an
// absent one -- the reader cannot tell a wrong method name from a right one,
// where a missing frame at least reads as missing:
//
//   * a level whose bci is not a spec-legal bytecode index (JVMS 4.9.1) or
//     whose label is empty refuses the whole ROW, not just that level;
//   * a rewound emission is dropped: rows are appended in emission order, so a
//     later row whose `native_offset` is not strictly greater than the last
//     kept one proves the buffer was rewound between them, and every kept row
//     at or above that offset is discarded (that is the backstop; the three
//     splice rollback paths also truncate explicitly);
//   * a `safepoint_bci` two rows disagree about is POISONED rather than
//     resolved to either answer. One bci covers a whole spliced region, so a
//     splice containing two calls with different chains genuinely cannot be
//     told apart from the safepoint-id slot alone -- the innermost frame's
//     only evidence. `chain_for_safepoint_bci` then answers `None` and the
//     frame is reported exactly as it is today.

/// Exclusive upper bound on a bytecode index: `Code.code_length` must be less
/// than 65536 (JVMS 4.9.1). Mirrors `conservative_roots::plausible_bci`'s
/// bound on the consumer side, derived from the spec rather than from a list
/// of the synthetic pcs this backend stamps (`ENTRY_POLL_BC_PC`,
/// `SP_ID_UNSET_BC_PC`), both of which are far above it and are rejected by
/// the same one test.
///
/// `pub(crate)` since 2026-09-11: `ir_lower::record_npe_trap_site` is the
/// optimizing tier's twin of [`record_npe_trap_site`] and screens its bci
/// against the same bound. A second copy of the constant there would be a
/// second place to fix when the bound is re-derived.
pub(crate) const INLINE_FRAME_MAX_BCI: usize = 65_536;

// ---------------------------------------------------------------------------
// The guarded-virtual MISS EDGE, and why it has to poison its own bci
// ---------------------------------------------------------------------------
//
// A guarded virtual/interface site (PGO-02) emits, at ONE `cur_bc_pc`: the
// receiver load and null test, a `CMP`/`JNE` guard per admitted variant, each
// variant's SPLICED BODY, and -- reached when every guard misses -- the
// ordinary dispatch call, emitted by `bytecode_walk`'s unchanged
// normal-dispatch tail. Confirmed against the source 2026-09-01: the
// `0xb6 | 0xb7 | 0xb9` arm's guard chain calls `try_emit_inline_site(pc, site)`
// once per variant and then falls THROUGH to that tail, and `self.cur_bc_pc`
// is assigned once per outer bytecode at the top of the walk and is not moved
// by the inline walk. Every one of those program points therefore carries the
// same safepoint id.
//
// THE MISS EDGE RECORDS NO ROW OF ITS OWN. `record_inline_frame_row` is
// called from exactly two places -- `emit_inline_direct_call` and
// `emit_inline_dispatch_call` -- and both are calls emitted from INSIDE a
// spliced body. The top-level dispatch tail does not call it, and could not
// usefully: `build_inline_frame_chain` answers `None` on an empty scope stack,
// and at the miss edge the stack IS empty, because `try_emit_inline_site`
// popped its scope before returning.
//
// That leaves the bci holding exactly ONE chain -- the splice's -- so
// `from_rows` never sees a disagreement to poison it with. An innermost
// compiled frame (the only kind that keys on the safepoint id, because it owns
// no return address on this stack) suspended on the miss edge is then handed
// the chain of a splice THAT DID NOT RUN. A trace naming a method the program
// was never inside is worse than a missing frame: a missing frame reads as
// missing, a fabricated one reads as true. It is the exact outcome
// `conservative_roots::compiled_frame_inline_chain`'s own doc says this area
// refuses, and the exact-key rule stated there protects only frames that HAVE
// the exact key -- which the innermost frame never does.
//
// THE REMEDY IS EVIDENCE, NOT A SECOND MECHANISM. A row is pushed for the
// guarded splice carrying that same `safepoint_bci` and an EMPTY chain. The
// disagreement rule already in `from_rows` then does the work it was written
// for: two rows under one bci that do not agree, so the slot goes to `None`
// and `chain_for_safepoint_bci` refuses. Nothing new decides anything, and
// key 1 is untouched -- the empty-chain row lands at its own exact
// `native_offset`, where "no inlined frames here" is the correct answer for
// the guard bytes it names.
//
// WHY THE EMITTER AND NOT THE WALK. The walk holds one safepoint id and
// nothing else; it cannot tell the miss edge from the splice beside it. Only
// the emitter knows a miss edge exists under that bci. The blunt walk-side
// alternative -- refuse key 2 whenever the artifact holds any guarded site --
// would take the inlined callees back out of every innermost frame in the
// common case, which is the regression this workstream exists to prevent.
//
// WHY THIS CANNOT DISTURB THE 2026-09-01 WITNESS. `probes/StackTraceAfterOsr.java`
// reports `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]`, and `probe` IS
// the innermost compiled frame, so `leaf`/`mid`/`outer` do come from key 2 --
// "poison more" is precisely the direction that could break it. It cannot, for
// two independent reasons, either one sufficient on its own:
//
//   * the witness's chain is `probe -> outer -> mid -> leaf` and every one of
//     those calls is `invokestatic` (0xb8). The guard chain lives in the
//     `0xb6 | 0xb7 | 0xb9` arm and is entered only for `op != 0xb7`; 0xb8 is a
//     different arm entirely, which splices through `try_emit_inline(pc)` with
//     no guard, no variants and no miss edge -- the splice REPLACES the call.
//     A statically bound splice is not gated in below and pushes no row;
//   * `inline_guard_variants` -- the map this poison is gated on, and the same
//     map `bytecode_walk`'s guard chain is driven from -- is populated only
//     for `plan.is_speculative()`, i.e. only at virtual/interface sites, and
//     only when `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is set, which is
//     default-OFF and documented "unsoaked". On the default path that map is
//     empty for every pc in every method, so the gate is false everywhere and
//     not one extra row is recorded in the entire run.
//
// The change can only make a reported chain SHORTER, never longer or
// different, and only at a pc that carries a receiver guard.

/// `CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON=1` -- measurement-only escape
/// hatch that stops the guarded-splice poison row being recorded, so a frame
/// that vanished from a trace is attributable in one RUN rather than one
/// BUILD.
///
/// It gets its own name rather than riding `CRATONVM_JIT_NO_INLINE_FRAME_MAP`:
/// that switch turns the WHOLE producer off, so it cannot separate "the poison
/// took this frame" from "the map never had it". Setting this one reinstates
/// the fabricated frame and is not a supported configuration -- it exists so
/// those two hypotheses are one variable apart.
///
/// Cached, like `inline_call_map_at_return_disabled` above: the question is
/// asked once per guarded splice emitted, which is a compile-time path, and
/// the answer cannot change within a process.
fn inline_miss_edge_poison_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON")
    })
}

/// Cached `CRATONVM_DBG_JITC`, for the one site below that is reached on a
/// THROW rather than at compile time. Re-reading the environment during a
/// stack walk would be the only runtime cost this change has. The key is
/// REUSED, not minted -- this file already prints its splice decisions under
/// it -- so the flag surface grows by exactly one name.
fn inline_frame_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC"))
}

/// Census of the miss-edge poison, index-parallel with
/// [`INLINE_MISS_EDGE_POISON_COUNTS`].
///
/// Five numbers rather than one, for the reason
/// [`INLINE_CALL_MAP_AT_RETURN_NAMES`] gives at length: a poison that never
/// fires and a poison that fires constantly are indistinguishable without a
/// number, and this project has repeatedly been bitten by an instrument armed
/// where it cannot fire. On the default path
/// (`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` unset) every entry here is EXPECTED
/// to read zero, and that zero is the positive evidence the witness is
/// untouched rather than an absence of evidence.
///
/// * `rows-emitted` -- guarded splices that pushed a poison row. Compile-time
///   engagement: the fix is compiled in AND a guarded splice happened.
/// * `bcis-poisoned` -- safepoint bcis a finished map left at `None`, from ANY
///   cause. The denominator: it counts the pre-existing
///   two-calls-under-one-splice disagreement too, so "the poison fired" and
///   "the bci was already ambiguous" stay separable.
/// * `bcis-poisoned-by-miss-edge` -- of those, the ones a poison row
///   contributed to. This is the fix's own engagement, and it is deliberately
///   NOT expected to equal `rows-emitted`: a guarded splice whose body emitted
///   no call of its own leaves that bci holding a single empty-chain row,
///   which is `Some([])` and not poisoned -- and `Some([])` is the same "no
///   inlined frames here" answer the bci already gave when it held no row at
///   all, so nothing is lost in that case.
/// * `lookups-refused` -- innermost-frame key-2 lookups that found a poisoned
///   slot. Runtime engagement: each one is a frame the trace deliberately does
///   not show, and would have shown WRONGLY before.
/// * `reverted` -- `CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON` was set, so no
///   row was pushed. Counted at the site the row would have been pushed at, so
///   a reverted run still reports how often the fix WOULD have engaged.
#[allow(dead_code)]
pub const INLINE_MISS_EDGE_POISON_NAMES: [&str; 5] = [
    "rows-emitted",
    "bcis-poisoned",
    "bcis-poisoned-by-miss-edge",
    "lookups-refused",
    "reverted",
];

static INLINE_MISS_EDGE_POISON_COUNTS: [std::sync::atomic::AtomicU64; 5] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Index into [`INLINE_MISS_EDGE_POISON_NAMES`]: a poison row was pushed.
const INLINE_MISS_EDGE_ROWS: usize = 0;
/// Index: a finished map left a safepoint bci at `None`, from any cause.
const INLINE_MISS_EDGE_BCIS_POISONED: usize = 1;
/// Index: ...and a poison row contributed to that bci.
const INLINE_MISS_EDGE_BCIS_BY_MISS: usize = 2;
/// Index: a key-2 lookup found a poisoned slot and refused.
const INLINE_MISS_EDGE_LOOKUPS_REFUSED: usize = 3;
/// Index: the kill switch is set, so the pre-fix (unpoisoned) map stands.
const INLINE_MISS_EDGE_REVERTED: usize = 4;

/// One bump, taken by every path so the counter cannot be forgotten at one of
/// them. `by == 0` returns early: `from_rows` calls this once per finished map
/// with a per-artifact total, and the overwhelming majority of artifacts have
/// nothing to add.
fn note_inline_miss_edge(slot: usize, by: u64) {
    if by == 0 {
        return;
    }
    if let Some(c) = INLINE_MISS_EDGE_POISON_COUNTS.get(slot) {
        c.fetch_add(by, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Read the census. Wiring it into `jit-method-stats` is the same two-line
/// re-export/format pair `inline_live_slot_clamps` already has, and both of
/// those files belong to other agents on 2026-09-01, so the edits are written
/// out in `.agent-requests/D2-flags.txt` rather than made here. Until they
/// land the census is readable in ONE run under `CRATONVM_DBG_JITC`, which
/// prints a line per compiled artifact that recorded rows and a line per
/// refused lookup.
#[allow(dead_code)]
pub fn inline_miss_edge_poison_counts() -> [u64; 5] {
    let mut out = [0u64; 5];
    for (i, slot) in INLINE_MISS_EDGE_POISON_COUNTS.iter().enumerate() {
        out[i] = slot.load(std::sync::atomic::Ordering::Relaxed);
    }
    out
}

/// Where an inline null check would raise, as a stack trace needs it.
///
/// An implicit NPE in compiled code is not thrown where it happens: the inline
/// `TEST`/`JZ` reaches a stub that flags the NPE, loads the deopt sentinel and
/// runs the EPILOGUE, and the `java/lang/NullPointerException` is constructed
/// from the interpreter afterwards. `vm/src/jit/helpers.rs` snapshots the live
/// compiled frames inside that stub so the trace keeps them -- but the trapping
/// frame published no safepoint id there (an inline null check is not a
/// GC-capable call), so `activation_bci` correctly refused the stale id in the
/// slot and the recovered frame printed `-1`.
///
/// This is the side channel that is NOT the GC's. The emitter holds the
/// trapping bci at every one of these sites already; it records it here, gives
/// the site an id, and emits a ten-byte COLD trampoline that passes the id to
/// the helper alongside the JEP-358 action code. The fast path -- `TEST`, `JZ`
/// -- is byte-for-byte unchanged, and a method with no inline null check
/// records nothing and emits nothing.
///
/// `chain` is the same `ScopeDesc`-shaped list `InlineFrameMap` rows carry, so
/// a trap INSIDE a spliced body reports the callee frames too, and `bci` is the
/// ENCLOSING compiled method's own index, never a pc from a callee's code.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct NpeTrapSite {
    /// Bytecode index IN THE ENCLOSING COMPILED METHOD.
    pub bci: u32,
    /// Spliced callees at this program point, INNERMOST FIRST. Empty for a
    /// trap that is not inside a splice.
    pub chain: Vec<InlineFrameLevel>,
}

/// The finished trap table for one compiled artifact, keyed by the id baked
/// into each site's trampoline.
///
/// Empty -- and holding no allocation -- for every method with no inline null
/// check, and for the whole feature switched off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct NpeTrapMap {
    /// Ascending by id.
    sites: Vec<(u32, NpeTrapSite)>,
}

#[allow(dead_code)]
impl NpeTrapMap {
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// The site a trampoline's key names, or `None`.
    ///
    /// `None` on a key this artifact never issued is the whole safety argument:
    /// ids are monotonic within a compile and are never reused, so a key that
    /// survived a rewind, or one read against the wrong artifact, MISSES. It
    /// cannot land on a different site and hand a frame a line from somewhere
    /// else -- the failure mode this area refuses everywhere.
    pub fn get(&self, key: u32) -> Option<&NpeTrapSite> {
        self.sites
            .binary_search_by_key(&key, |(id, _)| *id)
            .ok()
            .map(|i| &self.sites[i].1)
    }

    /// Build a map from rows a backend collected itself.
    ///
    /// The single-pass backend records through the thread-local session
    /// ([`record_npe_trap_site`] / [`finish_npe_trap_recording`]) because its
    /// emitter is reached from a dozen places that would otherwise all have to
    /// be handed a table. The OPTIMIZING backend lowers one graph in one
    /// function and already carries its sibling table (`inline_frame_rows`) as
    /// a plain field, so it collects these the same way and hands them over
    /// here. Two producers, one consumer, and the ids are per-ARTIFACT either
    /// way — `get` binary-searches within one map and never across two.
    ///
    /// Sorted here rather than trusted: a lowerer that pushed out of order
    /// would otherwise turn every lookup into a silent miss or, worse, a hit on
    /// a neighbouring site.
    pub fn from_rows(mut sites: Vec<(u32, NpeTrapSite)>) -> Self {
        sites.sort_unstable_by_key(|(id, _)| *id);
        NpeTrapMap { sites }
    }
}

/// Whether compiles record a trapping bci for their inline null checks.
///
/// Default ON. `CRATONVM_JIT_NO_NPE_TRAP_LINES=1` records nothing and emits no
/// trampoline, so every inline null-check site branches straight to the shared
/// per-action stub exactly as it did before 2026-09-02 and a frame recovered
/// from the NPE snapshot goes back to reporting `-1`. Both halves -- the ten
/// cold bytes per site and the line in the trace -- revert together, which is
/// what makes the cost and the behaviour one A/B inside one binary.
///
/// It DEPENDS on `CRATONVM_JIT_NO_INLINE_FRAME_MAP`, and deliberately: the
/// enclosing-method bci of a trap inside a spliced body is the outermost
/// splice's `entry_bci`, which only `INLINE_FRAME_SCOPES` knows. With that
/// session closed there is no way to tell a callee's pc from the compiling
/// method's, so this records nothing rather than a bci out of another method's
/// code.
#[allow(dead_code)]
pub fn npe_trap_lines_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_NPE_TRAP_LINES"))
}

/// Record the trap site of one inline null check and return its key, or `0`
/// for "not described" -- which every caller passes straight through to the
/// emitter, where it selects the historical shared-stub shape.
///
/// `bc_pc` is the trapping bytecode index in whatever code the emitter is
/// walking: the compiling method's own for a top-level opcode, the CALLEE's
/// inside a splice. The two are separated here rather than at the call sites,
/// which do not know which they are in.
///
/// Ids are monotonic and never reused, so nothing truncates this table on a
/// rollback. A rewound splice leaves its rows behind and they are unreachable:
/// the machine code that carried their keys was overwritten, and no later site
/// can be issued the same id. The alternative -- an index into a `Vec` that
/// rollback truncates -- makes a key mean a DIFFERENT site after the rewind,
/// which is a wrong line rather than a missing one.
#[allow(dead_code)]
pub(super) fn record_npe_trap_site(bc_pc: usize) -> u32 {
    if !npe_trap_lines_enabled() || !inline_frame_recording() {
        return 0;
    }
    let described = INLINE_FRAME_SCOPES.with(|s| {
        let scopes = s.borrow();
        if scopes.is_empty() {
            // Not inside a splice: the walk's pc IS this method's bci, and
            // there are no callee frames to name.
            return (bc_pc < INLINE_FRAME_MAX_BCI).then(|| (bc_pc as u32, Vec::new()));
        }
        // Inside one: the enclosing method's index is the OUTERMOST splice's
        // entry bci -- where the invoke this whole nest replaces lives in the
        // compiling method's own code.
        let outer = scopes[0].entry_bci;
        if outer >= INLINE_FRAME_MAX_BCI {
            return None;
        }
        // The same all-or-nothing chain the row recorder uses: a level with an
        // out-of-spec bci or an empty label refuses the WHOLE site, because a
        // chain with a hole attaches its remaining entries to the wrong caller.
        let chain = build_inline_frame_chain(scopes.as_slice())?;
        Some((outer as u32, chain))
    });
    let Some((bci, chain)) = described else {
        return 0;
    };
    let id = NPE_TRAP_NEXT_ID.with(|c| {
        let next = c.get().wrapping_add(1);
        c.set(next);
        next
    });
    // The trampoline carries the key in the upper 24 bits of one imm32; a
    // compile with more sites than that describes no more of them.
    if id >= (1 << 24) {
        return 0;
    }
    NPE_TRAP_SITES.with(|v| v.borrow_mut().push((id, NpeTrapSite { bci, chain })));
    id
}

/// Discard whatever an abandoned compile left behind and open a fresh trap
/// table. Called from the same session guard as
/// [`begin_inline_frame_recording`].
#[allow(dead_code)]
pub fn begin_npe_trap_recording() {
    NPE_TRAP_SITES.with(|v| v.borrow_mut().clear());
    NPE_TRAP_NEXT_ID.with(|c| c.set(0));
}

/// Close the trap table and hand it back. IDEMPOTENT, like
/// [`finish_inline_frame_recording`]: a second call takes an empty vector and
/// returns an empty map, so the success path may call it directly and still let
/// the session guard's `Drop` run.
#[allow(dead_code)]
pub fn finish_npe_trap_recording() -> NpeTrapMap {
    let mut sites = NPE_TRAP_SITES.with(|v| std::mem::take(&mut *v.borrow_mut()));
    // Ascending by construction (ids come from a counter). Sorted anyway
    // because `get` binary-searches, and an out-of-order push would otherwise
    // turn a lookup into a silent miss or, worse, a hit on a neighbour.
    sites.sort_unstable_by_key(|(id, _)| *id);
    NpeTrapMap { sites }
}

/// One level of an inline chain: a spliced callee, and the bytecode index --
/// in THAT callee's own code -- of the call leading one level further in.
///
/// `label` is `"class/Name.method:descriptor"`, the same shape
/// `CompiledMethod::method_label` carries, so the consumer parses it with the
/// splitter `stackwalker::compiled_frame_entry` already has rather than a
/// second one.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct InlineFrameLevel {
    /// `"class/Name.method:descriptor"`.
    pub label: String,
    /// Bytecode index inside `label`'s method.
    pub bci: u32,
    /// `ClassId` of the class `label` names, or `0` when the resolver supplied
    /// none. Carried so a consumer that must answer in `ClassId` -- the JEP 403
    /// deep-reflection gate, `Class.forName`'s caller loader -- can expand an
    /// inlined level WITHOUT resolving a JIT label by name, which in a
    /// security-relevant path would be a guess. See `InlineSite::class_id`.
    pub class_id: u32,
}

/// One PC-keyed row: at `native_offset` (equivalently, under safepoint id
/// `safepoint_bci`), `chain` is the list of inlined callees the enclosing
/// compiled frame is standing inside, INNERMOST FIRST.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct InlineFrameRow {
    pub native_offset: u32,
    pub safepoint_bci: u32,
    pub chain: Vec<InlineFrameLevel>,
}

/// The finished map for one compiled artifact.
///
/// Empty -- and holding no allocation -- for every method that splices
/// nothing, which is the overwhelming majority. Built once at the end of a
/// compile and then immutable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code, clippy::type_complexity)]
pub struct InlineFrameMap {
    /// Ascending by offset. Exact: a parent frame's return address names one
    /// program point and nothing else.
    by_native_offset: Vec<(u32, Vec<InlineFrameLevel>)>,
    /// Ascending by bci. `None` = two rows under this bci disagreed, so the
    /// safepoint-id evidence cannot pick between them and the lookup refuses.
    by_safepoint_bci: Vec<(u32, Option<Vec<InlineFrameLevel>>)>,
}

#[allow(dead_code, clippy::type_complexity)]
impl InlineFrameMap {
    /// Nothing recorded -- the state of every compile that splices nothing,
    /// and the state produced when the map is switched off.
    pub fn is_empty(&self) -> bool {
        self.by_native_offset.is_empty() && self.by_safepoint_bci.is_empty()
    }

    /// How many PC rows are held. Worth reporting separately from "a map was
    /// built": that a map exists and that it named a live PC are different
    /// facts, and a count of the first cannot stand for the second.
    pub fn len(&self) -> usize {
        self.by_native_offset.len()
    }

    /// The chain at an exact return-address offset -- the evidence available
    /// for every frame BELOW the innermost one.
    pub fn chain_for_native_offset(&self, native_offset: u32) -> Option<&[InlineFrameLevel]> {
        self.by_native_offset
            .binary_search_by_key(&native_offset, |(o, _)| *o)
            .ok()
            .map(|i| self.by_native_offset[i].1.as_slice())
    }

    /// The chain under a safepoint id -- the only evidence the INNERMOST frame
    /// has, since it owns no return address on this stack.
    ///
    /// `None` both for "no row" and for "rows disagreed". The caller cannot act
    /// differently on the two and must not: an ambiguous chain and an absent
    /// one both mean no inlined frame may be reported here.
    pub fn chain_for_safepoint_bci(&self, safepoint_bci: u32) -> Option<&[InlineFrameLevel]> {
        let i = self
            .by_safepoint_bci
            .binary_search_by_key(&safepoint_bci, |(b, _)| *b)
            .ok()?;
        let chain = self.by_safepoint_bci[i].1.as_deref();
        if chain.is_none() {
            // A POISONED bci: a frame this trace deliberately does not show.
            // Counted -- and named under the debug key -- because a lost frame
            // has to be attributable to this refusal in ONE run. A silent
            // shortening reads exactly like the pre-map behaviour it replaced,
            // which is the shape that let the defect this poison closes go
            // unseen in the first place. The `.ok()?` above is NOT counted:
            // "no row under this bci" is a different fact from "the rows under
            // it disagreed", and only the second is a frame given up.
            note_inline_miss_edge(INLINE_MISS_EDGE_LOOKUPS_REFUSED, 1);
            if inline_frame_dbg() {
                eprintln!(
                    "[cratonvm-jitc] inline-frame-map REFUSED safepoint_bci={safepoint_bci} (two program points under one bci disagree) census={:?}",
                    inline_miss_edge_poison_counts(),
                );
            }
        }
        chain
    }

    /// Collapse the raw emission-order rows into the two lookup tables.
    ///
    /// `code_len` is the artifact's final code length; a row past it describes
    /// bytes that are not in the artifact and is dropped.
    pub(crate) fn from_rows(rows: Vec<InlineFrameRow>, code_len: usize) -> Self {
        // Rewind backstop. Rows are appended in emission order, so their
        // offsets are strictly increasing UNLESS the buffer was rewound
        // between two of them. When it was, every row at or above the new
        // offset describes machine code that no longer exists.
        let mut kept: Vec<InlineFrameRow> = Vec::new();
        for row in rows {
            while kept
                .last()
                .map(|last| last.native_offset >= row.native_offset)
                .unwrap_or(false)
            {
                kept.pop();
            }
            kept.push(row);
        }
        let limit = u32::try_from(code_len).unwrap_or(u32::MAX);
        kept.retain(|r| r.native_offset <= limit);

        let mut by_native_offset: Vec<(u32, Vec<InlineFrameLevel>)> =
            Vec::with_capacity(kept.len());
        let mut by_safepoint_bci: Vec<(u32, Option<Vec<InlineFrameLevel>>)> = Vec::new();
        // Parallel to `by_safepoint_bci` while it is being built: did a
        // guarded-splice MISS-EDGE row contribute to this slot? Census only --
        // the poisoning itself is the ordinary disagreement rule below,
        // unchanged. An EMPTY chain identifies such a row unambiguously and
        // needs no extra field: `build_inline_frame_chain` answers `None` on
        // an empty scope stack and otherwise yields at least one level, so a
        // row recorded by `record_inline_frame_row` can never be empty.
        let mut saw_miss_edge: Vec<bool> = Vec::new();
        for r in &kept {
            by_native_offset.push((r.native_offset, r.chain.clone()));
            let is_miss_edge = r.chain.is_empty();
            // `position` rather than `find`, so the parallel vector above can
            // be indexed with the same slot. Same lookup, same order.
            match by_safepoint_bci
                .iter()
                .position(|(b, _)| *b == r.safepoint_bci)
            {
                Some(i) => {
                    let agrees = match &by_safepoint_bci[i].1 {
                        Some(existing) => *existing == r.chain,
                        None => false,
                    };
                    if !agrees {
                        by_safepoint_bci[i].1 = None;
                    }
                    if is_miss_edge {
                        saw_miss_edge[i] = true;
                    }
                }
                None => {
                    by_safepoint_bci.push((r.safepoint_bci, Some(r.chain.clone())));
                    saw_miss_edge.push(is_miss_edge);
                }
            }
        }
        // Census BEFORE the sort, which reorders `by_safepoint_bci` and would
        // desync the parallel vector. Two numbers, because "this bci is
        // ambiguous" and "the miss-edge poison is what made it ambiguous" are
        // different readings and one total cannot separate them.
        let mut poisoned = 0u64;
        let mut poisoned_by_miss_edge = 0u64;
        for (i, (_, chain)) in by_safepoint_bci.iter().enumerate() {
            if chain.is_none() {
                poisoned += 1;
                if saw_miss_edge.get(i).copied().unwrap_or(false) {
                    poisoned_by_miss_edge += 1;
                }
            }
        }
        note_inline_miss_edge(INLINE_MISS_EDGE_BCIS_POISONED, poisoned);
        note_inline_miss_edge(INLINE_MISS_EDGE_BCIS_BY_MISS, poisoned_by_miss_edge);
        if inline_frame_dbg() && !kept.is_empty() {
            eprintln!(
                "[cratonvm-jitc] inline-frame-map rows={} bcis={} poisoned={poisoned} by-miss-edge={poisoned_by_miss_edge} {:?}={:?}",
                kept.len(),
                by_safepoint_bci.len(),
                INLINE_MISS_EDGE_POISON_NAMES,
                inline_miss_edge_poison_counts(),
            );
        }
        by_native_offset.sort_by_key(|(o, _)| *o);
        by_safepoint_bci.sort_by_key(|(b, _)| *b);
        Self {
            by_native_offset,
            by_safepoint_bci,
        }
    }
}

/// One live splice while the emitter is inside it.
struct InlineFrameScope {
    /// `"class/Name.method:descriptor"` of the callee being spliced.
    label: String,
    /// `ClassId` of that callee's class, or `0`. See
    /// [`InlineFrameLevel::class_id`].
    class_id: u32,
    /// Where the invoke this splice replaces lives in the ENCLOSING bytecode --
    /// the compiling method's own code for a top-level splice, the enclosing
    /// callee's code for a nested one. Frozen at the push, which is why it can
    /// still name the enclosing level after the walk has moved on.
    entry_bci: usize,
    /// Where THIS splice's walk currently stands, in the callee's own code.
    /// `usize::MAX` until the walk sets it, which refuses the row rather than
    /// publishing a bci nothing produced.
    cur_pc: usize,
}

thread_local! {
    /// Is a recording session open on this thread? A plain `Cell<bool>` so
    /// every hook below can bail on one thread-local read: a compile that
    /// splices nothing never reaches this file at all, and a compile that
    /// splices while the session is closed pays exactly this read per splice
    /// and per emitted call.
    static INLINE_FRAME_RECORDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Rows for the compile in progress, in emission order.
    static INLINE_FRAME_ROWS: std::cell::RefCell<Vec<InlineFrameRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The live splice stack, OUTERMOST first.
    static INLINE_FRAME_SCOPES: std::cell::RefCell<Vec<InlineFrameScope>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// `(id, site)` for every inline null check this compile described, ascending
    /// by id. See [`record_npe_trap_site`].
    static NPE_TRAP_SITES: std::cell::RefCell<Vec<(u32, NpeTrapSite)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Next id. MONOTONIC across one compile and never reused, which is what
    /// makes a stale key a MISS rather than a wrong answer -- see
    /// [`record_npe_trap_site`].
    static NPE_TRAP_NEXT_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether compiles emit a PC -> inline-chain map at all.
///
/// Default ON. `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` records nothing, so the
/// retained metadata and the extra trace frames disappear together and both
/// the cost and the behaviour are A/B-able inside ONE binary -- the same shape
/// as `CRATONVM_JIT_NO_COMPILED_FRAME_LINES`, and for the same reason: a trace
/// that looks wrong after warm-up must be attributable to one environment
/// variable rather than to a rebuild.
#[allow(dead_code)]
pub fn inline_frame_map_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_INLINE_FRAME_MAP"))
}

/// Open a recording session for one compile. Anything left over from an
/// abandoned compile on this thread is discarded here rather than inherited,
/// because a row from a previous compile names an offset in a DIFFERENT code
/// buffer.
#[allow(dead_code)]
pub fn begin_inline_frame_recording() {
    let on = inline_frame_map_enabled();
    INLINE_FRAME_ROWS.with(|r| r.borrow_mut().clear());
    INLINE_FRAME_SCOPES.with(|s| s.borrow_mut().clear());
    INLINE_FRAME_RECORDING.with(|c| c.set(on));
}

/// Close the session and hand back the finished map.
///
/// Always leaves the thread with no session open and no retained rows, on
/// every exit from a compile -- including the ones that discard the artifact.
#[allow(dead_code)]
pub fn finish_inline_frame_recording(code_len: usize) -> InlineFrameMap {
    let was_recording = INLINE_FRAME_RECORDING.with(|c| c.replace(false));
    let rows = INLINE_FRAME_ROWS.with(|r| std::mem::take(&mut *r.borrow_mut()));
    INLINE_FRAME_SCOPES.with(|s| s.borrow_mut().clear());
    if !was_recording {
        return InlineFrameMap::default();
    }
    InlineFrameMap::from_rows(rows, code_len)
}

#[inline]
fn inline_frame_recording() -> bool {
    INLINE_FRAME_RECORDING.with(std::cell::Cell::get)
}

/// `"class/Name.method:descriptor"` for a resolved site -- the shape
/// `CompiledMethod::method_label` uses, built from the same three strings the
/// invalidation triple is built from.
fn inline_site_label(site: &crate::InlineSite) -> String {
    format!(
        "{}.{}:{}",
        site.class_name, site.method_name, site.descriptor
    )
}

fn push_inline_frame_scope(label: String, class_id: u32, entry_bci: usize) {
    if !inline_frame_recording() {
        return;
    }
    INLINE_FRAME_SCOPES.with(|s| {
        s.borrow_mut().push(InlineFrameScope {
            label,
            class_id,
            entry_bci,
            cur_pc: usize::MAX,
        })
    });
}

fn pop_inline_frame_scope() {
    if !inline_frame_recording() {
        return;
    }
    INLINE_FRAME_SCOPES.with(|s| {
        s.borrow_mut().pop();
    });
}

/// Point the innermost live splice at the callee instruction being emitted.
///
/// Kept separate from `Compiler::inline_walk_at`, which looks like it would
/// serve: a nested splice overwrites it and `try_emit_nested_inline` does not
/// restore it, so after a nested body returns it names the INNER walk's last
/// position while the enclosing walk is still emitting the miss edge of the
/// same invoke. Reading it there would attribute the miss-edge call to a bci
/// in another method's bytecode.
fn set_inline_frame_scope_pc(cur_pc: usize) {
    if !inline_frame_recording() {
        return;
    }
    INLINE_FRAME_SCOPES.with(|s| {
        if let Some(top) = s.borrow_mut().last_mut() {
            top.cur_pc = cur_pc;
        }
    });
}

fn inline_frame_rows_len() -> usize {
    if !inline_frame_recording() {
        return 0;
    }
    INLINE_FRAME_ROWS.with(|r| r.borrow().len())
}

/// Discard rows recorded by an abandoned splice. Called from every rollback
/// path that rewinds the buffer, beside the `deopt_points` truncation it
/// mirrors: a row surviving a rollback would name an offset the fall-through
/// call path has since overwritten with different code.
fn truncate_inline_frame_rows(n: usize) {
    if !inline_frame_recording() {
        return;
    }
    INLINE_FRAME_ROWS.with(|r| r.borrow_mut().truncate(n));
}

/// The chain for the live splice stack, INNERMOST FIRST, or `None` when any
/// level is not fully described.
///
/// Level `i`'s bci is where control leaves level `i` -- the walk's current
/// position for the innermost level, and the frozen `entry_bci` of the level
/// one deeper for every other. Refusing the whole chain on one bad level is
/// deliberate: a chain with a hole is not a shorter chain, it is a chain whose
/// remaining entries attach to the wrong caller.
fn build_inline_frame_chain(scopes: &[InlineFrameScope]) -> Option<Vec<InlineFrameLevel>> {
    if scopes.is_empty() {
        return None;
    }
    let innermost = scopes.len() - 1;
    let mut chain: Vec<InlineFrameLevel> = Vec::with_capacity(scopes.len());
    let mut i = innermost + 1;
    while i > 0 {
        i -= 1;
        let bci = if i == innermost {
            scopes[innermost].cur_pc
        } else {
            scopes[i + 1].entry_bci
        };
        if bci >= INLINE_FRAME_MAX_BCI || scopes[i].label.is_empty() {
            return None;
        }
        chain.push(InlineFrameLevel {
            label: scopes[i].label.clone(),
            // Cast: guarded above by the JVMS 4.9.1 bound.
            bci: bci as u32,
            class_id: scopes[i].class_id,
        });
    }
    Some(chain)
}

/// Record one row at the return address of a call emitted from inside a
/// spliced body.
///
/// `native_offset` must be the buffer position immediately AFTER the `CALL`,
/// because that is the value a parent frame's RBP-chain walk reads out of
/// `[rbp+8]` and the key `OopMapEntry::native_pc_offset` uses.
fn record_inline_frame_row(native_offset: usize, safepoint_bci: usize) {
    if !inline_frame_recording() {
        return;
    }
    let chain = INLINE_FRAME_SCOPES.with(|s| build_inline_frame_chain(s.borrow().as_slice()));
    let Some(chain) = chain else {
        return;
    };
    if safepoint_bci >= INLINE_FRAME_MAX_BCI {
        return;
    }
    let native_offset = match u32::try_from(native_offset) {
        Ok(v) => v,
        Err(_) => return,
    };
    INLINE_FRAME_ROWS.with(|r| {
        r.borrow_mut().push(InlineFrameRow {
            native_offset,
            // Cast: guarded above by the JVMS 4.9.1 bound.
            safepoint_bci: safepoint_bci as u32,
            chain,
        })
    });
}

/// Record the guarded-virtual site's MISS-EDGE row: the splice's
/// `safepoint_bci`, and an EMPTY chain, so `from_rows` poisons that bci.
///
/// See the block comment above [`inline_miss_edge_poison_disabled`] for the
/// whole argument. Three details of the call:
///
///  * `safepoint_bci` MUST be the same `self.cur_bc_pc` the calls inside the
///    body are recorded under, not the site `pc` the caller happens to hold.
///    They are equal today (the walk assigns `cur_bc_pc = pc` once per outer
///    bytecode and the inline walk does not move it), but poisoning a bci the
///    splice's own rows are not filed under would leave the real one intact
///    and the fabricated frame in place -- a fix that reads as applied and is
///    not;
///  * `native_offset` is the buffer position at the START of the splice. That
///    is the only program point inside this bci this file can name -- the miss
///    edge's own bytes are emitted by `bytecode_walk`'s dispatch tail. It is a
///    legal key for `by_native_offset`: key 1 is an EXACT match, so it answers
///    for that offset and nothing else, and the answer it gives there (an
///    empty chain, i.e. no inlined frames) is the truth for the guard bytes it
///    points at;
///  * it is pushed BEFORE the body is walked, so the row list stays ascending
///    in `native_offset` and `from_rows`' rewind backstop keeps every row the
///    splice goes on to record. Were two rows ever to share an offset, the
///    backstop would drop the EARLIER one, which loses a chain rather than
///    inventing one -- fail-closed in the same direction as everything else
///    here.
fn record_inline_frame_miss_edge_row(native_offset: usize, safepoint_bci: usize) {
    if !inline_frame_recording() {
        return;
    }
    if inline_miss_edge_poison_disabled() {
        // Counted even when reverted, so an A/B pair reports the same
        // engagement on both arms and a zero on the treatment arm cannot be
        // mistaken for "the guarded site never happened".
        note_inline_miss_edge(INLINE_MISS_EDGE_REVERTED, 1);
        return;
    }
    if safepoint_bci >= INLINE_FRAME_MAX_BCI {
        // The same JVMS 4.9.1 screen `record_inline_frame_row` applies. A bci
        // this file would refuse to record a chain under is one no chain can
        // be found under either, so there is nothing to poison.
        return;
    }
    let native_offset = match u32::try_from(native_offset) {
        Ok(v) => v,
        Err(_) => return,
    };
    note_inline_miss_edge(INLINE_MISS_EDGE_ROWS, 1);
    INLINE_FRAME_ROWS.with(|r| {
        r.borrow_mut().push(InlineFrameRow {
            native_offset,
            // Cast: guarded above by the JVMS 4.9.1 bound.
            safepoint_bci: safepoint_bci as u32,
            // The whole point: a chain that agrees with no other chain, so the
            // existing disagreement rule in `from_rows` poisons this bci.
            chain: Vec::new(),
        })
    });
}

fn record_merge_state(
    states: &mut [Option<(usize, Vec<bool>)>],
    target: usize,
    depth: usize,
    marks: &[bool],
) -> bool {
    match states.get_mut(target) {
        None => false,
        Some(slot) => match slot {
            Some((recorded_depth, recorded_marks)) => {
                *recorded_depth == depth && recorded_marks.as_slice() == marks
            }
            None => {
                *slot = Some((depth, marks.to_vec()));
                true
            }
        },
    }
}

/// How deep the CALLEE's operand stack may be at a branch or a merge point.
///
/// Each live value costs one reserved frame slot for the whole splice plus a
/// load/store pair per incoming path, so this is a real cost and not just a
/// safety bound. Java's own expression stack at a merge is almost always 1 —
/// the `cond ? a : b` diamond and the null-guard shape
/// (`iconst_1; goto L; iconst_0; L: ireturn`) both merge exactly one value.
/// Four leaves room for nested conditionals without letting an unusual body
/// reserve an unbounded region.
pub(crate) const MAX_INLINE_MERGE_DEPTH: usize = 4;

/// Can an ARRAY be the receiver of a call whose constant pool names
/// `class_name` as the receiver's static type?
///
/// An array is assignable to exactly `java/lang/Object`, `java/lang/Cloneable`
/// and `java/io/Serializable` (JVMS 4.10.1.2), and the verifier holds every
/// other invoke's receiver to a subtype of the named class. It matters to a
/// RECEIVER CLASS-ID GUARD: an array header carries its COMPONENT's class id at
/// offset 0 (a primitive array carries 0), so `CMP DWORD [recv], Foo` passes
/// for a `Foo[]` receiver. The MIC/PIC cascades screen `KIND_TAGS` on every
/// probe; the guarded-inline chains screen it only where this answers `true`,
/// which keeps every other guarded site byte-identical.
///
/// An EMPTY name (no resolved owner to reason from) answers `true`: the guard
/// costs one compare, and not knowing the static type is not evidence that an
/// array cannot arrive.
pub(super) fn static_receiver_admits_arrays(class_name: &str) -> bool {
    class_name.is_empty()
        || matches!(
            class_name,
            "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
        )
}

/// Lengths of the deferred patch lists an inline rollback has to rewind
/// besides the ones every rollback site truncates by hand
/// (`exception_check_stubs`, `deopt_stubs`, `forward_patches`,
/// `jump_table_patches`, `self_call_patches`, `bounds_check_stubs`,
/// `null_check_store_stubs`) -- see `Compiler::late_patch_checkpoint`.
///
/// Each of these holds buffer offsets recorded by code a splice emits, and
/// none was rewound, so an abandoned splice left entries naming offsets that
/// the fall-through call path then re-emits with different bytes:
///
/// * `local_handler_stubs` -- a splice's post-invoke exception check inside a
///   `try` (local handlers armed, the default under precise frames) records
///   its `JO`/`JNE` there; `emit_local_handler_stubs` then writes a `rel32`
///   into whatever the kept code has at that offset. The same corruption the
///   `exception_check_stubs` truncation was added for
///   (`GroovyScriptFactoryTests`), on the list added after it.
/// * `helper_call_patches` / `rip_abs_disp32_patches` -- read only by the loop
///   unroller, which re-resolves every entry inside a copied body: a stale
///   entry makes it rewrite four bytes of the COPY of unrelated code
///   (`rel32 - shift`).
///
/// Round 9 wave 9 (arr9).
#[derive(Clone, Copy, Debug)]
pub(super) struct LatePatchCheckpoint {
    helper_call_patches: usize,
    rip_abs_disp32_patches: usize,
    local_handler_stubs: usize,
}

/// The safepoint metadata a splice can add, as it stood before the splice —
/// see `Compiler::safepoint_meta_checkpoint`.
#[derive(Clone, Copy, Debug)]
struct SafepointMetaCheckpoint {
    /// `oop_maps.len()`.
    oop_maps: usize,
    /// `incomplete_oop_maps`.
    incomplete_oop_maps: usize,
    /// The one bytecode pc every safepoint inside the splice is filed under.
    bc_pc: u32,
    /// Whether `bc_pc` was already in `safepoint_pcs`.
    spilled: bool,
    /// Whether `bc_pc` was already in `mapped_safepoint_pcs`.
    mapped: bool,
}

impl Compiler {
    // -----------------------------------------------------------------------
    // Object headers, fields, allocation, string layout
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/objects.rs`.

    /// Emit inlined callee bytecode at the given caller PC.
    ///
    /// Returns `true` if inlining succeeded, `false` to fall back to a normal call.
    /// The callee's locals are allocated in the caller's spill area so no new frame
    /// is needed. Forward branches within the callee are tracked and patched after
    /// emission. On return, the callee's result (if any) is on the caller's operand
    /// stack.
    /// Speculatively inline the callee at `pc`. On any mid-body bail this
    /// rolls back ALL speculative state — emitted machine code, the
    /// simulated operand stack, its oop-mark vector, and the spill
    /// cursor — so the caller can cleanly fall back to a normal call.
    ///
    /// Historically the `return false` bail points inside the inline
    /// interpreter reset only `next_spill_offset`; the partially-emitted
    /// callee body (and any operand-stack pops) were left in place. The
    /// caller then emitted a *second*, full dispatch for the same call
    /// site — duplicate/garbage code that miscompiled the method (boxed
    /// values came back as 0). Snapshotting + rollback here makes a bail
    /// fully transparent.
    /// Null-check the receiver of a STATICALLY BOUND call that is about to be
    /// spliced (an `invokespecial`, top-level or nested).
    ///
    /// JVMS 6.5 raises `NullPointerException` at the invoke itself, before the
    /// callee's first instruction. A splice has no CALL, so the only thing that
    /// would ever raise it is the spliced body dereferencing `this` — and a
    /// body that does not (`return 3;`, `return helper(x);`) ran to completion
    /// on a null receiver. The top-level direct-call arm fixed exactly this for
    /// its own CALL (`emit_precise_null_check_field_store` after the pops); a
    /// splice of the same callee skipped it.
    ///
    /// Not emitted for:
    ///  * a static callee — no receiver;
    ///  * `<init>` — its receiver is the result of `new` or the constructor's
    ///    own `this`, which the verifier guarantees non-null;
    ///  * a guarded (virtual/interface) splice — its guard chain already sends
    ///    a null receiver to the ordinary dispatch.
    ///
    /// The receiver is PEEKED (the splice pops it), so this emits a TEST/JZ and
    /// changes no compiler state other than the buffer and the null-check stub
    /// list, both of which every rollback path already restores.
    pub(super) fn emit_spliced_receiver_null_check(
        &mut self,
        callee_is_static: bool,
        callee_is_init: bool,
        recv_depth: usize,
    ) {
        if callee_is_static || callee_is_init || recv_depth == 0 {
            return;
        }
        let Some(recv_idx) = self.stack.len().checked_sub(recv_depth) else {
            // The splice itself refuses an underflowing stack; nothing to check.
            return;
        };
        let recv_slot = self.stack[recv_idx];
        self.load_slot_to_reg(RAX, recv_slot);
        self.emit_precise_null_check_field_store();
    }

    /// The guarded inline `getfield` for a SPLICED callee's body (r9 wave 3,
    /// x64obj3; `perf-spliced-callee-getfield-always-calls-the-helper`).
    ///
    /// The splice walker's `0xb4` arm used to CALL `jit_getfield` for every
    /// read — `IrEscapeProbe` paid 18 M helper calls for one `c.v` in a
    /// spliced `consume(Cell c)` — while the top-level arms (`op_field.rs`,
    /// "sp-compact-inline-slowpath" / "sp-legacy-inline-slowpath") have read
    /// inline since July. This is the same sequence, with the checked helper
    /// as its slow edge only:
    ///
    /// 1. with a baked compact offset, the layout-replacement epoch guard
    ///    first (its fallback form clobbers R11/RCX, so before the receiver
    ///    load);
    /// 2. the receiver: `emit_guarded_getfield_receiver_check` against the READ
    ///    bounds table (null, unaligned or outside every published region →
    ///    helper). A splice has no stack-type proof and no implicit-null
    ///    bookkeeping of its own, so the trusted-oop shortcut is not used;
    /// 3. the per-object `GC_FLAG_COMPACT` bit: compact → the packed field at
    ///    `HEADER_SIZE + c_off` in its own width, or (no compact offset for this
    ///    pc) → the helper; legacy → the 16-byte `Value` cell, with the
    ///    `FIELD_CELL_TAG_OBJECT` test before a reference payload is trusted;
    /// 4. slow edge: the unchanged `jit_getfield` call + sentinel check.
    ///
    /// Result in RAX; the caller pushes and marks it. Returns `false` having
    /// emitted NOTHING when the shape is not admitted — the same predicate the
    /// top-level arms use (`guarded_inline_getfield_enabled()`, a published
    /// read table, no narrow-oops width hazard) — and the caller keeps its
    /// helper-only sequence. `CRATONVM_JIT_GETFIELD_HELPER=1` (the guarded
    /// arm's kill switch) therefore restores the old code here too.
    pub(super) fn emit_spliced_inline_getfield(
        &mut self,
        obj_slot: StackSlot,
        field_index: usize,
        type_tag: u8,
        compact: Option<(u32, bool)>,
        cpc: usize,
    ) -> bool {
        if narrow_oops_block_inline_fields()
            || !guarded_inline_getfield_enabled()
            || self.helpers.read_bounds_addr == 0
        {
            return false;
        }
        let Some(legacy_cell_off) = field_index
            .checked_mul(SLOT_SIZE)
            .and_then(|o| o.checked_add(HEADER_SIZE))
            .and_then(|o| i32::try_from(o).ok())
        else {
            return false;
        };
        let reads_ref =
            matches!(type_tag, b'L' | b'[') || compact.is_some_and(|(_, is_ref)| is_ref);
        let mut slow: Vec<usize> = Vec::new();
        if compact.is_some() && jit_sp_field_layout_guard_enabled() {
            slow.extend(self.emit_layout_epoch_guard());
        }
        self.load_slot_to_reg(RAX, obj_slot);
        let read_bounds = self.helpers.read_bounds_addr;
        slow.extend(self.emit_guarded_getfield_receiver_check(read_bounds));
        let mut done: Vec<usize> = Vec::new();
        match compact {
            Some((c_off, c_is_ref)) => {
                // Cast: a compact offset plus the header is bounded by the
                // object size, far below i32::MAX.
                let cell_off = (HEADER_SIZE + c_off as usize) as i32;
                self.emit_mov_r32_mem_disp32(RCX, RAX, cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32);
                self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                let legacy_patch = self.emit_jcc_rel32_patch(0x84); // JZ -> legacy
                if c_is_ref {
                    self.emit_mov_r64_mem_disp32(RAX, RAX, cell_off);
                } else {
                    match type_tag {
                        b'J' | b'D' => self.emit_mov_r64_mem_disp32(RAX, RAX, cell_off),
                        // Contradictory metadata (see the top-level arm): a
                        // non-ref-classified reference slot is a whole
                        // `Value` cell; read its pointer payload.
                        b'L' | b'[' => self.emit_mov_r64_mem_disp32(
                            RAX,
                            RAX,
                            cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                        ),
                        b'F' => self.emit_mov_r32_mem_disp32(RAX, RAX, cell_off),
                        b'Z' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 8, false),
                        b'B' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 8, true),
                        b'C' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, false),
                        b'S' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, true),
                        _ => self.emit_movsxd_r64_mem_disp32(RAX, RAX, cell_off),
                    }
                }
                done.push(self.emit_jmp_rel32_patch());
                self.patch_rel32_to_here(legacy_patch);
            }
            None => {
                if cratonvm_types::compact_ref_fields_enabled() {
                    // No compact offset for this pc: the uniform cell load
                    // below is valid for a legacy object only.
                    self.emit_mov_r32_mem_disp32(
                        RCX,
                        RAX,
                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                    );
                    self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                    slow.push(self.emit_jcc_rel32_patch(0x85)); // JNZ -> helper
                }
            }
        }
        // --- legacy 16-byte `Value` cell ---
        if reads_ref {
            // The cell must SAY it holds a reference before its payload is
            // read as one (the Tomcat/Derby `addr=0x5` SIGSEGV, 2026-08-23).
            self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX+disp32], imm8
            self.buf
                .emit(&(legacy_cell_off + FIELD_CELL_TAG_OFFSET as i32).to_le_bytes());
            self.buf
                .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
            slow.push(self.emit_jcc_rel32_patch(0x85)); // JNE -> helper
        }
        if reads_ref || matches!(type_tag, b'J' | b'D') {
            self.emit_mov_r64_mem_disp32(
                RAX,
                RAX,
                legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
            );
        } else if type_tag == b'F' {
            self.emit_mov_r32_mem_disp32(
                RAX,
                RAX,
                legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
            );
        } else {
            self.emit_movsxd_r64_mem_disp32(
                RAX,
                RAX,
                legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
            );
        }
        done.push(self.emit_jmp_rel32_patch());
        // --- slow edge: the checked helper, exactly the old arm ---
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, cpc);
        crate::metrics::note_getfield_arm(if compact.is_some() { 1 } else { 2 });
        // (vm_ptr, obj_ptr, field_index) -> value | i64::MIN.
        cratonvm_jit_api::assert_helper_call_shape!("getfield", int_args = 3, returns_value = true);
        self.emit_call_absolute(self.helpers.getfield);
        self.emit_post_invoke_exception_check(type_tag);
        for p in done {
            self.patch_rel32_to_here(p);
        }
        true
    }

    /// Truncate the three lockstep deopt vectors (`deopt_points`,
    /// `deopt_point_pcs`, `deopt_boxes`) back to `checkpoint` — AND drop every
    /// `deopt_box_ptr_by_bci` / `exc_frame_box_ptr_by_bci` entry that names one
    /// of the boxes being dropped.
    ///
    /// The splice rollbacks used to truncate the vectors only. Both maps hold a
    /// RAW pointer into a `deopt_boxes` element and are consulted by later
    /// emission as "this bci already has a snapshot" (`contains_key`, then the
    /// stored pointer is baked into a stub as an imm64). An entry inserted by a
    /// rolled-back splice — `emit_precise_null_check_field_store` inside a
    /// spliced `putfield` does it on a protected pc — therefore outlived the
    /// `Box` it named: the fall-through call at the SAME bci found the key,
    /// skipped building its own point, and baked a pointer to freed memory
    /// into the stub it deopts through.
    fn truncate_deopt_points_to(&mut self, checkpoint: usize) {
        if let Some(dropped) = self.deopt_boxes.get(checkpoint..) {
            if !dropped.is_empty() {
                let dropped: rustc_hash::FxHashSet<*const crate::deopt::DeoptimizationPoint> =
                    dropped.iter().map(|b| std::ptr::addr_of!(**b)).collect();
                self.deopt_box_ptr_by_bci
                    .retain(|_, p| !dropped.contains(&*p));
                self.exc_frame_box_ptr_by_bci
                    .retain(|_, p| !dropped.contains(&*p));
                // The third map of the same shape (OSR-exit / reason-8 indy
                // trap snapshots, `osr.rs`) was never purged. Since r9 wave 6
                // `emit_deopt_stubs` fails the compile closed on a key naming
                // no live box instead of baking it, so a stale entry here now
                // costs the whole method rather than a dangling pointer; purge
                // it with the other two.
                self.osr_exit_box_ptr_by_bci
                    .retain(|_, p| !dropped.contains(&*p));
            }
        }
        self.deopt_points.truncate(checkpoint);
        self.deopt_point_pcs.truncate(checkpoint);
        self.deopt_boxes.truncate(checkpoint);
    }

    /// Capture the safepoint metadata a splice can add, for
    /// [`Self::restore_safepoint_meta`].
    ///
    /// Every safepoint inside a splice is filed under ONE bytecode pc — the
    /// enclosing invoke's (`cur_bc_pc` is not moved by the inline walk) — so
    /// "was that pc already in the set" is the whole of what a rollback needs
    /// to know about the two pc sets.
    fn safepoint_meta_checkpoint(&self) -> SafepointMetaCheckpoint {
        let bc_pc = self.cur_bc_pc as u32; // Cast: a bytecode pc fits u32
        SafepointMetaCheckpoint {
            oop_maps: self.oop_maps.len(),
            incomplete_oop_maps: self.incomplete_oop_maps,
            bc_pc,
            spilled: self.safepoint_pcs.contains(&bc_pc),
            mapped: self.mapped_safepoint_pcs.contains(&bc_pc),
        }
    }

    /// Take back the oop maps, the incomplete-map count and the pc-set
    /// insertions a rolled-back splice made.
    ///
    /// The rollbacks restored the buffer and every patch list but left these,
    /// so an abandoned splice's maps survived keyed on the very `bytecode_pc`
    /// the fall-through call at that site files ITS map under. Every GC reader
    /// unions the maps of one safepoint id, so the dead maps' slots were
    /// treated as live at the real call; an abandoned INCOMPLETE map still
    /// counted in `incomplete_oop_maps` (turning `fully_oop_covered` off for a
    /// method whose emitted code was complete); and a pc the abandoned splice
    /// inserted into `mapped_safepoint_pcs` stayed there. All fail-closed or
    /// precision losses rather than lost roots — but each one is a map
    /// describing code that does not exist.
    fn restore_safepoint_meta(&mut self, cp: SafepointMetaCheckpoint) {
        self.oop_maps.truncate(cp.oop_maps);
        self.incomplete_oop_maps = cp.incomplete_oop_maps;
        if !cp.spilled {
            self.safepoint_pcs.remove(&cp.bc_pc);
        }
        if !cp.mapped {
            self.mapped_safepoint_pcs.remove(&cp.bc_pc);
        }
    }

    /// Capture the lengths [`Self::restore_late_patches`] rewinds to. See
    /// [`LatePatchCheckpoint`] for why these three lists.
    pub(super) fn late_patch_checkpoint(&self) -> LatePatchCheckpoint {
        LatePatchCheckpoint {
            helper_call_patches: self.helper_call_patches.len(),
            rip_abs_disp32_patches: self.rip_abs_disp32_patches.len(),
            local_handler_stubs: self.local_handler_stubs.len(),
        }
    }

    /// Drop every entry an abandoned splice appended to the three lists of
    /// [`LatePatchCheckpoint`]. All three are append-only while the body is
    /// walked (the unroller extends the first two only at an outer-method
    /// back edge, never inside a splice; `local_handler_stubs` is drained
    /// only after the walk), so truncating to the recorded length removes
    /// exactly the splice's entries.
    pub(super) fn restore_late_patches(&mut self, cp: LatePatchCheckpoint) {
        self.helper_call_patches.truncate(cp.helper_call_patches);
        self.rip_abs_disp32_patches
            .truncate(cp.rip_abs_disp32_patches);
        self.local_handler_stubs.truncate(cp.local_handler_stubs);
    }

    pub(super) fn try_emit_inline(&mut self, pc: usize) -> bool {
        let Some(site) = self.inline_sites.get(&pc).cloned() else {
            return false;
        };
        self.try_emit_inline_site(pc, &site)
    }

    /// [`Self::try_emit_inline`] for a body that is NOT the `inline_sites`
    /// entry for `pc`.
    ///
    /// A bimorphic site splices two different callee bodies behind two guards
    /// at one caller pc — two overriding subclasses are two different methods,
    /// which is the entire point of a two-way split — so exactly one of them
    /// can be the `inline_sites` entry. Both go through this function, so both
    /// get the same rollback set and the same deopt-metadata postcondition.
    pub(super) fn try_emit_inline_site(&mut self, pc: usize, site: &crate::InlineSite) -> bool {
        // THE PRECISE-FRAMES INTERLOCK, stated a third time, at the one place
        // that can observe it being broken (round 10 wave 7).
        //
        // `precise_exception_frames` and splicing are mutually exclusive, and
        // this is the only file in which that matters for a reason other than
        // policy. The splice walk reaches TWO conditional deopt-point publishers
        // — `emit_post_invoke_exception_check` (its nested-invoke arm) and
        // `emit_precise_null_check_field_store` — and each of them publishes
        // exactly when `self.precise_exception_frames` is set and the pc is
        // protected.
        //
        // An earlier draft of this comment said four, naming
        // `emit_post_alloc_oom_check` and the two array arms. Round 10's `bybci`
        // lane enumerated this file's entire cross-file call surface and found
        // those are not reachable from here at all, while
        // `emit_precise_null_check_field_store` — which IS — was missing from the
        // list. A list that is wrong in both directions is worse than no list,
        // because the whole point of naming them is that someone relaxing the
        // interlock has to go and read them. Each also keys on `Compiler::dbg_last_pc`,
        // which only the OUTER walk assigns, so the pc is an ENCLOSING-method pc
        // while `current_bytecode_owner` names the CALLEE: a point stamped with
        // the callee's key at the caller's bci, which the identity clause of
        // `splice_point_is_misidentified` does not catch.
        //
        // The interlock has FOUR statements in total. The other three are
        // elsewhere and none is sufficient by itself:
        //
        //   * `plan_inline` refuses every site with
        //     `InlineRefusal::PreciseExceptionFrames` — a policy decision, and
        //     policy is what a future wave relaxes ("leaf bodies with no throwing
        //     opcode are surely safe");
        //   * `build_single_pass_tables` calls `inline_sites.clear()`;
        //   * and, since round 10, `inline_guard_variants.clear()` in the same
        //     block. Before that the first of the two was alone, and it did NOT
        //     close the guarded-virtual splice path — which has been on by
        //     default since N8 — so the interlock rested on `plan_inline`'s
        //     policy rather than on either clear. (The function is
        //     `build_single_pass_tables`, not `try_compile_inner`, which an
        //     earlier draft named.)
        //
        // So: refuse here too. This costs one already-loaded `bool` test per
        // splice attempt and cannot change any artifact today, because a compile
        // with `precise_exception_frames` reaches this function with both maps
        // empty. If it ever DOES fire, the two statements above have drifted, and
        // the reader who relaxed one of them needs the publishers named above to
        // say whose pc they are publishing before this refusal can be lifted.
        if self.precise_exception_frames {
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                eprintln!(
                    "[cratonvm-jitc] inline-refused {}.{}{} at pc={pc}: \
                     precise_exception_frames (no run has ever printed this line; \
                     see the interlock comment in x64/inlining.rs)",
                    site.class_name, site.method_name, site.descriptor,
                );
            }
            return false;
        }
        let buf_checkpoint = self.buf.pos();
        let stack_checkpoint = self.stack.clone();
        let oop_marks_checkpoint = self.stack_oop_marks.clone();
        // The scratch-XMM allocation mask is the one piece of operand-stack
        // state NOT derived from `self.stack` (scratch GPRs are found by
        // scanning the stack; scratch XMMs are a bitmask). The body's opening
        // `flush_scratch_registers` zeroes it, so restoring only the stack on a
        // bail would bring back `Xmm(n)` entries whose register the mask then
        // calls free -- and the next `alloc_scratch_xmm` hands it out again
        // over the live value. Restored with the stack on every rollback here
        // and in the two nested wrappers below (r9 wave 7, review7a).
        let scratch_xmm_checkpoint = self.scratch_xmm_in_use;
        let spill_checkpoint = self.next_spill_offset;
        // groovyjarjarasm-asm-handler-getexceptiontablesize-sigsegv-20260713:
        // the buffer/stack/oop-marks/spill rollback above is NOT the full set
        // of speculative side effects `try_emit_inline_body` can produce. Any
        // bytecode instruction it simulates (e.g. an inlined `invoke*` via
        // `emit_post_invoke_exception_check`) can also push a **patch-site
        // offset** -- a raw `usize` into `self.buf` -- onto one of these
        // deferred patch-list fields. Those offsets are only meaningful while
        // they point at the placeholder bytes (`0F 84 00 00 00 00` etc.) that
        // were live when they were recorded. A bail rewinds `self.buf` past
        // them (via `rewind_to` above) and the fall-through normal-call path
        // then emits *different* code over that same buffer range -- but
        // without this snapshot/truncate, the stale offset(s) from the
        // abandoned attempt survive in the Vec and get blindly patched later
        // (`emit_exception_check_stub` / `emit_deopt_stubs` / `patch_branches`
        // / `patch_self_calls`, all of which run once at the very end of
        // `compile_bytecode` over the FINAL, already-reused buffer), corrupting
        // whatever real instruction now lives at that stale offset. Root-caused
        // via a live trace on `groovyjarjarasm.asm.Handler.getExceptionTableSize`
        // (pulled in by Groovy's ASM-based class generation under
        // `GroovyScriptFactoryTests`): a stale `exception_check_stubs` entry
        // from a rewound inline attempt got patched into the middle of the
        // *kept* method's precise-maps safepoint-id store, scribbling a bogus
        // immediate byte and a corrupt REX prefix into otherwise-valid JIT
        // code -- an immediate SIGSEGV the instant the (very hot, called
        // thousands of times) method next ran, well before any test
        // discovery. Snapshot every such deferred patch-list field here and
        // truncate back on bail, mirroring the buffer/stack rollback above.
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();
        // Round 9 wave 9 (arr9): the late patch lists no rollback used to
        // rewind -- see `LatePatchCheckpoint`.
        let late_patch_checkpoint = self.late_patch_checkpoint();
        // Reload-elision mirror: the inline mini-emitter replays CALLEE
        // bytecode whose internal joins the position rule cannot see (the
        // main loop's branch-target invalidation covers only OUTER-method
        // pcs). Suppress the mechanism for the duration and drop any live
        // mirror on both entry and exit; a bail additionally rewinds the
        // buffer, which would otherwise let a stale recorded position
        // "validate" against different, re-emitted code.
        let mirror_suppressed_checkpoint = self.slot_mirror_suppressed;
        self.slot_mirror = None;
        self.slot_mirror_suppressed = true;
        let deopt_points_checkpoint = self.deopt_points.len();
        let safepoint_meta_checkpoint = self.safepoint_meta_checkpoint();
        // The scope for this splice — the ENCLOSING method's frame at the
        // invoke, captured before the callee's body is emitted. See
        // `push_inline_scope`; popped on BOTH exits below, because a scope left
        // behind by a bailed splice would attach a caller frame to every later
        // point in the enclosing method.
        // M1: the callee's identity rides with the scope. Everything published
        // from inside this splice — the `FrameState::method_key` and the bci
        // space it is expressed in — is derived from it, which is what makes
        // the postcondition below satisfiable rather than permanently false.
        let callee_key = format!(
            "{}.{}:{}",
            site.class_name, site.method_name, site.descriptor
        );
        // The identity a point published inside this splice must NOT carry —
        // read before the push, which is the only moment it is still the
        // enclosing owner. `self.method_key` today (nothing nests through this
        // wrapper: a nested splice goes through `try_emit_nested_inline`, which
        // pushes no scope), but written as the accessor so that a nested splice
        // which ever does push one is checked against the enclosing CALLEE
        // rather than against the root.
        let enclosing_owner_key = self.current_bytecode_owner().to_string();
        self.push_inline_scope(
            pc,
            site.callee_num_args,
            callee_key.clone(),
            // The callee's OWN local count, which is the geometry a point
            // published inside this splice is described with — see
            // `InlineCalleeScope::max_locals`, and `build_frame_state_at` for
            // why the root's `num_locals` was the wrong answer.
            site.callee_max_locals,
        );
        // The inline-frame map's own scope, pushed in lockstep with the
        // deopt scope above and popped beside it. Separate because the two
        // answer different questions: `push_inline_scope` records the
        // ENCLOSING frame's state so a deopt can rebuild it, and stamps the
        // compiling method's key on every level; this one records the
        // CALLEE's identity, which is what a stack trace has to name.
        let inline_frame_rows_checkpoint = inline_frame_rows_len();
        push_inline_frame_scope(inline_site_label(site), site.class_id, pc);
        // The guarded-virtual MISS EDGE's poison row. In one line: a
        // receiver-guarded site emits guard, splice and miss edge under ONE
        // `cur_bc_pc`; the miss edge records no row of its own; without this
        // the innermost frame's key-2 lookup hands a frame suspended on the
        // miss edge the chain of a splice that did not run. The long form,
        // including why this cannot disturb the 2026-09-01 witness, is the
        // block comment above `inline_miss_edge_poison_disabled`.
        //
        // `inline_guard_variants` is the gate because it is the SAME map
        // `bytecode_walk`'s guard chain is driven from: populated only for
        // `plan.is_speculative()` and only under
        // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`, so it is true for exactly the
        // sites that HAVE a miss edge and empty for every site on the default
        // path. A statically bound splice (`invokestatic` / `invokespecial`,
        // which is what the witness inlines) REPLACES its call and has no miss
        // edge, so it is not gated in and gives up nothing.
        //
        // `self.cur_bc_pc`, not `pc`: the row has to be filed under the same
        // key the calls inside the body are, and that is what those record.
        //
        // Pushed here rather than after the walk for two reasons. The row
        // order stays ascending in `native_offset`, which is what `from_rows`'
        // rewind backstop reads; and the rollback below already truncates to
        // `inline_frame_rows_checkpoint`, so a refused splice takes its poison
        // row with it -- there is no miss edge to protect where there was no
        // splice.
        if self.inline_guard_variants.contains_key(&pc) {
            record_inline_frame_miss_edge_row(buf_checkpoint, self.cur_bc_pc);
        }
        let walk_at_checkpoint = self.inline_walk_at;
        self.inline_walk_at = (usize::MAX, 0);
        // The callee-local oop scope this splice pushes lives exactly as long
        // as its body is being emitted. Truncated on BOTH exits, like every
        // other speculative side effect above: a scope left behind by a bailed
        // splice would name spill slots the fall-through call path has already
        // handed to something else.
        let oop_scope_checkpoint = self.inline_oop_scopes.len();
        let inline_ok = self.try_emit_inline_body(pc, site);
        self.inline_oop_scopes.truncate(oop_scope_checkpoint);
        let bailed_at = self.inline_walk_at;
        // A nested splice runs the same walk, so restore the enclosing walk's
        // position on the way out: otherwise an inner body that finished
        // cleanly would overwrite where the OUTER one stands.
        self.inline_walk_at = walk_at_checkpoint;
        self.pop_inline_scope();
        pop_inline_frame_scope();
        self.slot_mirror_suppressed = mirror_suppressed_checkpoint;
        self.slot_mirror = None;
        // PGO-02 §3, enforced rather than argued.
        //
        // An inlined body — statically bound or behind a receiver guard — is
        // entered and left inside ONE machine frame, the caller's own, and a
        // deopt point published inside it has to describe TWO interpreter frames
        // to be honest about that. `deopt::FrameState::caller` is the field that
        // can say so and `push_inline_scope` is the producer that fills it, both
        // since 2026-08-18; before that an inlined scope was not representable at
        // all, and a point published from inside a spliced body could only name
        // the CALLER's method with the CALLEE's bci — a well-formed description
        // of a stack that never existed, which is the exact failure class the
        // 2026-08-01 deopt-metadata audit found three of.
        //
        // The safety argument used to be a claim about the source ("the
        // emitter contains no `build_and_record_deopt_point` on this path").
        // That claim is one future edit away from being false, and nothing
        // would fail when it became false. Check the postcondition instead.
        //
        // (It is also not quite true as stated, which is the better reason not to
        // rest on it: the splice walk contains no SNAPSHOT call, but its
        // nested-invoke and precise-field-store arms reach
        // `emit_post_invoke_exception_check` / `emit_precise_null_check_field_store`,
        // each of which publishes a point when `precise_exception_frames` is set. What keeps them quiet is
        // the interlock, not the absence of a call — see the refusal at the top of
        // this function.)
        //
        // **What it checks changed 2026-08-18, and was WRONG until then.** The
        // old check refused any published metadata at all, because an inlined
        // scope was not representable. `push_inline_scope` made a caller chain
        // representable, so the check was relaxed to "refuse a point that has
        // no caller scope" — and that is not the property. A caller chain says
        // "there is an outer frame"; it says nothing at all about whether the
        // INNERMOST frame describes the callee.
        //
        // M1 then made it able to. `build_frame_state_at` stamps
        // `method_key: self.current_bytecode_owner()` and publishes the bci
        // through `resume_bci_for`, so inside a splice both name the callee
        // (before M1 the key was `self.method_key`, assigned once per compile and
        // never swapped, while the bci went through the ENCLOSING method's
        // loop-rewrite table — the malformed pair this whole exercise exists not
        // to produce, and one the VM's identity gate
        // `deopt_resume::deopt_frame_matches_method` would ACCEPT, because the
        // key does name the method being resumed).
        //
        // Round 10 wave 7 closed the third axis: the frame CONTENTS, which M1
        // left reading the root method's locals under the callee's name, and
        // added the bci-range clause below so that the converse pair — an
        // enclosing pc under the callee's name, which is what the `dbg_last_pc`
        // publishers above would produce — is refused rather than admitted.
        //
        // So the postcondition asks what it was always meant to ask: a point
        // published from inside this splice must carry a caller chain, an
        // innermost `method_key` naming the spliced callee, AND a bci inside that
        // callee's code. The difference from simply reverting to "refuse any
        // published metadata" is that the check states the property, so the edit
        // that produces well-formed callee metadata is admitted automatically and
        // nothing else is.
        //
        // Nothing publishes from inside a splice today
        // (`emit_inline_invoke_into_rax` deliberately omits
        // `snapshot_pre_intrinsic_call`; the reason-9/10/11 publishers are
        // interlocked off), so this is a gate on a future edit, which is
        // precisely what it is for.
        //
        // Checked over the points this body added, not over the whole vector:
        // an enclosing method's own earlier points legitimately have no caller
        // scope, and scanning them would refuse every splice in any method that
        // publishes anything at all.
        let published_misidentified_point = self.deopt_points[deopt_points_checkpoint..]
            .iter()
            .any(|p| {
                splice_point_is_misidentified(
                    p,
                    &callee_key,
                    &enclosing_owner_key,
                    site.callee_code_len,
                )
            });
        // A raw stub with no matching point is metadata this check cannot read,
        // and it is what `force_inline_deopt_publication` injects. Refuse it as
        // before rather than assume it is well-formed.
        let published_unreadable_stub = self.deopt_stubs.len() > deopt_stubs_checkpoint
            && self.deopt_points.len() == deopt_points_checkpoint;
        let published_deopt_metadata = published_misidentified_point || published_unreadable_stub;
        // Is this population non-empty? A relaxation nobody can see is
        // indistinguishable from no relaxation, and the only splices affected
        // are the ones that publish — which the old rule refused, so there is
        // no prior count of them anywhere. Named per splice under
        // `CRATONVM_DBG_JITC` rather than guessed at.
        if (self.deopt_points.len() > deopt_points_checkpoint || published_unreadable_stub)
            && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC")
        {
            eprintln!(
                "[cratonvm-jitc] inline-splice {}.{}{} at pc={pc} published {} point(s): {}",
                site.class_name,
                site.method_name,
                site.descriptor,
                self.deopt_points.len() - deopt_points_checkpoint,
                if published_deopt_metadata {
                    "REFUSED (a point that does not identify the callee)"
                } else {
                    "admitted, every point names the callee under a caller scope"
                },
            );
        }
        if inline_ok && !published_deopt_metadata {
            true
        } else {
            // A body that published deopt metadata is rolled back through the
            // SAME path a mid-body bail takes — including the deopt lists
            // themselves, which the truncations below cover.
            // Discard every speculative side effect of the abandoned
            // inline attempt so the fall-through normal-call path starts
            // from exactly the pre-inline machine state.
            self.buf.rewind_to(buf_checkpoint);
            self.stack = stack_checkpoint;
            self.stack_oop_marks = oop_marks_checkpoint;
            self.scratch_xmm_in_use = scratch_xmm_checkpoint;
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            self.forward_patches.truncate(forward_patches_checkpoint);
            self.jump_table_patches
                .truncate(jump_table_patches_checkpoint);
            self.self_call_patches
                .truncate(self_call_patches_checkpoint);
            self.bounds_check_stubs
                .truncate(bounds_check_stubs_checkpoint);
            self.null_check_store_stubs
                .truncate(null_check_store_stubs_checkpoint);
            self.restore_late_patches(late_patch_checkpoint);
            // The three lockstep deopt vectors, AND every by-bci map entry that
            // names a box being dropped -- see `truncate_deopt_points_to`.
            self.truncate_deopt_points_to(deopt_points_checkpoint);
            self.restore_safepoint_meta(safepoint_meta_checkpoint);
            truncate_inline_frame_rows(inline_frame_rows_checkpoint);
            crate::metrics::note_inline_call_arm(6);
            // Name the rollback. The count alone ("outer-splice-rolled-back=1")
            // says a planned splice was thrown away without saying by what, and
            // that has stood as an open question on the netty exhaustive-loop
            // pages since 2026-08-18.
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                let (bail_pc, bail_op) = bailed_at;
                if bail_pc == usize::MAX {
                    eprintln!(
                        "[cratonvm-jitc] inline-rollback {}.{}{} at pc={pc}: before the walk started (prologue/args/merge-region reservation) or refused by the deopt-metadata postcondition",
                        site.class_name, site.method_name, site.descriptor,
                    );
                } else {
                    eprintln!(
                        "[cratonvm-jitc] inline-rollback {}.{}{} at pc={pc}: callee_pc={bail_pc} op=0x{bail_op:02x}",
                        site.class_name, site.method_name, site.descriptor,
                    );
                }
            }
            false
        }
    }

    /// The test-only injection point for the deopt-metadata postcondition
    /// above; a no-op outside tests. The test twin (and
    /// `force_inline_deopt_publication`) live in the test-gated `impl` block
    /// at the bottom of this file, so no test gate sits above production code
    /// (the panic-free ratchet stops scanning at the first one).
    #[cfg(not(test))]
    #[inline(always)]
    fn inline_test_publish_hook(&mut self) {}

    /// Inline-emission body. MUST only be called via [`Self::try_emit_inline`],
    /// which snapshots and restores compiler state around it. A `false`
    /// return from anywhere inside is safe precisely because of that
    /// wrapper — the bail sites here therefore no longer need to unwind
    /// `next_spill_offset` by hand.
    fn try_emit_inline_body(&mut self, pc: usize, site: &crate::InlineSite) -> bool {
        let site = site.clone();
        self.inline_test_publish_hook();

        // An inlined callee emits arbitrary code that uses the caller-saved
        // scratch GPRs (R8/R9) and FP temporaries (XMM0-7) — exactly the
        // registers the deferred-spill operand model (`StackSlot::Scratch` /
        // `StackSlot::Xmm`) parks live values in. Inlining is a call boundary,
        // so flush every caller-live Scratch/Xmm operand to its frame slot
        // BEFORE emitting the callee body — otherwise the callee clobbers a
        // value still live on the caller's operand stack (e.g. a computed
        // double argument or a result held across the call), silently
        // corrupting it. Without this, `leaf(x) + leaf(x*0.5)` and the whole
        // commons-math FastMath.sin family miscompiled under JIT. `try_emit_inline`
        // snapshots+restores all state, so a later mid-body bail rolls this back.
        self.flush_scratch_registers();
        let fp_arith = inline_fp_arith_enabled();

        let callee_code = &site.callee_code;
        let callee_len = site.callee_code_len;
        let callee_num_args = site.callee_num_args;
        let callee_max_locals = site.callee_max_locals;
        let _return_type = site.return_type;
        let (callee_param_jvm_slots, callee_param_slot_span) =
            crate::compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
        if callee_param_jvm_slots.len() != callee_num_args {
            return false;
        }

        let callee_locals_size = callee_max_locals.max(callee_param_slot_span);
        // The cursor as the CALLER's operand stack sees it *before* any of this
        // splice's reservations move it. The invoke's result belongs at the
        // depth the caller's stack reaches once the arguments are popped, which
        // is at or below this — never above it. See `caller_post_pop_spill`.
        let caller_spill_pre_reserve = self.next_spill_offset;
        // Allocate callee locals in caller's spill area.
        let Some(callee_local_base) =
            self.reserve_spill_slots(callee_locals_size, SpillReason::InlineLocals)
        else {
            return false;
        };

        // Make those locals DESCRIBABLE at the safepoints this body emits.
        // Without it they are named by nothing -- see `Compiler::
        // inline_oop_scopes` for the miscompile that produced. Pushed here
        // rather than in the wrapper because the base address is only known
        // now; popped by the wrapper, which owns every other rollback.
        {
            let callee_param_oop_mask =
                crate::compute_param_oop_mask(&site.descriptor, site.callee_is_static);
            let (masks, reached) = compute_local_oop_masks(
                callee_code,
                callee_len,
                callee_locals_size,
                callee_param_oop_mask,
            );
            self.inline_oop_scopes.push(InlineOopScope {
                local_base: callee_local_base,
                num_locals: callee_locals_size,
                masks,
                reached,
                cur_pc: 0,
            });
        }

        // Pop arguments from caller stack and store into callee locals.
        // Args are pushed left-to-right, so stack top = last arg.
        // For instance methods, arg0 = objectref ('this').
        let stack_len = self.stack.len();
        if stack_len < callee_num_args {
            self.next_spill_offset = callee_local_base;
            return false;
        }

        // Spill cursor as the CALLER's operand stack sees it with this invoke's
        // arguments popped — where the return value belongs, and the single
        // thing this splice must restore before pushing it.
        //
        // It cannot be read off `next_spill_offset` after the loop below, and
        // that is the whole trap: `reserve_spill_slots` above has ALREADY moved
        // the cursor past the argument slots (it has to — they stay live until
        // `load_slot_to_reg` marshals each into a callee local), so `pop_stack`'s
        // reclaim arm (`off == next_spill_offset - 8`) can no longer recognise
        // any of them as the top slot and never rewinds. Derive it from the
        // slots themselves: popping the top `n` operands frees every frame slot
        // from the DEEPEST popped one upward, so the lowest popped `Frame`
        // offset is exactly the caller's new top. An argument held in a
        // register (`Scratch`/`Xmm`/`CalleeSaved`) owns no frame slot and
        // correctly does not move the cursor — hence the `min` over `Frame`
        // arms only, seeded with the pre-reservation cursor for a callee whose
        // arguments are all register-resident (and for a no-arg callee, where
        // the caller's top does not move at all).
        //
        // Store args into the callee's JVM local slots. Category-2 parameters
        // consume two JVM slots while the JIT operand stack carries one i64
        // value, so the descriptor-derived slot map must mirror the normal
        // prologue layout (`(JJI)J` -> slots 0, 2, 4).
        let mut caller_post_pop_spill = caller_spill_pre_reserve;
        for i in (0..callee_num_args).rev() {
            let slot = self.pop_stack();
            if let StackSlot::Frame(off) = slot {
                caller_post_pop_spill = caller_post_pop_spill.min(off);
            }
            let local_idx = callee_param_jvm_slots[i];
            let local_off = callee_local_base + (local_idx as i32) * 8; // Cast: x86-64 immediate encoding
            self.load_slot_to_reg(RAX, slot);
            self.emit_store_local(local_off, RAX);
        }
        // ...AND A POP DOES NOT FREE A SLOT A DEEPER ENTRY STILL OWNS.
        //
        // The `min` above is the whole answer only while frame offsets are
        // handed out in stack ORDER, so that the popped arguments are exactly
        // the topmost slots. Two mechanisms break that, and both are load-
        // bearing elsewhere:
        //
        //  * `flush_scratch_registers` (called at the top of this function)
        //    gives every `Scratch`/`Xmm` operand a FRESH slot at the cursor,
        //    whatever its depth — so a buried operand can end up above
        //    shallower ones;
        //  * `invalidate_callee_saved` does the same for every stack entry
        //    aliasing a local register when that local is written, which an
        //    `iinc` on a register-resident loop counter does on every
        //    iteration.
        //
        // When either has fired, `min` over the popped arguments rewinds the
        // cursor BELOW a slot the caller still owns, and the next reservation
        // — this splice's own `callee_local_base` on the NEXT invoke in the
        // same expression, or the `push_from_rax` that lands the return value
        // — hands that address out twice. The buried operand then reads back
        // whatever the new owner stored.
        //
        // Measured on bc-java `LEATest` inside `SimpleTestTest` (193 tests in
        // one JVM, which is what makes `LEAEngine.generate128RoundKeys` hot
        // enough to be compiled with `rol32` spliced in):
        //
        // ```java
        // pWork[j] = rol32(pWork[j] + rol32(myDelta, j++), ROT3);
        // ```
        //
        // The array-store INDEX (`j`) is pushed early and stays live across
        // two splices; `iinc j` repoints it to a fresh top slot; the inner
        // splice then rewinds the cursor under it, and the outer splice stores
        // its argument 0 — `myDelta` — straight onto the index. The result was
        // `ArrayIndexOutOfBoundsException: Index -1007687205`, which is
        // `0xC3EFE9DB`: LEA's `DELTA[0]`, the value of `myDelta` on the first
        // iteration, read as an array index.
        //
        // `pop_stack`'s own reclaim arm carries the same guard, added for the
        // same reason one table over; this is that guard on the path that
        // bypasses it.
        if !inline_live_slot_clamp_disabled() {
            let live_top = self
                .stack
                .iter()
                .filter_map(|slot| match *slot {
                    StackSlot::Frame(off) => Some(off + 8),
                    _ => None,
                })
                .max();
            if let Some(live_top) = live_top {
                if live_top > caller_post_pop_spill {
                    caller_post_pop_spill = live_top;
                    note_inline_live_slot_clamp();
                }
            }
        }

        // Zero-init every non-parameter callee local. Do not start at
        // `callee_num_args`: that compact arg count is not a JVM local index
        // when category-2 parameters are present.
        for i in 0..callee_locals_size {
            if callee_param_jvm_slots.contains(&i) {
                continue;
            }
            let local_off = callee_local_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            self.emit_xor_reg_self(RAX);
            self.emit_store_local(local_off, RAX);
        }

        // Track forward branches within the inlined code: (patch_offset, target_callee_pc)
        let mut branch_patches: Vec<(usize, usize)> = Vec::new();
        // Map callee PC → native offset for branch targets
        let mut callee_pc_to_native: Vec<i64> = vec![-1; callee_len + 1];

        // The canonical home for values live across a branch. Reserved BELOW
        // the callee's operand area (`save_spill`, taken immediately after), so
        // storing into it can never alias the operand slot being read from.
        // Costs `MAX_INLINE_MERGE_DEPTH` slots for the whole splice whether or
        // not the body branches; that is the price of not having to know
        // whether it does before walking it.
        let Some(merge_base) =
            self.reserve_spill_slots(MAX_INLINE_MERGE_DEPTH, SpillReason::InlineMerge)
        else {
            self.next_spill_offset = callee_local_base;
            return false;
        };
        // What each branch target's incoming paths agreed on: `(depth, marks)`.
        // Every branch in a spliced body is FORWARD (`target <= cpc` bails), so
        // a target's entry is always written before the walk arrives at it.
        let mut merge_states: Vec<Option<(usize, Vec<bool>)>> = vec![None; callee_len + 1];

        let mut cpc: usize = 0;
        let save_spill = self.next_spill_offset;
        // Operand-stack depth belonging to the CALLER; the callee's operands
        // sit above it. The callee operand stack is "empty" exactly when
        // `self.stack.len() == caller_base_depth`.
        let caller_base_depth = self.stack.len();
        // Callee branch targets (merge points). Branchy callees inline only
        // when every merge has an EMPTY callee operand stack (enforced by the
        // merge-point reset below + the per-branch checks). 419a6f5 blanket-
        // bailed ALL branches to stop a value-merge slot desync (the
        // `iconst_1; goto L; iconst_0; L: ireturn` diamond); this restores the
        // provably-safe subset (e.g. `x>=0?x:-x`) while still bailing diamonds.
        let callee_branch_targets = bytecode_analysis::branch_target_map(callee_code, callee_len);
        // True after an instruction that does NOT fall through (goto/return/
        // athrow) so the merge-point check can distinguish a dead fall-through
        // (stale slots — safe to reset) from a live value-merge (must bail).
        let mut prev_was_terminator = false;

        while cpc < callee_len {
            let op = callee_code[cpc];
            // Name the spot for a rollback report (see `inline_walk_at`). A
            // bail leaves this at the instruction it died on.
            self.inline_walk_at = (cpc, op);
            // ...and keep the inline-frame scope pointed there too. See
            // `set_inline_frame_scope_pc` for why this is not read off
            // `inline_walk_at` at record time.
            set_inline_frame_scope_pc(cpc);
            // Keep this splice's scope pointed at the instruction being
            // emitted, so a safepoint inside the body reads the oop-local mask
            // for the right callee pc. `last_mut`: a nested splice pushes its
            // own scope and owns the cursor while it runs.
            if let Some(scope) = self.inline_oop_scopes.last_mut() {
                scope.cur_pc = cpc;
            }

            // Merge-point handling.
            //
            // A live path arriving with a value used to REFUSE the splice: two
            // paths put the same value in different places and the symbolic
            // model can only name one. Now both sides store to the merge region
            // and the target reads from there, so a value-producing merge is
            // representable and the `iconst_1; goto L; iconst_0; L: ireturn`
            // diamond splices.
            //
            // ORDER IS LOAD-BEARING. The fall-through's spill is emitted BEFORE
            // `callee_pc_to_native[cpc]` records the label. Emitted after, the
            // taken branch — which already stored its own values on its way
            // here — would land on the label and re-run the fall-through's
            // stores over slots holding whatever that path last left there.
            // Silent wrong values on exactly one of the two paths, which is the
            // failure this whole construct exists to avoid.
            if callee_branch_targets.get(cpc).copied().unwrap_or(false) {
                let recorded = merge_states[cpc].clone();
                if prev_was_terminator {
                    // A dead fall-through: nothing arrives here except the
                    // branches, so adopt what they agreed on and emit nothing.
                    match recorded {
                        Some((depth, marks)) => {
                            self.adopt_merge_slots(caller_base_depth, merge_base, depth, &marks)
                        }
                        None => {
                            self.stack.truncate(caller_base_depth);
                            self.stack_oop_marks.truncate(caller_base_depth);
                        }
                    }
                } else {
                    let Some((depth, marks)) =
                        self.spill_callee_stack_to_merge_slots(caller_base_depth, merge_base, R11)
                    else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    // The paths must agree on how many values are live and on
                    // which of them are references. Verified bytecode
                    // guarantees both, so a disagreement means this walk has
                    // mis-modelled something — refuse rather than pick one.
                    // Marks in particular: calling a non-reference an oop hands
                    // a moving collector a word it will try to relocate.
                    if let Some((rec_depth, rec_marks)) = &recorded {
                        if *rec_depth != depth || *rec_marks != marks {
                            self.next_spill_offset = callee_local_base;
                            return false;
                        }
                    }
                    self.adopt_merge_slots(caller_base_depth, merge_base, depth, &marks);
                }
                self.next_spill_offset = save_spill;
            }
            callee_pc_to_native[cpc] = self.buf.pos() as i64; // Cast: address arithmetic

            match op {
                // nop
                0x00 => {
                    cpc += 1;
                }

                // aconst_null
                0x01 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (op as i32) - 3; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lconst_0, lconst_1
                0x09 | 0x0a => {
                    let val = (op as i64) - 9; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // fconst_0, fconst_1, fconst_2
                0x0b..=0x0d => {
                    let fval: f32 = (op - 0x0b) as f32; // Cast: JIT ABI convention
                    let bits = fval.to_bits() as i64; // Cast: JIT ABI convention
                    self.emit_mov_imm32_sx(RAX, bits as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // dconst_0, dconst_1
                0x0e | 0x0f => {
                    let dval: f64 = (op - 0x0e) as f64; // Cast: JIT ABI convention
                    let bits = dval.to_bits() as i64; // Cast: JIT ABI convention
                    if bits == 0 {
                        self.emit_xor_reg_self(RAX);
                    } else {
                        self.emit_mov_imm64(RAX, bits);
                    }
                    self.push_from_rax();
                    cpc += 1;
                }

                // bipush
                0x10 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let val = callee_code[cpc + 1] as i8 as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 2;
                }

                // sipush
                0x11 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let val =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc
                //
                // `site.ldc_info` / `site.ldc2w_info` are keyed by the
                // callee bytecode PC (see the inline resolver in
                // `try_jit_compile_callee`), NOT the CP index. Match on
                // `cpc` — the prior CP-index lookup silently missed and
                // pushed 0 for the constant.
                //
                // A MISS IS A REFUSAL, NEVER A ZERO. Those tables carry only
                // the constants this emitter can model as an immediate —
                // `Integer` and `Float`. Every other `ldc` kind (String, Class,
                // MethodHandle, MethodType, condy) names a constant whose value
                // is a *reference*, materialised at run time by
                // `helpers.ldc_string` / `helpers.ldc_class_cp`, which this
                // mini-emitter does not call. Substituting 0 for one is wrong
                // code: it pushes `null` where the callee's body pushes a live
                // object. That is not theoretical — it shipped. A one-line
                // `Dialect.extractPattern(unit) { return "extract(?1 from ?2)"; }`
                // inlined into `H2Dialect.extractPattern` compiled to
                // `xor eax,eax; ret`, so every Hibernate HQL `extract()` /
                // `cast()` / `str()` query died in
                // `PatternRenderer.<init>` with
                // `NullPointerException: ... because "pattern" is null` — 17 of
                // the 34 method failures across FunctionTests /
                // StandardFunctionTests / ASTParserLoadingTest / HQLTest on the
                // 2026-08-11 Linux full-suite run, all green under `--nojit`.
                // Refusing costs one real call; this cost correct answers.
                0x12 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let Some((_, val)) = site.ldc_info.iter().find(|(p, _)| *p == cpc) else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 2;
                }

                // ldc_w — same contract as `ldc` above.
                0x13 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let Some((_, val)) = site.ldc_info.iter().find(|(p, _)| *p == cpc) else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc2_w — same contract. Only `Long` and `Double` are
                // modellable here; anything else is a refusal, not a zero.
                0x14 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let Some((_, val)) = site.ldc2w_info.iter().find(|(p, _)| *p == cpc) else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    self.emit_mov_imm64(RAX, *val);
                    self.push_from_rax();
                    cpc += 3;
                }

                // iload, lload, fload, dload, aload
                0x15 | 0x16 | 0x17 | 0x18 | 0x19 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 2;
                }

                // iload_0..iload_3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lload_0..lload_3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // fload_0..fload_3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // dload_0..dload_3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // aload_0..aload_3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // istore, lstore, fstore, dstore, astore
                0x36 | 0x37 | 0x38 | 0x39 | 0x3a => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 2;
                }

                // istore_0..istore_3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // lstore_0..lstore_3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // fstore_0..fstore_3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // dstore_0..dstore_3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // astore_0..astore_3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // pop
                0x57 => {
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // pop2
                0x58 => {
                    let _ = self.pop_stack();
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // dup
                0x59 => {
                    let slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, slot);
                    self.push_from_rax();
                    self.push_from_rax();
                    cpc += 1;
                }

                // swap
                0x5f => {
                    let a = self.pop_stack();
                    let b = self.pop_stack();
                    // Read BOTH operands before the first push: push_from_rax
                    // reuses the just-reclaimed lower spill slot, which is
                    // exactly `b`'s frame slot when both operands are
                    // frame-resident — storing into it before reading `b`
                    // duplicated value1 into both result slots.
                    self.load_slot_to_reg(RAX, a);
                    self.load_slot_to_reg(RCX, b);
                    self.push_from_rax();
                    self.emit_mov_reg_reg(RAX, RCX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iadd
                //
                // THE INT SLOTS IN THIS FRAME ARE SIGN-EXTENDED 64-BIT VALUES,
                // and every consumer relies on it: `i2l` is a no-op here, an
                // `iastore` writes the low word, and `lmul`/`if_icmp*` read the
                // full register. A 64-bit `ADD RAX, RCX` keeps that invariant
                // only while the 32-bit result does not overflow — on
                // overflow it produces the true 65-bit sum instead of the
                // wrapped `int` Java specifies, and the difference is 2^32,
                // which is exactly what a later `i2l` would have thrown away.
                // So compute in 32 bits and re-establish the invariant, the way
                // `ineg`, `ishl`, `ishr` and `iushr` below already do.
                0x60 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // ADD EAX, ECX ; MOVSXD RAX, EAX
                    self.buf.emit(&[0x01, 0xC8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ladd
                0x61 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // isub — 32-bit then sign-extend, for the reason `iadd` gives.
                0x64 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // SUB EAX, ECX ; MOVSXD RAX, EAX
                    self.buf.emit(&[0x29, 0xC8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lsub
                0x65 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // imul — 32-bit then sign-extend. Same invariant as `iadd`,
                // and this is the one that actually bites: a 32-bit product
                // overflows for ordinary inputs, where a sum rarely does.
                //
                // MEASURED. BouncyCastle's HAETAE reduces with
                //
                //     private static int montgomeryReduce(long a) {
                //         int t = (int) a * QINV;             // QINV = 0x380F0401
                //         long tt = a - ((long) t * Q);
                //         return (int) (tt >> 32);
                //     }
                //
                // where the `int` multiply is DELIBERATELY allowed to overflow —
                // that truncation is the algorithm. Inlined into
                // `HAETAEEngine.ntt`, the 64-bit `IMUL RAX, RCX` left the full
                // product in the slot, the `(long) t` that follows was a no-op
                // on an already-64-bit value, and `t * Q` was therefore computed
                // from a number ~2^32 times too large. Every value the transform
                // produced after that was unreduced garbage
                // (`-827016358` where the answer is `-27922`).
                //
                // That is the whole of the HAETAE defect: `pqc` KAT vector 5
                // verifying false and vector 6 never terminating, both of which
                // read as "a JIT bug somewhere in `ntt`" for weeks. `ntt` itself
                // is compiled correctly; what was wrong was the copy of
                // `montgomeryReduce` spliced into it.
                0x68 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // IMUL EAX, ECX ; MOVSXD RAX, EAX
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lmul
                0x69 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
                // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN,
                // matches dividend) before issuing CDQ; IDIV ECX.
                0x6c => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ldiv — JVMS-compliant guards; emits CQO; IDIV RCX with the
                // LONG_MIN / -1 overflow special-case materialised inline.
                0x6d => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // irem — JVMS-compliant guards; INT_MIN % -1 yields 0.
                0x70 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lrem — JVMS-compliant guards; LONG_MIN % -1 yields 0.
                0x71 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ineg
                0x74 => {
                    self.pop_to_rax();
                    // NEG EAX
                    self.buf.emit(&[0xF7, 0xD8]);
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lneg
                0x75 => {
                    self.pop_to_rax();
                    // NEG RAX
                    self.rex_w();
                    self.buf.emit(&[0xF7, 0xD8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishl
                0x78 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL EAX, CL
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshl
                0x79 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL RAX, CL
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishr
                0x7a => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SAR EAX, CL
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshr
                0x7b => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iushr
                0x7c => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHR EAX, CL
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lushr
                0x7d => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iand
                0x7e => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]); // AND RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // land
                0x7f => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ior
                0x80 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]); // OR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lor
                0x81 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ixor
                0x82 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]); // XOR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lxor
                0x83 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iinc
                0x84 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let inc = callee_code[cpc + 2] as i8 as i32; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    // ADD RAX, imm32
                    self.rex_w();
                    self.buf.emit_byte(0x05);
                    self.buf.emit(&inc.to_le_bytes());
                    self.emit_store_local(local_off, RAX);
                    cpc += 3;
                }

                // i2l — identity in our i64 representation
                0x85 => {
                    cpc += 1;
                }

                // l2i — truncate to 32-bit, sign-extend back
                0x88 => {
                    self.pop_to_rax();
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2b
                0x91 => {
                    self.pop_to_rax();
                    // MOVSX RAX, AL
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2c
                0x92 => {
                    self.pop_to_rax();
                    // MOVZX EAX, AX (zero-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                    // Upper 32 bits auto-zeroed
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2s
                0x93 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AX (sign-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lcmp
                0x94 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // CMP RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]);
                    // SETG AL (1 if >)
                    self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                    // MOVZX EAX, AL
                    self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                    // SETL CL
                    self.buf.emit(&[0x0F, 0x9C, 0xC1]);
                    // MOVZX ECX, CL
                    self.buf.emit(&[0x0F, 0xB6, 0xC9]);
                    // SUB EAX, ECX
                    self.buf.emit(&[0x29, 0xC8]);
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ── float / double (r9 wave 7, review7a) ────────────────────
                //
                // Each arm is the OUTER walk's arm (`op_arith.rs`), calling the
                // same helper with the same arguments; see
                // `inline_fp_arith_enabled` for why the shared operand-stack
                // model makes that sound here. `frem`/`drem` (a helper CALL)
                // are deliberately not among them and still refuse the splice.

                // fadd, fsub, fmul, fdiv — ADDSS / SUBSS / MULSS / DIVSS.
                0x62 | 0x66 | 0x6a | 0x6e if fp_arith => {
                    let sse_op = match op {
                        0x62 => 0x58,
                        0x66 => 0x5C,
                        0x6a => 0x59,
                        _ => 0x5E,
                    };
                    self.emit_float_binop(sse_op);
                    cpc += 1;
                }

                // dadd, dsub, dmul, ddiv — ADDSD / SUBSD / MULSD / DIVSD.
                0x63 | 0x67 | 0x6b | 0x6f if fp_arith => {
                    let sse_op = match op {
                        0x63 => 0x58,
                        0x67 => 0x5C,
                        0x6b => 0x59,
                        _ => 0x5E,
                    };
                    self.emit_double_binop(sse_op);
                    cpc += 1;
                }

                // fneg — flip the sign bit of the low word.
                0x76 if fp_arith => {
                    self.pop_to_rax();
                    self.buf.emit_byte(0x35); // XOR EAX, imm32
                    self.buf.emit(&0x8000_0000u32.to_le_bytes());
                    self.push_from_rax();
                    cpc += 1;
                }

                // dneg — flip bit 63.
                0x77 if fp_arith => {
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xBA, 0xF8, 63]); // BTC r/m64, imm8
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2f, i2d, l2f, l2d — CVTSI2SS / CVTSI2SD into XMM0.
                0x86 | 0x87 | 0x89 | 0x8a if fp_arith => {
                    let prefix = if matches!(op, 0x86 | 0x89) {
                        0xF3
                    } else {
                        0xF2
                    };
                    let wide = matches!(op, 0x89 | 0x8a);
                    self.emit_int_to_fp_xmm0(prefix, wide);
                    self.stack_push(StackSlot::Xmm(0), false);
                    cpc += 1;
                }

                // f2i, f2l, d2i, d2l — truncate toward zero, NaN -> 0,
                // out-of-range -> MIN/MAX (`emit_fp_to_int_nan_fixup`).
                0x8b | 0x8c | 0x8e | 0x8f if fp_arith => {
                    let is_double = matches!(op, 0x8e | 0x8f);
                    let is_long = matches!(op, 0x8c | 0x8f);
                    self.pop_fp_operand_to_xmm0(is_double);
                    // CVTTSS2SI / CVTTSD2SI {EAX|RAX}, XMM0
                    self.buf.emit_byte(if is_double { 0xF2 } else { 0xF3 });
                    if is_long {
                        self.buf.emit_byte(0x48);
                    }
                    self.buf.emit(&[0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(is_double, is_long);
                    if !is_long {
                        // MOVSXD RAX, EAX — the int-slot invariant.
                        self.rex_w();
                        self.buf.emit(&[0x63, 0xC0]);
                    }
                    self.push_from_rax();
                    cpc += 1;
                }

                // f2d — CVTSS2SD XMM0, XMM0.
                0x8d if fp_arith => {
                    self.pop_fp_operand_to_xmm0(false);
                    self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    cpc += 1;
                }

                // d2f — CVTSD2SS XMM0, XMM0.
                0x90 if fp_arith => {
                    self.pop_fp_operand_to_xmm0(true);
                    self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    cpc += 1;
                }

                // fcmpl, fcmpg, dcmpl, dcmpg — -1/0/1, NaN per the `g`/`l`.
                0x95..=0x98 if fp_arith => {
                    self.emit_fcmp(matches!(op, 0x97 | 0x98), matches!(op, 0x96 | 0x98));
                    cpc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — conditional branch on <cmp> 0.
                //
                // Restored (419a6f5 blanket-bailed all branches). Inlining a
                // branchy callee is sound in this single-linear-pass emitter
                // ONLY when no operand is live across a merge: operand-stack
                // slots are handed out by a growing `next_spill_offset`, so two
                // paths reaching a merge with a value would hold it in
                // different frame slots (the `iconst_1; goto L; iconst_0; L:
                // ireturn` diamond — the bug 419a6f5 fixed). We therefore
                // require the callee operand stack to be EMPTY after the branch
                // pops its operands (here) AND at every target (the merge-point
                // reset at the loop top); any value-merge bails. Forward
                // branches only — backward edges (loops) re-enter an already-
                // emitted target whose slot layout we can't re-canonicalise.
                0x99..=0x9e => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.pop_to_rax();
                    // Store anything live across this branch to the merge
                    // region, and record what the target must agree with. R11
                    // rather than RAX: the comparison operands are already
                    // loaded and must survive to the `Jcc` below.
                    let Some((merge_depth, merge_marks)) =
                        self.spill_callee_stack_to_merge_slots(caller_base_depth, merge_base, R11)
                    else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    if !record_merge_state(&mut merge_states, target, merge_depth, &merge_marks) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                    let cc = match op {
                        0x99 => 0x84u8, // JE
                        0x9a => 0x85,   // JNE
                        0x9b => 0x8C,   // JL
                        0x9c => 0x8D,   // JGE
                        0x9d => 0x8F,   // JG
                        0x9e => 0x8E,   // JLE
                        _ => {
                            self.next_spill_offset = callee_local_base;
                            return false;
                        }
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4) — int compare branch. Same
                // empty-stack-at-merge safety as 0x99..0x9e above.
                0x9f..=0xa4 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    // Store anything live across this branch to the merge
                    // region, and record what the target must agree with. R11
                    // rather than RAX: the comparison operands are already
                    // loaded and must survive to the `Jcc` below.
                    let Some((merge_depth, merge_marks)) =
                        self.spill_callee_stack_to_merge_slots(caller_base_depth, merge_base, R11)
                    else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    if !record_merge_state(&mut merge_states, target, merge_depth, &merge_marks) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.load_slot_to_reg(RCX, top);
                    self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                    let cc = match op {
                        0x9f => 0x84u8, // JE
                        0xa0 => 0x85,   // JNE
                        0xa1 => 0x8C,   // JL
                        0xa2 => 0x8D,   // JGE
                        0xa3 => 0x8F,   // JG
                        0xa4 => 0x8E,   // JLE
                        _ => {
                            self.next_spill_offset = callee_local_base;
                            return false;
                        }
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // goto (0xa7) — unconditional forward branch. Same safety.
                0xa7 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    // Store anything live across this branch to the merge
                    // region, and record what the target must agree with. R11
                    // rather than RAX: the comparison operands are already
                    // loaded and must survive to the `Jcc` below.
                    let Some((merge_depth, merge_marks)) =
                        self.spill_callee_stack_to_merge_slots(caller_base_depth, merge_base, R11)
                    else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    if !record_merge_state(&mut merge_states, target, merge_depth, &merge_marks) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.buf.emit_byte(0xE9); // JMP rel32
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ireturn, lreturn, areturn, freturn, dreturn
                0xac | 0xad | 0xb0 | 0xae | 0xaf => {
                    // `areturn` returns an object reference, and the popped
                    // entry's own mark says the same thing. The pop/push pair
                    // below moves the value to a new slot, and `push_from_rax`
                    // always marks its push `false` — so without carrying the
                    // mark across, inlining a reference-returning callee erases
                    // the oop tag of its result. Under moving-young that entry
                    // is then neither published nor rewritable.
                    let ret_is_oop =
                        op == 0xb0 || self.stack_oop_marks.last().copied().unwrap_or(false);
                    // Pop callee's return value → push onto caller stack
                    self.pop_to_rax();
                    if op == 0xac {
                        // JVMS §6.5 `ireturn` narrowing, at the splice's return
                        // join: the callee's own compiled body would narrow in
                        // its epilogue, and splicing it must not lose that — an
                        // inlined `()Z` returning 2 is 0 to its caller. Each
                        // `ireturn` of a branchy callee comes through here.
                        let tag = crate::narrowed_int_return_tag(&site.descriptor);
                        self.emit_narrow_int_return(tag);
                    }
                    // Reclaim the callee's locals AND its merge region, back to
                    // the depth the CALLER's operand stack reached when this
                    // invoke's arguments were popped. `save_spill` is the
                    // callee's operand base, which sits `callee_locals_size +
                    // MAX_INLINE_MERGE_DEPTH` slots ABOVE that — pushing the
                    // result from there parks it above its semantic
                    // operand-stack depth, and every later push in the caller's
                    // basic block inherits the shift.
                    //
                    // The linear walk stays self-consistent, so nothing looks
                    // wrong — until the first branch target after the splice,
                    // whose depth is re-established from the bytecode at the
                    // canonical `base_spill_offset + i*8` (see the revived-merge
                    // reconstruction in `bytecode_walk.rs`). Writer and reader
                    // then address different slots and the method computes with
                    // a stale one.
                    //
                    // Measured on ECJ's `OperandStack.pop(OperandCategory)`,
                    // whose inlined `TypeIds.getCategory(id)` result landed two
                    // slots deep while the `tableswitch` merge's `if_icmpeq`
                    // read the true slot 0 — so the compiled body compared the
                    // raw `TypeBinding.id` against the expected category, and
                    // every JSP compiled after that method tiered up threw
                    // `AssertionError: Unexpected operand at stack top`
                    // (tomcat/ecj-operandstack-*.md). The same defect had been
                    // fixed once in the direct-call arms of `bytecode_walk.rs`
                    // (2026-08-06); it came back through this arm when
                    // `0f55466d0` (2026-08-18) taught the splicer to inline a
                    // value-producing branch merge, which is what made that
                    // branchy callee inlinable in the first place.
                    //
                    // Safe: the load above already read the value out of the
                    // callee's slot, and `caller_post_pop_spill` is strictly
                    // below `callee_local_base`, so the store cannot alias
                    // anything the callee still owns.
                    self.next_spill_offset = caller_post_pop_spill;
                    self.push_from_rax();
                    if ret_is_oop {
                        self.mark_top_as_oop();
                    }
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Use callee_len as the "after inline" target
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // return (void)
                0xb1 => {
                    // Same reclaim as the value-returning arm above. A void
                    // splice pushes nothing, so the caller's NEXT push is the
                    // one that would inherit the shift.
                    //
                    // At the OUTER level `reset_spills()` — which the main walk
                    // runs at every instruction boundary — already lowers the
                    // cursor to just past the highest live operand and repairs
                    // this on its own; no test can tell the two apart there, and
                    // one asserting otherwise was written and then deleted for
                    // passing either way. It IS load-bearing for a nested
                    // splice: the mini-walk in this function has no
                    // per-instruction reset, so an inner void body would leave
                    // the enclosing CALLEE's cursor parked in the inner callee's
                    // abandoned frame region.
                    self.next_spill_offset = caller_post_pop_spill;
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // getfield (0xb4) — use callee's field_info
                0xb4 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // `site.field_info` is keyed by the callee bytecode PC
                    // (`fpc` in `try_jit_compile_callee` / the inline
                    // resolver), NOT by the constant-pool index. Using the
                    // CP index here silently mismatched — a multi-field
                    // callee could pick another field op's `field_index`
                    // and read/write the wrong slot. Look up by `cpc`.
                    if let Some((_, field_index, type_tag)) =
                        site.field_info.iter().find(|(p, _, _)| *p == cpc).copied()
                    {
                        let obj_slot = self.pop_stack();
                        // r9w3 (x64obj3): the same guarded inline load the
                        // top-level `getfield` arms emit, with this helper
                        // call as its slow edge only. See
                        // `emit_spliced_inline_getfield`.
                        let compact = site
                            .compact_field_info
                            .iter()
                            .find(|(p, _, _)| *p == cpc)
                            .map(|&(_, off, is_ref)| (off, is_ref));
                        if self.emit_spliced_inline_getfield(
                            obj_slot,
                            field_index,
                            type_tag,
                            compact,
                            cpc,
                        ) {
                            self.push_from_rax();
                            if type_tag == b'L'
                                || type_tag == b'['
                                || compact.is_some_and(|(_, is_ref)| is_ref)
                            {
                                self.mark_top_as_oop();
                            }
                            cpc += 3;
                            prev_was_terminator = false;
                            continue;
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, cpc);
                        crate::metrics::note_getfield_arm(0);
                        // ABI-3 — (vm_ptr, obj_ptr, field_index). `emit_call_absolute` takes
                        // a bare address, so this assertion is the only thing tying the three
                        // registers written above to the helper's declared arity.
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "getfield",
                            int_args = 3,
                            returns_value = true
                        );
                        self.emit_call_absolute(self.helpers.getfield);
                        // The checked helper returns the `i64::MIN` deopt/NPE
                        // sentinel on a bad (stale/corrupt) receiver instead of a
                        // real field value — without this guard the sentinel is
                        // pushed and treated as legitimate data, turning what used
                        // to be an immediate SIGSEGV into silent corruption/hangs
                        // downstream. Mirrors the invoke-site guard below.
                        self.emit_post_invoke_exception_check(type_tag);
                        self.push_from_rax();
                        // Mirrors the top-level `getfield` arms and the inlined
                        // `getstatic` arm just below: a reference field's value
                        // is a live oop and must be tagged, or it is invisible
                        // to both the precise oop map and the shadow stack.
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                    } else {
                        // Cannot resolve field — bail out
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putfield (0xb5) — use callee's field_info
                0xb5 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // Keyed by callee bytecode PC — see the `getfield`
                    // note above. Mismatching CP index vs PC here is the
                    // bug that dropped constructor field writes (boxed
                    // ints / `String.value` came back as 0).
                    if let Some((_, field_index, type_tag)) =
                        site.field_info.iter().find(|(p, _, _)| *p == cpc).copied()
                    {
                        super::bytecode_walk::note_field_site(
                            &self.method_key,
                            "putfield/inlined",
                            cpc,
                            field_index,
                            type_tag,
                        );
                        let val_slot = self.pop_stack();
                        let obj_slot = self.pop_stack();
                        // Make a null receiver a real Java NPE before either
                        // the inline store or a legacy helper can turn it into
                        // a silent no-op. Protected sites retain their precise
                        // exceptional frame for javac monitor cleanup.
                        self.load_slot_to_reg(RAX, obj_slot);
                        self.emit_precise_null_check_field_store();
                        if type_tag == b'L' || type_tag == b'[' {
                            let compact_offset = site
                                .compact_field_info
                                .iter()
                                .find(|(p, _, is_ref)| *p == cpc && *is_ref)
                                .map(|(_, offset, _)| *offset);
                            let fresh_ctor_first_store =
                                inline_site_is_fresh_ctor_first_store(&site, cpc, field_index);
                            // GATED reference store — the FOURTH door.
                            //
                            // This arm was wired into the top-level `putfield`
                            // on 2026-09-02 and not into this one, so a
                            // workload whose reference stores live inside
                            // INLINED callees never reached it. bt18 is exactly
                            // that workload: it reported `gated=2` from two
                            // cold top-level sites and a run-time census of
                            // ZERO executions, while its hot stores went
                            // through the two arms below. The sites here were
                            // not even counted as declines, so the census could
                            // not show the gap either.
                            //
                            // Ordered after the fresh-constructor arm, which is
                            // a proven cheaper specialization (a `new`-produced
                            // object needs no barrier at all, so it emits no
                            // gates), and before the general body arm, which
                            // requires the field's old value to be null and
                            // keys on the STORE-side bounds table rather than
                            // on the collector's published plan.
                            // The fresh-constructor arm is preferred ONLY where
                            // it can actually run. It bails to a full helper
                            // call unless the STORE-side region table holds
                            // live bounds, and the ONLY writer of
                            // `JIT_REGION_BOUNDS` is
                            // `GenerationalHeap::publish_region_bounds` -- G1
                            // publishes the read-side table only and ZGC
                            // neither, as `gen_heap.rs` says where it mirrors
                            // them. (`publish_movable_bounds`, which G1 and ZGC
                            // do call, writes a DIFFERENT table and has nothing
                            // to do with this predicate; an earlier version of
                            // this comment had the two collectors exactly
                            // backwards.) So under ZGC that specialization is
                            // dead and every constructor field store it owns
                            // was an out-of-line `jit_putfield_object`; bt18 is
                            // made of exactly those stores, which is where the
                            // 136.6M inline stores below come from.
                            let fresh_ctor_arm_is_live = fresh_ctor_first_store
                                && region_bounds_are_live(self.helpers.region_bounds_addr);
                            let gated_inlined = !fresh_ctor_arm_is_live
                                && gated_ref_store_enabled()
                                && inline_putfield_enabled()
                                && !narrow_oops_block_inline_fields()
                                && cratonvm_types::compact_ref_fields_enabled()
                                && match compact_offset {
                                    // Cast: a compact field offset plus the
                                    // header is bounded by the object size.
                                    Some(offset) => {
                                        let cell_off =
                                            (cratonvm_types::HEADER_SIZE + offset as usize) as i32;
                                        // `false`: this door has no
                                        // stack-type tracker to prove the
                                        // receiver is an oop, so the gated arm
                                        // takes the full containment check
                                        // against the READ bounds rather than
                                        // a bare null test.
                                        self.emit_gated_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            cell_off,
                                            false,
                                        )
                                    }
                                    None => false,
                                };
                            if gated_inlined {
                                // Complete: store, both gate sequences, the
                                // helper fallback and the out-of-bounds drop
                                // all converge inside that emitter.
                            } else if inline_putfield_enabled()
                                && !narrow_oops_block_inline_fields()
                                && cratonvm_types::compact_ref_fields_enabled()
                                && self.helpers.region_bounds_addr != 0
                            {
                                if let Some(offset) = compact_offset {
                                    if fresh_ctor_arm_is_live {
                                        self.emit_inline_fresh_ctor_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            offset,
                                        );
                                    } else {
                                        // Reached only when the gated arm
                                        // declined, so this is the
                                        // pre-existing behaviour, unchanged --
                                        // and now COUNTED, which it was not
                                        // before.
                                        super::note_ungated_ref_store();
                                        self.emit_inline_body_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            offset,
                                        );
                                    }
                                } else {
                                    super::note_ungated_ref_store();
                                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                    self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32);
                                    self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                    // (vm_ptr, obj_ptr, field_index, value) -> ().
                                    cratonvm_jit_api::assert_helper_call_shape!(
                                        "putfield_object",
                                        int_args = 4,
                                        returns_value = false
                                    );
                                    self.emit_call_absolute(self.helpers.putfield_object);
                                }
                            } else {
                                super::note_ungated_ref_store();
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                // (vm_ptr, obj_ptr, field_index, value) -> ().
                                cratonvm_jit_api::assert_helper_call_shape!(
                                    "putfield_object",
                                    int_args = 4,
                                    returns_value = false
                                );
                                self.emit_call_absolute(self.helpers.putfield_object);
                            }
                        } else if self.try_emit_inline_primitive_putfield(
                            // r9w3 (x64obj3): the same flag-gated inline
                            // primitive store the top-level `putfield` tries
                            // (`CRATONVM_JIT_INLINE_PRIM_PUTFIELD`), keyed on
                            // the CALLEE's compact row for this pc. `false`:
                            // a splice has no stack-type proof of the receiver.
                            site.compact_field_info
                                .iter()
                                .find(|(p, _, _)| *p == cpc)
                                .map(|&(_, off, is_ref)| (off, is_ref)),
                            obj_slot,
                            val_slot,
                            field_index,
                            type_tag,
                            false,
                        ) {
                            // Complete: the inline store, and the unchanged
                            // helper call on every path it declined.
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            // `jit_putfield_int` stores `Value::Int` whole into a
                            // legacy cell; narrow a sub-int field's value here, as
                            // the top-level `putfield` and the interpreter do
                            // (r9 wave 2, ea request 1). No-op for J/F/D/I and
                            // for javac output.
                            self.emit_narrow_to_field_tag(ARG_REGS[2], type_tag);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
                            // The primitive stores take (obj_ptr, field_index, value) with NO
                            // `vm_ptr` — three registers, not `putfield_object`'s four — and
                            // return nothing. One sequence serves four slots; all four are
                            // asserted.
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_int",
                                int_args = 3,
                                returns_value = false
                            );
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_long",
                                int_args = 3,
                                returns_value = false
                            );
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_float",
                                int_args = 3,
                                returns_value = false
                            );
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "putfield_double",
                                int_args = 3,
                                returns_value = false
                            );
                            self.emit_call_absolute(helper);
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // getstatic (0xb2) — use callee's static_field_info
                //
                // Same direct-load-or-helper split as the top-level 0xb2 arm;
                // the long note there explains what is baked and which sites
                // still take `jit_getstatic`.
                0xb2 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    // `static_field_info` is keyed by callee bytecode PC,
                    // not CP index — match on `cpc` (see the `getfield`
                    // note above).
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site
                        .static_field_info
                        .iter()
                        .find(|(p, _, _, _, _)| *p == cpc)
                        .copied()
                    {
                        // Direct load, no helper CALL — see the top-level 0xb2
                        // arm. `flush_scratch_registers` deliberately moved
                        // INSIDE the helper branch: the inline form clobbers
                        // only RAX, so spilling the operand-stack cache for it
                        // would give back part of what it saves. The `else`
                        // arm below abandons the whole inline attempt, so it
                        // needs no flush either.
                        if !self.try_emit_inline_getstatic(
                            class_id_raw,
                            field_index,
                            type_tag,
                            is_volatile,
                        ) {
                            self.flush_scratch_registers();
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                            self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                                                                     // `<clinit>` on first touch: a safepoint. See the
                                                                                     // top-level 0xb2 arm.
                            self.emit_pre_safepoint_spill();
                            // (vm_ptr, class_id, field_index) -> value | sentinel.
                            cratonvm_jit_api::assert_helper_call_shape!(
                                "getstatic",
                                int_args = 3,
                                returns_value = true
                            );
                            self.emit_call_absolute(self.helpers.getstatic);
                            self.emit_oop_map_for_safepoint();
                            // jit-linewrapper-flushtype-npe fix (2026-07-17):
                            // see the matching fix + comment at the top-level
                            // 0xb2 arm -- same helper, same missing
                            // post-invoke exception check for a `<clinit>`
                            // failure surfaced via the deopt sentinel.
                            self.emit_post_invoke_exception_check(type_tag);
                            // Volatile static: no fence after the read by
                            // default (x86-64 loads are acquire; the store
                            // side keeps its MFENCE). Opt back in with
                            // CRATONVM_JIT_VOLATILE_LOAD_FENCE=1.
                            if is_volatile && crate::runtime_lowering::volatile_load_fence_enabled()
                            {
                                self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                            }
                            self.push_from_rax();
                            // T1.1.a (fix, 2026-07-07) — see the matching fix at
                            // the top-level 0xb2 arm: a reference-typed static
                            // field must not keep push_from_rax's default
                            // non-oop mark, or it decodes wrong in a precise
                            // GC/deopt oop map while live.
                            if type_tag == b'L' || type_tag == b'[' {
                                self.mark_top_as_oop();
                            }
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putstatic (0xb3) — use callee's static_field_info
                //
                // Helper-only, like the top-level 0xb3 arm: the address
                // machinery exists now, but the SATB pre-barrier and the
                // first-touch block creation live in `set_static_shared`. See
                // the note on the top-level 0xb3 arm.
                0xb3 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // Keyed by callee bytecode PC — match on `cpc`.
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site
                        .static_field_info
                        .iter()
                        .find(|(p, _, _, _, _)| *p == cpc)
                        .copied()
                    {
                        let val_slot = self.pop_stack();
                        let helper_fn: usize = match type_tag {
                            b'J' => self.helpers.putstatic_long,
                            b'F' => self.helpers.putstatic_float,
                            b'D' => self.helpers.putstatic_double,
                            b'L' | b'[' => self.helpers.putstatic_object,
                            _ => self.helpers.putstatic_int,
                        };
                        // Volatile static: emit MFENCE before write (SeqCst release)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(ARG_REGS[3], val_slot);
                        // JVMS §6.5 `putstatic`: narrow a sub-int static's
                        // value, as the top-level `0xb3` arm and the
                        // interpreter do (r9 wave 7, putstatic7). No-op for
                        // every tag but Z/B/C/S.
                        self.emit_narrow_to_field_tag(ARG_REGS[3], type_tag);
                        // `<clinit>` on first touch: a safepoint. See the
                        // top-level 0xb2 arm.
                        self.emit_pre_safepoint_spill();
                        // One sequence serves five slots, so all five are asserted rather
                        // than four of them being "the same shape as the one above".
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putstatic_int",
                            int_args = 4,
                            returns_value = true
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putstatic_long",
                            int_args = 4,
                            returns_value = true
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putstatic_float",
                            int_args = 4,
                            returns_value = true
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putstatic_double",
                            int_args = 4,
                            returns_value = true
                        );
                        cratonvm_jit_api::assert_helper_call_shape!(
                            "putstatic_object",
                            int_args = 4,
                            returns_value = true
                        );
                        self.emit_call_absolute(helper_fn);
                        self.emit_oop_map_for_safepoint();
                        // jit-putstatic-clinit-gap fix (2026-07-17): the
                        // helper now runs `<clinit>` on first touch before
                        // writing and, on failure, returns the `i64::MIN`
                        // deopt sentinel instead of `0` (mirrors
                        // `jit_getstatic`'s sentinel; see the fix comment on
                        // `jit_putstatic_class_init_guard` in
                        // `vm/src/jit/helpers.rs`). `putstatic` is
                        // void-returning, so route it through the shared
                        // void-helper exception-check convention (same one
                        // the `invokestatic` arraycopy dispatch call uses)
                        // rather than pushing a value.
                        self.emit_post_invoke_exception_check(b'V');
                        // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // if_acmpeq/if_acmpne (0xa5/0xa6) — reference compare branch.
                // Same empty-stack-at-merge safety as 0x99..0x9e.
                0xa5 | 0xa6 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    // Store anything live across this branch to the merge
                    // region, and record what the target must agree with. R11
                    // rather than RAX: the comparison operands are already
                    // loaded and must survive to the `Jcc` below.
                    let Some((merge_depth, merge_marks)) =
                        self.spill_callee_stack_to_merge_slots(caller_base_depth, merge_base, R11)
                    else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    if !record_merge_state(&mut merge_states, target, merge_depth, &merge_marks) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX
                    let cc = if op == 0xa5 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ifnull/ifnonnull (0xc6/0xc7) — null-compare branch. Same safety.
                0xc6 | 0xc7 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.pop_to_rax();
                    // Store anything live across this branch to the merge
                    // region, and record what the target must agree with. R11
                    // rather than RAX: the comparison operands are already
                    // loaded and must survive to the `Jcc` below.
                    let Some((merge_depth, merge_marks)) =
                        self.spill_callee_stack_to_merge_slots(caller_base_depth, merge_base, R11)
                    else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    if !record_merge_state(&mut merge_states, target, merge_depth, &merge_marks) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.rex_w();
                    self.buf.emit(&[0x85, 0xC0]); // TEST RAX, RAX
                    let cc = if op == 0xc6 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // invokespecial (0xb7) — ONLY resolver-proven no-op super
                // constructor calls (`site.elided_invoke_pcs`): the target is
                // `java/lang/Object.<init>()V` (or an elidable trivial chain
                // to it), so the call has no observable effect. Pop the
                // receiver the preceding `aload_0` pushed — a compile-time
                // stack-model adjustment, no machine code — and continue.
                // This is what admits CONSTRUCTOR bodies to inlining. Any
                // other invokespecial bails to the dispatch fallback.
                //
                // Everything else — and every 0xb6/0xb8/0xb9 — is either
                // SPLICED IN TURN (`site.nested_sites`) or emitted as the
                // ordinary dispatch call (`site.resolved_invoke_infos`). A pc
                // in neither map still bails, which is what the whole arm did
                // before either map existed.
                0xb6 | 0xb7 | 0xb8 | 0xb9 => {
                    // invokeinterface is 5 bytes; the rest are 3.
                    let width = if op == 0xb9 { 5 } else { 3 };
                    if cpc + width > callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    if op == 0xb7 && site.elided_invoke_pcs.contains(&cpc) {
                        let _ = self.pop_stack();
                        cpc += width;
                        prev_was_terminator = false;
                        continue;
                    }

                    // Nesting first: a body is strictly better than a call,
                    // and a nested bail falls back to the call below because
                    // `try_emit_nested_inline` rolls itself back completely.
                    let nested = site.nested_sites.iter().find(|n| n.callee_pc == cpc);
                    if let Some(nested) = nested {
                        if nested.guard_class_id == 0 {
                            // Statically bound: nothing null-checks the
                            // receiver unless this does. Inside the
                            // enclosing splice, so its rollback covers it.
                            self.emit_spliced_receiver_null_check(
                                nested.site.callee_is_static,
                                nested.site.method_name == "<init>",
                                nested.site.callee_num_args,
                            );
                            if self.try_emit_nested_inline(&nested.site) {
                                crate::metrics::note_inline_call_arm(2);
                                cpc += width;
                                prev_was_terminator = false;
                                continue;
                            }
                            crate::metrics::note_inline_call_arm(5);
                        } else if let Some(&resolved) = site
                            .resolved_invoke_infos
                            .iter()
                            .find(|r| r.callee_pc == cpc)
                        {
                            // Devirtualised splice: the receiver class the
                            // CALLEE's own profile says dominates this site,
                            // certified by an exact class-id compare, with the
                            // miss edge taking the ordinary call. Needs the
                            // resolved record for that miss edge, which is why
                            // the resolver keeps a guarded pc's dispatch entry.
                            if self.emit_guarded_nested_inline(nested, &resolved) {
                                crate::metrics::note_inline_call_arm(3);
                                cpc += width;
                                prev_was_terminator = false;
                                continue;
                            }
                            crate::metrics::note_inline_call_arm(4);
                        }
                    }

                    let Some(&resolved) = site
                        .resolved_invoke_infos
                        .iter()
                        .find(|r| r.callee_pc == cpc)
                    else {
                        // No resolved target: either the gate is off (the
                        // resolver refused the site and this is unreachable) or
                        // an `InlineSite` reached the emitter without passing
                        // through `try_compile_inner`'s interning step. Bail to
                        // the real call rather than guess a target.
                        self.next_spill_offset = callee_local_base;
                        return false;
                    };
                    if !self.emit_inline_invoke(&resolved) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += width;
                }

                // Unsupported opcode in inline context — bail out
                _ => {
                    // Restore spill offset and return false to fall back to a call
                    self.next_spill_offset = callee_local_base;
                    return false;
                }
            }

            // For the next iteration's merge-point check: goto (0xa7) and the
            // returns (0xac..=0xb1) jump away, so the following PC is reachable
            // only as a branch target (a dead fall-through whose stale slots
            // the merge-point reset may clear). Everything else falls through.
            prev_was_terminator = matches!(op, 0xa7 | 0xac..=0xb1);
        }

        // Mark the "after inline" position for return-jumps
        callee_pc_to_native[callee_len] = self.buf.pos() as i64; // Cast: address arithmetic

        // Patch all forward branches
        for (patch_off, target_cpc) in &branch_patches {
            let target_native = if *target_cpc < callee_pc_to_native.len() {
                callee_pc_to_native[*target_cpc]
            } else {
                self.buf.pos() as i64 // Cast: address arithmetic
            };
            if target_native < 0 {
                // Target not yet emitted (shouldn't happen for forward branches after full emission)
                // Fall back: point to current position
                let rel32 = (self.buf.pos() as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(*patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            } else {
                let rel32 = (target_native as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(*patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            }
        }

        // Reclaim the callee's locals and merge region, leaving the cursor
        // exactly where the CALLER's operand stack now tops out.
        //
        // This must agree instruction-for-instruction with the `xreturn` arms
        // above, which is why both read `caller_post_pop_spill`: a callee with
        // several `return`s emits one such arm per return, and the walk falls
        // out of the loop with whichever one came last — so a cursor recomputed
        // here from anything else would silently disagree with the code that
        // was actually emitted.
        //
        // The return value occupies ONE slot at `caller_post_pop_spill`; the
        // cursor must stay ABOVE it. Handing that slot back instead is a real
        // bug with a name: the next push (a sibling call's argument) reused it,
        // and `leaf(a) + leafBig(a)` miscompiled because `leaf(a)`'s result was
        // overwritten by `iload a` for leafBig's argument.
        let next_spill = if self.stack.len() > caller_base_depth {
            let Some(end) = self.checked_spill_range_end(caller_post_pop_spill, 1) else {
                return false;
            };
            end
        } else {
            // Void callee: nothing pushed, callee frame fully reclaimed.
            caller_post_pop_spill
        };
        self.next_spill_offset = next_spill;

        true
    }

    /// Spill the CALLEE's live operand stack into the merge region, so a
    /// branch and its target agree on where those values live.
    ///
    /// The inline emitter models the operand stack symbolically: a value may be
    /// in a frame slot, a scratch GPR or an XMM register, and WHICH depends on
    /// the path taken to get there. At a merge point two paths arrive with the
    /// same values in different places, and the model can only name one — which
    /// is why `try_emit_inline_body` refused a branch with a non-empty callee
    /// stack outright (419a6f5, the `iconst_1; goto L; iconst_0; L: ireturn`
    /// diamond).
    ///
    /// The fix is the standard one: give every merge a CANONICAL home. Each
    /// path stores its values to `merge_base + i*8` before transferring
    /// control, and the target's model reads them from exactly there. The
    /// region is reserved BELOW the callee's operand area, so a store into it
    /// can never alias the slot it is reading from.
    ///
    /// `scratch` must be a register the caller is not holding a live value in.
    /// The conditional-branch arms have already loaded their comparison
    /// operands into RAX (and RCX), so they pass R11.
    ///
    /// Returns `(depth, oop marks)` — what the target must agree with — or
    /// `None` when the stack is deeper than the reserved region, which refuses
    /// the splice rather than writing past it. Does NOT change the model: the
    /// branching path is about to leave, and the fall-through continues with
    /// its values where they already are.
    fn spill_callee_stack_to_merge_slots(
        &mut self,
        caller_base_depth: usize,
        merge_base: i32,
        scratch: u8,
    ) -> Option<(usize, Vec<bool>)> {
        let depth = self.stack.len().checked_sub(caller_base_depth)?;
        if depth > MAX_INLINE_MERGE_DEPTH {
            return None;
        }
        for i in 0..depth {
            let slot = self.stack[caller_base_depth + i];
            self.load_slot_to_reg(scratch, slot);
            self.emit_store_local(merge_base + (i as i32) * 8, scratch); // Cast: x86-64 immediate encoding
        }
        let marks = self.stack_oop_marks[caller_base_depth..].to_vec();
        Some((depth, marks))
    }

    /// Join the hit paths of a guarded virtual inline to the miss path.
    ///
    /// `op_invoke`'s guard chain emits, per variant, `CMP class; JNE miss;
    /// <spliced body>; JMP done`, then the ordinary dispatch as the miss path,
    /// and lands every `JMP done` after it. The code after `done` is compiled
    /// against ONE operand-stack model -- the dispatch path's, because that is
    /// the Rust-level continuation. The splice's result, however, lands at
    /// `caller_post_pop_spill` (the lowest popped argument slot, clamped above
    /// the live stack), while the dispatch path pushes at its own
    /// `post_pop_spill` or, for `F`/`D`, in XMM0. The two agree only while
    /// frame offsets were handed out in stack order.
    ///
    /// They are not, whenever `flush_scratch_registers` (which runs right
    /// before the guard chain) gives a register-resident operand a fresh slot
    /// above a shallower one. `HeapCharBuffer.compact` is exactly that shape:
    /// `aload_0; iconst_0; invokevirtual ix(I)I` with `this` in a callee-saved
    /// register. The constant took slot 0x38, the flushed receiver 0x40; the
    /// splice left `ix(0)` at 0x38 and the join read 0x40 -- the receiver's
    /// pointer bits -- as `arraycopy`'s `destPos`. That is H2 `TestBigResult`'s
    /// `ArrayIndexOutOfBoundsException: arraycopy: destination index -535859552`,
    /// its `NegativeArraySizeException: nbits < 0` and its missing rows.
    ///
    /// So each hit that disagrees with the join model gets a stub which moves
    /// its values to where the join expects them. Moves go through the machine
    /// stack when there is more than one, so every source is read before any
    /// destination is written. A hit whose stack DEPTH differs cannot be
    /// joined by any move and refuses the compile.
    pub(super) fn reconcile_guarded_inline_join(&mut self, hits: Vec<(usize, Vec<StackSlot>)>) {
        fn same_slot(a: StackSlot, b: StackSlot) -> bool {
            match (a, b) {
                (StackSlot::Frame(x), StackSlot::Frame(y)) => x == y,
                (StackSlot::CalleeSaved(x), StackSlot::CalleeSaved(y)) => x == y,
                (StackSlot::Scratch(x, ..), StackSlot::Scratch(y, ..)) => x == y,
                (StackSlot::Xmm(x), StackSlot::Xmm(y)) => x == y,
                _ => false,
            }
        }
        let join_stack = self.stack.clone();
        let mut stubs: Vec<(usize, Vec<(StackSlot, StackSlot)>)> = Vec::new();
        for (patch, hit_stack) in hits {
            if hit_stack.len() != join_stack.len() {
                self.patch_rel32_to_here(patch);
                self.fail("singlepass-codegen/guarded-inline-join-depth");
                continue;
            }
            let moves: Vec<(StackSlot, StackSlot)> = hit_stack
                .iter()
                .zip(&join_stack)
                .filter(|(h, j)| !same_slot(**h, **j))
                .map(|(h, j)| (*h, *j))
                .collect();
            if moves.is_empty() {
                self.patch_rel32_to_here(patch);
            } else {
                stubs.push((patch, moves));
            }
        }
        if stubs.is_empty() {
            return;
        }
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
            eprintln!(
                "[cratonvm-jitc] guarded-inline join at pc={}: {} hit path(s) reconciled to the dispatch model",
                self.cur_bc_pc,
                stubs.len(),
            );
        }
        // The dispatch path falls through; it skips the stubs.
        let skip = self.emit_jmp_rel32_patch();
        let mut to_join = Vec::with_capacity(stubs.len());
        for (patch, moves) in stubs {
            self.patch_rel32_to_here(patch);
            if let [(src, dst)] = moves[..] {
                self.load_slot_to_reg(RAX, src);
                self.store_rax_to_join_slot(dst);
            } else {
                for (src, _) in &moves {
                    self.load_slot_to_reg(RAX, *src);
                    self.buf.emit_byte(0x50); // PUSH RAX
                }
                for (_, dst) in moves.iter().rev() {
                    self.buf.emit_byte(0x58); // POP RAX
                    self.store_rax_to_join_slot(*dst);
                }
            }
            to_join.push(self.emit_jmp_rel32_patch());
        }
        self.patch_rel32_to_here(skip);
        for p in to_join {
            self.patch_rel32_to_here(p);
        }
        // A join: no store before it is adjacent to what runs after it.
        self.slot_mirror = None;
    }

    /// Write RAX to where `slot` says the value lives. The inverse of
    /// [`Self::load_slot_to_reg`], for [`Self::reconcile_guarded_inline_join`].
    fn store_rax_to_join_slot(&mut self, slot: StackSlot) {
        match slot {
            StackSlot::Frame(off) => self.emit_store_local(off, RAX),
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => {
                if reg != RAX {
                    self.emit_mov_reg_reg(reg, RAX);
                }
            }
            StackSlot::Xmm(xmm) => self.emit_movq_xmm_from_rax(xmm),
        }
    }

    /// Point the model at the merge region — the other half of
    /// [`Self::spill_callee_stack_to_merge_slots`].
    ///
    /// Called at the target once every incoming path has stored its values
    /// there, so from here on the emitter reads them from one place regardless
    /// of which path an execution actually took.
    fn adopt_merge_slots(
        &mut self,
        caller_base_depth: usize,
        merge_base: i32,
        depth: usize,
        marks: &[bool],
    ) {
        self.stack.truncate(caller_base_depth);
        self.stack_oop_marks.truncate(caller_base_depth);
        for i in 0..depth {
            self.stack
                .push(StackSlot::Frame(merge_base + (i as i32) * 8)); // Cast: x86-64 immediate encoding
            self.stack_oop_marks
                .push(marks.get(i).copied().unwrap_or(false));
        }
    }

    /// Emit a call the SPLICED body makes, as the ordinary
    /// `jit_invoke_dispatch` sequence.
    ///
    /// `info_addr` is a `*const JitInvokeInfo` parked as a `usize` in
    /// [`crate::InlineSite::resolved_invoke_infos`] — resolved against the
    /// CALLEE's constant pool and interned into this compile's own arena by
    /// `try_compile_inner`, so the address stays valid for as long as the
    /// emitted code that bakes it.
    ///
    /// This is deliberately the *plain* dispatch, not the MIC/PIC ladder the
    /// top-level `invokevirtual` arm uses: an inline cache slot is allocated
    /// per CALLER pc, and a callee-internal pc has none. The win being bought
    /// here is not a cheaper call — it is that the ENCLOSING body becomes
    /// inlineable at all, which it never was while any `invoke*` refused the
    /// whole site.
    ///
    /// Mirrors the top-level `invokestatic` dispatch site instruction for
    /// instruction: contiguous args buffer built at the pre-pop spill cursor,
    /// four-argument helper call, oop map for the safepoint, post-invoke
    /// exception check, cursor reclaimed to the popped-args depth, then the
    /// result pushed. Returns `false` (having emitted nothing that matters —
    /// the caller rolls back) when the spill region cannot hold the buffer.
    ///
    /// EXCEPTION ROUTING. `emit_post_invoke_exception_check` keys the shared
    /// sentinel exit on `dbg_last_pc`, which is assigned only by the OUTER
    /// bytecode walk and therefore still holds the CALLER's invoke pc for the
    /// whole splice. That is the correct attribution, not an accident of
    /// bookkeeping: an exception escaping an inlined body belongs to the call
    /// site in the enclosing method, and the enclosing method's exception table
    /// is the one that must be searched. The already-shipped spliced `getfield`
    /// / `getstatic` / `arraycopy` sites rely on exactly the same thing.
    fn emit_inline_invoke_into_rax(&mut self, resolved: &crate::ResolvedInlineInvoke) -> bool {
        // SAFETY: see the doc comment — the pointee is owned by this compile's
        // `_jit_invoke_infos` arena and outlives the code being emitted.
        let info = resolved.info_addr as *const crate::JitInvokeInfo;
        if info.is_null() {
            return false;
        }
        let num_args = resolved.num_jit_args;
        let return_type = resolved.return_type;

        // A call boundary: no caller-live value may sit in a scratch GPR or an
        // XMM temporary across it. Same reason `try_emit_inline_body` flushes
        // on entry.
        self.flush_scratch_registers();

        if self.stack.len() < num_args {
            return false;
        }
        // Deliberately NO `snapshot_pre_intrinsic_call` here, unlike the
        // top-level dispatch sites. That snapshot publishes a deopt point keyed
        // by BCI, and inside a splice the only bci available is the callee's —
        // a different bytecode space from the one the enclosing artifact's
        // metadata is indexed by. Publishing one would also trip
        // `try_emit_inline_site`'s postcondition and refuse the splice. The
        // cost of omitting it is reach, not correctness: a trap that would have
        // resumed at this site instead leaves the whole method to the
        // interpreter.
        let pre_pop_spill = self.next_spill_offset;
        // `pop_invoke_args`, not a bare `pop_stack` loop — the SAME channel
        // every top-level invoke arm pops through. It hands back each
        // argument's oop mark (so the staged copies below can be NAMED in this
        // call's safepoint map) and records the register homes of reference
        // arguments in `pending_call_oop_arg_regs` (so the register oop mask
        // does not clear the bit of a register still holding one across the
        // CALL). The bare loop did neither: a reference argument staged into
        // the deopt-service copy or the dispatch buffer sat in the frame
        // unnamed while the map claimed complete coverage, and a moving
        // collection inside the callee left the service copy — which
        // `emit_inline_callee_deopt_check` reads AFTER the call — pointing at
        // from-space.
        let (arg_slots, arg_oops) = self.pop_invoke_args(num_args);
        let post_pop_spill = self.next_spill_offset;

        if resolved.direct_entry != 0 {
            if !self.emit_inline_direct_call(resolved, info, &arg_slots, &arg_oops, pre_pop_spill) {
                return false;
            }
            crate::metrics::note_inline_call_arm(0);
        } else if !self.emit_inline_dispatch_call(info, &arg_slots, &arg_oops, pre_pop_spill) {
            return false;
        } else {
            crate::metrics::note_inline_call_arm(1);
        }
        self.emit_post_invoke_exception_check(return_type);
        self.next_spill_offset = post_pop_spill;
        true
    }

    /// Push a call's result from RAX onto the operand stack, per the descriptor.
    ///
    /// Separate from [`Self::emit_inline_invoke_into_rax`] because a guarded
    /// splice has TWO arms producing the result and must push exactly once,
    /// after they join — see `emit_guarded_nested_inline`. Pushing inside each
    /// arm would give the two paths different frame slots while the emitter's
    /// model named only one of them, which is a silent wrong value on whichever
    /// path the model does not describe.
    fn push_call_result(&mut self, return_type: u8) {
        if return_type == b'V' {
            return;
        }
        if matches!(return_type, b'D' | b'F') {
            self.push_from_rax_as_xmm0();
        } else {
            self.push_from_rax();
            if return_type == b'L' || return_type == b'[' {
                self.mark_top_as_oop();
            }
        }
    }

    /// [`Self::emit_inline_invoke_into_rax`] followed by the result push — the
    /// ordinary, unguarded form.
    fn emit_inline_invoke(&mut self, resolved: &crate::ResolvedInlineInvoke) -> bool {
        if !self.emit_inline_invoke_into_rax(resolved) {
            return false;
        }
        self.push_call_result(resolved.return_type);
        true
    }

    /// The spliced call as a raw `CALL` to the callee's compiled entry — the
    /// same sequence the top-level `direct_calls` arm emits, and the thing that
    /// makes a call-carrying splice worth doing at all.
    ///
    /// Every piece here has a reason the top-level arm already documents, and
    /// two of them are the difference between this and the dispatch form:
    ///
    ///  * **the service copy.** A baked direct call has no dispatch-helper
    ///    frame to recover its arguments from when the callee deopts, so the
    ///    arguments are copied into a contiguous frame range ABOVE the argument
    ///    slots (`reserve_direct_call_service_slots` refuses an overlap — copy
    ///    into the range it is reading and the callee gets arg0 in every slot)
    ///    and `emit_inline_callee_deopt_check` recovers them from there. A
    ///    direct call to a Java callee without one is a compile failure at the
    ///    top level and is refused here too, rather than emitted unserviced.
    ///  * **`emit_post_call_rbp_republish`.** The callee may have re-entered
    ///    the VM and moved the frame record.
    ///
    /// The keep-alive for `resolved.direct_entry` is registered at INTERNING
    /// time (`intern_inline_invoke_targets` -> `_direct_callee_entries`), not
    /// here — the emitter must not be the only thing that knows an address was
    /// baked, because a rolled-back splice would then leave a pin nothing
    /// removes, and a bailed compile would leave one nothing adds.
    fn emit_inline_direct_call(
        &mut self,
        resolved: &crate::ResolvedInlineInvoke,
        info: *const crate::JitInvokeInfo,
        arg_slots: &[super::StackSlot],
        arg_oops: &[bool],
        args_frame_top: i32,
    ) -> bool {
        let Some(service_args_base) =
            self.reserve_direct_call_service_slots(args_frame_top, arg_slots)
        else {
            // No room for the deopt-service copy. Refusing the splice is the
            // only safe answer: an unserviced direct call to a Java callee
            // cannot recover its arguments if the callee deopts.
            return false;
        };
        for (i, slot) in arg_slots.iter().enumerate() {
            self.load_slot_to_reg(R11, *slot);
            let off = service_args_base + ((arg_slots.len() - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
            self.emit_store_local(off, R11);
            // The top-level direct-call arms' channel, verbatim: the service
            // copy is a contiguous frame range written before the CALL and
            // read after it, so each reference in it is named in this call's
            // map and published on the shadow stack by the full spill below.
            if direct_call_arg_maps_enabled() && arg_oops.get(i).copied().unwrap_or(false) {
                self.pending_staged_arg_oops.push(off);
            }
        }
        // `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS=0` restores the unnamed
        // arrangement — and with it the fail-closed flag the top-level arms
        // raise in that configuration, so the map does not claim coverage of a
        // reference it does not name.
        if !direct_call_arg_maps_enabled() && arg_oops.iter().any(|&o| o) {
            self.pending_staged_args_unmapped = true;
        }
        let total_sub = self.emit_stack_arg_setup(arg_slots, resolved.direct_needs_context);
        self.emit_pre_safepoint_spill();
        self.emit_call_absolute(resolved.direct_entry);
        // The return address into THIS artifact -- the key a parent
        // frame's RBP-chain walk reads out of `[rbp+8]`, recorded before
        // the republish/oop-map bytes move the cursor past it. Held in a
        // local because the oop map below has to be filed under this same
        // number and `buf.pos()` will no longer be it by then.
        let return_pc = self.buf.pos();
        record_inline_frame_row(return_pc, self.cur_bc_pc);
        self.emit_post_call_rbp_republish();
        // A direct call to a compiled callee is still a safepoint: the callee
        // may allocate and trigger GC transitively. The caller's operand stack
        // below the splice is covered precisely by its oop marks; the callee
        // locals this splice reserved live in the frame's spill area and are
        // covered by the same conservative frame sweep as every other spill
        // slot — over-approximate, hence pinned by a moving collector.
        let maps_before = self.oop_maps.len();
        self.emit_oop_map_for_safepoint();
        // That map was stamped at `buf.pos()`, which the republish above has
        // already moved 9-20 bytes past the return address a walker keys on.
        // Move the key back onto `return_pc`; the emitted bytes are untouched.
        restamp_call_oop_map_at_return(&mut self.oop_maps, maps_before, return_pc);
        self.emit_stack_arg_cleanup(total_sub);
        self.emit_inline_callee_deopt_check(info, arg_slots.len(), service_args_base);
        true
    }

    /// The spliced call through the blind `jit_invoke_dispatch` helper.
    ///
    /// Only reachable with `CRATONVM_JIT_INLINE_CALL_DISPATCH` on: measured
    /// 2026-08-18, emitting an admitted call this way took the JUnit assertion
    /// chain from 47 to 163-266 ns/iter (`disp_calls` 3 870 -> 2 003 361 over
    /// 2 000 000 iterations), because the call it replaced was already
    /// direct-bound. Kept because it is the arm that reproduces that result.
    ///
    /// Mirrors the top-level `invokestatic` dispatch site: contiguous args
    /// buffer built at the pre-pop spill cursor, four-argument helper call, oop
    /// map for the safepoint.
    fn emit_inline_dispatch_call(
        &mut self,
        info: *const crate::JitInvokeInfo,
        arg_slots: &[super::StackSlot],
        arg_oops: &[bool],
        args_base_offset: i32,
    ) -> bool {
        let num_args = arg_slots.len();
        if num_args > 0 {
            let Some(args_end) = self.checked_spill_range_end(args_base_offset, num_args) else {
                return false;
            };
            self.next_spill_offset = args_end;
            // Descending offsets = ascending addresses, because
            // `modrm_rbp_disp` negates the offset: arg[0] must end up at the
            // LOWEST address for the helper to read the buffer in order.
            for (i, slot) in arg_slots.iter().enumerate() {
                let buf_offset = args_base_offset + ((num_args - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                self.load_slot_to_reg(RAX, *slot);
                self.emit_store_local(buf_offset, RAX);
                // As the top-level dispatch sites do: once popped, the map
                // below is the only thing that can still name this reference.
                if arg_oops.get(i).copied().unwrap_or(false) {
                    self.pending_staged_arg_oops.push(buf_offset);
                }
            }
        }
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.emit_mov_imm64(ARG_REGS[1], info as i64); // Cast: function pointer for JIT call target
        if num_args > 0 {
            let buf_start = args_base_offset + ((num_args as i32) - 1) * 8; // Cast: x86-64 immediate encoding
            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
        } else {
            self.emit_xor_reg_self(ARG_REGS[2]);
        }
        self.emit_mov_imm32_sx(ARG_REGS[3], num_args as i32); // Cast: x86-64 immediate encoding
        self.emit_pre_safepoint_spill();
        // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        self.emit_call_absolute(self.helpers.invoke_dispatch);
        // As the direct arm: the return address is the walk's key.
        let return_pc = self.buf.pos();
        record_inline_frame_row(return_pc, self.cur_bc_pc);
        let maps_before = self.oop_maps.len();
        self.emit_oop_map_for_safepoint();
        // Nothing is emitted between the call and the map here -- there is no
        // republish on this arm -- so on the default path this is provably a
        // no-op that records `already-at-return`. It is kept for two reasons.
        // It is the CONTROL: a census where this arm reads `already` and the
        // direct arm reads `stamped` separates "the fix engaged" from "the
        // instrument fires on everything". And it is not unconditionally a
        // no-op: under `CRATONVM_SHADOW`, `emit_oop_map_for_safepoint` emits
        // the shadow reload BEFORE taking `buf.pos()`, so this arm's map drifts
        // off the return address too, by a different and larger amount.
        restamp_call_oop_map_at_return(&mut self.oop_maps, maps_before, return_pc);
        true
    }

    /// A devirtualised splice inside a splice: guard on the receiver's exact
    /// class, splice the body it dispatches to, and send the miss edge to the
    /// ordinary call.
    ///
    /// Shape, which is PGO-02's one level down:
    ///
    /// ```text
    ///     load receiver (deepest of the call's operands)   ; PEEKED, not popped
    ///     TEST rax, rax ; JZ  miss                          ; null fails every guard
    ///     CMP DWORD [rax+0], guard_class_id ; JNE miss
    ///     <spliced body>                                    ; consumes the operands
    ///     JMP done
    /// miss:
    ///     <ordinary call>                                   ; consumes the same operands
    /// done:
    /// ```
    ///
    /// The receiver is PEEKED because both arms consume the operands
    /// themselves, and the compiler's SYMBOLIC stack is restored between them
    /// so the second arm pops the same slots the first did. Whichever machine
    /// path a given execution takes, the operand stack the emitter goes on to
    /// model is the same — the invariant the top-level guarded-virtual arm
    /// documents at length, and the one a rewind here must not disturb.
    ///
    /// Returns `false` having emitted nothing (the buffer is rewound) when
    /// either arm refuses, so the caller falls through to the unguarded call.
    fn emit_guarded_nested_inline(
        &mut self,
        nested: &crate::NestedInlineSite,
        resolved: &crate::ResolvedInlineInvoke,
    ) -> bool {
        let recv_depth = resolved.num_jit_args;
        // A virtual/interface call always has a receiver; without one there is
        // nothing to guard on and the plan is malformed.
        if recv_depth == 0 || self.stack.len() < recv_depth {
            return false;
        }
        self.flush_scratch_registers();

        let buf_checkpoint = self.buf.pos();
        let stack_checkpoint = self.stack.clone();
        let oop_marks_checkpoint = self.stack_oop_marks.clone();
        // See `try_emit_inline_site`: the scratch-XMM mask rolls back with the
        // stack.
        let scratch_xmm_checkpoint = self.scratch_xmm_in_use;
        let spill_checkpoint = self.next_spill_offset;
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let deopt_points_checkpoint = self.deopt_points.len();
        let safepoint_meta_checkpoint = self.safepoint_meta_checkpoint();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();
        // Round 9 wave 9 (arr9): the late patch lists no rollback used to
        // rewind -- see `LatePatchCheckpoint`.
        let late_patch_checkpoint = self.late_patch_checkpoint();

        // Rows recorded by the hit arm's nested splice. This function
        // rewinds the buffer on three further paths AFTER that splice has
        // succeeded, so its own rollbacks must discard them too.
        let inline_frame_rows_checkpoint = inline_frame_rows_len();
        let recv_slot = self.stack[self.stack.len() - recv_depth];
        self.load_slot_to_reg(RAX, recv_slot);
        self.emit_test_r64_r64(RAX);
        let null_miss = self.emit_jcc_rel32_patch(0x84); // JZ miss
                                                         // The array-receiver guard, exactly as the top-level guarded-virtual
                                                         // chain emits it — see `static_receiver_admits_arrays`.
        let info_ptr = resolved.info_addr as *const crate::JitInvokeInfo;
        let recv_may_be_array = info_ptr.is_null() || {
            // SAFETY: interned into this compile's `_jit_invoke_infos` arena
            // (see `emit_inline_invoke_into_rax`), which outlives the code.
            let info = unsafe { &*info_ptr };
            static_receiver_admits_arrays(info.class_name)
        };
        let kind_miss = if recv_may_be_array {
            //   CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], Object
            self.buf.emit(&[
                0x80,
                0x78,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                cratonvm_types::ObjectKind::Object as u8,
            ]);
            Some(self.emit_jcc_rel32_patch(0x85)) // JNE miss
        } else {
            None
        };
        // CMP DWORD [RAX+0], guard_class_id — the same encoding the top-level
        // guarded-virtual arm and the String/CRC32 intrinsic guards use
        // (81 /7 id, ModRM 0x78 = mod00 /7 rm=RAX).
        self.buf.emit(&[0x81, 0x78, 0x00]);
        self.buf.emit(&nested.guard_class_id.to_le_bytes());
        let class_miss = self.emit_jcc_rel32_patch(0x85); // JNE miss

        if !self.try_emit_nested_inline(&nested.site) {
            self.buf.rewind_to(buf_checkpoint);
            self.stack = stack_checkpoint;
            self.stack_oop_marks = oop_marks_checkpoint;
            self.scratch_xmm_in_use = scratch_xmm_checkpoint;
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            // The three lockstep deopt vectors, AND every by-bci map entry that
            // names a box being dropped -- see `truncate_deopt_points_to`.
            self.truncate_deopt_points_to(deopt_points_checkpoint);
            self.restore_safepoint_meta(safepoint_meta_checkpoint);
            truncate_inline_frame_rows(inline_frame_rows_checkpoint);
            self.forward_patches.truncate(forward_patches_checkpoint);
            self.jump_table_patches
                .truncate(jump_table_patches_checkpoint);
            self.self_call_patches
                .truncate(self_call_patches_checkpoint);
            self.bounds_check_stubs
                .truncate(bounds_check_stubs_checkpoint);
            self.null_check_store_stubs
                .truncate(null_check_store_stubs_checkpoint);
            self.restore_late_patches(late_patch_checkpoint);
            return false;
        }
        // JOIN THE TWO ARMS IN RAX, and push once below.
        //
        // This is the subtle part, and getting it wrong is invisible: the two
        // arms park their result in DIFFERENT frame slots. The spliced body
        // reserved callee locals before popping, so its `ireturn` pushes above
        // them; the call pops and pushes below them. Both are internally
        // consistent, and the emitter's model can only name one — so the other
        // path computes with a slot nothing wrote. Draining the hit arm's value
        // into RAX here makes the two agree on a location the ABI already
        // guarantees, and `push_call_result` after the join is then the single
        // writer of the operand slot the model names.
        //
        // Total over every return kind: `pop_to_rax` handles a Frame,
        // CalleeSaved, Scratch or Xmm slot, and `push_call_result` reverses the
        // Xmm case for a `D`/`F` descriptor.
        if resolved.return_type != b'V' {
            if self.stack.len() != stack_checkpoint.len() - recv_depth + 1 {
                // The body did not leave exactly one value where the call
                // would. Refuse rather than guess which slot is live.
                self.buf.rewind_to(buf_checkpoint);
                self.stack = stack_checkpoint;
                self.stack_oop_marks = oop_marks_checkpoint;
                self.scratch_xmm_in_use = scratch_xmm_checkpoint;
                self.next_spill_offset = spill_checkpoint;
                self.exception_check_stubs
                    .truncate(exception_check_stubs_checkpoint);
                self.deopt_stubs.truncate(deopt_stubs_checkpoint);
                // The three lockstep deopt vectors, AND every by-bci map entry that
                // names a box being dropped -- see `truncate_deopt_points_to`.
                self.truncate_deopt_points_to(deopt_points_checkpoint);
                self.restore_safepoint_meta(safepoint_meta_checkpoint);
                truncate_inline_frame_rows(inline_frame_rows_checkpoint);
                self.forward_patches.truncate(forward_patches_checkpoint);
                self.jump_table_patches
                    .truncate(jump_table_patches_checkpoint);
                self.self_call_patches
                    .truncate(self_call_patches_checkpoint);
                self.bounds_check_stubs
                    .truncate(bounds_check_stubs_checkpoint);
                self.null_check_store_stubs
                    .truncate(null_check_store_stubs_checkpoint);
                self.restore_late_patches(late_patch_checkpoint);
                return false;
            }
            self.pop_to_rax();
        }
        let done = self.emit_jmp_rel32_patch();

        self.patch_rel32_to_here(null_miss);
        if let Some(kind_miss) = kind_miss {
            self.patch_rel32_to_here(kind_miss);
        }
        self.patch_rel32_to_here(class_miss);
        self.stack = stack_checkpoint.clone();
        self.stack_oop_marks = oop_marks_checkpoint.clone();
        // The miss edge starts from the checkpoint's stack, so from its mask
        // too -- not from whatever the hit arm's body left allocated.
        self.scratch_xmm_in_use = scratch_xmm_checkpoint;
        self.next_spill_offset = spill_checkpoint;
        // THIS miss edge needs no poison row, unlike the TOP-LEVEL guarded
        // site's (see `record_inline_frame_miss_edge_row`), and the reason is
        // worth writing down because the two look identical from a distance.
        //
        // This one is emitted from INSIDE a spliced body, so the scope stack
        // is not empty here: `try_emit_nested_inline` popped the hit arm's
        // scope, leaving the ENCLOSING splice's, whose `cur_pc` still names
        // this invoke (`set_inline_frame_scope_pc` is not clobbered by a
        // nested walk -- that is why it exists). `emit_inline_invoke_into_rax`
        // therefore reaches `record_inline_frame_row` and records a REAL row
        // for this program point, describing the enclosing chain, which is the
        // correct answer for it. That row is strictly shorter than any row the
        // hit arm recorded -- the hit arm's chains carry the nested callee on
        // top of the same enclosing levels -- so the two disagree under one
        // bci and `from_rows` poisons it already.
        //
        // The remaining case, a hit arm that recorded no row at all (a leaf
        // callee with no calls of its own), leaves this row alone under the
        // bci. It names the ENCLOSING splice, which did run; a frame suspended
        // inside the leaf body would be reported one level short. That is a
        // MISSING frame, not a fabricated one, and it is what the map already
        // does everywhere it has no row.
        if !self.emit_inline_invoke_into_rax(resolved) {
            // The miss edge cannot be emitted, so the guard has nowhere to land
            // and the whole construct is unusable. Rewind everything, including
            // the hit body, and let the caller take the plain call.
            self.buf.rewind_to(buf_checkpoint);
            self.stack = stack_checkpoint;
            self.stack_oop_marks = oop_marks_checkpoint;
            self.scratch_xmm_in_use = scratch_xmm_checkpoint;
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            // The three lockstep deopt vectors, AND every by-bci map entry that
            // names a box being dropped -- see `truncate_deopt_points_to`.
            self.truncate_deopt_points_to(deopt_points_checkpoint);
            self.restore_safepoint_meta(safepoint_meta_checkpoint);
            truncate_inline_frame_rows(inline_frame_rows_checkpoint);
            self.forward_patches.truncate(forward_patches_checkpoint);
            self.jump_table_patches
                .truncate(jump_table_patches_checkpoint);
            self.self_call_patches
                .truncate(self_call_patches_checkpoint);
            self.bounds_check_stubs
                .truncate(bounds_check_stubs_checkpoint);
            self.null_check_store_stubs
                .truncate(null_check_store_stubs_checkpoint);
            self.restore_late_patches(late_patch_checkpoint);
            return false;
        }
        self.patch_rel32_to_here(done);

        // One push, after the join, from the one location both arms agree on.
        // The operand stack the emitter goes on to model is therefore the same
        // whichever machine path an execution took — which is the property the
        // whole construct rests on, and the one a later merge point would
        // expose if it did not hold.
        self.push_call_result(resolved.return_type);
        true
    }

    /// Splice a body into a body: `try_emit_inline_site`'s rollback contract,
    /// for a call site whose pc lives in a CALLEE's bytecode space.
    ///
    /// Two things differ from the outer wrapper, both because the pc is a
    /// callee pc:
    ///
    ///  * no `push_inline_scope`. A scope is built by `build_frame_state_at`,
    ///    which reads the ENCLOSING method's locals at a bci — and a callee pc
    ///    indexes a different bytecode. There is no honest frame to record
    ///    here, so none is recorded.
    ///
    ///    **M1 closed half of that; round 10 wave 7 closed the other half, and
    ///    what keeps this rule in place is now a different, smaller thing.**
    ///    `current_bytecode_owner` / `resume_bci_for` give a point published
    ///    inside a splice the right IDENTITY and the right BCI SPACE, and
    ///    `build_frame_state_at` now builds its CONTENTS to the callee's own
    ///    geometry instead of reading the enclosing method's `local_liveness` /
    ///    `local_kinds` / `sr_local_prov_at` at a foreign pc. So a scope built
    ///    here would no longer be wrong about its contents — it would be
    ///    `[Unsupported; max_locals]`, i.e. an honest "cannot be described".
    ///
    ///    What is still missing at THIS level is the geometry itself: the
    ///    enclosing splice's `InlineCalleeScope` is the one on the stack, so a
    ///    frame captured here would be sized to the OUTER callee's `max_locals`
    ///    and measured against the OUTER callee's stack floor. Recording one
    ///    means pushing this level's own scope, and the reason not to is
    ///    unchanged: nothing here has a use for it, and an unused scope on the
    ///    stack is a scope `inline_caller_chain` would attach to points that
    ///    are not inside this body.
    ///  * consequently the postcondition stays the STRICT one the outer
    ///    wrapper used before inline scopes existed: any deopt metadata at all
    ///    refuses the nested splice. Nothing on this path publishes (the
    ///    invoke arm deliberately omits `snapshot_pre_intrinsic_call`, precise
    ///    exception frames and inlining are mutually exclusive, and every
    ///    trap-carrying opcode is refused by the resolver), so the rule is
    ///    inert — which is exactly why it is cheap to keep as a ratchet
    ///    against a future edit that starts publishing.
    ///
    /// A `false` return leaves the emitter byte-identical to before the
    /// attempt, so the invoke arm falls through to the ordinary dispatch call.
    fn try_emit_nested_inline(&mut self, site: &crate::InlineSite) -> bool {
        let buf_checkpoint = self.buf.pos();
        let stack_checkpoint = self.stack.clone();
        let oop_marks_checkpoint = self.stack_oop_marks.clone();
        // See `try_emit_inline_site`: the scratch-XMM mask rolls back with the
        // stack.
        let scratch_xmm_checkpoint = self.scratch_xmm_in_use;
        let spill_checkpoint = self.next_spill_offset;
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let deopt_points_checkpoint = self.deopt_points.len();
        let safepoint_meta_checkpoint = self.safepoint_meta_checkpoint();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();
        // Round 9 wave 9 (arr9): the late patch lists no rollback used to
        // rewind -- see `LatePatchCheckpoint`.
        let late_patch_checkpoint = self.late_patch_checkpoint();

        // `try_emit_inline_body` reads no per-pc state off `self` for the
        // OUTER pc it is handed — it uses it only for the `CRATONVM_DBG_JITC`
        // trace — so passing the enclosing splice's pc keeps that trace
        // pointing at the caller-visible call site.
        let outer_pc = self.dbg_last_pc;
        // The inline-frame scope for this level. `entry_bci` is the
        // ENCLOSING callee's pc -- `inline_walk_at.0`, read HERE, before
        // `try_emit_inline_body` overwrites it and does not restore it.
        let inline_frame_rows_checkpoint = inline_frame_rows_len();
        push_inline_frame_scope(
            inline_site_label(site),
            site.class_id,
            self.inline_walk_at.0,
        );
        // The callee-local oop scope `try_emit_inline_body` pushes lives
        // exactly as long as this nested body is being emitted -- the same
        // contract `try_emit_inline_site` keeps for a top-level splice, and
        // for the same reasons, on BOTH exits.
        //
        // This wrapper used to leave it behind. Every scope a nested splice
        // pushed then stayed open for the rest of the ENCLOSING body, and
        // three things read that stack as "the splices that are open now":
        //
        //  * `inline_locals_clear_of_range` exempts only the LAST scope as
        //    "the one that is returning". With a leaked inner scope on top, the
        //    enclosing splice's OWN locals read as an enclosing scope's live
        //    locals when it returns, and its result -- which lands on its own
        //    local 0 whenever `caller_post_pop_spill == callee_local_base` --
        //    was bumped one word up. The cursor is then restored to
        //    `caller_post_pop_spill + 8`, which IS that word, so the next push
        //    overwrote the result; and at a receiver-guarded site the miss
        //    edge's dispatch parks its result at the unbumped word, so the two
        //    arms of the guard disagreed about where the value lives.
        //  * `scope.cur_pc = cpc` goes to `last_mut()`, i.e. to the dead inner
        //    scope, so the enclosing body's safepoints read its oop-local
        //    masks at a stale pc.
        //  * the safepoint oop-map builders walk every scope, so dead inner
        //    locals kept being published as roots.
        //
        // Measured on netty `HttpPostMultipartRequestDecoder.
        // loadDataMultipartOptimized`: `delimiter.getBytes(data.getCharset())`
        // with `getCharset()` guard-spliced for `MixedAttribute`, whose body
        // nests `AbstractMixedHttpData.getCharset()` and, inside that, a
        // guarded `wrapped.getCharset()`. The hit arm left the receiver copy in
        // the word the continuation read, and `String.getBytes` was handed the
        // `MixedAttribute` as its `Charset`: `NoSuchMethodError:
        // MixedAttribute.newEncoder()`.
        let oop_scope_checkpoint = self.inline_oop_scopes.len();
        let inline_ok = self.try_emit_inline_body(outer_pc, site);
        self.inline_oop_scopes.truncate(oop_scope_checkpoint);
        pop_inline_frame_scope();
        let published = self.deopt_stubs.len() > deopt_stubs_checkpoint
            || self.deopt_points.len() > deopt_points_checkpoint;
        if inline_ok && !published {
            return true;
        }
        self.buf.rewind_to(buf_checkpoint);
        self.stack = stack_checkpoint;
        self.stack_oop_marks = oop_marks_checkpoint;
        self.scratch_xmm_in_use = scratch_xmm_checkpoint;
        self.next_spill_offset = spill_checkpoint;
        self.exception_check_stubs
            .truncate(exception_check_stubs_checkpoint);
        self.deopt_stubs.truncate(deopt_stubs_checkpoint);
        // The three lockstep deopt vectors, AND every by-bci map entry that
        // names a box being dropped -- see `truncate_deopt_points_to`.
        self.truncate_deopt_points_to(deopt_points_checkpoint);
        self.restore_safepoint_meta(safepoint_meta_checkpoint);
        truncate_inline_frame_rows(inline_frame_rows_checkpoint);
        self.forward_patches.truncate(forward_patches_checkpoint);
        self.jump_table_patches
            .truncate(jump_table_patches_checkpoint);
        self.self_call_patches
            .truncate(self_call_patches_checkpoint);
        self.bounds_check_stubs
            .truncate(bounds_check_stubs_checkpoint);
        self.null_check_store_stubs
            .truncate(null_check_store_stubs_checkpoint);
        self.restore_late_patches(late_patch_checkpoint);
        false
    }
}

#[cfg(test)]
mod splice_postcondition_tests {
    use super::splice_point_is_misidentified;
    use crate::deopt::{
        DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, ResumeSemantics,
    };

    const CALLER: &str = "Caller.run:()V";
    const CALLEE: &str = "Callee.leaf:(I)I";
    const NESTED: &str = "Nested.deeper:()V";

    /// The spliced callee's `code_len`, as `InlineSite::callee_code_len` reports
    /// it. Every `frame` below sits at bci 3, which is inside it, so the
    /// bci-range clause is satisfied unless a test says otherwise — the
    /// identity-shaped assertions are then about identity only.
    const CALLEE_CODE_LEN: usize = 8;

    fn frame(method_key: &str, caller: Option<Box<FrameState>>) -> FrameState {
        FrameState {
            method_key: method_key.to_string(),
            bci: 3,
            locals: vec![FrameValue::Int(1)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller,
        }
    }

    fn point(frame_state: FrameState) -> DeoptimizationPoint {
        DeoptimizationPoint {
            native_offset: 0x20,
            bci: frame_state.bci,
            reason: DeoptReason::BoundsCheck,
            action: DeoptAction::Reinterpret,
            speculation_id: 0,
            semantics: ResumeSemantics::REEXECUTE,
            frame_state,
        }
    }

    /// A point with no caller scope does not say it came from inside a splice.
    /// This is the case the old check caught, and it still refuses.
    #[test]
    fn a_point_with_no_caller_scope_is_refused() {
        assert!(splice_point_is_misidentified(
            &point(frame(CALLEE, None)),
            CALLEE,
            CALLER,
            CALLEE_CODE_LEN
        ));
    }

    /// The case the old check MISSED, and the one the single-pass backend
    /// actually produces: a caller chain is present, so the old rule admitted
    /// it, while the innermost frame still names the enclosing method at a bci
    /// that belongs to the callee.
    #[test]
    fn a_caller_scoped_point_naming_the_caller_is_still_refused() {
        let p = point(frame(CALLER, Some(Box::new(frame(CALLER, None)))));
        assert!(
            splice_point_is_misidentified(&p, CALLEE, CALLER, CALLEE_CODE_LEN),
            "a caller chain says there is an outer frame; it says nothing about \
             whether the innermost one describes the callee"
        );
    }

    /// What the check is meant to admit, once `build_frame_state_at` can stamp
    /// the callee's identity: an innermost frame naming the callee, under a
    /// caller scope naming the enclosing method.
    #[test]
    fn a_callee_identified_point_under_a_caller_scope_is_admitted() {
        let p = point(frame(CALLEE, Some(Box::new(frame(CALLER, None)))));
        assert!(!splice_point_is_misidentified(
            &p,
            CALLEE,
            CALLER,
            CALLEE_CODE_LEN
        ));
    }

    /// A point a NESTED splice published names the inner callee, and the outer
    /// splice's own postcondition must not refuse it for that.
    ///
    /// The outer check scans every point the outer body added, which includes
    /// the inner body's. Requiring all of them to name the OUTER callee would
    /// make nesting and publishing mutually exclusive, and would buy nothing:
    /// the inner splice held that point to this same rule with its own callee
    /// key a moment earlier.
    #[test]
    fn a_nested_splices_point_is_admitted_by_the_enclosing_splice() {
        let p = point(frame(
            NESTED,
            Some(Box::new(frame(CALLEE, Some(Box::new(frame(CALLER, None)))))),
        ));
        assert!(
            !splice_point_is_misidentified(&p, NESTED, CALLER, CALLEE_CODE_LEN),
            "the inner splice admits its own point"
        );
        assert!(
            !splice_point_is_misidentified(&p, CALLEE, CALLER, CALLEE_CODE_LEN),
            "and the outer splice must not refuse it for naming the inner callee"
        );
    }

    /// An empty key is the legacy-producer / superseded-guard-sentinel shape,
    /// which `deopt_frame_matches_method` refuses outright. A splice must not
    /// publish one either: it identifies nothing, so it cannot be checked
    /// against anything.
    #[test]
    fn a_point_with_an_empty_method_key_is_refused() {
        let p = point(frame("", Some(Box::new(frame(CALLER, None)))));
        assert!(splice_point_is_misidentified(
            &p,
            CALLEE,
            CALLER,
            CALLEE_CODE_LEN
        ));
    }

    /// The CONVERSE malformed pair, and the one the emitter would actually
    /// produce first (round 10 wave 7): the callee's identity on an
    /// ENCLOSING-method bci.
    ///
    /// `emit_post_invoke_exception_check` and friends key their reason-9 frame on
    /// `Compiler::dbg_last_pc`, which only the OUTER walk assigns, so inside a
    /// splice the pc is the caller's invoke while `current_bytecode_owner` stamps
    /// the callee. bci 40 is past the end of an 8-byte callee, which is what that
    /// looks like whenever the enclosing method is the larger of the two — the
    /// usual case, since the callee was inlined into it.
    #[test]
    fn a_callee_identified_point_at_an_enclosing_bci_is_refused() {
        let mut fs = frame(CALLEE, Some(Box::new(frame(CALLER, None))));
        fs.bci = 40;
        let mut p = point(fs);
        // `build_and_record_deopt_point` publishes one `resume_bci_for` result
        // into both fields; keep the fixture's two in step so this test is about
        // the range clause and not about a shape no producer emits.
        p.bci = 40;
        assert!(
            splice_point_is_misidentified(&p, CALLEE, CALLER, CALLEE_CODE_LEN),
            "a bci outside the callee's own code cannot be a callee pc, whatever \
             the key says"
        );
        // The control: the same point inside the callee's code is admitted, so
        // the clause is a range test and not a blanket refusal of every
        // callee-identified point.
        let mut inside = point(frame(CALLEE, Some(Box::new(frame(CALLER, None)))));
        inside.bci = inside.frame_state.bci;
        assert!(!splice_point_is_misidentified(
            &inside,
            CALLEE,
            CALLER,
            CALLEE_CODE_LEN
        ));
    }

    /// The enclosing owner of a NESTED splice is a CALLEE, not the compiling
    /// method, and the check is handed that key rather than
    /// `Compiler::method_key`.
    ///
    /// Without this the nested case has no guard at all: a point stamped with
    /// the ENCLOSING CALLEE's key at an inner-callee bci is the same malformed
    /// pair one level down, and comparing it against the root's key admits it.
    /// No splice nests through `try_emit_inline_site` today
    /// (`try_emit_nested_inline` pushes no scope and refuses any publication),
    /// so this pins the shape the check must keep refusing if one ever does.
    #[test]
    fn the_enclosing_callee_is_refused_for_a_nested_splice() {
        let p = point(frame(
            CALLEE,
            Some(Box::new(frame(CALLEE, Some(Box::new(frame(CALLER, None)))))),
        ));
        assert!(
            splice_point_is_misidentified(&p, NESTED, CALLEE, CALLEE_CODE_LEN),
            "the enclosing owner of a nested splice is the outer CALLEE; naming it \
             is the caller-named/callee-bci'd pair one level down"
        );
        assert!(
            !splice_point_is_misidentified(&p, NESTED, CALLER, CALLEE_CODE_LEN),
            "and comparing against the ROOT method's key — what this check was \
             handed before wave 7 — admits it, which is why the enclosing owner \
             is read from `current_bytecode_owner`"
        );
    }
}

#[cfg(test)]
impl Compiler {
    /// Test-only injection point for the splice deopt-metadata postcondition.
    ///
    /// A guard nobody can make fire is a guard nobody has tested. This lets
    /// `inline_publishing_a_deopt_point_is_refused` produce the one state the
    /// check exists to catch — a spliced body that published deopt metadata —
    /// without waiting for a future emitter change to produce it accidentally.
    pub(super) fn force_inline_deopt_publication(&mut self) {
        self.deopt_stubs.push((self.buf.pos(), 0, 0));
    }

    /// The test twin of the production no-op `inline_test_publish_hook`.
    fn inline_test_publish_hook(&mut self) {
        if super::INLINE_TEST_PUBLISHES_DEOPT.with(std::cell::Cell::get) {
            self.force_inline_deopt_publication();
        }
    }
}
