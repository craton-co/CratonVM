use super::*;
use crate::constant_pool::ConstantPoolEntry;

/// `[1]=Class(O) [2]=Utf8 O [3]=Class(O$A) [4]=Utf8 O$A [5]=Utf8 A
/// [6]=Class(O$B) [7]=Utf8 O$B`.
fn cp() -> ConstantPool {
    ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::ClassReference { name_index: 2 },
        ConstantPoolEntry::Utf8("O".into()),
        ConstantPoolEntry::ClassReference { name_index: 4 },
        ConstantPoolEntry::Utf8("O$A".into()),
        ConstantPoolEntry::Utf8("A".into()),
        ConstantPoolEntry::ClassReference { name_index: 7 },
        ConstantPoolEntry::Utf8("O$B".into()),
    ])
}

fn e(inner: u16, outer: u16, name: u16, flags: u16) -> InnerClassInfo {
    InnerClassInfo {
        inner_class_info_index: inner,
        outer_class_info_index: outer,
        inner_name_index: name,
        inner_class_access_flags: flags,
    }
}

#[test]
fn hotspot_inner_classes_checks() {
    let cp = cp();
    use InnerClassesVerdict as V;
    assert_eq!(inner_classes_verdict(&[e(3, 1, 5, 9), e(6, 1, 0, 8)], &cp), V::Keep);
    assert_eq!(inner_classes_verdict(&[e(1, 1, 5, 9)], &cp), V::BothOuterAndInner);
    assert_eq!(
        inner_classes_verdict(&[e(3, 1, 5, 9), e(3, 1, 5, 9)], &cp),
        V::DuplicateEntry
    );
    assert_eq!(inner_classes_verdict(&[e(3, 1, 5, 9), e(3, 1, 5, 1)], &cp), V::Ignore);
    // O$A's outer is O$B and O$B's outer is O$A.
    assert_eq!(inner_classes_verdict(&[e(3, 6, 5, 9), e(6, 3, 0, 8)], &cp), V::Ignore);
    // An anonymous entry (no outer) is fine.
    assert_eq!(inner_classes_verdict(&[e(3, 0, 0, 0)], &cp), V::Keep);
}
