// SPDX-License-Identifier: Apache-2.0
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
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_THIS_NONNULL").as_deref(),
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
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_RECEIVER_NULL_ELIM").as_deref(),
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
mod magic_div64_tests {
    /// Software model of the emitted `emit_ldiv_magic64` sequence:
    /// t = mulhi_signed(magic, n) (+ n when magic < 0);
    /// q = (t >> shift) + (n logical>> 63).
    fn model_q(n: i64, magic: i64, shift: u32) -> i64 {
        let mut t = ((n as i128 * magic as i128) >> 64) as i64;
        if magic < 0 {
            t = t.wrapping_add(n);
        }
        (t >> shift).wrapping_add(((n as u64) >> 63) as i64)
    }

    #[test]
    fn magic_signed_div64_matches_exact_division() {
        let divisors: [i64; 14] = [2, 3, 5, 6, 7, 9, 10, 11, 12, 25, 100, 1000, 7919, 1_000_003];
        let mut dividends: Vec<i64> = vec![
            0,
            1,
            -1,
            2,
            -2,
            i64::MAX,
            i64::MIN,
            i64::MAX - 1,
            i64::MIN + 1,
        ];
        // Pseudo-random spread (deterministic LCG) incl. sign flips.
        let mut x = 0x9E3779B97F4A7C15u64;
        for _ in 0..2000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            dividends.push(x as i64);
        }
        for &d in &divisors {
            let (magic, shift) = super::Compiler::magic_signed_div64(d);
            let mut extra = vec![d, -d, d - 1, 1 - d, d + 1, -d - 1, d * 3, -d * 3];
            extra.extend_from_slice(&dividends);
            for &n in &extra {
                let expect = n / d; // Rust trunc-toward-zero == JVM ldiv
                let got = model_q(n, magic, shift);
                assert_eq!(
                    got, expect,
                    "n={n} d={d} magic={magic:#x} shift={shift}: got {got}, want {expect}"
                );
            }
        }
    }
}

#[cfg(test)]
mod array_receiver_local_tests {
    // `super::` is the enclosing `null_check_elim` module, where these
    // helpers now live.
    use super::{array_receiver_local, bytecode_analysis};

    // Opcodes used below:
    //   0x2A aload_0, 0x19 aload, 0x11 sipush, 0x10 bipush, 0x03 iconst_0,
    //   0x32 aaload, 0x53 aastore, 0x2E iaload.

    #[test]
    fn simple_aload0_iconst0_iaload() {
        // aload_0; iconst_0; iaload
        let code = [0x2Au8, 0x03, 0x2E];
        assert_eq!(array_receiver_local(&code, 2), Some(0));
    }

    #[test]
    fn aload_u8_then_sipush_index() {
        // aload 5; sipush 0x0003; aaload
        // sipush operand low byte is 0x03 (== iconst_0). The OLD backward scan
        // read code[pc-1]=0x03 and mis-decoded it as a single-byte index op,
        // mislocating the aload. The forward-validated decode must still find
        // the real aload at offset 0 (local 5).
        let code = [0x19u8, 0x05, 0x11, 0x00, 0x03, 0x32];
        // pc of aaload = 5.
        assert_eq!(array_receiver_local(&code, 5), Some(5));
    }

    #[test]
    fn misaligned_collision_is_rejected_or_correct() {
        // Construct a method where a backward scan would be fooled but the
        // forward walk disambiguates. bipush index whose operand equals 0x2A
        // (aload_0): aload_1; bipush 0x2A; iastore-style aaload.
        // aload_1 = 0x2B, bipush = 0x10, operand 0x2A, aaload = 0x32.
        let code = [0x2Bu8, 0x10, 0x2A, 0x32];
        // Forward walk: 0:aload_1, 1:bipush(2), 3:aaload. Receiver local = 1.
        assert_eq!(array_receiver_local(&code, 3), Some(1));
    }

