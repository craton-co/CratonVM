use super::*;

/// Round 14 wave 4 (lane calls2): a site whose invoke row carries a zero
/// `info_ptr` (a builder driven without `lib.rs`'s planner) is refused
/// before the pointer is read. The expander used to dereference it first
/// and fault.
#[test]
fn a_zero_info_ptr_is_refused_not_dereferenced() {
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    assert!(!b.try_string_access_intrinsic(1, 0, 1));
    assert!(!b.try_string_access_intrinsic(1, 0, 2));
}
