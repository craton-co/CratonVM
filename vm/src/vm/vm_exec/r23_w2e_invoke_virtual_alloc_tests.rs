use super::*;

/// Every arm reads as the name it carries, and an `Arc` arm shares the
/// class's allocation rather than copying it.
#[test]
fn r23_w2e_class_name_reads_as_its_str() {
    let arc: std::sync::Arc<str> = std::sync::Arc::from("java/util/ArrayList");
    let shared = InvokeVirtualClassName::Shared(arc.clone());
    assert_eq!(&*shared, "java/util/ArrayList");
    assert!(std::ptr::eq(shared.as_ptr(), arc.as_ptr()), "no copy");
    assert_eq!(
        &*InvokeVirtualClassName::Static("java/lang/Object"),
        "java/lang/Object"
    );
    assert_eq!(
        &*InvokeVirtualClassName::Owned("<unknown class 7>".into()),
        "<unknown class 7>"
    );
}

/// The receiver-prepended list stays inline up to eight values and keeps
/// order past that.
#[test]
fn r23_w2e_args_stay_inline_for_common_arities() {
    let mut a: InvokeVirtualArgs = smallvec::SmallVec::new();
    a.push(Value::Object(None));
    a.extend_from_slice(&[Value::Int(1); 7]);
    assert!(!a.spilled(), "receiver + 7 arguments is inline");
    a.push(Value::Int(9));
    assert!(a.spilled());
    assert_eq!(a.len(), 9);
    assert_eq!(a[8], Value::Int(9));
}

/// Proposals item 2: the checked reference copy copies the proven prefix
/// raw and stops at the first element it cannot prove cheaply (here an
/// array element, which only the caller's `aastore` predicate may judge);
/// a backward same-array copy and a non-ZGC heap are declined untouched.
#[test]
fn r23_w2e_checked_ref_copy_stops_at_the_first_unproven_element() {
    use cratonvm_types::ArrayElementType as E;
    const X: u32 = 0x7_B0B0;
    let shared = std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let heap = &shared.mem.heap;
    let a = heap.alloc_object(ClassId::new(X), 0);
    let b = heap.alloc_object(ClassId::new(X), 0);
    let ia = heap.alloc_array(ClassId::new(0), E::Int, 1);
    let src = heap.alloc_array(ClassId::new(0), E::Reference, 5);
    let dst = heap.alloc_array(ClassId::new(X), E::Reference, 5);
    for (i, v) in [Some(a), None, Some(b), Some(ia), Some(a)].into_iter().enumerate() {
        let _ = heap.set_array_element(src, i, Value::Object(v));
    }
    let mut thread = JvmThread::new(ThreadId(0), "r23-w2e-checked-copy");
    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let got = NativeHeapAccess::copy_reference_array_elements_checked(
        &mut ctx,
        src,
        0,
        dst,
        0,
        5,
        ClassId::new(X),
    );
    if !(cfg!(feature = "zgc") && !(heap.is_generational() || heap.is_g1())) {
        assert_eq!(got, None, "only a quiet ZGC heap is served");
        return;
    }
    assert_eq!(got, Some(3), "a, null, b copied; the int[] element stops it");
    let read = |i: usize| heap.get_array_element(dst, i).unwrap();
    assert_eq!(read(0), Value::Object(Some(a)));
    assert_eq!(read(1), Value::Object(None));
    assert_eq!(read(2), Value::Object(Some(b)));
    assert_eq!(read(3), Value::Object(None), "nothing past the stop");
    // A same-array copy to a higher index is the caller's backward path.
    let back = NativeHeapAccess::copy_reference_array_elements_checked(
        &mut ctx,
        src,
        0,
        src,
        1,
        2,
        ClassId::new(X),
    );
    assert_eq!(back, None);
}
