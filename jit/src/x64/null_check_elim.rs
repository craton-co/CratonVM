// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Null-check elimination.
//!
//! Moved verbatim out of `x64.rs`'s `HIGH-1 / Fix 1 — null-check elimination helper`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

/// [`preceding_aload_nonnull_local`] against a caller-supplied
/// instruction-start map. Identical answer for every input; the only
/// difference is who paid for `starts`.
///
/// # `starts` is a parameter, not a cache
///
/// The implementation this replaces built the map inline and defended that
/// with: "not worth a pointer-keyed cache (which would risk an ABA stale-map
/// hit)". The refusal was correct and is not being reversed — a map keyed by
/// `code.as_ptr()` is unsound here, because a `Vec<u8>` of bytecode can be
/// dropped and a *different* method's bytecode allocated at the same address
/// within one compile (the loop-rewrite path in `x64/licm.rs` hands the walk a
/// freshly-built `Vec<u8>` that replaces the original), so a pointer hit does
/// not imply a content hit. A stale map is a mis-located `aload`, and a
/// mis-located `aload` is the miscompile the SOUNDNESS FIX above documents.
///
/// A parameter is not that. There is no key, no lookup, no hit-or-miss and
/// therefore no ABA question to get wrong: the caller holds a `&[bool]` that
/// borrows for as long as it is used, and the borrow checker proves the `code`
/// it was derived from is the same `code` still in scope. The only way to pass
/// a wrong map is to build it from a different array *by hand*, which is a
/// visible, local mistake rather than an invisible aliasing one — and the
/// bound checks below turn even that into `None` rather than a panic.
///
/// # The map must be the one the convenience form would have built
///
/// `starts` must be `bytecode_analysis::instruction_starts(code, code.len())`
/// for the same `code`. Two traps:
///
/// * `Compiler::walk_control` (and its siblings) take a `code_len` parameter
///   that is threaded from `jit_compile`'s caller and is NOT guaranteed to
///   equal `code.len()`. Building the map with that value instead would give a
///   map of a different length over the same bytes, and a *shorter* one turns
///   provable sites into `None` (a silent de-optimisation) while a longer one
///   would index past the slice inside `instruction_starts` itself. Pass
///   `code.len()`.
/// * The map is position-dependent, so it cannot be shared across the
///   pre-rewrite and post-rewrite bytecode of a loop transform. `x64/licm.rs`
///   already treats those as two different arrays with two different maps.
///
/// # Bound checks
///
/// Every index into `starts` goes through `get`, so a short map yields `None`
/// instead of a panic. This is not theoretical tidiness: these helpers run on
/// a compile thread under the module's `deny(clippy::panic)` gate, where an
/// out-of-range index is a process-wide abort, not a failed optimisation.
/// The `None` is always the conservative answer — the caller keeps its runtime
/// check — so the degraded mode is slower code, never wrong code.
pub(super) fn preceding_aload_nonnull_local_with_starts(
    code: &[u8],
    pc: usize,
    starts: &[bool],
) -> Option<usize> {
    if pc == 0 {
        return None;
    }
    let code_len = code.len();
    if pc > code_len {
        return None;
    }
    // aload_0..aload_3 — 1-byte opcode; only trust it if the map confirms
    // `pc - 1` is a real instruction start (aload_0..3 are always exactly
    // 1 byte long, so a genuine start there necessarily ends at `pc`).
    if starts.get(pc - 1).copied().unwrap_or(false) {
        let prev1 = code[pc - 1];
        if (0x2A..=0x2D).contains(&prev1) {
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            return Some((prev1 - 0x2A) as usize);
        }
    }
    // aload <u8> — 2-byte instruction; only trust it if the map confirms
    // `pc - 2` is a real instruction start (aload <u8> is always exactly 2
    // bytes, so a genuine start there necessarily ends at `pc`).
    if pc >= 2 && starts.get(pc - 2).copied().unwrap_or(false) && code[pc - 2] == 0x19 {
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        return Some(code[pc - 1] as usize);
    }
    None
}

