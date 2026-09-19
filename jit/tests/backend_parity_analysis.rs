// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// REVIEW-NOTE: this file does not compile until `jit/src/lib.rs` gains
//
//     pub mod backend_parity;
//
// between `pub mod aarch64_backend;` and `pub mod bailout;`. That one line is
// the whole wiring; see the REVIEW-NOTE at the top of
// `jit/src/backend_parity.rs`, which this change was not permitted to add.

//! Tests for the backend-parity analyser, over hand-assembled x86-64.
//!
//! # Why hand-assembled and not compiler output
//!
//! The analyser's job is to survive bytes it was not expecting, and compiler
//! output is by definition bytes the compiler chose. The interesting cases —
//! a `0xF3` inside a displacement, a `0xC4` inside an immediate, an
//! instruction truncated by the end of the buffer — are ones no correct
//! emitter produces, so a corpus of real bodies cannot exercise them. They are
//! also exactly the cases where a substring search, which is what this module
//! exists not to be, gets the wrong answer.
//!
//! Every expected encoding below is written out with its mnemonic and checked
//! against the Intel SDM encoding rules. Where a sequence also appears in the
//! JIT's own emitter tests (`jit/src/x64/vec_emit.rs`), that is noted — those
//! are bytes the backend is asserted to emit, so agreeing with them is a
//! second, independent check. A wrong expected-byte here would be worse than
//! no test at all: it would pin the decoder to a fiction.
//!
//! # Why every test takes a lock
//!
//! The census is process-global and monotone, so an exact delta is only
//! readable by a test that no sibling test is racing. `cargo test` runs the
//! tests in one binary on parallel threads, and every test in this file calls
//! [`profile_body`] at least once, so every one of them moves
//! `bodies_profiled`. Serialising the whole file is cheap (these are
//! microsecond decodes) and is the only way the census assertions can be
//! equalities rather than inequalities. `ir_optimize.rs` records the same
//! lesson the expensive way: `the_census_counts_the_reads_this_pass_removed`
//! first failed reading 2 for its own 1.

