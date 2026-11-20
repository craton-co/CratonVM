//! mcs21 review: `VolatileCallSite.setTarget` is its own native (the
//! fenced store), `MutableCallSite.setTarget` the unfenced one.
use super::*;

/// The registry hands each class its own row: a `VolatileCallSite` never
/// reaches the unfenced store.
#[test]
fn mcs21_volatile_call_site_set_target_is_the_fenced_native() {
    let mut registry = NativeMethodRegistry::new();
    register_p60_callsite(&mut registry);
    let desc = "(Ljava/lang/invoke/MethodHandle;)V";
    let volatile = registry
        .find("java/lang/invoke/VolatileCallSite", "setTarget", desc)
        .expect("VolatileCallSite.setTarget is registered");
    let mutable = registry
        .find("java/lang/invoke/MutableCallSite", "setTarget", desc)
        .expect("MutableCallSite.setTarget is registered");
    assert_eq!(volatile as usize, vcs_set_target as usize);
    assert_eq!(mutable as usize, cs_set_target as usize);
    assert_ne!(volatile as usize, mutable as usize);
}