    #[test]
    fn no_aload_returns_none() {
        // iconst_1; iconst_0; iaload — no array receiver aload present.
        let code = [0x04u8, 0x03, 0x2E];
        assert_eq!(array_receiver_local(&code, 2), None);
    }

    #[test]
    fn instruction_start_map_basic() {
        // aload_0; sipush 0x0102; aaload
        let code = [0x2Au8, 0x11, 0x01, 0x02, 0x32];
        let starts = bytecode_analysis::instruction_starts(&code, code.len());
        assert_eq!(starts, vec![true, true, false, false, true]);
    }
}

#[cfg(test)]
mod preceding_aload_nonnull_local_tests {
    use super::preceding_aload_nonnull_local;

    // Regression coverage for the ES RestClient JIT-only spurious-NPE fix
    // (2026-07-10): `preceding_aload_nonnull_local` used to guess the
    // instruction preceding `pc` by reading a raw byte at `pc - 1` / `pc - 2`,
    // the exact unsoundness `array_receiver_local_tests` above already
    // covers for the sibling array-null-check helper.

    #[test]
    fn rejects_getfield_operand_collision() {
        // aload_0; getfield #42; ifnonnull ...
        // `getfield #42` encodes as [0xB4, 0x00, 0x2A] — the low byte of
        // the constant-pool index (0x2A) numerically equals the `aload_0`
        // opcode. A backward byte read at `pc - 1` (pc = the ifnonnull's
        // own PC, 4) would misidentify the getfield's trailing operand
        // byte as a genuine `aload_0` and wrongly borrow `this`'s
        // provable non-nullity for the getfield's *result*. Real-world
        // hit: org.apache.http.client.methods.HttpRequestWrapper.getParams(),
        // whose lazily-initialized `params` field sat at constant-pool
        // index 42 — the miscompiled `ifnonnull` always took the
        // "already non-null" branch and the lazy-init store never ran.
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xb4, 0x00, 0x2a, // 1: getfield #42
            0xc7, 0x00, 0x00, // 4: ifnonnull (target irrelevant here)
        ];
        assert_eq!(preceding_aload_nonnull_local(&code, 4), None);
    }

    #[test]
    fn accepts_genuine_aload_0() {
        // aload_0; ifnonnull ... — the legitimate pattern this helper
        // exists to recognise must still resolve to local 0.
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0xc7, 0x00, 0x00, // 1: ifnonnull
        ];
        assert_eq!(preceding_aload_nonnull_local(&code, 1), Some(0));
    }

    #[test]
    fn accepts_genuine_aload_u8() {
        // aload 5; ifnonnull ... — the two-byte wide-index form.
        let code: Vec<u8> = vec![
            0x19, 0x05, // 0: aload 5
            0xc7, 0x00, 0x00, // 2: ifnonnull
        ];
        assert_eq!(preceding_aload_nonnull_local(&code, 2), Some(5));
    }

    #[test]
    fn rejects_putfield_operand_collision() {
        // aload_0; aload_1; putfield #298 (0x012A — high byte 0x01, low
        // byte 0x2A); ifnonnull. Same collision, but on `putfield`'s
        // trailing operand byte instead of `getfield`'s, and at a larger
        // constant-pool index to show the bug is not specific to #42.
        let code: Vec<u8> = vec![
            0x2a, // 0: aload_0
            0x2b, // 1: aload_1
            0xb5, 0x01, 0x2a, // 2: putfield #298
            0xc7, 0x00, 0x00, // 5: ifnonnull
        ];
        assert_eq!(preceding_aload_nonnull_local(&code, 5), None);
    }
}

#[cfg(test)]
mod arraylength_receiver_tests {
    use super::*;

