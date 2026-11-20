use super::*;

/// The offset of `access_flags` in a class file: just past the constant
/// pool.
fn access_flags_offset(bytes: &[u8]) -> usize {
    let count = u16::from_be_bytes([bytes[8], bytes[9]]) as usize;
    let mut at = 10;
    let mut index = 1;
    while index < count {
        let tag = bytes[at];
        at += 1;
        let (len, slots) = match tag {
            1 => (
                2 + u16::from_be_bytes([bytes[at], bytes[at + 1]]) as usize,
                1,
            ),
            3 | 4 => (4, 1),
            5 | 6 => (8, 2),
            7 | 8 | 16 | 19 | 20 => (2, 1),
            9 | 10 | 11 | 12 | 17 | 18 => (4, 1),
            15 => (3, 1),
            other => panic!("constant pool tag {other}"),
        };
        at += len;
        index += slots;
    }
    at
}

#[test]
fn zs4_rp_hotspot_masks() {
    // jlink's `$Holder` classes: ACC_PRIVATE | ACC_FINAL | ACC_SUPER.
    assert_eq!(hotspot_stored_class_flags(0x0032, 69), 0x0030);
    assert_eq!(hotspot_stored_class_flags(0x8000, 53), 0x8000);
    assert_eq!(hotspot_stored_class_flags(0x8000, 52), 0);
    // A pre-50 interface is abstract.
    assert_eq!(hotspot_stored_class_flags(0x0201, 49), 0x0601);
    // Inner entries keep private / protected / static, drop the rest.
    assert_eq!(hotspot_stored_inner_class_flags(0x001A, 69), 0x001A);
    assert_eq!(hotspot_stored_inner_class_flags(0x00DA, 69), 0x001A);
    assert_eq!(hotspot_stored_inner_class_flags(0x0208, 49), 0x0608);
}

/// Fails without the fix: the stored flags kept the `ACC_PRIVATE` bit a
/// top-level class file may carry, and `Class.getModifiers()` (the
/// mirror's `modifiers`, `hotspot_class_modifiers`) answered 18 where
/// HotSpot answers 16 for `DirectMethodHandle$Holder` of `lib/modules`.
#[test]
fn zs4_rp_a_defined_class_drops_unrecognized_access_bits() {
    let mut bytes = include_bytes!("../../tests/fixtures/wp2_4b_redefine/Foo.v1.class").to_vec();
    let at = access_flags_offset(&bytes);
    let raw = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
    assert_eq!(raw & 0x0002, 0, "fixture is not private already");
    let patched = (raw | 0x0002 | 0x0010).to_be_bytes();
    bytes[at] = patched[0];
    bytes[at + 1] = patched[1];
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let id = mgr
        .define_class("Foo", &bytes, ClassLoaderId::UserDefined(0x5EED_4001))
        .expect("define");
    let stored = mgr.get_class(id).unwrap().access_flags.bits();
    assert_eq!(stored & 0x0002, 0, "ACC_PRIVATE is not a class modifier");
    assert_eq!(stored, hotspot_stored_class_flags(raw | 0x0012, 52));
    assert_ne!(stored & 0x0010, 0, "ACC_FINAL is kept");
}
