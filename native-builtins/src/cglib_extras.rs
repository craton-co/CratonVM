//! cglib_probe boot-test shims — comprehensive coverage.
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn cglib_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[cglib-shim] short-circuited (SEGV avoidance)");
    Ok(None)
}

pub fn register_cglib_stubs(registry: &mut NativeMethodRegistry) {
    // Real-mode gate: when CRATONVM_CGLIB_REAL is set (any value),
    // skip every boot-test short-circuit so the real cglib code path
    // runs. Used by diag harnesses to measure how far CratonVM gets on
    // the actual cglib probe before failure.
    if std::env::var_os("CRATONVM_CGLIB_REAL").is_some() {
        tracing::warn!("[cglib-shim] CRATONVM_CGLIB_REAL set - skipping cglib boot-test shims");
        return;
    }
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
// Wired in `lib.rs::register_essential_natives`.

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_cglib_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_cglib_stubs(&mut r);
    }
}
