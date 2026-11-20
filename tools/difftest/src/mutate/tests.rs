use super::*;

/// A minimal hand-built class blob: header + a tiny constant pool holding
/// one Integer(3)=7 and one Long(5)=9, used to exercise the CP walker and
/// the in-place mutation without needing javac.
fn fake_class() -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41];
    // constant_pool_count = 4 (entries 1..3; the Long at index 2 eats slot 3)
    b.extend_from_slice(&[0x00, 0x04]);
    // #1 Integer = 7
    b.push(3);
    b.extend_from_slice(&7i32.to_be_bytes());
    // #2 Long = 9 (occupies #2 and #3)
    b.push(5);
    b.extend_from_slice(&9i64.to_be_bytes());
    b
}

#[test]
fn walks_numeric_constants() {
    let c = fake_class();
    let consts = numeric_constants(&c);
    assert_eq!(consts.len(), 2);
    assert_eq!(consts[0].kind, NumericKind::Integer);
    assert_eq!(consts[1].kind, NumericKind::Long);
    // The Integer value is at offset 11 (after 10-byte header + 1 tag).
    assert_eq!(consts[0].value_offset, 11);
    assert_eq!(i32::from_be_bytes(c[11..15].try_into().unwrap()), 7);
}

#[test]
fn mutation_changes_a_value_and_preserves_length() {
    let c = fake_class();
    let mut rng = Rng::new(1);
    let m = mutate_constant(&c, &mut rng).expect("has numeric constants");
    assert_eq!(m.len(), c.len(), "mutation must be length-preserving");
    assert_ne!(m, c, "mutation must change at least one byte");
    // The header (magic + version + cp_count) is untouched.
    assert_eq!(&m[0..10], &c[0..10]);
}

#[test]
fn mutation_is_reproducible_for_a_seed() {
    let c = fake_class();
    let a = mutate_constant(&c, &mut Rng::new(42));
    let b = mutate_constant(&c, &mut Rng::new(42));
    assert_eq!(a, b);
}

#[test]
fn no_numeric_constants_returns_none() {
    // A header with cp_count=1 (empty pool) has nothing to mutate.
    let empty = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x41, 0, 1];
    assert!(numeric_constants(&empty).is_empty());
    assert!(mutate_constant(&empty, &mut Rng::new(1)).is_none());
}

#[test]
fn rejects_non_class_blob() {
    assert!(numeric_constants(b"not a class").is_empty());
}
