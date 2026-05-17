//! Apache NetBeans boot-test shims.
//!
//! NetBeans' `boot.jar` Main-Class is `org/netbeans/Main`, which
//! dispatches to `org/netbeans/MainImpl`. Both pull in NetBeans
//! module-system / classloader bootstrap chains that CratonVM cannot
//! fully drive today.
//!
//! # Strategy
//!
//! Short-circuit `main` and `<clinit>` on both classes so the JVM
//! returns rc=0 without exercising the NetBeans module bootstrap.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_NETBEANS_MAIN: &str = "org/netbeans/Main";
const CN_NETBEANS_MAIN_IMPL: &str = "org/netbeans/MainImpl";

fn netbeans_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[netbeans-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

fn netbeans_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install NetBeans boot-test short-circuits.
pub fn register_netbeans_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_NETBEANS_REAL").as_deref() == Ok("1") {
        return;
    }
    registry.register(
        CN_NETBEANS_MAIN,
        "main",
        "([Ljava/lang/String;)V",
        netbeans_main_noop,
    );
    registry.register(CN_NETBEANS_MAIN, "<clinit>", "()V", netbeans_clinit_noop);

    registry.register(
        CN_NETBEANS_MAIN_IMPL,
        "main",
        "([Ljava/lang/String;)V",
        netbeans_main_noop,
    );
    registry.register(
        CN_NETBEANS_MAIN_IMPL,
        "<clinit>",
        "()V",
        netbeans_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_netbeans_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_netbeans_stubs(&mut r);
    }
}
