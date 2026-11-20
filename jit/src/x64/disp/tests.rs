use super::*;
use cratonvm_types::{
    ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET,
    FIELD_CELL_TAG_OFFSET, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE,
};

// ── boundaries ────────────────────────────────────────────────────────

/// The acceptance boundary set: −129 through +256, plus the i32 edges and
/// the first values with no encoding at all.
#[test]
fn encode_picks_the_smallest_legal_form_at_every_boundary() {
    assert_eq!(Disp::encode(-129), Ok(Disp::Disp32(-129)));
    assert_eq!(Disp::encode(-128), Ok(Disp::Disp8(-128)));
    assert_eq!(Disp::encode(-1), Ok(Disp::Disp8(-1)));
    assert_eq!(Disp::encode(0), Ok(Disp::None));
    assert_eq!(Disp::encode(1), Ok(Disp::Disp8(1)));
    assert_eq!(Disp::encode(127), Ok(Disp::Disp8(127)));
    // The whole point of the type: 128 is NOT a disp8. `128 as u8` is 0x80,
    // which the CPU reads back as -128.
    assert_eq!(Disp::encode(128), Ok(Disp::Disp32(128)));
    assert_eq!(Disp::encode(256), Ok(Disp::Disp32(256)));
    assert_eq!(Disp::encode(i32::MIN as i64), Ok(Disp::Disp32(i32::MIN)));
    assert_eq!(Disp::encode(i32::MAX as i64), Ok(Disp::Disp32(i32::MAX)));
}

#[test]
fn values_past_disp32_have_no_encoding_and_must_error() {
    for v in [
        i32::MAX as i64 + 1,
        i32::MIN as i64 - 1,
        i64::MAX,
        i64::MIN,
        u32::MAX as i64,
    ] {
        assert_eq!(
            Disp::encode(v),
            Err(DispOutOfRange { value: v }),
            "{v} must be rejected, not truncated"
        );
        assert_eq!(
            Disp::encode32(v),
            Err(DispOutOfRange { value: v }),
            "{v} must be rejected by the forced-32-bit path too"
        );
        assert_eq!(
            Disp::encode_for_base(v, super::super::RBP),
            Err(DispOutOfRange { value: v })
        );
    }
}

/// `128 as u8` is the exact bug this module exists to prevent. Pin the
/// arithmetic so nobody "simplifies" the range check back to unsigned
/// reasoning.
#[test]
fn the_unsigned_narrowing_this_replaces_would_address_backwards() {
    let bad = 128u8 as i8; // what `HEADER_SIZE as u8` would produce at 128
    assert_eq!(
        bad, -128,
        "0x80 decodes as -128, i.e. 128 bytes BEFORE base"
    );
    assert!(
        Disp::encode(128).unwrap().as_disp8().is_none(),
        "the checked encoder must refuse to call 128 a disp8"
    );
}

#[test]
fn encode32_always_forces_the_wide_form() {
    for v in [
        0i64,
        1,
        -1,
        127,
        -128,
        128,
        i32::MAX as i64,
        i32::MIN as i64,
    ] {
        let d = Disp::encode32(v).expect("in i32 range");
        assert_eq!(d.mod_bits(), 0b10, "encode32({v}) must stay mod=10");
        assert_eq!(d.byte_len(), 4, "encode32({v}) must stay four bytes");
        assert_eq!(d.value(), v);
    }
}

// ── byte_len / mod_bits agreement ─────────────────────────────────────

#[test]
fn byte_len_and_mod_bits_never_disagree() {
    // Sweep both the near range exhaustively and the far range by sample:
    // a disagreement here is an instruction whose operand bytes are
    // misparsed by the CPU from the ModRM byte onwards.
    let mut values: Vec<i64> = (-600..=600).collect();
    values.extend([i32::MIN as i64, i32::MAX as i64, 65_536, -65_536]);
    for v in values {
        let d = Disp::encode(v).expect("in i32 range");
        let expect_len = match d.mod_bits() {
            0b00 => 0,
            0b01 => 1,
            0b10 => 4,
            other => unreachable!("illegal mod field {other}"),
        };
        assert_eq!(d.byte_len(), expect_len, "mod/len disagree for {v}");
        assert_eq!(d.value(), v, "encoding lost the value for {v}");

        let mut out = Vec::new();
        d.emit_into(&mut out);
        assert_eq!(out.len(), d.byte_len(), "emitted width wrong for {v}");
        let (buf, len) = d.bytes();
        assert_eq!(&buf[..len], out.as_slice(), "bytes()/emit_into disagree");
    }
}

