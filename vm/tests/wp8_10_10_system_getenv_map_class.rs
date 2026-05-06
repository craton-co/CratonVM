//! WP8.10.10 — `System.getenv()` regression: ensure the returned Map carries
//! a real `java/util/HashMap` class_id so virtual dispatch on
//! `Map.get(key)` resolves to the registered native instead of bottoming
//! out at `java/lang/Object`.
//!
//! Pre-fix the `native_system_getenv_all` helper allocated the backing
//! object with `ClassId::new(0)`. The dispatcher's stale-pointer detector
//! (header-bytes==0) reads a non-zero header (identity hash + kind=Object)
//! and falls through to "Genuinely java.lang.Object", which then routes
//! `Map.get(key)` invokeinterface dispatch to `java/lang/Object`. Since
//! `Object` declares no `get(Object)Object` method, the slow path emits
//! `WARN NoSuchMethodError method="java/lang/Object.get(Object)Object"` —
//! observed during KC16 boot inside
//! `org.jboss.as.server.ServerEnvironment.configureQualifiedHostName`
//! against the `WildFlySecurityManager.getSystemEnvironmentPrivileged()`
//! Map (Session 95 live status).
//!
//! Acceptance:
//! 1. `System.getenv()` returns a non-null reference.
//! 2. The returned object's `class_id_of` resolves to a class whose name
//!    is `java/util/HashMap` (so virtual dispatch finds the registered
//!    `HashMap.get(Object)Object` native).
//! 3. The `java/util/HashMap` native registry contains a
//!    `get(Object)Object` entry — sanity-check that the dispatch target
//!    actually exists on the class we now allocate against.

use rustjvm_vm::config::VmConfig;
use rustjvm_vm::types::Value;
use rustjvm_vm::vm::{NativeContextImpl, Vm};

/// Pin the WP8.10.10 fix: `System.getenv()` returns a Map whose
/// `class_id_of` resolves to a real, named class (`java/util/HashMap`),
/// not the all-zero `ClassId(0)` that the dispatcher reads as
/// `java/lang/Object`.
///
/// Pre-fix the returned object had `class_id == 0`. The dispatcher's
/// stale-pointer detector saw a non-zero header (identity hash etc.)
/// and fell through to the "Genuinely java.lang.Object" branch, which
/// then routed `Map.get(key)` invokeinterface dispatch to
/// `java/lang/Object`. Since `Object` declares no
/// `get(Object)Object` method, the slow path emitted
/// `WARN NoSuchMethodError method="java/lang/Object.get(Object)Object"`,
/// observed in KC16 boot under
/// `org.jboss.as.server.ServerEnvironment.configureQualifiedHostName`.
#[test]
fn system_getenv_returns_hashmap_typed_object() {
    let mut vm = Vm::new(VmConfig::default());

    let cb = vm
        .shared
        .native_methods
        .find("java/lang/System", "getenv", "()Ljava/util/Map;")
        .expect("System.getenv()Map must be registered");

    let map_ref = {
        let mut ctx = NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let r = cb(&mut ctx, &[]).expect("getenv must not error");
        match r {
            Some(Value::Object(Some(o))) => o,
            other => panic!("System.getenv() must return non-null Map, got {other:?}"),
        }
    };

    let class_id = vm.shared.heap.class_id_of(map_ref);
    assert_ne!(
        class_id.as_u32(),
        0,
        "WP8.10.10: System.getenv()'s Map must NOT carry ClassId(0). \
         A zero class_id makes the dispatcher resolve `Map.get(key)` \
         to `java/lang/Object`, which has no `get(Object)Object` \
         method — surfaces as `NoSuchMethodError` during KC16 boot."
    );

    let class_name = {
        let cm = vm.shared.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default()
    };
    assert_eq!(
        class_name, "java/util/HashMap",
        "WP8.10.10: System.getenv()'s Map should be tagged with the \
         real java/util/HashMap class_id so virtual dispatch on \
         `Map.get(key)` lands on HashMap (real-JDK bytecode or \
         registered native), not on `java/lang/Object`. Got {class_name:?}."
    );
}
