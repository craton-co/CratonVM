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