/// JEP 358 (helpful NPE) inline-codegen path — map the array load/store
/// opcode at `code[bc_pc]` to its [`npe_action`] code, so the inline
/// null-check failure stub can attach the right HotSpot-style action-only
/// message ("Cannot load from int array", "Cannot store to char array", …).
///
/// `baload`/`bastore` cannot distinguish `byte[]` from `boolean[]` at the
/// null site (same opcode), so both map to the byte action — matching the
/// per-type JIT helpers. An unexpected opcode (the function is only called
/// from array-load/store arms, so this is defensive) maps to
/// [`npe_action::NONE`] (unmessaged), never a wrong message.
pub(super) fn array_opcode_npe_action(code: &[u8], bc_pc: usize) -> u8 {
    match code.get(bc_pc).copied() {
        // Loads: iaload laload faload daload aaload baload caload saload
        Some(0x2e) => npe_action::ALOAD_INT,
        Some(0x2f) => npe_action::ALOAD_LONG,
        Some(0x30) => npe_action::ALOAD_FLOAT,
        Some(0x31) => npe_action::ALOAD_DOUBLE,
        Some(0x32) => npe_action::ALOAD_OBJECT,
        Some(0x33) => npe_action::ALOAD_BYTE,
        Some(0x34) => npe_action::ALOAD_CHAR,
        Some(0x35) => npe_action::ALOAD_SHORT,
        // Stores: iastore lastore fastore dastore aastore bastore castore sastore
        Some(0x4f) => npe_action::ASTORE_INT,
        Some(0x50) => npe_action::ASTORE_LONG,
        Some(0x51) => npe_action::ASTORE_FLOAT,
        Some(0x52) => npe_action::ASTORE_DOUBLE,
        Some(0x53) => npe_action::ASTORE_OBJECT,
        Some(0x54) => npe_action::ASTORE_BYTE,
        Some(0x55) => npe_action::ASTORE_CHAR,
        Some(0x56) => npe_action::ASTORE_SHORT,
        _ => npe_action::NONE,
    }
}

/// [`array_receiver_local`] against a caller-supplied instruction-start map.
///
/// `starts` must be `bytecode_analysis::instruction_starts(code, code.len())`
/// for this same `code` — the exact map [`array_receiver_local`] would have
/// built. [`preceding_aload_nonnull_local_with_starts`] documents the two ways
/// to get that wrong and why the answer is a parameter rather than a cache.
pub(super) fn array_receiver_local_with_starts(
    code: &[u8],
    pc: usize,
    starts: &[bool],
) -> Option<usize> {
    array_receiver_with_starts(code, pc, starts).map(|(local, _)| local)
}