    /// `getfield #42; arraylength` must NOT read the getfield's trailing
    /// index byte as `aload_0`.
    ///
    /// This is the shape that made `emit_null_check_arraylength` unsound
    /// until 2026-09-16: it decoded its receiver by reading `code[pc - 1]`
    /// backwards and matching `0x2A..=0x2D`. `getfield #42` encodes as
    /// `B4 00 2A`, so the byte before the `arraylength` is `0x2A` — the low
    /// half of the constant-pool index, which reads as `aload_0`. In an
    /// instance method the dataflow proves local 0 (`this`) non-null, so the
    /// null check was elided on a value that was never checked: a SIGSEGV
    /// where JVMS requires a `NullPointerException`.
    ///
    /// The receiver of the `arraylength` here is the *result of the
    /// getfield*, which lives on the operand stack and is not a local at
    /// all, so the only correct answer is `None`.
    #[test]
    fn a_getfield_index_byte_is_not_an_aload_before_arraylength() {
        // 0: aload_0            (this)
        // 1: getfield #42       -> B4 00 2A   (the 0x2A trap)
        // 4: arraylength        -> BE
        // 5: ireturn
        let code = [0x2A, 0xB4, 0x00, 0x2A, 0xBE, 0xAC];
        let starts = bytecode_analysis::instruction_starts(&code, code.len());
        assert!(starts[0] && starts[1] && starts[4] && starts[5]);
        assert!(
            !starts[3],
            "byte 3 is the getfield's low index byte, not an instruction start"
        );
        assert_eq!(
            preceding_aload_nonnull_local_with_starts(&code, 4, &starts),
            None,
            "the instruction before `arraylength` is a getfield, not an aload —              answering `Some(0)` here elides a null check on a stack value"
        );
        // And the wrapper agrees, since that is what the emitter calls.
        assert_eq!(preceding_aload_nonnull_local(&code, 4), None);
    }

    /// The two-byte arm has the same trap one byte further back: any 3-byte
    /// instruction whose FIRST operand byte is `0x19` used to read as
    /// `aload <second operand byte>` — an arbitrary local.
    #[test]
    fn a_sipush_operand_is_not_an_aload_before_arraylength() {
        // 0: sipush 0x1907      -> 11 19 07   (0x19 is the `aload` opcode)
        // 3: arraylength        -> BE
        // 4: ireturn
        let code = [0x11, 0x19, 0x07, 0xBE, 0xAC];
        let starts = bytecode_analysis::instruction_starts(&code, code.len());
        assert!(!starts[1], "byte 1 is sipush's high operand byte");
        assert_eq!(
            preceding_aload_nonnull_local_with_starts(&code, 3, &starts),
            None,
            "the backward read would have named local 7 from an sipush operand"
        );
    }

    /// The positive control: a real `aload_0` before `arraylength` still
    /// resolves, so the fix did not simply switch the elision off.
    #[test]
    fn a_real_aload_before_arraylength_still_resolves() {
        // 0: aload_0 ; 1: arraylength ; 2: ireturn
        let code = [0x2A, 0xBE, 0xAC];
        let starts = bytecode_analysis::instruction_starts(&code, code.len());
        assert_eq!(
            preceding_aload_nonnull_local_with_starts(&code, 1, &starts),
            Some(0)
        );
        // And the wide-index form.
        // 0: aload 7 ; 2: arraylength ; 3: ireturn
        let code2 = [0x19, 0x07, 0xBE, 0xAC];
        let starts2 = bytecode_analysis::instruction_starts(&code2, code2.len());
        assert_eq!(
            preceding_aload_nonnull_local_with_starts(&code2, 2, &starts2),
            Some(7)
        );
    }
}

#[cfg(test)]
mod starts_parameter_tests {
    use super::{
        array_receiver_local, array_receiver_local_with_starts, bytecode_analysis,
        preceding_aload_nonnull_local, preceding_aload_nonnull_local_with_starts,
    };

