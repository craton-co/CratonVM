// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Checked x86-64 memory-operand displacement encoding.
//!
//! # Why this module exists (P0: "eliminate unchecked disp8 construction")
//!
//! Before this module, every memory-operand site in the x64 emitter built its
//! own ModRM/SIB/displacement bytes by hand, and several of them narrowed a
//! displacement straight into a literal byte:
//!
//! ```text
//! self.buf.emit_byte(0x44);                 // ModRM: mod=01 (disp8) …
//! self.buf.emit_byte(0x88);                 // SIB
//! self.buf.emit_byte(HEADER_SIZE as u8);    // …and the disp byte
//! ```
//!
//! `as u8` on a value the assembler will read back as a **signed** byte is a
//! silent miscompile the moment the value exceeds 127: `128 as u8 == 0x80`,
//! which the CPU decodes as `-128`, so the instruction addresses memory
//! *before* the object instead of 128 bytes into it. Nothing faults; the wrong
//! bytes are simply read or written.
//!
//! Compile-time assertions elsewhere in the tree (`types/src/heap_types.rs`'s
//! `<= 127` check, `x64.rs`'s `header_size_fits_signed_disp8_and_is_qword_aligned`)
//! constrain today's layout constants, but they do not remove the *encoder*
//! hazard: they say nothing about a displacement computed at codegen time from
//! a frame depth, an unroll index, or a resolved field offset.
//!
//! [`Disp`] is that encoder. It picks the smallest legal form, refuses a value
//! that cannot be expressed at all, and hands back the exact `mod` bits that
//! must go into the ModRM byte, so the `mod` field and the displacement width
//! can no longer disagree.
//!
//! # The two x86 addressing special cases
//!
//! Both are properties of the *base register*, not of the displacement, and
//! both are silent mis-encodings rather than assembler errors:
//!
//! 1. **`base & 7 == 0b101` (RBP, R13) has no `mod=00` form.** In a ModRM byte
//!    with `mod=00`, `r/m=101` does not mean "\[RBP]" — it means RIP-relative
//!    addressing with a disp32 (and inside a SIB byte it means "no base,
//!    disp32"). A zero-displacement `[RBP]` or `[R13]` must therefore still be
//!    encoded as `mod=01` with an explicit `disp8` of `0`. Use
//!    [`Disp::encode_for_base`], which applies this rule, or ask
//!    [`base_requires_displacement`] directly.
//!
//! 2. **`base & 7 == 0b100` (RSP, R12) requires a SIB byte.** In a ModRM byte
//!    `r/m=100` means "a SIB byte follows", so `[RSP + disp]` cannot be spelled
//!    without one; the SIB is `scale=00, index=100 (none), base=100` = `0x24`.
//!    This module does not emit the SIB byte (that is the instruction
//!    emitter's job — the index/scale fields are its business), but
//!    [`base_requires_sib`] answers the question so a caller can assert it
//!    rather than discover it by executing the wrong instruction.
//!
//! Note that these two rules are independent: R12 needs a SIB *and* takes the
//! ordinary `mod=00` no-displacement form (SIB `base=100` is a real base), and
//! R13 needs a displacement *and* no SIB.
//!
//! # Failure handling
//!
//! `x64.rs` must never panic during codegen (see its module-level
//! `deny(clippy::panic)` gate), so every fallible entry point here returns
//! [`DispOutOfRange`] rather than panicking. A void-returning emitter that
//! cannot propagate a `Result` converts the error into a compile bail with
//! `self.buf.mark_overflowed()`, which the `compile` driver already checks
//! (`if buf.overflowed() { return None; }`) to fall back to the interpreter.
//!
//! [`disp8_const`] is the one exception: it is a `const fn` intended for
//! *constant* sites, where a value out of disp8 range is a build-time error,
//! not a runtime condition. Call it only in a `const` context.
//!
//! ## Relationship to `crate::bailout`
//!
//! [`DispOutOfRange::into_bailout`] converts an unencodable displacement into
//! the JIT's structured `Bailout` (`BailoutReason::DisplacementOutOfRange`), so
//! a caller on a `Result`-carrying path can report *why* the method was
//! declined instead of silently returning `None`. That is the only coupling
//! between the two modules, and it is one-directional: this module has no
//! dependency on the bailout machinery for its own operation, and the
//! single-pass emitter in `x64.rs` — which cannot return a `Result` from its
//! void emitters — still bails via `ExecutableBuffer::mark_overflowed`.

