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
    cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP").is_some()
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
    cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR").is_some()
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
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_LOCALS_FLOOR").is_none() {
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
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_CALL_MAP_AT_RETURN").is_some()
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
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OOPCOV").is_some() {
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
//     a real chain, IS retained on the artifact, and IS PC-indexed
//     (`find_deopt_point`). Two facts kill it. First, every level carries
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
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON").is_some()
    })
}

/// Cached `CRATONVM_DBG_JITC`, for the one site below that is reached on a
/// THROW rather than at compile time. Re-reading the environment during a
/// stack walk would be the only runtime cost this change has. The key is
/// REUSED, not minted -- this file already prints its splice decisions under
/// it -- so the flag surface grows by exactly one name.
fn inline_frame_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some())
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
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_NPE_TRAP_LINES").is_none()
    })
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
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_FRAME_MAP").is_none()
    })
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
        let buf_checkpoint = self.buf.pos();
        let stack_checkpoint = self.stack.clone();
        let oop_marks_checkpoint = self.stack_oop_marks.clone();
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
        // The scope for this splice — the ENCLOSING method's frame at the
        // invoke, captured before the callee's body is emitted. See
        // `push_inline_scope`; popped on BOTH exits below, because a scope left
        // behind by a bailed splice would attach a caller frame to every later
        // point in the enclosing method.
        self.push_inline_scope(pc, site.callee_num_args);
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
        // Every inlined body — statically bound or behind a receiver guard —
        // is entered and left inside ONE frame, the caller's own, and deopt
        // metadata has no way to say otherwise: `deopt::FrameState::caller`
        // exists but no producer populates it, so an inlined scope is not
        // representable. A deopt point published from inside a spliced body
        // would therefore name the CALLER's method with the CALLEE's bci — a
        // well-formed description of a stack that never existed, which is the
        // exact failure class the 2026-08-01 deopt-metadata audit found three
        // of.
        //
        // The safety argument used to be a claim about the source ("the
        // emitter contains no `build_and_record_deopt_point` on this path").
        // That claim is one future edit away from being false, and nothing
        // would fail when it became false. Check the postcondition instead.
        //
        // **What it checks changed 2026-08-18; why it exists did not.** The old
        // check refused any published metadata at all, because an inlined scope
        // was not representable — `FrameState::caller` had no producer, so a
        // point published here would have named the CALLER's method with the
        // CALLEE's bci. There is a producer now (`push_inline_scope`), so the
        // question is no longer "was anything published" but "does what was
        // published SAY it came from inside a splice". A point without a caller
        // scope is exactly the malformed frame the old rule was protecting
        // against, so that one still refuses.
        //
        // Checked over the points this body added, not over the whole vector:
        // an enclosing method's own earlier points legitimately have no caller
        // scope, and scanning them would refuse every splice in any method that
        // publishes anything at all.
        let published_unscoped_point = self.deopt_points[deopt_points_checkpoint..]
            .iter()
            .any(|p| p.frame_state.caller.is_none());
        // A raw stub with no matching point is metadata this check cannot read,
        // and it is what `force_inline_deopt_publication` injects. Refuse it as
        // before rather than assume it is well-formed.
        let published_unreadable_stub = self.deopt_stubs.len() > deopt_stubs_checkpoint
            && self.deopt_points.len() == deopt_points_checkpoint;
        let published_deopt_metadata = published_unscoped_point || published_unreadable_stub;
        // Is this population non-empty? A relaxation nobody can see is
        // indistinguishable from no relaxation, and the only splices affected
        // are the ones that publish — which the old rule refused, so there is
        // no prior count of them anywhere. Named per splice under
        // `CRATONVM_DBG_JITC` rather than guessed at.
        if (self.deopt_points.len() > deopt_points_checkpoint || published_unreadable_stub)
            && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some()
        {
            eprintln!(
                "[cratonvm-jitc] inline-splice {}.{}{} at pc={pc} published {} point(s): {}",
                site.class_name,
                site.method_name,
                site.descriptor,
                self.deopt_points.len() - deopt_points_checkpoint,
                if published_deopt_metadata {
                    "REFUSED (a point with no caller scope)"
                } else {
                    "admitted, every point carries its caller scope"
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
            self.deopt_points.truncate(deopt_points_checkpoint);
            truncate_inline_frame_rows(inline_frame_rows_checkpoint);
            crate::metrics::note_inline_call_arm(6);
            // Name the rollback. The count alone ("outer-splice-rolled-back=1")
            // says a planned splice was thrown away without saying by what, and
            // that has stood as an open question on the netty exhaustive-loop
            // pages since 2026-08-18.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
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

    /// Test-only injection point for the deopt-metadata postcondition above.
    ///
    /// A guard nobody can make fire is a guard nobody has tested. This lets
    /// `inline_publishing_a_deopt_point_is_refused` produce the one state the
    /// check exists to catch — a spliced body that published deopt metadata —
    /// without waiting for a future emitter change to produce it accidentally.
    #[cfg(test)]
    pub(super) fn force_inline_deopt_publication(&mut self) {
        self.deopt_stubs.push((self.buf.pos(), 0, 0));
    }

    /// Inline-emission body. MUST only be called via [`Self::try_emit_inline`],
    /// which snapshots and restores compiler state around it. A `false`
    /// return from anywhere inside is safe precisely because of that
    /// wrapper — the bail sites here therefore no longer need to unwind
    /// `next_spill_offset` by hand.
    fn try_emit_inline_body(&mut self, pc: usize, site: &crate::InlineSite) -> bool {
        let site = site.clone();
        #[cfg(test)]
        if super::INLINE_TEST_PUBLISHES_DEOPT.with(std::cell::Cell::get) {
            self.force_inline_deopt_publication();
        }

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
        let callee_branch_targets = compute_branch_targets(callee_code, callee_len);
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
                        _ => unreachable!(),
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
                        _ => unreachable!(),
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
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag, cpc);
                        crate::metrics::note_getfield_arm(0);
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
                                    self.emit_call_absolute(self.helpers.putfield_object);
                                }
                            } else {
                                super::note_ungated_ref_store();
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                self.emit_call_absolute(self.helpers.putfield_object);
                            }
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
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
                            self.emit_call_absolute(self.helpers.getstatic);
                            self.emit_oop_map_for_safepoint();
                            // jit-linewrapper-flushtype-npe fix (2026-07-17):
                            // see the matching fix + comment at the top-level
                            // 0xb2 arm -- same helper, same missing
                            // post-invoke exception check for a `<clinit>`
                            // failure surfaced via the deopt sentinel.
                            self.emit_post_invoke_exception_check(type_tag);
                            // Volatile static: emit MFENCE after read (SeqCst acquire)
                            if is_volatile {
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
                        // `<clinit>` on first touch: a safepoint. See the
                        // top-level 0xb2 arm.
                        self.emit_pre_safepoint_spill();
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
        let mut arg_slots = Vec::with_capacity(num_args);
        for _ in 0..num_args {
            arg_slots.push(self.pop_stack());
        }
        arg_slots.reverse();
        let post_pop_spill = self.next_spill_offset;

        if resolved.direct_entry != 0 {
            if !self.emit_inline_direct_call(resolved, info, &arg_slots, pre_pop_spill) {
                return false;
            }
            crate::metrics::note_inline_call_arm(0);
        } else if !self.emit_inline_dispatch_call(info, &arg_slots, pre_pop_spill) {
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
        let spill_checkpoint = self.next_spill_offset;
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let deopt_points_checkpoint = self.deopt_points.len();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();

        // Rows recorded by the hit arm's nested splice. This function
        // rewinds the buffer on three further paths AFTER that splice has
        // succeeded, so its own rollbacks must discard them too.
        let inline_frame_rows_checkpoint = inline_frame_rows_len();
        let recv_slot = self.stack[self.stack.len() - recv_depth];
        self.load_slot_to_reg(RAX, recv_slot);
        self.emit_test_r64_r64(RAX);
        let null_miss = self.emit_jcc_rel32_patch(0x84); // JZ miss
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
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            self.deopt_points.truncate(deopt_points_checkpoint);
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
                self.next_spill_offset = spill_checkpoint;
                self.exception_check_stubs
                    .truncate(exception_check_stubs_checkpoint);
                self.deopt_stubs.truncate(deopt_stubs_checkpoint);
                self.deopt_points.truncate(deopt_points_checkpoint);
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
                return false;
            }
            self.pop_to_rax();
        }
        let done = self.emit_jmp_rel32_patch();

        self.patch_rel32_to_here(null_miss);
        self.patch_rel32_to_here(class_miss);
        self.stack = stack_checkpoint.clone();
        self.stack_oop_marks = oop_marks_checkpoint.clone();
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
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            self.deopt_points.truncate(deopt_points_checkpoint);
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
        let spill_checkpoint = self.next_spill_offset;
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let deopt_points_checkpoint = self.deopt_points.len();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();

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
        let inline_ok = self.try_emit_inline_body(outer_pc, site);
        pop_inline_frame_scope();
        let published = self.deopt_stubs.len() > deopt_stubs_checkpoint
            || self.deopt_points.len() > deopt_points_checkpoint;
        if inline_ok && !published {
            return true;
        }
        self.buf.rewind_to(buf_checkpoint);
        self.stack = stack_checkpoint;
        self.stack_oop_marks = oop_marks_checkpoint;
        self.next_spill_offset = spill_checkpoint;
        self.exception_check_stubs
            .truncate(exception_check_stubs_checkpoint);
        self.deopt_stubs.truncate(deopt_stubs_checkpoint);
        self.deopt_points.truncate(deopt_points_checkpoint);
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
        false
    }
}