    /// One method body carrying BOTH documented operand/opcode collisions, so
    /// the equivalence below is asserted over bytes where a wrong answer is
    /// actually reachable rather than over a straight-line decode that any
    /// implementation would get right.
    ///
    ///   0: aload_0
    ///   1: getfield #42      `[0xB4, 0x00, 0x2A]` — the trailing constant-pool
    ///                        byte is numerically `aload_0`. This is the ES
    ///                        RestClient / `HttpRequestWrapper.getParams()`
    ///                        shape the SOUNDNESS FIX on
    ///                        `preceding_aload_nonnull_local` documents.
    ///   4: ifnonnull +3      the site that asks the question.
    ///   7: aload_1
    ///   8: sipush 3          `[0x11, 0x00, 0x03]` — the trailing byte is
    ///                        numerically `iconst_0`, the collision
    ///                        `array_receiver`'s SOUNDNESS FIX documents.
    ///  11: aaload            the other site that asks the question.
    ///  12: areturn
    fn collision_body() -> Vec<u8> {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x2a,               // 0: aload_0
            0xb4, 0x00, 0x2a,   // 1: getfield #42
            0xc7, 0x00, 0x03,   // 4: ifnonnull +3
            0x2b,               // 7: aload_1
            0x11, 0x00, 0x03,   // 8: sipush 3
            0x32,               // 11: aaload
            0xb0,               // 12: areturn
        ];
        code
    }

    /// The parameterised form and the convenience wrapper are the same
    /// function, at every pc.
    ///
    /// Asserted over the whole body (and a little past its end) rather than at
    /// the two interesting pcs, because the failure mode this guards against is
    /// an off-by-one introduced while threading `starts` — which shows up at
    /// some unremarkable pc, not at the one the author was thinking about.
    #[test]
    fn with_starts_and_wrapper_agree_at_every_pc() {
        let code = collision_body();
        let starts = bytecode_analysis::instruction_starts(&code, code.len());

        // The map the hoisted callers must reproduce. Spelled out so that a
        // change to `instruction_starts` that silently re-shapes it fails here
        // and not as a miscompile.
        assert_eq!(
            starts,
            vec![
                true,  // 0  aload_0
                true,  // 1  getfield
                false, // 2  cp index high
                false, // 3  cp index low (0x2A, NOT an aload_0)
                true,  // 4  ifnonnull
                false, // 5  branch offset high
                false, // 6  branch offset low
                true,  // 7  aload_1
                true,  // 8  sipush
                false, // 9  imm high
                false, // 10 imm low (0x03, NOT an iconst_0)
                true,  // 11 aaload
                true,  // 12 areturn
            ]
        );

        // Past the end too: both spellings must agree on out-of-range pcs, not
        // merely on in-range ones.
        for pc in 0..code.len() + 4 {
            assert_eq!(
                preceding_aload_nonnull_local_with_starts(&code, pc, &starts),
                preceding_aload_nonnull_local(&code, pc),
                "preceding_aload_nonnull_local disagrees with its _with_starts \
                 form at pc {pc}"
            );
            assert_eq!(
                array_receiver_local_with_starts(&code, pc, &starts),
                array_receiver_local(&code, pc),
                "array_receiver_local disagrees with its _with_starts form at \
                 pc {pc}"
            );
        }

        // Agreement on `None` everywhere would also pass the loop above, so
        // pin the two answers that carry the soundness claims.
        assert_eq!(
            preceding_aload_nonnull_local_with_starts(&code, 4, &starts),
            None,
            "the `ifnonnull` at 4 follows `getfield #42`, whose trailing 0x2A \
             is an operand byte and not an `aload_0`"
        );
        assert_eq!(
            array_receiver_local_with_starts(&code, 11, &starts),
            Some(1),
            "the `aaload` at 11 takes its receiver from the `aload_1` at 7, \
             across a 3-byte `sipush` whose trailing 0x03 is an operand byte \
             and not an `iconst_0`"
        );
    }

    /// A `starts` map built for a DIFFERENT, shorter code array must not be
    /// able to index out of range.
    ///
    /// This is the failure mode that makes the bound checks non-negotiable
    /// rather than defensive: these helpers run on a compile thread, the
    /// module denies `clippy::panic`, and an out-of-range `starts[pc]` would
    /// take the process down rather than decline an optimisation. Threading a
    /// map as a parameter is what makes a mismatched length expressible at
    /// all, so the test arrives with the parameter.
    ///
    /// The required degradation is one-directional: a short map may only turn
    /// `Some` into `None` — keep the runtime null check — never the reverse,
    /// and never a panic.
    #[test]
    fn a_map_from_a_shorter_code_array_cannot_index_out_of_bounds() {
        let code = collision_body();
        // A map for the first three bytes only: `[aload_0, getfield-opcode,
        // cp-index-high]`, length 3 against a 13-byte body.
        let short = bytecode_analysis::instruction_starts(&code[..3], 3);
        assert_eq!(short.len(), 3);

        for pc in 0..code.len() + 4 {
            // The assertion is that these two calls RETURN. Every pc from 3
            // upward indexes past `short`.
            let prec = preceding_aload_nonnull_local_with_starts(&code, pc, &short);
            let recv = array_receiver_local_with_starts(&code, pc, &short);

            if pc >= short.len() + 2 {
                assert_eq!(
                    prec, None,
                    "pc {pc} is beyond the short map's reach; the only sound \
                     answer is `None`"
                );
            }
            if pc >= short.len() {
                assert_eq!(
                    recv, None,
                    "pc {pc} is beyond the short map's reach; the only sound \
                     answer is `None`"
                );
            }
        }

        // And the degradation really is a degradation: the same pc that the
        // correct map proves is refused under the short one.
        let full = bytecode_analysis::instruction_starts(&code, code.len());
        assert_eq!(array_receiver_local_with_starts(&code, 11, &full), Some(1));
        assert_eq!(array_receiver_local_with_starts(&code, 11, &short), None);
    }
}