use cratonvm_jit::backend_parity::{
    backend_parity_census, compare_bodies, profile_body, regressions, BodyCapability, BodyProfile,
    DecodeStopReason, MethodId,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialises the whole file against the process-global census. See the module
/// header. Poisoning is recovered from rather than propagated: a panic in one
/// test must fail that test, not cascade into every later one.
fn guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A profile plus the assertion that the walk reached the end of the body.
///
/// Almost every test wants this; a test that meant to assert a stop says so
/// explicitly instead.
fn profile_all(name: &str, code: &[u8]) -> BodyProfile {
    let p = profile_body(code);
    assert!(
        !p.is_undecodable(),
        "{name}: the walk stopped at {:?} instead of decoding all {} bytes — \
         the decoder's table is missing something this test needs",
        p.undecodable,
        code.len()
    );
    p
}

// ---------------------------------------------------------------------------
// The capability enum's own bookkeeping
// ---------------------------------------------------------------------------

/// A variant missing from `ALL` would be silently exempt from every scan and
/// every census slot — the shape of the bug one level down.
#[test]
fn every_capability_is_registered() {
    let _g = guard();
    assert_eq!(
        BodyCapability::ALL.len(),
        BodyCapability::COUNT,
        "ALL and COUNT disagree; the census array is the wrong width"
    );
    for (i, cap) in BodyCapability::ALL.iter().enumerate() {
        assert_eq!(
            cap.ordinal(),
            i,
            "{cap:?} is at index {i} of ALL but its ordinal is {}",
            cap.ordinal()
        );
    }
    let mut names: Vec<&str> = BodyCapability::ALL.iter().map(|c| c.name()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "two capabilities share a name");
}

/// The module header claims `Evex` corresponds to no `SinglePassOnly` variant
/// and the other three do. That claim is load-bearing — it is what tells a
/// reader a finding on `Evex` means the decoder is out of sync rather than
/// that AVX-512 was emitted — so it is asserted rather than asserted in prose
/// alone.
#[test]
fn only_evex_maps_to_no_single_pass_only_variant() {
    let _g = guard();
    let unmapped: Vec<BodyCapability> = BodyCapability::ALL
        .iter()
        .copied()
        .filter(|c| c.single_pass_only_variants().is_none())
        .collect();
    assert_eq!(
        unmapped,
        vec![BodyCapability::Evex],
        "the set of capabilities with no SinglePassOnly counterpart changed; \
         update the module header's table, which says it is exactly Evex"
    );
}

// ---------------------------------------------------------------------------
// Positive detection
// ---------------------------------------------------------------------------

/// `C5 FD FE C1` — VPADDD ymm0, ymm0, ymm1.
///
/// Two-byte VEX: `C5` then `[R vvvv L pp]` = `FD` (R=1 inverted → reg < 8,
/// vvvv=1111 inverted → ymm0 as src1, L=1 → 256-bit, pp=01 → `66`), map is
/// implicitly `0F`, opcode `FE` = PADDD, ModRM `C1` = mod 11 / reg ymm0 /
/// rm ymm1. No immediate. The JIT asserts it emits exactly these four bytes in
/// `two_byte_vex_matches_the_known_vpaddd_encoding` (`x64/vec_emit.rs`).
#[test]
fn a_two_byte_vex_instruction_is_detected() {
    let _g = guard();
    let p = profile_all("vpaddd", &[0xC5, 0xFD, 0xFE, 0xC1]);
    assert_eq!(p.instructions, 1);
    assert_eq!(p.count(BodyCapability::Vex), 1);
    assert_eq!(p.count(BodyCapability::Evex), 0);
    assert_eq!(p.count(BodyCapability::RepString), 0);
}

/// `C4 E3 7D 39 C1 01` — VEXTRACTI128 xmm1, ymm0, 1.
///
/// Three-byte VEX: `C4`, then `[R X B mmmmm]` = `E3` (mmmmm = 3 → `0F 3A`),
/// then `[W vvvv L pp]` = `7D` (L=1, pp=01), opcode `39`, ModRM `C1`, and the
/// `imm8` lane selector `01` that map 3 always carries. Six bytes; dropping the
/// immediate would leave the walk one byte short of the end and is what makes
/// this a length test and not just a prefix test. Also asserted by
/// `x64/vec_emit.rs`.
#[test]
fn a_three_byte_vex_instruction_is_detected_with_its_imm8() {
    let _g = guard();
    let code = [0xC4, 0xE3, 0x7D, 0x39, 0xC1, 0x01];
    let p = profile_all("vextracti128", &code);
    assert_eq!(p.instructions, 1, "the imm8 was not accounted for");
    assert_eq!(p.count(BodyCapability::Vex), 1);
}

/// `C5 F8 77` — VZEROUPPER, the one VEX-encoded instruction with no ModRM byte.
///
/// A decoder that demands a ModRM after every VEX opcode reads the byte after
/// this one as a ModRM and desynchronises for the rest of the body, which is
/// why the epilogue emitter's own test (`x64/vec_emit.rs`) pins the same three
/// bytes.
#[test]
fn vzeroupper_has_no_modrm_and_is_still_vex() {
    let _g = guard();
    // VZEROUPPER followed by RET. If the `77` special case were missing, `C3`
    // would be eaten as a ModRM and the instruction count would be 1.
    let p = profile_all("vzeroupper;ret", &[0xC5, 0xF8, 0x77, 0xC3]);
    assert_eq!(p.instructions, 2, "VZEROUPPER consumed the following RET");
    assert_eq!(p.count(BodyCapability::Vex), 1);
}

/// `62 F1 7D 48 6F C1` — VMOVDQA32 zmm0, zmm1.
///
/// EVEX: `62`, then `P0 = F1` (R̄X̄B̄R̄' = 1111, bits 3:2 = 00, mm = 01 → `0F`),
/// `P1 = 7D` (W=0, v̄vvv = 1111, the mandatory 1 bit, pp = 01 → `66`),
/// `P2 = 48` (z=0, L'L = 10 → 512-bit, b=0, V̄' = 1, aaa = 000), opcode `6F`,
/// ModRM `C1`. Nothing in `jit/src` emits EVEX today — this is the tripwire
/// case, and the encoding is checked against the SDM rather than against an
/// emitter, because there is no emitter to check it against.
#[test]
fn an_evex_instruction_is_detected() {
    let _g = guard();
    let p = profile_all("vmovdqa32", &[0x62, 0xF1, 0x7D, 0x48, 0x6F, 0xC1]);
    assert_eq!(p.instructions, 1);
    assert_eq!(p.count(BodyCapability::Evex), 1);
    assert_eq!(
        p.count(BodyCapability::Vex),
        0,
        "EVEX must not also read as VEX"
    );
}

/// `F3 AA` — REP STOSB, the exact two bytes
/// `emit_bulk_zero_byte_fill_preheader` emits (`x64/simd.rs`:911).
#[test]
fn rep_stosb_is_detected() {
    let _g = guard();
    let p = profile_all("rep stosb", &[0xF3, 0xAA]);
    assert_eq!(p.instructions, 1);
    assert_eq!(p.count(BodyCapability::RepString), 1);
}

/// The other repeat-prefix orderings the backend actually emits.
///
/// `66 F3 AB` is REP STOSW — operand-size prefix BEFORE the repeat prefix.
/// `F3 48 AB` is REP STOSQ — repeat prefix, then REX.W, then the opcode, which
/// is the only legal order for a REX. `FC F3 A4` is CLD then REP MOVSB, the
/// arraycopy shape. All three are emitted by `x64/op_invoke.rs`; a decoder that
/// only accepts one prefix order silently misses two of the three.
#[test]
fn every_repeat_prefix_ordering_the_backend_emits_is_detected() {
    let _g = guard();
    for (name, code, insns) in [
        ("rep stosw", &[0x66, 0xF3, 0xAB][..], 1usize),
        ("rep stosq", &[0xF3, 0x48, 0xAB][..], 1),
        ("cld; rep movsb", &[0xFC, 0xF3, 0xA4][..], 2),
    ] {
        let p = profile_all(name, code);
        assert_eq!(p.instructions, insns, "{name}: wrong instruction count");
        assert_eq!(
            p.count(BodyCapability::RepString),
            1,
            "{name}: not counted as a REP string instruction"
        );
    }
}

/// The two `MOVABS` constants `emit_byte_sieve_preheader` materialises
/// (`x64/simd.rs`:1058-1059), preceded by the `PUSH RDI` / `PUSH RSI` that save
/// their Java-local homes and followed by the `ADD RAX, header` that the
/// emitter does next.
///
/// `48 BF` is `REX.W + B8+7` (RDI); `48 BE` is `REX.W + B8+6` (RSI). Each
/// carries a full eight-byte immediate, which is what makes `MOV r64, imm64`
/// the one opcode in the one-byte map whose immediate width `REX.W` changes.
#[test]
fn the_sieve_swar_constants_are_detected() {
    let _g = guard();
    let code = [
        0x57, // push rdi
        0x56, // push rsi
        0x48, 0xBF, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, // movabs rdi, 0x0101010101010101
        0x48, 0xBE, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
        0x80, // movabs rsi, 0x8080808080808080
        0x48, 0x83, 0xC0, 0x10, // add rax, 16
    ];
    let p = profile_all("sieve swar preamble", &code);
    assert_eq!(p.instructions, 5);
    assert_eq!(p.count(BodyCapability::SwarZeroByteScan), 2);
    assert_eq!(p.count(BodyCapability::Vex), 0);
}

/// The tail of `emit_bulk_zero_byte_fill_preheader`, decoded end to end.
///
/// A whole-sequence test rather than a single instruction, because the risk the
/// decoder carries is losing sync partway and reporting a capability at the
/// wrong place — which a one-instruction test cannot see.
#[test]
fn the_bulk_zero_fill_sequence_decodes_and_yields_exactly_one_finding() {
    let _g = guard();
    let code = [
        0x57, // push rdi
        0x48, 0x89, 0xC7, // mov rdi, rax
        0x4C, 0x01, 0xD7, // add rdi, r10
        0x48, 0x83, 0xC7, 0x10, // add rdi, 16
        0x31, 0xC0, // xor eax, eax
        0xF3, 0xAA, // rep stosb
        0x5F, // pop rdi
    ];
    let p = profile_all("bulk zero fill", &code);
    assert_eq!(p.instructions, 7);
    assert_eq!(p.len, code.len());
    assert_eq!(p.count(BodyCapability::RepString), 1);
}

// ---------------------------------------------------------------------------
// The false positives that make a substring search useless
// ---------------------------------------------------------------------------

/// The headline negative: `B8 F3 00 00 00` is `MOV EAX, 0xF3`.
///
/// A substring search for `F3` flags it as a `REP` prefix. It is an immediate
/// byte. Nothing about it is a string instruction, and the whole instruction is
/// five bytes long.
#[test]
fn an_f3_immediate_byte_is_not_a_rep_prefix() {
    let _g = guard();
    let p = profile_all("mov eax, 0xF3", &[0xB8, 0xF3, 0x00, 0x00, 0x00]);
    assert_eq!(p.instructions, 1);
    assert_eq!(
        p.count(BodyCapability::RepString),
        0,
        "an immediate byte was read as a prefix — this is the failure mode the \
         whole length decoder exists to avoid"
    );
}

/// The same for every escape byte, in every position a linear sweep can meet
/// them: an immediate, a displacement, and a SIB byte.
#[test]
fn escape_bytes_in_operands_are_not_escapes() {
    let _g = guard();
    let cases: [(&str, &[u8], usize); 6] = [
        // MOV EAX, 0x62 — `62` as an immediate byte.
        ("mov eax, 0x62", &[0xB8, 0x62, 0x00, 0x00, 0x00][..], 1),
        // MOV EAX, [RAX + 0x0000C5C4] — ModRM 80 = mod 10 / reg EAX / rm RAX,
        // so the next four bytes are a disp32 holding both VEX escape bytes.
        (
            "mov eax, [rax+0xC5C4]",
            &[0x8B, 0x80, 0xC4, 0xC5, 0x00, 0x00][..],
            1,
        ),
        // MOV EAX, [RAX + 0xF3] — `F3` as a displacement byte.
        (
            "mov eax, [rax+0xF3]",
            &[0x8B, 0x80, 0xF3, 0x00, 0x00, 0x00][..],
            1,
        ),
        // MOV EAX, [RDX + 0x10] — ModRM 44 = mod 01 / rm 100 selects a SIB,
        // and the SIB byte is `62` (scale 1, no index, base RDX).
        (
            "mov eax, [rdx+0x10] via SIB 0x62",
            &[0x8B, 0x44, 0x62, 0x10][..],
            1,
        ),
        // POPCNT EAX, ECX — `F3` here is a MANDATORY prefix selecting the
        // opcode, not a repeat prefix, and `0F B8` is not a string opcode.
        ("popcnt eax, ecx", &[0xF3, 0x0F, 0xB8, 0xC1][..], 1),
        // ENDBR64 — likewise `F3`-prefixed and likewise not a repeat.
        ("endbr64", &[0xF3, 0x0F, 0x1E, 0xFA][..], 1),
    ];
    for (name, code, insns) in cases {
        let p = profile_all(name, code);
        assert_eq!(p.instructions, insns, "{name}: wrong instruction count");
        for &cap in BodyCapability::ALL {
            assert_eq!(
                p.count(cap),
                0,
                "{name}: reported `{}` for a byte that is an operand, not a prefix",
                cap.name()
            );
        }
    }
}

/// `F3 90` is `PAUSE`. The `F3` is part of the opcode; `90` is `NOP`/`XCHG`,
/// not a string instruction. Counting it would flag the spin hint in every
/// monitor-enter path in the VM.
#[test]
fn pause_is_not_a_rep_string_instruction() {
    let _g = guard();
    let p = profile_all("pause", &[0xF3, 0x90]);
    assert_eq!(p.instructions, 1);
    assert_eq!(p.count(BodyCapability::RepString), 0);
}

/// A near-miss SWAR constant, and the same constant without `REX.W`.
///
/// `48 BF 02 01 ..` differs from the sieve's constant in one byte; `BF 01 01
/// 01 01` is a 32-bit `MOV EDI, 0x01010101`, which is a different instruction
/// with a different immediate width. Neither is the sieve.
#[test]
fn a_near_miss_constant_is_not_the_swar_signature() {
    let _g = guard();
    let near = [0x48, 0xBF, 0x02, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01];
    let narrow = [0xBF, 0x01, 0x01, 0x01, 0x01];
    for (name, code) in [
        ("movabs near-miss", &near[..]),
        ("mov edi, imm32", &narrow[..]),
    ] {
        let p = profile_all(name, code);
        assert_eq!(p.instructions, 1, "{name}: wrong length");
        assert_eq!(
            p.count(BodyCapability::SwarZeroByteScan),
            0,
            "{name}: matched the sieve signature it is not"
        );
    }
}

/// A prologue-shaped body that a substring search would flag six times.
///
/// It contains `F3` in a displacement, and `C5`, `C4`, `62` and `F3` all inside
/// one 32-bit immediate — and then two genuine VEX instructions. The point is
/// both halves at once: the sweep must stay in sync across the decoys AND still
/// find the real thing after them.
#[test]
fn a_mixed_body_finds_the_real_vex_and_none_of_the_decoys() {
    let _g = guard();
    let code = [
        0x55, // push rbp
        0x48, 0x89, 0xE5, // mov rbp, rsp
        0x48, 0x83, 0xEC, 0x20, // sub rsp, 0x20
        0x31, 0xC0, // xor eax, eax
        0x8B, 0x80, 0xF3, 0x00, 0x00, 0x00, // mov eax, [rax+0xF3]
        0xB8, 0xC5, 0xC4, 0x62, 0xF3, // mov eax, 0xF362C4C5
        0xC5, 0xFD, 0xFE, 0xC1, // vpaddd ymm0, ymm0, ymm1
        0xC5, 0xF8, 0x77, // vzeroupper
        0x48, 0x89, 0xEC, // mov rsp, rbp
        0x5D, // pop rbp
        0xC3, // ret
    ];
    let decoys = code.iter().filter(|&&b| b == 0xF3).count()
        + code.iter().filter(|&&b| b == 0xC4).count()
        + code.iter().filter(|&&b| b == 0x62).count();
    assert!(
        decoys >= 4,
        "the fixture lost its decoy bytes and no longer tests anything"
    );

    let p = profile_all("mixed body", &code);
    assert_eq!(p.instructions, 11);
    assert_eq!(p.len, 33);
    assert_eq!(
        p.count(BodyCapability::Vex),
        2,
        "expected VPADDD and VZEROUPPER and nothing else"
    );
    assert_eq!(p.count(BodyCapability::RepString), 0);
    assert_eq!(p.count(BodyCapability::Evex), 0);
}

// ---------------------------------------------------------------------------
// Failing closed
// ---------------------------------------------------------------------------

/// A truncated instruction at the end of the buffer reports `undecodable`
/// rather than panicking or guessing a length.
///
/// Both halves matter. `C5 FD FE` is a VEX instruction missing its ModRM: a
/// decoder that guessed would report a VEX finding from a body it never
/// finished reading. `90 B8 01` is a `NOP` followed by a `MOV EAX, imm32` with
/// two of its four immediate bytes missing: the stop offset must be 1, the
/// start of the instruction that failed, not 0 and not 3.
#[test]
fn a_truncated_instruction_reports_undecodable() {
    let _g = guard();

    let p = profile_body(&[0xC5, 0xFD, 0xFE]);
    let stop = p.undecodable.expect("a truncated VEX must not decode");
    assert_eq!(stop.offset, 0);
    assert_eq!(stop.reason, DecodeStopReason::Truncated);

    let p = profile_body(&[0x90, 0xB8, 0x01, 0x02]);
    let stop = p
        .undecodable
        .expect("a truncated MOV imm32 must not decode");
    assert_eq!(
        stop.offset, 1,
        "the stop offset must name the instruction that failed, not the byte \
         the reader ran out at"
    );
    assert_eq!(stop.reason, DecodeStopReason::Truncated);
    assert_eq!(p.instructions, 1, "the NOP before it did decode");
}

/// An opcode with no entry in the table stops the walk and names the byte.
///
/// `06` is `PUSH ES`, invalid in 64-bit mode. Meeting one almost always means
/// the walk is out of sync, so skipping a byte and carrying on would turn one
/// wrong answer into a whole body of them.
#[test]
fn an_untabled_opcode_stops_the_walk() {
    let _g = guard();
    let p = profile_body(&[0x90, 0x06, 0xC5, 0xFD, 0xFE, 0xC1]);
    let stop = p
        .undecodable
        .expect("PUSH ES is not decodable in 64-bit mode");
    assert_eq!(stop.offset, 1);
    assert_eq!(stop.reason, DecodeStopReason::UnknownOpcode(0x06));
    assert_eq!(
        p.count(BodyCapability::Vex),
        0,
        "the VEX instruction after the stop must not be counted — the walk \
         never reached it in sync"
    );
}

/// The inline jump table of a large `tableswitch` is data, and the walk says so
/// instead of decoding it.
///
/// The fixture is the dispatch sequence `x64/op_control.rs` emits —
/// `LEA RDX, [RIP+disp32]`, `MOVSXD RCX, [RDX+RAX*4]`, `ADD RCX, RDX`,
/// `JMP RCX` — followed by one four-byte table entry whose first byte is `C5`.
/// A sweep that ran on would report a VEX instruction inside a jump table,
/// which is exactly the noise this module refuses to generate.
#[test]
fn an_inline_jump_table_is_refused_rather_than_decoded() {
    let _g = guard();
    let code = [
        0x48, 0x8D, 0x15, 0x09, 0x00, 0x00, 0x00, // lea rdx, [rip+9]
        0x48, 0x63, 0x0C, 0x82, // movsxd rcx, [rdx+rax*4]
        0x48, 0x01, 0xD1, // add rcx, rdx
        0xFF, 0xE1, // jmp rcx
        0xC5, 0x00, 0x00, 0x00, // jump table entry, not an instruction
    ];
    let p = profile_body(&code);
    let stop = p.undecodable.expect("the jump table must not be decoded");
    assert_eq!(
        stop.reason,
        DecodeStopReason::InlineJumpTable,
        "the stop must name the jump table, not a generic decode failure — an \
         operator reading the census needs to know this body is structurally \
         unscannable rather than that the table is missing an opcode"
    );
    assert_eq!(
        stop.offset, 16,
        "the walk decoded the 7-byte LEA and then the 9-byte dispatch tail"
    );
    assert_eq!(
        p.count(BodyCapability::Vex),
        0,
        "the `C5` in the jump table was decoded as a VEX prefix"
    );
}

// ---------------------------------------------------------------------------
// regressions(): asymmetry, and blindness
// ---------------------------------------------------------------------------

/// `C5 FD FE C1` — a vectorised body. Stands in for a single-pass body that
/// took one of the SIMD lowerings.
const VECTOR_BODY: [u8; 4] = [0xC5, 0xFD, 0xFE, 0xC1];

/// `0F AF C1` — `IMUL EAX, ECX`. Stands in for the scalar IR body that
/// replaced it, which is the `cov-02` shape.
const SCALAR_BODY: [u8; 3] = [0x0F, 0xAF, 0xC1];

/// The finding the module exists for: the baseline vectorises and the
/// optimizing body does not.
#[test]
fn losing_vex_in_the_optimizing_body_is_a_finding() {
    let _g = guard();
    let baseline = profile_all("vector baseline", &VECTOR_BODY);
    let optimizing = profile_all("scalar optimizing", &SCALAR_BODY);
    assert_eq!(
        regressions(&baseline, &optimizing),
        vec![BodyCapability::Vex]
    );
}

/// And the direction that must NOT fire. An optimizing body that vectorises
/// where the baseline did not is the optimizing tier working; reporting it
/// would make the harness fire on every success it is supposed to permit.
#[test]
fn gaining_vex_in_the_optimizing_body_is_not_a_finding() {
    let _g = guard();
    let baseline = profile_all("scalar baseline", &SCALAR_BODY);
    let optimizing = profile_all("vector optimizing", &VECTOR_BODY);
    assert!(
        regressions(&baseline, &optimizing).is_empty(),
        "the comparison is not asymmetric; every vectorised method would be \
         reported as a regression"
    );
}

/// Several losses come back in `ALL` order, so a diff of two corpus runs is
/// stable rather than dependent on iteration order.
#[test]
fn multiple_losses_are_reported_in_a_stable_order() {
    let _g = guard();
    // VPADDD then REP STOSB: both capabilities in one body.
    let both = [0xC5, 0xFD, 0xFE, 0xC1, 0xF3, 0xAA];
    let baseline = profile_all("vector + rep", &both);
    let optimizing = profile_all("scalar", &SCALAR_BODY);
    assert_eq!(
        regressions(&baseline, &optimizing),
        vec![BodyCapability::Vex, BodyCapability::RepString]
    );
}

/// A body the walk could not finish yields no finding in either direction.
///
/// Under-counting the baseline would silently miss a real loss; under-counting
/// the optimizing body would invent one. Neither is a finding, and the operator
/// learns about it from the blind counter rather than from silence.
#[test]
fn an_undecodable_body_on_either_side_yields_no_finding() {
    let _g = guard();
    let blind = profile_body(&[0x06]);
    assert!(blind.is_undecodable());
    let vector = profile_all("vector", &VECTOR_BODY);
    let scalar = profile_all("scalar", &SCALAR_BODY);

    assert!(
        regressions(&blind, &scalar).is_empty(),
        "an undecodable baseline must not produce a finding"
    );
    assert!(
        regressions(&vector, &blind).is_empty(),
        "an undecodable optimizing body must not produce a finding"
    );
}

// ---------------------------------------------------------------------------
// compare_bodies() and the census
// ---------------------------------------------------------------------------

fn method() -> MethodId<'static> {
    MethodId {
        class_name: "CratonBench",
        method_name: "sieve",
        descriptor: "([ZI)I",
    }
}

