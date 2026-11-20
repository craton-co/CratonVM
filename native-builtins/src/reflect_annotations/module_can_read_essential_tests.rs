use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// `register_p59_module` (phases_late.rs) also registers this triple, but
/// that function is only reachable via the `synthetic-jdk`-feature-gated
/// `register_synthetic_overrides` — dead code in the default `cratonvm-cli`
/// build. `register_essential_natives` is what real-JDK-mode boot
/// actually uses, so THIS is the registration that has to exist for
/// `Module.canRead(Module)` to answer correctly instead of silently
/// returning the wrong boolean via real bytecode (confirmed:
/// `m.canRead(javaBaseModule)` returned `false` via real bytecode vs.
/// `true` on real HotSpot).
#[test]
fn register_essential_includes_module_can_read() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    assert!(
        registry
            .find("java/lang/Module", "canRead", "(Ljava/lang/Module;)Z")
            .is_some(),
        "Module.canRead(Module) must be registered in the essential \
         (real-JDK) native path, not just the synthetic-jdk-only \
         register_p59_module"
    );
}

/// Same gap as `canRead` above, but with a crash instead of a wrong
/// answer: real bytecode (`implAddExportsOrOpens`) reads
/// `this.descriptor.isOpen()` directly, and that field is never
/// populated, so a direct `Module.addExports`/`addOpens` call NPEs
/// ("Cannot invoke ... because \"this.descriptor\" is null") when only
/// `register_p59_module` (synthetic-jdk-only, dead in the default build)
/// has the registration.
#[test]
fn register_essential_includes_module_add_exports_and_opens() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    assert!(
        registry
            .find(
                "java/lang/Module",
                "addExports",
                "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;"
            )
            .is_some(),
        "Module.addExports(String, Module) must be registered in the \
         essential (real-JDK) native path, not just the \
         synthetic-jdk-only register_p59_module"
    );
    assert!(
        registry
            .find(
                "java/lang/Module",
                "addOpens",
                "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;"
            )
            .is_some(),
        "Module.addOpens(String, Module) must be registered in the \
         essential (real-JDK) native path, not just the \
         synthetic-jdk-only register_p59_module"
    );
}
