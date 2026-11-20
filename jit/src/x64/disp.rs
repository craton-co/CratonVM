// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    ///
    /// Test-only since round 12 wave 3: its one production caller was the
    /// deleted `x64/vec_emit.rs`. The emitters use [`Disp::bytes`].
    #[cfg(test)]
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
mod tests;