/// One real finding moves exactly the counters it should and no others.
#[test]
fn a_finding_moves_the_census() {
    let _g = guard();
    let before = backend_parity_census();
    let verdict = compare_bodies(method(), &VECTOR_BODY, &SCALAR_BODY);
    let after = backend_parity_census();

    assert_eq!(verdict.lost, vec![BodyCapability::Vex]);
    assert!(!verdict.is_blind());
    assert_eq!(verdict.baseline.count(BodyCapability::Vex), 1);
    assert_eq!(verdict.optimizing.count(BodyCapability::Vex), 0);

    assert_eq!(after.bodies_profiled - before.bodies_profiled, 2);
    assert_eq!(after.bodies_undecodable - before.bodies_undecodable, 0);
    assert_eq!(after.comparisons - before.comparisons, 1);
    assert_eq!(after.comparisons_blind - before.comparisons_blind, 0);
    assert_eq!(
        after.findings_for(BodyCapability::Vex) - before.findings_for(BodyCapability::Vex),
        1
    );
    assert_eq!(
        after.findings_for(BodyCapability::RepString)
            - before.findings_for(BodyCapability::RepString),
        0,
        "a capability nobody lost was counted"
    );
    assert_eq!(
        after.comparisons_sighted() - before.comparisons_sighted(),
        1
    );
}

