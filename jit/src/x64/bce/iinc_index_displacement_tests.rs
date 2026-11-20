use super::*;

// Locals: 0 = array, 1 = i, 2 = n.
const ALOAD_0: u8 = 0x2a;
const ILOAD_1: u8 = 0x1b;
const ICONST_0: u8 = 0x03;
const ISTORE_1: u8 = 0x3c;
const IASTORE: u8 = 0x4f;
const IALOAD: u8 = 0x2e;

/// `i++; a[i] = i;` — the shape that was miscompiled.
///
/// The access reads `i` AFTER the advance, so its index is the induction
/// variable plus one. Reporting a displacement of 0 here is what let the
/// consumer ask the proof about `a[i]` while the code executed `a[i + 1]`,
/// and the emitted `a.length >= n` guard then justified an elision for an
/// index that reaches `n`.
#[test]
fn iinc_before_the_access_displaces_the_index() {
    let code = [
        0x84, 0x01, 0x01,    // iinc 1, 1
        ALOAD_0, // pc 3
        ILOAD_1, // pc 4  <- the index
        ILOAD_1, // pc 5  <- the value
        IASTORE, // pc 6
    ];
    let out = analyze_array_access_operands(&code, 0, code.len(), &|_| None);
    assert_eq!(out.get(&6), Some(&(0, 1, 1)), "expected index = i + 1");
}

/// `a[i] = i; i++;` — the ordinary javac counted-loop shape.
///
/// Must keep a displacement of 0, or the fix would cost the optimization
/// on the case it exists for.
#[test]
fn iinc_after_the_access_leaves_the_index_alone() {
    let code = [
        ALOAD_0, // pc 0
        ILOAD_1, // pc 1
        ILOAD_1, // pc 2
        IASTORE, // pc 3
        0x84, 0x01, 0x01, // iinc 1, 1  (pc 4)
    ];
    let out = analyze_array_access_operands(&code, 0, code.len(), &|_| None);
    assert_eq!(out.get(&3), Some(&(0, 1, 0)), "expected the bare index i");
}

/// `iload i; iinc i, 1; ... iaload` — the index is the value `iload` READ,
/// so an advance between the load and the access must NOT be folded in.
#[test]
fn displacement_is_taken_at_the_iload_not_at_the_access() {
    let code = [
        ALOAD_0, // pc 0
        ILOAD_1, // pc 1  <- index, read before the advance
        0x84, 0x01, 0x01,   // iinc 1, 1  (pc 2)
        IALOAD, // pc 5
    ];
    let out = analyze_array_access_operands(&code, 0, code.len(), &|_| None);
    assert_eq!(out.get(&5), Some(&(0, 1, 0)));
}

/// A `*store` writes an arbitrary value, so the local stops being "the
/// induction variable plus a constant" and no access derived from it can
/// be described at all.
#[test]
fn a_store_to_the_index_local_refuses_the_access() {
    let code = [
        ICONST_0, // pc 0
        ISTORE_1, // pc 1
        ALOAD_0,  // pc 2
        ILOAD_1,  // pc 3
        ILOAD_1,  // pc 4
        IASTORE,  // pc 5
    ];
    let out = analyze_array_access_operands(&code, 0, code.len(), &|_| None);
    assert_eq!(out.get(&5), None, "a clobbered local must not be reported");
}

/// The packed `*store_<n>` forms decode to the right local.
#[test]
fn store_local_decodes_every_packed_form() {
    // istore_0..3 = 0x3b..0x3e, lstore_0..3 = 0x3f..0x42,
    // fstore_0..3 = 0x43..0x46, dstore_0..3 = 0x47..0x4a,
    // astore_0..3 = 0x4b..0x4e.
    for (op, want) in [
        (0x3bu8, 0usize),
        (0x3c, 1),
        (0x3d, 2),
        (0x3e, 3),
        (0x3f, 0),
        (0x42, 3),
        (0x43, 0),
        (0x46, 3),
        (0x47, 0),
        (0x4a, 3),
        (0x4b, 0),
        (0x4e, 3),
    ] {
        assert_eq!(store_local(&[op], 0), Some(want), "opcode {op:#04x}");
    }
    // The operand forms read the following byte.
    assert_eq!(store_local(&[0x36, 0x07], 0), Some(7));
    assert_eq!(store_local(&[0x3a, 0x02], 0), Some(2));
    // Not a store.
    assert_eq!(store_local(&[0x2a], 0), None);
}
