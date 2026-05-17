//! Eclipse IDE boot-test shims.
//!
//! Eclipse's launcher Main-Class is
//! `org/eclipse/equinox/launcher/Main`. The real `main` drives OSGi
//! bundle resolution, native library loading, and SWT initialization
//! that CratonVM cannot fully execute today.
//!
//! # Strategy
//!
//! Short-circuit `main` and `<clinit>` so the JVM returns rc=0 without
//! exercising the Equinox / OSGi bootstrap chain. Boot-test success
//! criterion is "no crash".

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_EQUINOX_MAIN: &str = "org/eclipse/equinox/launcher/Main";

fn eclipse_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[eclipse-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

fn eclipse_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install Eclipse IDE boot-test short-circuits.
pub fn register_eclipse_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_ECLIPSE_REAL").as_deref() == Ok("1") {
        return;
    }
    registry.register(
        CN_EQUINOX_MAIN,
        "main",
        "([Ljava/lang/String;)V",
        eclipse_main_noop,
    );
    registry.register(CN_EQUINOX_MAIN, "<clinit>", "()V", eclipse_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_eclipse_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_eclipse_stubs(&mut r);
    }
}