/// A blind comparison bumps the blind counters and no finding counter.
///
/// This is the distinction the whole module turns on: "no finding" and "not
/// checked" must not read the same to an operator.
#[test]
fn a_blind_comparison_is_counted_separately_and_finds_nothing() {
    let _g = guard();
    let before = backend_parity_census();
    // The baseline is a truncated VEX instruction: the walk stops before it
    // can establish anything, even though a substring search would "see" VEX.
    let verdict = compare_bodies(method(), &[0xC5, 0xFD, 0xFE], &SCALAR_BODY);
    let after = backend_parity_census();

    assert!(verdict.is_blind());
    assert!(
        verdict.lost.is_empty(),
        "a body the harness could not read produced a finding"
    );
    assert_eq!(after.bodies_profiled - before.bodies_profiled, 2);
    assert_eq!(after.bodies_undecodable - before.bodies_undecodable, 1);
    assert_eq!(after.comparisons - before.comparisons, 1);
    assert_eq!(after.comparisons_blind - before.comparisons_blind, 1);
    assert_eq!(
        after.comparisons_sighted() - before.comparisons_sighted(),
        0,
        "a blind comparison was counted as a sighted one, which is what would \
         let an operator read zero findings as a clean bill of health"
    );
    for &cap in BodyCapability::ALL {
        assert_eq!(
            after.findings_for(cap) - before.findings_for(cap),
            0,
            "`{}` was counted from a body that never finished decoding",
            cap.name()
        );
    }
}

/// Identical bodies are no finding at all, and are counted as sighted.
#[test]
fn identical_bodies_produce_no_finding() {
    let _g = guard();
    let before = backend_parity_census();
    let verdict = compare_bodies(method(), &VECTOR_BODY, &VECTOR_BODY);
    let after = backend_parity_census();
    assert!(verdict.lost.is_empty());
    assert!(!verdict.is_blind());
    assert_eq!(
        after.comparisons_sighted() - before.comparisons_sighted(),
        1
    );
    for &cap in BodyCapability::ALL {
        assert_eq!(after.findings_for(cap) - before.findings_for(cap), 0);
    }
}

/// The method identity a finding prints is the JVM-style one a bail log and a
/// perf-gate results directory already use, so the two can be grepped together.
#[test]
fn a_method_id_prints_the_jvm_style_identity() {
    let _g = guard();
    assert_eq!(method().to_string(), "CratonBench.sieve([ZI)I");
}
