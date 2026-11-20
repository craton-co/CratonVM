use super::{InlinePathEntry, INLINE_RESOLVE_STATE};
use cratonvm_jit::ir::IR_RECURSIVE_INLINE_MAX_COPIES;
use cratonvm_types::ClassId;

#[test]
fn a_method_stacks_to_the_recursive_bound_and_depth_zero_starts_fresh() {
    let c = ClassId::new(7);
    let mut held = vec![InlinePathEntry::enter(0, c, "as", "(I)V").expect("first copy")];
    for depth in 1..=IR_RECURSIVE_INLINE_MAX_COPIES {
        held.push(InlinePathEntry::enter(depth, c, "as", "(I)V").expect("within the bound"));
    }
    let past = IR_RECURSIVE_INLINE_MAX_COPIES + 1;
    assert!(InlinePathEntry::enter(past, c, "as", "(I)V").is_none());
    // Another descriptor, class or method is another method.
    assert!(InlinePathEntry::enter(past, c, "as", "(J)V").is_some());
    assert!(InlinePathEntry::enter(past, ClassId::new(8), "as", "(I)V").is_some());
    // A depth-0 resolution nested here does not inherit the path, and gives
    // it back whole.
    let fresh = InlinePathEntry::enter(0, c, "as", "(I)V").expect("a fresh path");
    assert_eq!(INLINE_RESOLVE_STATE.with(|s| s.borrow().inline_path.len()), 1);
    drop(fresh);
    assert_eq!(INLINE_RESOLVE_STATE.with(|s| s.borrow().inline_path.len()), past);
    held.into_iter().rev().for_each(drop);
    assert_eq!(INLINE_RESOLVE_STATE.with(|s| s.borrow().inline_path.len()), 0);
}