#[cfg(test)]
mod receiver_elision_tests {
    use super::preceding_aload_nonnull_local;
    use crate::null_check_elim::analyze_with_receiver;

    /// The two halves the getfield emitter ANDs together, tested together.
    ///
    /// `emit_trusted_oop_receiver_check_at` elides only when
    /// `preceding_aload_nonnull_local` names a local AND the dataflow proves
    /// it. Testing either alone would miss the pairing -- which is where the
    /// bug would be, since the two are indexed by the same bci and derived
    /// from the same array.
    #[test]
    fn the_getfield_receiver_resolves_to_this_and_the_dataflow_proves_it() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x2A,               // 0: aload_0
            0xB4, 0x00, 0x01,   // 1: getfield #1
            0xAC,               // 4: ireturn
        ];
        assert_eq!(
            preceding_aload_nonnull_local(&code, 1),
            Some(0),
            "the receiver of a getfield is the value the preceding aload pushed"
        );

        let seeded = analyze_with_receiver(&code, code.len(), true);
        assert!(seeded.is_nonnull(1, 0), "and the seed proves it at bci 1");

        let unseeded = analyze_with_receiver(&code, code.len(), false);
        assert!(
            !unseeded.is_nonnull(1, 0),
            "without the seed the very first `this.field` in a method still \
             pays the check -- this is the arm the seed removes"
        );
    }

    /// A `getfield` whose receiver did not come from an `aload` is refused
    /// outright, so the elision can never be handed a local it did not derive.
    /// The chained form `this.a.b` is the common instance: the inner
    /// `getfield` pushes the receiver of the outer one.
    #[test]
    fn a_chained_getfield_receiver_is_not_attributed_to_a_local() {
        #[rustfmt::skip]
        let code: Vec<u8> = vec![
            0x2A,               // 0: aload_0
            0xB4, 0x00, 0x01,   // 1: getfield #1   (this.a)
            0xB4, 0x00, 0x02,   // 4: getfield #2   (.b)
            0xAC,               // 7: ireturn
        ];
        assert_eq!(
            preceding_aload_nonnull_local(&code, 4),
            None,
            "`this.a` is not a local, and its nullness is not local 0's"
        );
    }
}