/// A checked x86-64 memory-operand displacement, paired with the ModRM `mod`
/// field that must accompany it.
///
/// Construct with [`Disp::encode`], [`Disp::encode32`] or
/// [`Disp::encode_for_base`] — never by writing the variant directly from a
/// cast, which would reintroduce exactly the hazard this type exists to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disp {
    /// `mod=00` — no displacement bytes follow the ModRM/SIB.
    None,
    /// `mod=01` — one **signed** displacement byte follows.
    Disp8(i8),
    /// `mod=10` — four signed displacement bytes follow, little-endian.
    Disp32(i32),
}

/// A displacement that cannot be expressed in any x86-64 memory operand.
///
/// x86-64 memory displacements are at most 32 bits signed; anything outside
/// `i32::MIN..=i32::MAX` has no encoding at all and the emitter must bail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispOutOfRange {
    /// The displacement that was asked for.
    pub value: i64,
}

impl std::fmt::Display for DispOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "displacement {} does not fit an x86-64 signed disp32 ({}..={})",
            self.value,
            i32::MIN,
            i32::MAX
        )
    }
}

impl std::error::Error for DispOutOfRange {}

impl DispOutOfRange {
    /// Lift this into the JIT's structured compilation bailout.
    ///
    /// A displacement with no encoding is exactly
    /// `BailoutReason::DisplacementOutOfRange`: the method loses its compiled
    /// body and runs interpreted, which is always semantically valid. Use this
    /// on paths that can carry a `Result`; the single-pass `x64.rs` emitters
    /// have void signatures and bail through
    /// `ExecutableBuffer::mark_overflowed` instead.
    pub fn into_bailout(self) -> crate::bailout::Bailout {
        crate::bailout::Bailout::new(crate::bailout::BailoutReason::DisplacementOutOfRange {
            disp: self.value,
        })
    }
}

impl From<DispOutOfRange> for crate::bailout::Bailout {
    fn from(e: DispOutOfRange) -> Self {
        e.into_bailout()
    }
}

impl Disp {
    /// Smallest displacement an x86-64 memory operand can express.
    pub const MIN: i64 = i32::MIN as i64;
    /// Largest displacement an x86-64 memory operand can express.
    pub const MAX: i64 = i32::MAX as i64;

    /// Choose the smallest legal encoding for `value`.
    ///
    /// `0` becomes [`Disp::None`], `-128..=127` becomes [`Disp::Disp8`], and
    /// everything else that fits a signed 32-bit value becomes
    /// [`Disp::Disp32`]. Errors when `value` does not fit `i32`.
    ///
    /// **Callers whose base register may be RBP/R13 must use
    /// [`Disp::encode_for_base`] instead** — `Disp::None` is not a legal
    /// encoding there.
    pub fn encode(value: i64) -> Result<Disp, DispOutOfRange> {
        if value == 0 {
            return Ok(Disp::None);
        }
        if let Ok(v) = i8::try_from(value) {
            return Ok(Disp::Disp8(v));
        }
        match i32::try_from(value) {
            Ok(v) => Ok(Disp::Disp32(v)),
            Err(_) => Err(DispOutOfRange { value }),
        }
    }

    /// Like [`Disp::encode`] but always produces the 32-bit form.
    ///
    /// Needed wherever the instruction's byte length must be known before the
    /// displacement's final value is: RIP-relative operands, patchable sites
    /// whose displacement is back-filled after emission, and the
    /// fixed-shape TLAB/JvmThread accessors whose offsets reach hundreds of
    /// bytes from the struct base. Errors when `value` does not fit `i32`.
    pub fn encode32(value: i64) -> Result<Disp, DispOutOfRange> {
        match i32::try_from(value) {
            Ok(v) => Ok(Disp::Disp32(v)),
            Err(_) => Err(DispOutOfRange { value }),
        }
    }

    /// [`Disp::encode`] plus the RBP/R13 rule: those bases have no `mod=00`
    /// form, so a zero displacement is promoted to an explicit `disp8` of `0`.
    ///
    /// `base` is a full 0..=15 register number; only its low three bits matter.
    /// See the module docs for why `mod=00, r/m=101` means RIP-relative rather
    /// than `[RBP]`.
    pub fn encode_for_base(value: i64, base: u8) -> Result<Disp, DispOutOfRange> {
        let d = Self::encode(value)?;
        if matches!(d, Disp::None) && base_requires_displacement(base) {
            // `[RBP]` / `[R13]` with mod=00 would decode as RIP-relative.
            return Ok(Disp::Disp8(0));
        }
        Ok(d)
    }

    /// The ModRM `mod` field for this displacement: `0b00`, `0b01` or `0b10`.
    pub fn mod_bits(self) -> u8 {
        match self {
            Disp::None => 0b00,
            Disp::Disp8(_) => 0b01,
            Disp::Disp32(_) => 0b10,
        }
    }