/// [`array_receiver`] against a caller-supplied instruction-start map.
///
/// The map is the whole soundness argument of this function (see the
/// SOUNDNESS FIX note in the body), so the contract on `starts` is not
/// advisory: it must be `bytecode_analysis::instruction_starts(code,
/// code.len())` for this same `code`. Handing over a map built from different
/// bytes is the mis-located-`aload` miscompile again, arrived at by a
/// different route. See [`preceding_aload_nonnull_local_with_starts`] for the
/// parameter-is-not-a-cache argument and for the `code_len`-vs-`code.len()`
/// trap; every index into `starts` below goes through `get`, so a short or
/// absent map degrades to `None` — the conservative answer — rather than
/// panicking on a compile thread.
///
/// # Loads only, in practice
///
/// The decode is `aload N; <index push>; <access>`, the operand order of an
/// array LOAD. A store's stack is `[arrayref, index, value]`, so the pair
/// before a store is `<index push>; <value push>` and the match would need an
/// `aload` to push the `int` index, which no verified method does: for every
/// store opcode this answers `None`. Stores have their own decoder,
/// [`array_store_receiver_with_starts`] (round 11 wave 2, lane x64loop;
/// `docs/feature-designs/jit-ir-optimizations.md`).
/// The `dup` entry in the index-push set is dead for the same reason.
pub(super) fn array_receiver_with_starts(
    code: &[u8],
    pc: usize,
    starts: &[bool],
) -> Option<(usize, usize)> {
    if pc == 0 {
        return None;
    }
    let code_len = code.len();
    if pc >= code_len {
        return None;
    }
    // SOUNDNESS FIX (array_receiver_local): the previous implementation guessed
    // instruction boundaries by reading `code[pc - 1]` / `code[pc - 2]` /
    // `code[pc - 3]` and matching them against opcode values. That is unsound:
    // for a multi-byte index push (sipush/iload-u8/bipush) the byte at `pc - 1`
    // is an OPERAND, not an opcode, and when that operand byte happens to equal
    // a single-byte index opcode (e.g. an `sipush` low byte of 0x03 looks like
    // `iconst_0`) the receiver `aload` is mis-located → the inline null check is
    // wrongly elided → SIGSEGV on a null array. We instead validate against a
    // forward-walked instruction-start map and only accept a decode whose
    // instructions chain forward EXACTLY onto `pc`. On any misalignment we
    // return `None`, so the caller conservatively KEEPS the runtime null check.
    //
    // Building the map is O(code_len), and the previous implementation built it
    // HERE, once per inline array-access site. It defended that with "not worth
    // a pointer-keyed cache (which would risk an ABA stale-map hit)". The
    // refusal of the cache stands — see the doc comment above — but the
    // conclusion did not follow: the alternative to a cache is a parameter.
    // A 4 KB method with 200 array accesses rebuilds 200 maps of 4096 bools,
    // ~800 K redundant writes plus 200 allocations, for one method's compile.
    // The map is now a parameter, built once per method by
    // `Compiler::compile_bytecode` (`x64/bytecode_walk.rs`) and threaded to
    // every emitter that asks this question; the per-site convenience wrappers
    // are test-build-only. See the block at the bottom of this file.

    // The array load/store at `pc` must itself be a real instruction start
    // (it always is when reached from the codegen walk, but assert via the map).
    // `get`, not `[]`: a caller-supplied map that is too short must answer
    // "not a start" — i.e. no elision — not abort the compile thread.
    if !starts.get(pc).copied().unwrap_or(false) {
        return None;
    }

    // Locate the index-push: the unique instruction whose start `s_idx < pc`
    // satisfies `s_idx + bytecode_analysis::step(.. , s_idx) == pc`. Scan back over the
    // (at most 3) bytes an index push can occupy, accepting only a real start.
    let mut idx_pc: Option<usize> = None;
    for back in 1..=3usize {
        let cand = match pc.checked_sub(back) {
            Some(c) => c,
            None => break,
        };
        if !starts.get(cand).copied().unwrap_or(false) {
            continue;
        }
        // A real instruction start: it must end exactly at `pc` to be the
        // immediately-preceding instruction (the index push).
        if cand + bytecode_analysis::step(code, cand) == pc {
            idx_pc = Some(cand);
        }
        // The nearest real start that ends at `pc` is the predecessor; since we
        // scan increasing `back`, the FIRST such hit is the closest. But a real
        // start that does NOT end at `pc` means `pc` is mid-instruction relative
        // to it — impossible once we've confirmed `starts[pc]`, so keep scanning
        // only until we find the predecessor.
        if idx_pc.is_some() {
            break;
        }
    }
    let idx_pc = idx_pc?;

    // The index push must be one of the recognised single-instruction index
    // forms. Validate the OPCODE at the instruction start (not a trailing byte).
    let idx_op = code[idx_pc];
    let idx_ok = matches!(idx_op,
        // iconst_m1..iconst_5 (0x02..=0x08), iload_0..3 (0x1A..=0x1D),
        // dup (0x59 — index already on stack from a dup pair),
        // bipush (0x10), iload <u8> (0x15), sipush (0x11).
        0x02..=0x08 | 0x1A..=0x1D | 0x59 | 0x10 | 0x15 | 0x11
    );
    if !idx_ok {
        return None;
    }

    // Locate the aload: the instruction whose start `s_a < idx_pc` ends exactly
    // at `idx_pc`. Again accept only a real, forward-aligned start.
    let mut aload_pc: Option<usize> = None;
    for back in 1..=2usize {
        let cand = match idx_pc.checked_sub(back) {
            Some(c) => c,
            None => break,
        };
        if !starts.get(cand).copied().unwrap_or(false) {
            continue;
        }
        if cand + bytecode_analysis::step(code, cand) == idx_pc {
            aload_pc = Some(cand);
        }
        if aload_pc.is_some() {
            break;
        }
    }
    let aload_pc = aload_pc?;

    let aop = code[aload_pc];
    if (0x2A..=0x2D).contains(&aop) {
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        return Some(((aop - 0x2A) as usize, idx_pc));
    }
    // aload <u8> — the index byte is at aload_pc+1, which is strictly < idx_pc
    // because this instruction's forward length is 2 and it ends at idx_pc.
    if aop == 0x19 && aload_pc + 1 < idx_pc {
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        return Some((code[aload_pc + 1] as usize, idx_pc));
    }
    None
}

