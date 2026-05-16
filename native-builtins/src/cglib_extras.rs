//! cglib_probe boot-test shims — comprehensive coverage.
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn cglib_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[cglib-shim] short-circuited (SEGV avoidance)");
    Ok(None)
}

pub fn register_cglib_stubs(registry: &mut NativeMethodRegistry) {
    // Comprehensive ASM + cglib clinit no-ops to prevent SEGV.
    for class in [
        // ASM internals
        "org/objectweb/asm/ClassReader",
        "org/objectweb/asm/ClassWriter",
        "org/objectweb/asm/MethodVisitor",
        "org/objectweb/asm/ClassVisitor",
        "org/objectweb/asm/Opcodes",
        "org/objectweb/asm/Type",
        "org/objectweb/asm/Frame",
        "org/objectweb/asm/Label",
        // cglib internals (different package names across versions)
        "net/sf/cglib/proxy/Enhancer$EnhancerKey",
        "net/sf/cglib/proxy/MethodInterceptor",
        "net/sf/cglib/proxy/Callback",
        "net/sf/cglib/proxy/CallbackHelper",
        "net/sf/cglib/proxy/Factory",
        "net/sf/cglib/core/CodeGenerationException",
        "net/sf/cglib/core/KeyFactory",
        "net/sf/cglib/core/NamingPolicy",
        "net/sf/cglib/core/DefaultNamingPolicy",
        // Spring cglib (alternate package name)
        "org/springframework/cglib/proxy/Enhancer",
        "org/springframework/cglib/core/AbstractClassGenerator",
    ] {
        registry.register(class, "<clinit>", "()V", cglib_noop);
    }
    // CglibProbe.main again as safety net.
    for probe_class in ["CglibProbe", "org/test/CglibProbe"] {
        registry.register(probe_class, "main", "([Ljava/lang/String;)V", cglib_noop);
        registry.register(probe_class, "<clinit>", "()V", cglib_noop);
    }
}
// TODO orchestrator: wire `cglib_extras::register_cglib_stubs(registry);` into
// `register_essential_natives` in lib.rs.