/// Emitted bytes must decode back to the displacement asked for — the
/// property an `as u8` cast breaks.
#[test]
fn emitted_bytes_round_trip_through_a_signed_decode() {
    for v in [-32_768i64, -129, -128, -1, 0, 1, 127, 128, 256, 100_000] {
        let d = Disp::encode(v).expect("in i32 range");
        let mut out = Vec::new();
        d.emit_into(&mut out);
        let decoded = match out.len() {
            0 => 0i64,
            1 => out[0] as i8 as i64,
            4 => i32::from_le_bytes([out[0], out[1], out[2], out[3]]) as i64,
            n => unreachable!("illegal displacement width {n}"),
        };
        assert_eq!(decoded, v, "the CPU would read {decoded}, not {v}");
    }
}

// ── addressing special cases ──────────────────────────────────────────

/// RBP/R13 have no `mod=00` form; a zero displacement must still emit a
/// `disp8` of zero or the operand becomes RIP-relative.
#[test]
fn rbp_and_r13_never_get_the_zero_displacement_form() {
    for base in [super::super::RBP, super::super::R13] {
        assert!(base_requires_displacement(base), "base {base}");
        assert_eq!(
            Disp::encode_for_base(0, base),
            Ok(Disp::Disp8(0)),
            "[{base}] with mod=00 would decode as RIP-relative"
        );
        assert_eq!(Disp::encode_for_base(0, base).unwrap().mod_bits(), 0b01);
        assert_eq!(Disp::encode_for_base(0, base).unwrap().byte_len(), 1);
    }
    // Every other base keeps the compact form.
    for base in [
        super::super::RAX,
        super::super::RCX,
        super::super::RSP,
        super::super::R12,
        super::super::R11,
    ] {
        assert!(!base_requires_displacement(base), "base {base}");
        assert_eq!(Disp::encode_for_base(0, base), Ok(Disp::None));
    }
    // Non-zero displacements are unaffected by the rule.
    assert_eq!(
        Disp::encode_for_base(8, super::super::RBP),
        Ok(Disp::Disp8(8))
    );
    assert_eq!(
        Disp::encode_for_base(1024, super::super::R13),
        Ok(Disp::Disp32(1024))
    );
}

/// RSP/R12 are the SIB rule, and it is *independent* of the RBP/R13 rule.
#[test]
fn rsp_and_r12_are_the_sib_bases_and_only_those() {
    for base in [super::super::RSP, super::super::R12] {
        assert!(base_requires_sib(base), "base {base} needs a SIB byte");
        assert!(
            !base_requires_displacement(base),
            "the SIB rule and the displacement rule are different registers"
        );
    }
    for base in [
        super::super::RAX,
        super::super::RBP,
        super::super::R13,
        super::super::R11,
        super::super::RDI,
    ] {
        assert!(!base_requires_sib(base), "base {base}");
    }
}

// ── ModRM bytes match the hand-written literals in x64.rs ─────────────

/// `Disp::modrm` must reproduce, bit for bit, the ModRM literals the
/// emitter has always written by hand. If it does not, routing a call site
/// through this module changes the instruction it emits.
#[test]
fn modrm_reproduces_the_literals_x64_hand_wrote() {
    const RM_RBP: u8 = 0b101;
    const RM_SIB: u8 = 0b100;

    // `modrm_rbp_disp` / `emit_load_caller_arg`: reg=0 over [RBP].
    assert_eq!(Disp::Disp8(0).modrm(0, RM_RBP), 0x45);
    assert_eq!(Disp::Disp32(0).modrm(0, RM_RBP), 0x85);
    // …with a non-zero reg field folded in, as `0x45 | (reg << 3)` did.
    for reg in 0u8..8 {
        assert_eq!(Disp::Disp8(0).modrm(reg, RM_RBP), 0x45 | (reg << 3));
        assert_eq!(Disp::Disp32(0).modrm(reg, RM_RBP), 0x85 | (reg << 3));
    }

    // `emit_mov_rsp_disp_from_reg`: [RSP + disp] via SIB, all three widths.
    for reg in 0u8..8 {
        assert_eq!(Disp::None.modrm(reg, RM_SIB), 0x04 | (reg << 3));
        assert_eq!(Disp::Disp8(0).modrm(reg, RM_SIB), 0x44 | (reg << 3));
        assert_eq!(Disp::Disp32(0).modrm(reg, RM_SIB), 0x84 | (reg << 3));
    }

    // The inline array emitters' `0x44` / `0x54` ModRM + SIB + disp8 shape
    // (`MOVSXD RAX, [RAX + RCX*4 + HEADER_SIZE]` and friends).
    assert_eq!(Disp::Disp8(0).modrm(0, RM_SIB), 0x44); // reg=RAX
    assert_eq!(Disp::Disp8(0).modrm(2, RM_SIB), 0x54); // reg=RDX

    // `emit_test_mem8_imm8`: `0xF6 /0` with mod=01 over a plain base.
    for base in 0u8..8 {
        assert_eq!(Disp::Disp8(0).modrm(0, base), 0x40 | base);
    }

    // The disp32 field accessors (`emit_mov_r64_mem_disp32` et al).
    for reg in 0u8..8 {
        for base in 0u8..8 {
            assert_eq!(
                Disp::Disp32(0).modrm(reg, base),
                0x80 | (reg << 3) | base,
                "reg={reg} base={base}"
            );
        }
    }
}

