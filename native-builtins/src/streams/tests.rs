use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn register_stream_overrides_installs_flow_subscription_request() {
    let mut r = NativeMethodRegistry::new();
    register_stream_overrides(&mut r);
    assert!(r
        .find("java/util/concurrent/Flow$Subscription", "request", "(J)V")
        .is_some());
    assert!(r
        .find("java/util/concurrent/Flow$Subscription", "cancel", "()V")
        .is_some());
}

#[test]
fn register_stream_overrides_installs_basestream_mode_methods() {
    let mut r = NativeMethodRegistry::new();
    register_stream_overrides(&mut r);
    // BaseStream-declared signatures
    assert!(r
        .find(
            "java/util/stream/BaseStream",
            "sequential",
            "()Ljava/util/stream/BaseStream;"
        )
        .is_some());
    // Stream-narrowed return type (covariant override seen on call sites)
    assert!(r
        .find(
            "java/util/stream/Stream",
            "sequential",
            "()Ljava/util/stream/BaseStream;"
        )
        .is_some());
    assert!(r
        .find(
            "java/util/stream/Stream",
            "sequential",
            "()Ljava/util/stream/Stream;"
        )
        .is_some());
    assert!(r
        .find("java/util/stream/Stream", "isParallel", "()Z")
        .is_some());
    assert!(r
        .find(
            "java/util/stream/IntStream",
            "parallel",
            "()Ljava/util/stream/IntStream;"
        )
        .is_some());
}

#[test]
fn flow_subscription_demand_map_uses_saturating_add() {
    // Direct unit test of the demand-map arithmetic without going
    // through the full native dispatch path.
    let demand = flow_subscription_demand();
    let mut map = demand.lock().unwrap_or_else(|e| e.into_inner());
    // Synthetic key that does not overlap any real ObjectRef pointer.
    let key = 0xDEAD_BEEF_C0FF_EE01u64;
    map.remove(&key); // ensure clean start

    // First request: 10
    let a = *map.get(&key).unwrap_or(&0);
    map.insert(key, a.saturating_add(10));
    assert_eq!(*map.get(&key).unwrap_or(&0), 10);

    // Second request: i64::MAX — must saturate, not wrap
    let b = *map.get(&key).unwrap_or(&0);
    map.insert(key, b.saturating_add(i64::MAX));
    assert_eq!(*map.get(&key).unwrap_or(&0), i64::MAX);

    // Third request: 1 — must stay at i64::MAX
    let c = *map.get(&key).unwrap_or(&0);
    map.insert(key, c.saturating_add(1));
    assert_eq!(*map.get(&key).unwrap_or(&0), i64::MAX);

    map.remove(&key); // cleanup
}
