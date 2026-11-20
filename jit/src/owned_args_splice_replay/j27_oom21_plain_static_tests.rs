use super::plain_static_store;
use crate::ir::Op;

fn store(is_volatile: bool) -> Op {
    Op::StoreStatic {
        class_id: 3,
        field_index: 1,
        type_tag: b'L',
        is_volatile,
    }
}

#[test]
fn only_a_built_non_volatile_store_vouches() {
    assert!(plain_static_store([&store(false)]));
    assert!(!plain_static_store([&store(true)]));
    assert!(!plain_static_store([&store(false), &store(true)]));
    assert!(!plain_static_store(std::iter::empty::<&Op>()));
}