/// The instruction that ends exactly at `pc`, found among the real starts in
/// `pc - max_back ..= pc - 1`; `None` when there is none (or `pc` itself is
/// not a start).
fn start_ending_at(code: &[u8], starts: &[bool], pc: usize, max_back: usize) -> Option<usize> {
    if !starts.get(pc).copied().unwrap_or(false) {
        return None;
    }
    (1..=max_back)
        .filter_map(|back| pc.checked_sub(back))
        .find(|&cand| {
            starts.get(cand).copied().unwrap_or(false)
                && cand + bytecode_analysis::step(code, cand) == pc
        })
}

/// The array-STORE twin of [`array_receiver_with_starts`]: for
/// `aload N; <index push>; <value push>; xastore` at `pc`, answer
/// `(N, idx_pc, val_pc)`.
///
/// A store's stack is `[arrayref, index, value]`, so the receiver is three
/// instructions back. Both pushes must be single instructions that consume
/// nothing — a constant, a local load (`iconst*`/`lconst*`/`fconst*`/`dconst*`,
/// `bipush`, `sipush`, `ldc*`, `*load`, `aconst_null`; the index push from the
/// load decoder's `int` set) — so the three pushes are exactly the three
/// instructions before the store, and none of them writes `N` between the
/// `aload` and the store.
///
/// Textual adjacency is the dataflow only when nothing jumps in after the
/// `aload`, so a caller that elides on this answer must refuse when ANY of
/// `pc`, `val_pc` and `idx_pc` is a merge point. Two are not enough:
/// `aload b; iload j; goto L; … aload a; iload i; L: iload v; iastore` decodes
/// as `a`, while the array stored into may be `b`.
pub(super) fn array_store_receiver_with_starts(
    code: &[u8],
    pc: usize,
    starts: &[bool],
) -> Option<(usize, usize, usize)> {
    if pc >= code.len() || !matches!(code[pc], 0x4f..=0x56) {
        return None;
    }
    let val_pc = start_ending_at(code, starts, pc, 3)?;
    let value_ok = matches!(
        code[val_pc],
        // aconst_null, iconst_m1..dconst_1, bipush, sipush, ldc, ldc_w, ldc2_w,
        // iload..aload <u8>, iload_0..aload_3
        0x01..=0x2d
    );
    if !value_ok {
        return None;
    }
    let idx_pc = start_ending_at(code, starts, val_pc, 3)?;
    // The `int` index pushes the load decoder accepts, minus its dead `dup`.
    if !matches!(code[idx_pc], 0x02..=0x08 | 0x1A..=0x1D | 0x10 | 0x15 | 0x11) {
        return None;
    }
    let aload_pc = start_ending_at(code, starts, idx_pc, 2)?;
    let aop = code[aload_pc];
    if (0x2A..=0x2D).contains(&aop) {
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        return Some(((aop - 0x2A) as usize, idx_pc, val_pc));
    }
    if aop == 0x19 && aload_pc + 1 < idx_pc {
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        return Some((code[aload_pc + 1] as usize, idx_pc, val_pc));
    }
    None
}

// ---------------------------------------------------------------------------
// Receiver null-check elision — flags and engagement census
// ---------------------------------------------------------------------------

/// Seed `this` as non-null at method entry — **default ON**, opt out with
/// `CRATONVM_JIT_THIS_NONNULL=0`.
///
/// Separate from [`receiver_null_elim_enabled`] because the blast radii are
/// different, and a single switch would have made them indistinguishable in a
/// bisect. This one widens a fact that THREE existing consumers already read
/// (the inline array null-check elision, the `ifnull`/`ifnonnull` branch
/// elision, and now the getfield receiver guard); that one adds the third
/// consumer. Turning this off restores the previous entry state — nothing
/// proven on entry — exactly.
pub(super) fn this_nonnull_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Drop the `getfield` receiver's `TEST`/`JZ` when the dataflow proves it
/// non-null — **default ON**, opt out with `CRATONVM_JIT_RECEIVER_NULL_ELIM=0`.
///
/// Off restores an unconditional `emit_trusted_oop_receiver_check` at both
/// getfield arms, so the off arm is the previous binary's behaviour rather
/// than a degraded one.
pub(super) fn receiver_null_elim_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