#[cfg(test)]
mod receiver_elision_reach_tests {
    /// The two `getfield` arms must consult the dataflow, and the other four
    /// receiver-guard sites must NOT.
    ///
    /// This is a source scan because the property is about which CALL each arm
    /// makes, and the arms are unreachable from a unit test without a full
    /// compile fixture. It is worth the brittleness: the edit that breaks it —
    /// "unify these six sites on one helper" — looks like tidying and is a
    /// miscompile at four of them.
    ///
    /// * The two `getfield` arms are identifiable by their return shape,
    ///   `(…, None)`: they hand back a patch list and a separate null patch.
    ///   Their receiver is the top-of-stack value the preceding `aload`
    ///   pushed, which is what `preceding_aload_nonnull_local` decodes.
    /// * The two `putfield` arms must stay on the bare check. `putfield`'s
    ///   stack is `[…, objectref, value]`, so the preceding push is the stored
    ///   VALUE — attributing the receiver's nullness to it is the shape of the
    ///   Tomcat `MessageBytes.setString` miscompile that
    ///   `opcode_dereferences_receiver` documents.
    /// * The two `checkcast` arms must stay on the bare check for a different
    ///   reason: `checkcast` does not throw on a null receiver at all (a null
    ///   casts to anything), so its `JZ` targets a legal null path rather than
    ///   an NPE. Eliding it would let a null fall into the `KIND_TAGS` byte
    ///   compare and fault.
    #[test]
    fn only_the_getfield_arms_consult_the_null_check_dataflow() {
        let src = crate::x64::bytecode_walk::WALK_SOURCES;
        // Counted on the call NAME, not on an argument spelling: arm 2's call is
        // wrapped across lines, and an earlier version of this test matched
        // "(code, pc," and silently scored it as one site instead of two.
        let consulting = src.matches("emit_trusted_oop_receiver_check_at(").count();
        // Every arm that can elide the check MUST bind a recovery address,
        // and the two numbers are checked against each other rather than
        // against a constant. An arm that opts in without binding leaves the
        // site pending; that is caught at runtime by
        // `has_unbound_implicit_null_sites`, but as a refused compile on a
        // live workload rather than here.
        //
        // Note the pairing is what is asserted, not the literal `true`. Arm 1
        // opts in unconditionally; arm 2 opts in exactly when
        // `compact_ref_fields_enabled()`, because that is when it emits the
        // `GC_FLAGS` read the fault lands on. A future arm may well pass a
        // third expression -- what it may not do is pass one without binding.
        let binds = src.matches("self.bind_implicit_null_recovery()").count();
        assert_eq!(
            binds, consulting,
            "{consulting} getfield arms consult the dataflow but {binds} bind a \
             recovery address. Each arm that may elide the receiver check has \
             to bind the slow path the fault recovers into, at the point that \
             slow path begins."
        );
        assert_eq!(
            consulting, 2,
            "expected exactly the two `getfield` arms to consult the dataflow; \
             found {consulting}. A THIRD consulting site is only correct if its \
             receiver is the value the immediately-preceding `aload` pushed — \
             see this test's doc comment for the two shapes where it is not."
        );
        let bare = src
            .matches("self.emit_trusted_oop_receiver_check()")
            .count();
        // 2026-09-12: 4 -> 5. The `instanceof` arm now takes checkcast's
        // inline class-id guard, and its null test must stay bare for the
        // checkcast reason: null is a legal `instanceof` operand (answer 0),
        // so its `JZ` targets a result stub, not an NPE, and an elided test
        // would let null fall into the `KIND_TAGS` byte compare and fault.
        // 2026-09-18: 5 -> 6. Round 9's inline primitive `putfield`
        // (`CRATONVM_JIT_INLINE_PRIM_PUTFIELD`, `op_field.rs`) keeps the bare
        // check: it has no dataflow proof wired, and for a store an elided
        // receiver test costs a wild write, not a wrong read.
        assert_eq!(
            bare, 6,
            "expected the three `putfield` (two helper-routed, one inline              primitive), two `checkcast` and one `instanceof` arms to keep the              unconditional check; found {bare}. Moving one of them onto the              `_at` form is a miscompile, not a simplification."
        );
    }
}

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
