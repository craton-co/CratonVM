//! CRC32 / CRC32C intrinsic family — BAIL verification.
//!
//! The CRC32 intrinsic family deliberately registers nothing. This test
//! pins that decision so a future change that accidentally wires up a
//! CRC32 sentinel without a correctness oracle fails loudly.
//!
//! Why the family bails (see the `INTRINSIC REGION: CRC32` block in
//! `jit/src/lib.rs::try_resolve_intrinsic` for the full rationale):
//!
//!  * `java/util/zip/CRC32C` — the ONLY target the x86 `CRC32` instruction
//!    could accelerate, because that instruction computes the Castagnoli
//!    CRC-32C (poly 0x1EDC6F41) — has NO implementation anywhere in this
//!    VM. `native-builtins` registers natives for `java/util/zip/CRC32`
//!    only. With no native CRC32C oracle there is nothing to differentially
//!    test against, and the class is never instantiated.
//!
//!  * `java/util/zip/CRC32` uses the IEEE 802.3 reflected polynomial
//!    0xEDB88320, which the hardware `CRC32` instruction CANNOT compute.
//!    A correct inline path would need a 256-entry static table or
//!    PCLMULQDQ folding. Worse, the running-crc field layout is not
//!    statically known to the JIT: synthetic mode stores a complemented
//!    `Value::Long` in slot 0, real-JDK mode stores an `int` at an
//!    unknown offset and `update([BII)V` is pure Java bytecode. Threading
//!    the crc through the receiver field would be unsound.
//!
//! Correctness over coverage: bailing is the roadmap-sanctioned outcome.

use cratonvm_jit::try_resolve_intrinsic;

/// Every CRC32 / CRC32C instance method the brief targets must resolve to
/// `None` — the matcher registers no intrinsic for this family.
#[test]
fn crc32_family_registers_nothing() {
    // java.util.zip.CRC32 — IEEE polynomial, cannot use hardware CRC32.
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32", "update", "(I)V"),
        None,
        "CRC32.update(I)V must NOT be registered: IEEE poly, hardware \
         CRC32 instruction computes the wrong (Castagnoli) polynomial",
    );
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32", "update", "([BII)V"),
        None,
        "CRC32.update([BII)V must NOT be registered: IEEE poly + the \
         running-crc field layout is not statically known to the JIT",
    );

    // java.util.zip.CRC32C — hardware-eligible polynomial, but no native
    // implementation / oracle exists in this VM, and the class is never
    // instantiated. Nothing to be bit-identical to.
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32C", "update", "(I)V"),
        None,
        "CRC32C.update(I)V must NOT be registered: no native CRC32C \
         oracle exists in native-builtins",
    );
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32C", "update", "([BII)V"),
        None,
        "CRC32C.update([BII)V must NOT be registered: no native CRC32C \
         oracle exists in native-builtins",
    );

    // Also the single-arg full-array form and getValue, for completeness.
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32", "update", "([B)V"),
        None,
    );
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32C", "update", "([B)V"),
        None,
    );
    assert_eq!(
        try_resolve_intrinsic("java/util/zip/CRC32", "getValue", "()J"),
        None,
    );
}

/// Sanity check that the matcher itself is alive — a non-CRC32 intrinsic
/// still resolves. This ensures the `None` results above are genuine
/// "not registered" answers, not a matcher that returns `None` for
/// everything (which would make the assertions above vacuous).
#[test]
fn matcher_still_resolves_a_known_intrinsic() {
    assert!(
        try_resolve_intrinsic("java/lang/Math", "sqrt", "(D)D").is_some(),
        "control: Math.sqrt is a registered intrinsic — if this fails the \
         CRC32 None-assertions are vacuous",
    );
}
