//! Apache NetBeans boot-test + real-launcher diagnostic shims.
//!
//! NetBeans' `boot.jar` Main-Class is `org/netbeans/Main`, which
//! immediately dispatches to `org/netbeans/MainImpl.execute(...)`.
//! `MainImpl.execute` walks four system properties to build the
//! application classpath:
//!
//!   * `netbeans.user`       — user-config dir, scanned for `core/*`,
//!                              `core/patches/*`, `core/locale/*` jars
//!   * `netbeans.home`       — install dir, scanned the same way
//!   * `netbeans.dirs`       — `;`/`:`-separated cluster dirs
//!   * `netbeans.classpath`  — explicit `;`/`:` jar list
//!
//! It then constructs a `BootClassLoader` (an `org/netbeans/JarClassLoader`
//! subclass) over the resulting jar list and reflectively loads the
//! class named by the `netbeans.mainclass` property (default
//! `org.netbeans.core.startup.Main`).
//!
//! # Default mode (RUSTJVM_NETBEANS_REAL unset)
//!
//! Short-circuit `Main.main` / `<clinit>` and `MainImpl.main` /
//! `<clinit>` so the JVM exits rc=0 without exercising the NetBeans
//! module bootstrap.
//!
//! # Real mode (RUSTJVM_NETBEANS_REAL=1)
//!
//! The short-circuit is disabled. The launcher then prints
//!
//! ```text
//! Cannot set netbeans.buildnumber property no OpenIDE-Module-Build-Version found
//! Exception in thread "main" java/lang/ClassNotFoundException
//! ```
//!
//! and exits. The ClassNotFoundException is thrown by
//! `BootClassLoader.loadClass("org.netbeans.core.startup.Main")` —
//! the default `netbeans.mainclass`. Root cause: the repro classpath
//! covered only `platform/lib/*.jar`, which contains
//! `org/netbeans/MainImpl$BootClassLoader` itself but *not*
//! `org/netbeans/core/startup/Main` (that class lives in
//! `platform/core/core.jar`).
//!
//! ## Diagnostic / partial fix
//!
//! The repro must supply both:
//!
//!   * `-Dnetbeans.home=<netbeans>/platform` (so `MainImpl.execute`'s
//!     `build_cp(new File(netbeans.home), ...)` picks up
//!     `platform/core/core.jar`), AND
//!   * `-Dnetbeans.user=<writable-dir>` (so the user-config branch
//!     doesn't drop into `--userdir` parsing).
//!
//! With those properties set, the launcher can find
//! `org.netbeans.core.startup.Main`. The remaining failure (Lookup
//! framework / module-system init) is beyond what a small shim can
//! address and stays out of scope.
//!
//! `scripts/loop-run-seq.sh` is updated to pass these properties in
//! real-mode runs so the diagnostic is reproducible.

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
///
/// **Default mode** (`RUSTJVM_NETBEANS_REAL` unset): short-circuit
/// `Main.main` / `<clinit>` and `MainImpl.main` / `<clinit>` so the
/// JVM exits rc=0 without driving the NetBeans module system.
///
/// **Real mode** (`RUSTJVM_NETBEANS_REAL=1`): no shims are
/// registered. The real launcher is allowed to run end-to-end. See
/// the file-level doc comment for the required system properties.
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
