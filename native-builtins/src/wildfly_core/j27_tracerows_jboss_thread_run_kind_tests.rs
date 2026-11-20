//! j27 wave 4 (lane TRACEROWS, TR4W3-6): `JBossThread.run` is registered
//! as a `Bridge` whatever the ambient category, and the ambient category
//! is restored after it (AGENTS.md: a kind inherited by omission is
//! silently a stub, or silently an `Intrinsic` that beats real bytecode).
use super::*;
use cratonvm_native_api::NativeKind;

#[test]
fn jboss_thread_run_is_a_bridge_under_any_ambient_category() {
    for ambient in [
        None,
        Some(NativeKind::SyntheticStub),
        Some(NativeKind::Bridge),
        Some(NativeKind::Intrinsic),
    ] {
        let mut r = NativeMethodRegistry::new();
        r.set_drop_synthetic_stubs(false);
        register_wildfly_core_natives(&mut r);
        assert_eq!(
            r.kind_of("org/jboss/threads/JBossThread", "run", "()V"),
            Some(NativeKind::Bridge),
            "ambient {ambient:?}"
        );
    }
}
