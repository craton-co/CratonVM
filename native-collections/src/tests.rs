// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

#[cfg(test)]
mod r15w7_drainc_tp_target_tests {
    use super::*;
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;

    /// `r15w6-sweepC-review-findings` S1: a wrapper whose `e` is a pool
    /// delegates to the pool (it used to run `ThreadPoolExecutor`'s body on
    /// the wrapper), and a real pool declaring its own `e` that holds no pool
    /// runs its own body (it used to get the synthetic one).
    #[test]
    fn a_wrapper_delegates_and_a_pool_with_its_own_e_runs_its_body() {
        let mut ctx = MockNativeContext::new();
        let workers = ctx.fresh_object_ref();
        let pool = ctx.fresh_object_ref();
        ctx.set_field_by_name(pool, "workers", Value::Object(Some(workers)));
        let wrapper = ctx.fresh_object_ref();
        ctx.set_field_by_name(wrapper, "e", Value::Object(Some(pool)));
        let sub = ctx.fresh_object_ref();
        let not_a_pool = ctx.fresh_object_ref();
        ctx.set_field_by_name(sub, "workers", Value::Object(Some(workers)));
        ctx.set_field_by_name(sub, "e", Value::Object(Some(not_a_pool)));
        let placeholder = ctx.fresh_object_ref();

        assert!(matches!(tp_target(&mut ctx, wrapper), TpTarget::Delegate(p) if p == pool));
        assert!(matches!(tp_target(&mut ctx, pool), TpTarget::Own));
        assert!(matches!(tp_target(&mut ctx, sub), TpTarget::Own));
        assert!(matches!(
            tp_target(&mut ctx, placeholder),
            TpTarget::Synthetic
        ));
        // The old predicate, for contrast: both shapes answered the other way.
        assert!(tp_is_real(&mut ctx, wrapper));
        assert!(!tp_is_real(&mut ctx, sub));
    }
}

#[test]
fn stream_to_list_falls_back_when_synthetic_jdk_has_no_immutable_factory() {
    use cratonvm_native_api::NativeHeapAccess;
    use cratonvm_types::Value;

    let mut ctx = cratonvm_native_api::test_mock::MockNativeContext::new();
    let result = crate::streams::native_stream_to_list_jdk(&mut ctx, &[]).unwrap();
    let Some(Value::Object(Some(list))) = result else {
        panic!("expected an immutable list, got {result:?}");
    };
    assert!(matches!(ctx.get_field(list, 0), Value::Object(Some(_))));
    assert_eq!(ctx.get_field(list, 1), Value::Int(1));
}
