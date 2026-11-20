use super::begin_join_region;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{BlockKind, NativeHeapAccess, NativeThreadAccess};
use cratonvm_types::ClassId;

#[test]
fn a_platform_join_publishes_the_target_and_a_virtual_one_does_not() {
    let mut ctx = MockNativeContext::new();
    let target = ctx.alloc_object(ClassId::new(0), 1);
    begin_join_region(&mut ctx, target, BlockKind::TimedObjectWait);
    ctx.end_blocking_region();
    begin_join_region(&mut ctx, target, BlockKind::Parked);
    ctx.end_blocking_region();
    assert_eq!(
        *ctx.region_wait_objects.borrow(),
        vec![(target, BlockKind::TimedObjectWait)]
    );
    assert_eq!(ctx.blocking_region_counts(), (2, 2));
}
