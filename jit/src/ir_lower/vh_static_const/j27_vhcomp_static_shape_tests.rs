use super::*;
use crate::direct_helpers::{UnsafeBitOp, UnsafeRmwOp};

#[test]
fn receiver_less_int_and_long_modes_are_static_shapes() {
    let s = vh_static_site_shape("getAndAdd", "(I)I").expect("int xadd");
    assert_eq!((s.op, s.kind, s.ret), (VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'I', b'I'));
    let s = vh_static_site_shape("compareAndSet", "(JJ)Z").expect("long cas");
    assert_eq!((s.op, s.kind, s.values.len()), (VhConstOp::Rmw(UnsafeRmwOp::Cas), b'J', 2));
    let s = vh_static_site_shape("getAndBitwiseOr", "(I)I").expect("int or");
    assert_eq!(s.op, VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or)));
    let s = vh_static_site_shape("get", "()J").expect("long get");
    assert_eq!((s.op, s.kind), (VhConstOp::Get, b'J'));
    let s = vh_static_site_shape("setVolatile", "(I)V").expect("int set");
    assert_eq!(s.op, VhConstOp::Set { volatile: true });
    let s = vh_static_site_shape("getAndAdd", "(I)V").expect("discarded xadd");
    assert_eq!(s.ret, b'V');
}

#[test]
fn instance_reference_mixed_and_float_shapes_are_refused() {
    for (m, d) in [
        ("getAndAdd", "(LFoo;I)I"),
        ("get", "(LFoo;)I"),
        ("get", "()Ljava/lang/Object;"),
        ("getAndSet", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        ("getAndAdd", "(J)I"),
        ("getAndAdd", "(F)F"),
        ("compareAndSet", "(II)I"),
        ("getAndAdd", "([II)I"),
        ("invokeExact", "(I)I"),
    ] {
        assert!(vh_static_site_shape(m, d).is_none(), "{m}{d}");
    }
}

#[test]
fn a_cell_fills_once_valid_and_clears() {
    let cell = VhStaticCell::new();
    assert_eq!(cell.filed(), None);
    assert!(!cell.fill(0, 1, b'I'), "a null base cell");
    assert!(!cell.fill(0x1000, 1, b'L'), "a reference static");
    assert!(!cell.fill(0x1000, usize::MAX / 2, b'I'), "an index past disp32");
    assert_eq!(cell.filed(), None);
    assert!(cell.fill(0x1000, 3, b'J'));
    assert_eq!(cell.filed(), Some((0x1000, 48, b'J')));
    cell.clear();
    assert_eq!(cell.filed(), None);
}

#[test]
fn the_cell_query_is_one_the_vm_decodes() {
    let word = vh_const_query(WELL_KNOWN_VH_CONST_STATIC_CELL_TAG, 7, 3);
    assert_eq!(
        super::super::vh_const_query_decode(word),
        Some((WELL_KNOWN_VH_CONST_STATIC_CELL_TAG, 7, 3))
    );
}