/// `mod` is a two-bit field; `modrm` must never let it bleed into `reg`.
#[test]
fn modrm_masks_reg_and_rm_to_three_bits() {
    // R11 (11) and R13 (13) as bases: the fourth bit belongs to REX.B and
    // must not appear in the ModRM byte.
    assert_eq!(
        Disp::Disp8(0).modrm(super::super::R11, super::super::R13),
        Disp::Disp8(0).modrm(3, 5)
    );
    assert_eq!(Disp::Disp32(0).modrm(0xFF, 0xFF), 0xBF);
}

// ── layout constants ──────────────────────────────────────────────────

/// `disp8_const` is checked, and checked with **signed** reasoning.
#[test]
fn disp8_const_accepts_the_signed_range_only() {
    const H: i8 = disp8_const(HEADER_SIZE as i64);
    assert_eq!(H as i64, HEADER_SIZE as i64);
    assert_eq!(disp8_const(127), 127);
    assert_eq!(disp8_const(-128), -128);
    assert_eq!(disp8_const(0), 0);
    // 128 and 255 are rejected: they are legal `u8`s but not legal disp8s.
    // (Const-evaluated, so the rejection is a build error — it cannot be
    // asserted with `should_panic` without moving the call to runtime.)
    assert!(i8::try_from(128i64).is_err());
    assert!(i8::try_from(255i64).is_err());
}

