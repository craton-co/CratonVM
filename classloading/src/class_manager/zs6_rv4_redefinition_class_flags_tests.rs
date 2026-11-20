use super::*;

/// A version-49 interface `zs6/I` whose class file omits `ACC_ABSTRACT`
/// (`ACC_PUBLIC | ACC_INTERFACE`), with no members.
fn old_interface_without_abstract() -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 49];
    b.extend_from_slice(&[0x00, 0x05]); // constant_pool_count
    b.extend_from_slice(&[0x01, 0x00, 0x05]);
    b.extend_from_slice(b"zs6/I"); // #1 Utf8
    b.extend_from_slice(&[0x07, 0x00, 0x01]); // #2 Class #1
    b.extend_from_slice(&[0x01, 0x00, 0x10]);
    b.extend_from_slice(b"java/lang/Object"); // #3 Utf8
    b.extend_from_slice(&[0x07, 0x00, 0x03]); // #4 Class #3
    b.extend_from_slice(&[0x02, 0x01]); // ACC_PUBLIC | ACC_INTERFACE
    b.extend_from_slice(&[0x00, 0x02, 0x00, 0x04]); // this, super
    b.extend_from_slice(&[0x00; 8]); // no interfaces, fields, methods, attributes
    b
}

/// Fails without the fix: the stored flags carry the `ACC_ABSTRACT` a
/// pre-50 interface gains at define (as HotSpot's parser adds it), the
/// new bytes' raw flags do not, and redefining the class with its OWN
/// bytes was refused as `ClassModifiersChanged`. HotSpot compares two
/// parsed classes and accepts it.
#[test]
fn zs6rv4_an_old_interface_redefines_with_its_own_bytes() {
    let bytes = old_interface_without_abstract();
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let id = mgr
        .define_class("zs6/I", &bytes, ClassLoaderId::UserDefined(0x5EED_6001))
        .expect("define");
    let stored = mgr.get_class(id).unwrap().access_flags.bits();
    assert_eq!(stored, 0x0601, "ACC_ABSTRACT added at define");
    let checked = mgr.check_redefinition(id, &bytes, &RedefineOptions::default());
    assert!(checked.is_ok(), "same bytes, same modifiers: {checked:?}");
    // Positive control: a real modifier change is still refused.
    let mut changed = bytes.clone();
    let at = changed.len() - 14;
    assert_eq!(&changed[at..at + 2], &[0x02, 0x01]);
    changed[at] = 0x12; // + ACC_SYNTHETIC
    let refused = mgr.check_redefinition(id, &changed, &RedefineOptions::default());
    assert!(refused.is_err(), "ACC_SYNTHETIC added must be refused");
}