static RECEIVER_NULL_CHECKS_ELIDED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static RECEIVER_NULL_CHECKS_EMITTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(super) fn note_receiver_null_check_elided() {
    RECEIVER_NULL_CHECKS_ELIDED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn note_receiver_null_check_emitted() {
    RECEIVER_NULL_CHECKS_EMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Implicit sites, split by which `getfield` arm emitted them.
///
/// Split because the two arms are not the same bet. The compact arm is the
/// common path; the legacy-cell arm is rare (13 sites against 1,705 on an H2
/// workload) and was widened into this feature on the argument that it *could*
/// carry sites the proof cannot reach, not on a measurement that it does.
///
/// A single total would have made that unanswerable, which is the shape this
/// codebase keeps getting caught by: a widening that never fires looks
/// identical to one that fires usefully. If `arm2` stays 0 across real
/// workloads, the widening is dead code and should be withdrawn — the counter
/// exists so that is a reading rather than an argument.
static RECEIVER_NULL_CHECKS_IMPLICIT: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

pub(super) fn note_receiver_null_check_implicit(arm: usize) {
    if let Some(c) = RECEIVER_NULL_CHECKS_IMPLICIT.get(arm) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Sites that dropped the check in favour of the fault.
///
/// Deliberately NOT folded into `elided`. An elided site carries a proof and
/// cannot fault; an implicit site has no proof and depends on the signal
/// handler to turn its fault into an NPE. One is an optimisation and the other
/// is a liability, and a single counter covering both would hide which of them
/// a workload is actually running on.
pub fn receiver_null_check_implicit_count() -> u64 {
    RECEIVER_NULL_CHECKS_IMPLICIT
        .iter()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .sum()
}

/// `(compact_arm, legacy_arm)` implicit sites. See the static's doc for why
/// the split is the point.
pub fn receiver_null_check_implicit_by_arm() -> (u64, u64) {
    (
        RECEIVER_NULL_CHECKS_IMPLICIT[0].load(std::sync::atomic::Ordering::Relaxed),
        RECEIVER_NULL_CHECKS_IMPLICIT[1].load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(elided, emitted)` getfield receiver null checks, process-wide.
///
/// The pair, not the ratio: an all-zero pair and a zero-elided pair look the
/// same in a percentage and want opposite fixes — the first means the arm was
/// never reached (no trusted-oop getfield compiled at all), the second that it
/// was reached and the dataflow proved nothing. A soak that reports neither
/// number has not measured whether this feature engaged.
pub fn receiver_null_check_counts() -> (u64, u64) {
    (
        RECEIVER_NULL_CHECKS_ELIDED.load(std::sync::atomic::Ordering::Relaxed),
        RECEIVER_NULL_CHECKS_EMITTED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

#[cfg(test)]
mod magic_div64_tests;

#[cfg(test)]
mod array_receiver_local_tests;

#[cfg(test)]
mod preceding_aload_nonnull_local_tests;

#[cfg(test)]
mod arraylength_receiver_tests;

#[cfg(test)]
mod starts_parameter_tests;

#[cfg(test)]
mod receiver_elision_tests;

#[cfg(test)]
mod receiver_elision_reach_tests;

// ---------------------------------------------------------------------------
// The per-method instruction-start map: where it is built, and the one trap.
// ---------------------------------------------------------------------------
//
// The review finding (A1, finding 14) was that `instruction_starts` was rebuilt
// per CALL SITE rather than per METHOD -- a full linear decode plus a
// `vec![false; code_len]` at every `ifnull`/`ifnonnull`, every receiver guard
// and every array access.
//
// **This is done.** `Compiler::compile_bytecode` (`x64/bytecode_walk.rs`) builds
// the map once and threads `insn_starts: &[bool]` through `walk_control`,
// `walk_array` and `walk_field` to the three emitters
// (`emit_null_check_array_store_at`, `emit_null_check_array_load_at` in
// `x64/arrays.rs`, `emit_trusted_oop_receiver_check_at` in `x64/objects.rs`),
// which is the parameter-not-a-field shape the original note argued for: a
// parameter cannot be read where it was not passed, so no other method on
// `Compiler` can see a map whose validity is scoped to one call of
// `compile_bytecode`.
//
// The zero-extra-argument wrappers in this file -- `preceding_aload_nonnull_
// local`, `array_receiver_local`, `array_receiver` -- are now `#[cfg(test)]`.
// They had no non-test callers left, and gating them is what makes the finding
// impossible to reintroduce: a production site cannot rebuild the map per call
// because the function that would do so does not exist in a production build.
// The `_with_starts` forms are the API.
//
// THE `code_len` TRAP -- still live, and the reason this is not a free edit:
//
// `compile_bytecode`, `walk_control`, `walk_array` and `walk_field` all take a
// `code_len: usize` that is NOT `code.len()`. It is threaded in from
// `jit_compile`'s caller (`x64/driver.rs`) and re-bound by the loop-rewrite path
// to `LoopXform::code_len`. The map is built with `code.len()`, and every
// decode's soundness argument in this file is stated against that map. Building
// it from the `code_len` parameter would silently change behaviour: a SHORTER
// map turns provable sites into `None` (a quiet de-optimisation no test would
// catch), and a LONGER one indexes past the slice inside `instruction_starts`
// itself. If a follow-up decides `code_len` is the right length, that is a
// separate argued change with its own test.
//
// ALSO CLOSED, recorded because the earlier note listed it as outstanding:
// `Compiler::emit_null_check_arraylength` (`x64/arrays.rs`) decoded its
// receiver by reading bytes backwards with no boundary validation -- the one
// array helper that never got the fix the two SOUNDNESS FIX blocks above
// describe. It was fixed on 2026-09-16 and now takes the same `starts` map and
// calls `preceding_aload_nonnull_local_with_starts`. The worked example is at
// that function: `getfield #42` (`B4 00 2A`) followed by `arraylength` puts
// `0x2A` at `bc_pc - 1`, which read as an opcode is `aload_0` -- `this`, proven
// non-null -- and elided the check on a field that can be null.

// ---------------------------------------------------------------------------
// The test-build-only convenience wrappers. They sit below every production
// item on purpose: the panic-free ratchet (`jit/tests/panic_free_compile_ratchet.rs`)
// scans a file only up to its first test gate, and these three used to put that
// gate at line 70, above ~300 lines of production decoders it then never read.
// ---------------------------------------------------------------------------

/// If the bytecode instruction immediately preceding `pc` is an `aload`
/// of some local, return the local index. Otherwise return `None`.
///
/// Used by the ifnull / ifnonnull codegen to decide whether the value
/// on top of the operand stack came from a known local — if so, the
/// caller can consult `NullCheckInfo::is_nonnull` and elide the inline
/// `TEST reg, reg; Jcc` sequence.
///
/// Only the common encodings are recognised:
///   * `aload_0..3`  (single-byte opcodes 0x2A..=0x2D)
///   * `aload <u8>`  (0x19 + 1-byte index)
///
/// The 4-byte `wide; aload` form is not recognised — its prevalence in
/// real classes is essentially zero, and bailing out is always safe
/// (we simply emit the regular runtime check).
///
/// SOUNDNESS FIX (2026-07-10, ES RestClient JIT-only spurious-NPE family):
/// the previous implementation guessed the preceding instruction by reading
/// raw bytes at `code[pc - 1]` / `code[pc - 2]` and matching them against
/// opcode values — exactly the class of bug `array_receiver_local` above
/// already documents and fixes ("SOUNDNESS FIX (array_receiver_local)").
/// That backward guess is unsound: for a multi-byte instruction (e.g.
/// `getfield`/`putfield`'s big-endian u16 constant-pool index), a trailing
/// OPERAND byte can numerically collide with the aload_0..3 opcode range
/// (0x2A..=0x2D) or the `aload <u8>` prefix (0x19). Confirmed instance:
/// `getfield #42` encodes as bytes `[0xB4, 0x00, 0x2A]` — the low byte of
/// index 42 is 0x2A, which is ALSO the `aload_0` opcode. For a method whose
/// `params`-style lazy-init field happened to land at constant-pool index
/// 42 (`org.apache.http.client.methods.HttpRequestWrapper.getParams()`,
/// hit by the real-world repro), `ifnonnull` immediately after that
/// `getfield` misread the getfield's trailing index byte as `aload_0`
/// (`this`, always non-null) and wrongly proved the null check dead —
/// eliding it into an unconditional jump. Since `this` is unrelated to the
/// getfield's *result*, this unsoundly skipped the method's entire
/// lazy-initialization branch, permanently returning the (still-null)
/// field. We now validate against a forward-walked instruction-start map
/// (`bytecode_analysis::instruction_starts`, the same one `array_receiver_local` uses) and
/// only accept a decode whose candidate instruction is a genuine boundary
/// — never a coincidental operand byte. On any misalignment we return
/// `None`, so the caller conservatively keeps the runtime `TEST`+`Jcc`.
///
/// # Convenience form — builds the map itself
///
/// This spelling rebuilds the instruction-start map on every call, which is
/// O(`code.len()`) time AND an O(`code.len()`) allocation. That is the right
/// shape for a one-shot question (a test, a single site) and the wrong shape
/// inside a walk: a caller that asks the question once per `ifnull` /
/// `ifnonnull` in a method pays the whole method's length again per site.
/// **Such a caller should hoist the map and call
/// [`preceding_aload_nonnull_local_with_starts`] instead** — see that
/// function's doc for why the map is a *parameter* rather than a cache.
///
/// The map this builds is `instruction_starts(code, code.len())`. A hoisting
/// caller MUST build it with that same length, not with some other notion of
/// "code length" it happens to be holding — see the
/// `preceding_aload_nonnull_local_with_starts` doc, which spells out why
/// `Compiler::walk_control`'s `code_len` parameter is specifically NOT the
/// value to pass.
#[cfg(test)]
pub(super) fn preceding_aload_nonnull_local(code: &[u8], pc: usize) -> Option<usize> {
    let starts = bytecode_analysis::instruction_starts(code, code.len());
    preceding_aload_nonnull_local_with_starts(code, pc, &starts)
}

/// Round-11 HIGH-2 helper — for array load/store opcodes at `pc`, try
/// to identify the local index that sourced the *array receiver* on
/// the operand stack. The standard javac pattern is:
///
///   `aload N; <push index>; <iaload/iastore/...>`
///
/// where `<push index>` is one of the single-byte index-loading
/// opcodes (iload_0..3, iconst_*, bipush, sipush, iload) and the
/// terminal opcode is the array load/store. When we recognise this
/// pattern we return `Some(N)`; the caller consults
/// `NullCheckInfo::is_nonnull(pc, N)` to decide whether the inline
/// `TEST RAX, RAX; JZ stub` can be elided.
///
/// Returns `None` whenever the index push isn't a single-instruction
/// form we recognise. The caller falls back to emitting the runtime
/// check on `None` (always sound).
///
/// **Convenience form.** Builds the instruction-start map per call. A caller
/// in a loop — one inline array access per iteration is the shape that made
/// this a review finding — should hoist the map and call
/// [`array_receiver_local_with_starts`]; see
/// [`preceding_aload_nonnull_local_with_starts`] for why the map is a
/// parameter and not a cache.
#[cfg(test)]
pub(super) fn array_receiver_local(code: &[u8], pc: usize) -> Option<usize> {
    // Routed through the parameterised form rather than through
    // `array_receiver`, so that the two spellings share one code path and a
    // future edit cannot make the wrapper and the hoisted form drift apart.
    let starts = bytecode_analysis::instruction_starts(code, code.len());
    array_receiver_local_with_starts(code, pc, &starts)
}

/// [`array_receiver_local`], plus the PC of the index push that sits between
/// the `aload` and the array access.
///
/// The pattern names the receiver by TEXTUAL adjacency, which is the dataflow
/// only when neither the access nor the index push can be jumped to. A caller
/// that elides a check on the strength of the pair must therefore also refuse
/// when either PC is a merge point (`NullCheckInfo::is_merge_point`) — the
/// operand may otherwise have been pushed on another path.
///
/// **Convenience form**, same as [`array_receiver_local`]: it builds the
/// instruction-start map on every call. [`array_receiver_with_starts`] is the
/// form for a caller that already has one.
#[cfg(test)]
pub(super) fn array_receiver(code: &[u8], pc: usize) -> Option<(usize, usize)> {
    let starts = bytecode_analysis::instruction_starts(code, code.len());
    array_receiver_with_starts(code, pc, &starts)
}

#[cfg(test)]
mod r11_x64loop_store_receiver_tests;

#[cfg(test)]
mod r11w2_x64loop_store_receiver_decoder_tests;