    /// Number of displacement bytes that follow the ModRM/SIB: 0, 1 or 4.
    pub fn byte_len(self) -> usize {
        match self {
            Disp::None => 0,
            Disp::Disp8(_) => 1,
            Disp::Disp32(_) => 4,
        }
    }

    /// Append the displacement bytes (little-endian) to `out`.
    pub fn emit_into(self, out: &mut Vec<u8>) {
        match self {
            Disp::None => {}
            // Cast: x86-64 displacement byte — the value is already range-checked
            // as a *signed* i8 by the constructors, so this reinterpretation is
            // the encoding, not a narrowing.
            Disp::Disp8(v) => out.push(v as u8),
            Disp::Disp32(v) => out.extend_from_slice(&v.to_le_bytes()),
        }
    }

    /// The displacement bytes as a fixed buffer plus their length, for callers
    /// that write into something other than a `Vec<u8>` (the JIT's
    /// `ExecutableBuffer`, for instance). `buf[..len]` is the byte sequence.
    pub fn bytes(self) -> ([u8; 4], usize) {
        let mut buf = [0u8; 4];
        match self {
            Disp::None => {}
            // Cast: see `emit_into`.
            Disp::Disp8(v) => buf[0] = v as u8,
            Disp::Disp32(v) => buf.copy_from_slice(&v.to_le_bytes()),
        }
        (buf, self.byte_len())
    }

    /// The displacement value this encoding carries.
    pub fn value(self) -> i64 {
        match self {
            Disp::None => 0,
            Disp::Disp8(v) => v as i64,
            Disp::Disp32(v) => v as i64,
        }
    }

    /// The complete ModRM byte for a *memory* operand using this displacement.
    ///
    /// `reg` is the ModRM `reg` field (an opcode extension for `/n` forms) and
    /// `rm` the `r/m` field — pass `0b100` when a SIB byte follows, which
    /// [`base_requires_sib`] identifies. Only the low three bits of each are
    /// used; REX.R/REX.B carry the fourth bit and remain the caller's job.
    ///
    /// Using this instead of a hand-written literal is what keeps the `mod`
    /// field and the emitted displacement width from drifting apart.
    pub fn modrm(self, reg: u8, rm: u8) -> u8 {
        (self.mod_bits() << 6) | ((reg & 7) << 3) | (rm & 7)
    }

    /// The `disp8` payload, or `None` when this is not the 8-bit form.
    ///
    /// For the handful of sites that emit a hard-coded `mod=01` ModRM byte and
    /// therefore *require* the 8-bit form: they can ask, and bail when the
    /// answer is `None`, instead of truncating.
    pub fn as_disp8(self) -> Option<i8> {
        match self {
            Disp::Disp8(v) => Some(v),
            _ => Option::None,
        }
    }
}

/// Does `base` have no `mod=00` (displacement-free) encoding?
///
/// True for RBP (5) and R13 (13): `r/m=101` with `mod=00` is RIP-relative.
/// Such an operand needs at least a `disp8` of zero.
pub fn base_requires_displacement(base: u8) -> bool {
    base & 7 == 0b101
}

/// Does `base` require a SIB byte to be addressable at all?
///
/// True for RSP (4) and R12 (12): `r/m=100` in ModRM means "SIB follows", so
/// there is no way to name those registers as a base without one. The
/// index-free SIB byte is `0x24` (`scale=00, index=100 (none), base=100`).
pub fn base_requires_sib(base: u8) -> bool {
    base & 7 == 0b100
}

/// Const-context checked narrowing for layout constants.
///
/// Use at sites whose displacement is a compile-time constant (an object header
/// offset, an array element base) so that a layout change which pushes the
/// constant past 127 is a **build failure** rather than an instruction that
/// silently addresses backwards from the object.
///
/// ```ignore
/// const ELEM_BASE: i8 = disp8_const(cratonvm_types::HEADER_SIZE as i64);
/// ```
///
/// Call this only in a `const` context: the range check is a `panic!`, which is
/// a compile error during const evaluation but an ordinary runtime panic if the
/// call is evaluated at run time — and codegen must never panic. For a runtime
/// value use [`Disp::encode`] and handle the `Err`.
// The panic is the const-evaluation failure mechanism; there is no `Result` in
// const context and no other way to reject the value at build time.
#[allow(clippy::panic)]
pub const fn disp8_const(value: i64) -> i8 {
    if value < i8::MIN as i64 || value > i8::MAX as i64 {
        panic!(
            "layout constant does not fit a SIGNED disp8 (-128..=127); the instruction would \
             address memory BEFORE the base register. Switch that emission site to disp32."
        );
    }
    // Cast: range-checked immediately above.
    value as i8
}

#[cfg(test)]
mod tests {
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
}
