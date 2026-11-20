use super::*;

fn fake_ref(n: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the table only stores and copies it.
    unsafe { ObjectRef::from_raw((n * 8 + 8) as *mut u8) }
}

#[test]
fn one_descriptor_answers_one_object() {
    let t = MethodTypeTable::new();
    assert!(t.get("(I)I").is_none());
    let first = t.insert_if_absent("(I)I", fake_ref(1));
    let second = t.insert_if_absent("(I)I", fake_ref(2));
    assert_eq!(first, fake_ref(1));
    assert_eq!(second, fake_ref(1), "a later build answers the first row");
    assert_eq!(t.get("(I)I"), Some(fake_ref(1)));
    assert_eq!(t.insert_if_absent("(J)I", fake_ref(3)), fake_ref(3));
    assert_eq!(t.len(), 2);
}

#[test]
fn a_full_table_answers_the_callers_object_and_stores_nothing() {
    let t = MethodTypeTable::new();
    for i in 0..MethodTypeTable::CAPACITY {
        t.insert_if_absent(&format!("(I)L{i};"), fake_ref(i + 1));
    }
    let extra = fake_ref(MethodTypeTable::CAPACITY + 10);
    assert_eq!(t.insert_if_absent("()V", extra), extra);
    assert!(t.get("()V").is_none());
    assert_eq!(t.len(), MethodTypeTable::CAPACITY);
}

#[test]
fn scan_and_remap_cover_only_this_table() {
    let a = MethodTypeTable::new();
    let b = MethodTypeTable::new();
    a.insert_if_absent("(I)I", fake_ref(1));
    b.insert_if_absent("(I)I", fake_ref(1));
    let mut roots = Vec::new();
    a.scan_roots(&mut |r| roots.push(r));
    assert_eq!(roots, vec![fake_ref(1)]);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(fake_ref(1).as_ptr() as usize, fake_ref(9).as_ptr() as usize);
    b.remap_roots(&map);
    assert_eq!(b.get("(I)I"), Some(fake_ref(9)), "b's own row moves");
    assert_eq!(a.get("(I)I"), Some(fake_ref(1)), "a is untouched");
}

/// Round 16 wave 15 (mhapp): one descriptor naming two loaders' classes of
/// one name is two rows, and neither is the descriptor-only row.
#[test]
fn class_keyed_rows_of_one_descriptor_are_distinct() {
    let t = MethodTypeTable::new();
    let app = class_keyed("(LBox;)I", &[40]);
    let user = class_keyed("(LBox;)I", &[41]);
    assert_ne!(app, user);
    assert_ne!(app, "(LBox;)I");
    assert_eq!(
        class_keyed("(LA;LB;)LC;", &[1, 22, 333]),
        "(LA;LB;)LC;\u{0}1,22,333"
    );
    assert_eq!(t.insert_if_absent(&app, fake_ref(1)), fake_ref(1));
    assert_eq!(t.insert_if_absent(&user, fake_ref(2)), fake_ref(2));
    assert!(t.get("(LBox;)I").is_none(), "no descriptor-only row");
    assert_eq!(t.get(&app), Some(fake_ref(1)));
    assert_eq!(t.get(&user), Some(fake_ref(2)));
}

/// An unloaded class's rows go; descriptor-only rows and rows naming
/// other classes stay.
#[test]
fn forget_classes_drops_only_rows_naming_a_gone_class() {
    let t = MethodTypeTable::new();
    t.insert_if_absent("(I)I", fake_ref(1));
    t.insert_if_absent(&class_keyed("(LBox;)I", &[40]), fake_ref(2));
    t.insert_if_absent(&class_keyed("(LBox;LOther;)I", &[41, 7]), fake_ref(3));
    t.insert_if_absent(&class_keyed("(LOther;)V", &[7]), fake_ref(4));
    assert_eq!(t.forget_classes(&|id| id == 41), 1);
    assert_eq!(t.len(), 3);
    assert_eq!(t.forget_classes(&|id| id == 999), 0);
    assert_eq!(t.forget_classes(&|id| id == 7 || id == 40), 2);
    assert_eq!(
        t.get("(I)I"),
        Some(fake_ref(1)),
        "descriptor-only rows stay"
    );
    assert_eq!(t.len(), 1);
}

#[test]
fn a_zero_forwarding_drops_the_row() {
    let t = MethodTypeTable::new();
    t.insert_if_absent("(I)I", fake_ref(1));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(fake_ref(1).as_ptr() as usize, 0);
    t.remap_roots(&map);
    assert!(t.get("(I)I").is_none());
    assert!(t.is_empty());
}
