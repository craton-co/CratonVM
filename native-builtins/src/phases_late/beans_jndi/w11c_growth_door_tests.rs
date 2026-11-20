use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::{ObjectRef, Value};

/// Clears the thread-local refusal knob even when an assertion panics.
struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

#[test]
fn property_change_listener_add_takes_the_reclaiming_grow() {
    let _reset = KnobReset;
    let mut registry = NativeMethodRegistry::new();
    register_p72_beans(&mut registry);
    let pcs = "java/beans/PropertyChangeSupport";
    let init = registry
        .find(pcs, "<init>", "(Ljava/lang/Object;)V")
        .expect("PropertyChangeSupport.<init> should be registered");
    let add = registry
        .find(
            pcs,
            "addPropertyChangeListener",
            "(Ljava/beans/PropertyChangeListener;)V",
        )
        .expect("PropertyChangeSupport.addPropertyChangeListener should be registered");

    let mut ctx = mock_ctx();
    let cid = ctx.ensure_class_initialized(pcs).unwrap();
    let this = ctx.alloc_object(cid, 2);
    let source = ctx.fresh_object_ref();
    init(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(source))],
    )
    .unwrap();

    let listeners: Vec<ObjectRef> = (0..40).map(|_| ctx.fresh_object_ref()).collect();
    let mut reclaims = 0;
    for listener in &listeners {
        // Armed before every add; only a grow takes a fallible door, and
        // the next arm overwrites a leftover.
        mock_refuse_allocations(1, true);
        add(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(*listener))],
        )
        .unwrap();
        reclaims += mock_reclaims();
    }
    mock_refuse_allocations(0, false);
    assert!(
        reclaims >= 1,
        "no listener-list grow took the reclaim ladder: the registration is \
         still on the single-attempt door"
    );

    let list = match ctx.get_field(this, 1) {
        Value::Object(Some(list)) => list,
        other => panic!("listener list missing: {other:?}"),
    };
    assert_eq!(
        cratonvm_native_collections::native_al_size(&mut ctx, &[Value::Object(Some(list))])
            .unwrap(),
        Some(Value::Int(40))
    );
    for (i, listener) in listeners.iter().enumerate() {
        let got = cratonvm_native_collections::native_al_get(
            &mut ctx,
            &[Value::Object(Some(list)), Value::Int(i as i32)],
        )
        .unwrap();
        assert_eq!(got, Some(Value::Object(Some(*listener))), "listener {i}");
    }
}