/// **Every object-layout constant this crate bakes into a displacement,
/// encoded explicitly.**
///
/// The existing tripwires count *occurrences* of these constants in the
/// emitter sources; this one asserts what actually matters — that each
/// value still has a legal encoding in the form the emitter uses for it.
/// A layout change that pushes one past 127 fails here instead of
/// producing an instruction that reads 128 bytes *before* the object.
#[test]
fn every_layout_constant_used_as_a_displacement_is_encodable() {
    // Sites that emit a hard-coded `mod=01` ModRM byte and therefore MUST
    // stay inside signed disp8. Each entry names the emitter shape it
    // feeds, so a failure says which instruction to widen to disp32.
    let disp8_sites: [(&str, i64); 10] = [
        // `emit_int_aload_regs`, `emit_byte_aload_regs`, `emit_ref_aload_regs`,
        // the fastore/dastore MOVSS/MOVSD stores, the array-copy/fill helpers:
        // `[base + index*scale + HEADER_SIZE]`.
        ("HEADER_SIZE (array element base)", HEADER_SIZE as i64),
        // `MOV EDX, [RAX + ARRAY_LENGTH_OFFSET]` — the load behind every
        // inline bounds check.
        (
            "ARRAY_LENGTH_OFFSET (bounds-check load)",
            ARRAY_LENGTH_OFFSET as i64,
        ),
        // `emit_test_mem8_imm8(base, GC_FLAGS_BYTE_OFFSET, GC_FLAG_*)`.
        (
            "GC_FLAGS_BYTE_OFFSET (per-object flag test)",
            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i64,
        ),
        // `CMP BYTE [recv + KIND_TAGS_BYTE_OFFSET], ObjectKind::Object` in the
        // PIC receiver-kind guard.
        (
            "KIND_TAGS_BYTE_OFFSET (PIC kind guard)",
            cratonvm_types::KIND_TAGS_BYTE_OFFSET as i64,
        ),
        // Field-cell payload biases, added on top of a cell address.
        ("FIELD_CELL_TAG_OFFSET", FIELD_CELL_TAG_OFFSET as i64),
        (
            "FIELD_CELL_PAYLOAD32_OFFSET",
            FIELD_CELL_PAYLOAD32_OFFSET as i64,
        ),
        (
            "FIELD_CELL_PAYLOAD64_OFFSET",
            FIELD_CELL_PAYLOAD64_OFFSET as i64,
        ),
        // `emit_matrix_dot_element` computes `HEADER_SIZE + index_delta*8`
        // (or `*4` under narrow oops) for an unroll batch of 8. These are
        // the widest *computed* disp8s in the emitter: at HEADER_SIZE=32
        // they reach 88, and a header grow or a wider unroll walks them
        // straight past 127.
        (
            "matrix-dot b[k] at unroll index 7 (8-byte refs)",
            HEADER_SIZE as i64 + 7 * REF_ELEMENT_SIZE as i64,
        ),
        (
            "matrix-dot b[k] at unroll index 7 (narrow oops)",
            HEADER_SIZE as i64 + 7 * 4,
        ),
        (
            "matrix-dot a[k] at unroll index 7",
            HEADER_SIZE as i64 + 7 * 4,
        ),
    ];
    for (what, value) in disp8_sites {
        let encoded = Disp::encode(value)
            .unwrap_or_else(|e| panic!("{what}: {value} has no encoding at all ({e})"));
        assert!(
            matches!(encoded, Disp::None | Disp::Disp8(_)),
            "{what} is {value}, which no longer fits a SIGNED disp8. The emitter writes a \
             hard-coded mod=01 ModRM byte there, so the instruction would address \
             {} bytes BEFORE the base register. Widen that site to disp32 (mod=10) \
             before changing the layout.",
            -(value as u8 as i8 as i64)
        );
        assert_eq!(encoded.value(), value);
        assert_eq!(disp8_const(value) as i64, value);
    }

    // Sites already emitted as disp32 (`emit_mov_r64_mem_disp32` and the
    // rest of the `*_disp32` family). They only have to fit i32 — but they
    // must not be "optimized" back into a literal disp8 byte, so pin the
    // fact that some of them genuinely exceed 127.
    let disp32_sites: [(&str, i64); 4] = [
        ("legacy field cell 0", HEADER_SIZE as i64),
        (
            "legacy field cell 6 (first one past disp8)",
            HEADER_SIZE as i64 + 6 * SLOT_SIZE as i64,
        ),
        (
            "legacy field cell 63",
            HEADER_SIZE as i64 + 63 * SLOT_SIZE as i64,
        ),
        (
            "compact ref field at body offset 4096",
            HEADER_SIZE as i64 + 4096,
        ),
    ];
    for (what, value) in disp32_sites {
        let encoded = Disp::encode32(value)
            .unwrap_or_else(|e| panic!("{what}: {value} does not fit disp32 ({e})"));
        assert_eq!(encoded.mod_bits(), 0b10, "{what} must stay mod=10");
        assert_eq!(encoded.value(), value);
    }
    // The load-bearing half of the previous block: a legacy object with
    // enough fields already addresses past 127, so the field accessors can
    // never be narrowed to a literal disp8.
    //
    // The bound moved from six fields to seven when HEADER_SIZE went
    // 32 -> 24 on 2026-08-06 (24 + 6*16 = 120 now FITS disp8; 24 + 7*16 =
    // 136 does not). This tripwire is what caught that, which is its whole
    // purpose — it made the shrink re-derive the claim instead of letting
    // "the accessors must be disp32" quietly stop being true.
    assert!(
        i8::try_from(HEADER_SIZE as i64 + 7 * SLOT_SIZE as i64).is_err(),
        "if this ever fits disp8 the disp32 field accessors stopped being load-bearing; \
         re-derive the bound before shrinking them"
    );
}

/// An unencodable displacement must arrive at the compiler report as the
/// dedicated reason, carrying the offending value.
#[test]
fn out_of_range_lifts_to_the_displacement_bailout_reason() {
    let e = Disp::encode(1i64 << 40).expect_err("2^40 has no disp32");
    assert_eq!(e.value, 1i64 << 40);
    let b = e.into_bailout();
    assert_eq!(
        b.reason,
        crate::bailout::BailoutReason::DisplacementOutOfRange { disp: 1i64 << 40 }
    );
    // The `From` impl must agree with the inherent method.
    assert_eq!(crate::bailout::Bailout::from(e), b);
}

/// The register numbers this module's special-case rules are stated in
/// terms of must be the same ones the emitter allocates with.
#[test]
fn special_case_register_numbers_match_the_emitters_tables() {
    assert_eq!(super::super::RSP, 4);
    assert_eq!(super::super::RBP, 5);
    assert_eq!(super::super::R12, 12);
    assert_eq!(super::super::R13, 13);
}
