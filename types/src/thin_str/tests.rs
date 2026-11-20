use super::*;

#[test]
fn zs2fp2_thin_arc_str_is_12_bytes_align_4() {
    assert_eq!(std::mem::size_of::<ThinArcStr>(), 12);
    assert_eq!(std::mem::align_of::<ThinArcStr>(), 4);
    // The null niche survives `packed`.
    assert_eq!(std::mem::size_of::<Option<ThinArcStr>>(), 12);
    // An enum beside an 8-byte payload fits in 16 bytes.
    #[allow(dead_code)]
    enum E {
        A(ThinArcStr),
        B(i64),
        C { x: u16, y: u16 },
    }
    assert_eq!(std::mem::size_of::<E>(), 16);
}

#[test]
fn zs2fp2_thin_arc_str_shares_the_arc_and_its_count() {
    let arc: Arc<str> = Arc::from("java/lang/Object");
    let thin = ThinArcStr::from(Arc::clone(&arc));
    assert_eq!(Arc::strong_count(&arc), 2);
    assert!(thin.ptr_eq_arc(&arc));
    let back = thin.to_arc();
    assert!(Arc::ptr_eq(&back, &arc));
    assert_eq!(Arc::strong_count(&arc), 3);
    let c = thin.clone();
    assert_eq!(Arc::strong_count(&arc), 4);
    assert!(ThinArcStr::ptr_eq(&c, &thin));
    drop(c);
    drop(back);
    assert_eq!(Arc::strong_count(&arc), 2);
    let moved = thin.into_arc();
    assert_eq!(Arc::strong_count(&arc), 2);
    drop(moved);
    assert_eq!(Arc::strong_count(&arc), 1);
}

#[test]
fn zs2fp2_thin_arc_str_compares_hashes_and_prints_as_str() {
    use std::collections::hash_map::DefaultHasher;
    let a = ThinArcStr::from("x\u{e9}y");
    let b = ThinArcStr::from(String::from("x\u{e9}y"));
    assert!(!ThinArcStr::ptr_eq(&a, &b));
    assert_eq!(a, b);
    assert_eq!(a, *"x\u{e9}y");
    assert_eq!(&*a, "x\u{e9}y");
    assert_eq!(a.len(), 4);
    let h = |s: &dyn Fn(&mut DefaultHasher)| {
        let mut st = DefaultHasher::new();
        s(&mut st);
        st.finish()
    };
    assert_eq!(h(&|st| a.hash(st)), h(&|st| "x\u{e9}y".hash(st)));
    let arc: Arc<str> = Arc::from("x\u{e9}y");
    assert_eq!(format!("{a:?}"), format!("{arc:?}"));
    assert_eq!(format!("{a}"), format!("{arc}"));
    let empty = ThinArcStr::default();
    assert_eq!(&*empty, "");
    assert_eq!(&*empty.to_arc(), "");
    let a = ThinArcStr::from("a");
    let b = ThinArcStr::from("b");
    assert!(a < b);
}

#[test]
fn zs2fp2_thin_arc_str_keeps_interning_identity() {
    let pool = crate::intern::StringPool::new();
    let a = ThinArcStr::from(pool.intern_arc("java/lang/String"));
    let b = ThinArcStr::from(pool.intern_arc("java/lang/String"));
    assert!(ThinArcStr::ptr_eq(&a, &b));
    // Held by `a` and `b`: not prunable.
    assert_eq!(pool.prune_unreferenced(), 0);
    drop(a);
    drop(b);
    assert_eq!(pool.prune_unreferenced(), 1);
}
