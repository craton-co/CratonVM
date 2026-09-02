// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Null-check elimination.
//!
//! Moved verbatim out of `x64.rs`'s `HIGH-1 / Fix 1 — null-check elimination helper`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

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
/// (`instruction_start_map`, the same one `array_receiver_local` uses) and
/// only accept a decode whose candidate instruction is a genuine boundary
/// — never a coincidental operand byte. On any misalignment we return
/// `None`, so the caller conservatively keeps the runtime `TEST`+`Jcc`.
pub(super) fn preceding_aload_nonnull_local(code: &[u8], pc: usize) -> Option<usize> {
    if pc == 0 {
        return None;
    }
    let code_len = code.len();
    if pc > code_len {
        return None;
    }
    let starts = instruction_start_map(code, code_len);
    // aload_0..aload_3 — 1-byte opcode; only trust it if the map confirms
    // `pc - 1` is a real instruction start (aload_0..3 are always exactly
    // 1 byte long, so a genuine start there necessarily ends at `pc`).
    if starts[pc - 1] {
        let prev1 = code[pc - 1];
        if (0x2A..=0x2D).contains(&prev1) {
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            return Some((prev1 - 0x2A) as usize);
        }
    }
    // aload <u8> — 2-byte instruction; only trust it if the map confirms
    // `pc - 2` is a real instruction start (aload <u8> is always exactly 2
    // bytes, so a genuine start there necessarily ends at `pc`).
    if pc >= 2 && starts[pc - 2] && code[pc - 2] == 0x19 {
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
pub(super) fn array_receiver_local(code: &[u8], pc: usize) -> Option<usize> {
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
    // Building the map is O(code_len). This helper runs once per inline
    // array-access site at COMPILE time (not per array element at runtime), and
    // `jit_scan` bounds the method size, so the per-site recompute is cheap and
    // not worth a pointer-keyed cache (which would risk an ABA stale-map hit).
    // Correctness over micro-optimization: always build a fresh, exact map.
    let starts = instruction_start_map(code, code_len);

    // The array load/store at `pc` must itself be a real instruction start
    // (it always is when reached from the codegen walk, but assert via the map).
    if !starts[pc] {
        return None;
    }

    // Locate the index-push: the unique instruction whose start `s_idx < pc`
    // satisfies `s_idx + bytecode_len_at(.. , s_idx) == pc`. Scan back over the
    // (at most 3) bytes an index push can occupy, accepting only a real start.
    let mut idx_pc: Option<usize> = None;
    for back in 1..=3usize {
        let cand = match pc.checked_sub(back) {
            Some(c) => c,
            None => break,
        };
        if !starts[cand] {
            continue;
        }
        // A real instruction start: it must end exactly at `pc` to be the
        // immediately-preceding instruction (the index push).
        if cand + bytecode_len_at(code, cand) == pc {
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
        if !starts[cand] {
            continue;
        }
        if cand + bytecode_len_at(code, cand) == idx_pc {
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
        return Some((aop - 0x2A) as usize);
    }
    // aload <u8> — the index byte is at aload_pc+1, which is strictly < idx_pc
    // because this instruction's forward length is 2 and it ends at idx_pc.
    if aop == 0x19 && aload_pc + 1 < idx_pc {
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        return Some(code[aload_pc + 1] as usize);
    }
    None
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
    use super::{array_receiver_local, instruction_start_map};

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
        let starts = instruction_start_map(&code, code.len());
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
        let src = include_str!("bytecode_walk.rs");
        let consulting = src.matches("emit_trusted_oop_receiver_check_at(code, pc)").count();
        assert_eq!(
            consulting, 2,
            "expected exactly the two `getfield` arms to consult the dataflow; \
             found {consulting}. A THIRD consulting site is only correct if its \
             receiver is the value the immediately-preceding `aload` pushed — \
             see this test's doc comment for the two shapes where it is not."
        );
        let bare = src.matches("self.emit_trusted_oop_receiver_check()").count();
        assert_eq!(
            bare, 4,
            "expected the two `putfield` and two `checkcast` arms to keep the \
             unconditional check; found {bare}. Moving one of them onto the \
             `_at` form is a miscompile, not a simplification."
        );
    }
}
