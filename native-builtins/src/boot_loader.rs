// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! C4 — Boot loader native methods.
//!
//! `jdk.internal.loader.BootLoader.<clinit>` in real JDK 25 calls three
//! HotSpot natives:
//!
//! - `setBootLoaderUnnamedModule0(Ljava/lang/Module;)V` — registers the
//!   unnamed module of the boot loader with the VM's module layer. We do
//!   not model JPMS module layers, so this is a no-op.
//! - `getSystemPackageLocation(Ljava/lang/String;)Ljava/lang/String;` —
//!   returns the URL string for a given boot package. Returning null is
//!   legal ("unknown/not-recorded location") and keeps callers happy.
//! - `getSystemPackageNames()[Ljava/lang/String;` — returns the names of
//!   all packages defined by the boot loader. Returning an empty `String[]`
//!   is legal (same as "no packages recorded"); callers just iterate zero
//!   times.
//!
//! Without these three stubs, `BootLoader.<clinit>` raises an
//! `UnsatisfiedLinkError` and gets swallowed by the clinit harness, which
//! leaves `BootLoader.INSTANCE` null. Any downstream class that reads
//! `BootLoader.INSTANCE` (e.g. `URLClassPath.<clinit>`,
//! `ClassLoader.getResources`) then NPEs, which breaks every
//! `ServiceLoader` user (SLF4J, JDBC autodetection, etc).
//!
//! # This is NOT the only file that registers `BootLoader` natives
//!
//! Three more live in `lib.rs`, and one of them carries a constraint that is
//! easy to break from here — retagging "the `BootLoader` natives" is exactly
//! the shape of change that would do it:
//!
//! * `loadLibrary(Ljava/lang/String;)V` — a deliberate `Ok(None)` no-op, in
//!   `register_essential_natives_with_shims`. Its `NativeKind` is **AMBIENT**
//!   (a bare `register`, taking the enclosing `set_category(Bridge)`) and it
//!   **must stay `Bridge`**: `NativeKind::allowed_in` drops `SyntheticStub`
//!   and only `SyntheticStub` under `JdkOnly`, so a `SyntheticStub` there
//!   deletes the no-op in strict mode, runs real `NativeLibraries` bytecode in
//!   its place, and restores the JDK native-library lock the short-circuit
//!   exists to avoid during Linux boot-class `<clinit>`. Measured, one row:
//!   `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — `bridge`,
//!   `kind_stated=0`. Records: W6-6-nativelibraries-load-fabricated-success.md
//!   and W5-1-loadlibrary-allowlist-too-wide.md in docs/known-issues/jdk-only.
//! * `setBootLoaderUnnamedModule0` is registered **twice** — here and in
//!   `lib.rs` — and `register()` is last-write-wins, so the two bodies must
//!   stay interchangeable. Both are no-ops today, and both state `Bridge`.
//! * `findResourceAsStream` (`lib.rs`, delegating to `classloader`).
//!
//! The five registrations in this file all state their kind explicitly AND sit
//! inside a `set_category(Bridge)` scope, so nothing here is ambient.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, Value};

/// `setBootLoaderUnnamedModule0(Ljava/lang/Module;)V` — no-op in CratonVM.
///
/// In HotSpot this pins the boot loader's unnamed `Module` into the VM's
/// module layer table. We don't model JPMS module layers, so the argument
/// is discarded.
fn native_set_boot_loader_unnamed_module0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// `getSystemPackageLocation(Ljava/lang/String;)Ljava/lang/String;` —
/// returns null ("location not recorded"). This is a valid response even
/// on real HotSpot for packages without a known location.
fn native_get_system_package_location(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `getSystemPackageNames()[Ljava/lang/String;` — returns an empty
/// `String[]`. Callers iterate zero times, which is equivalent to "boot
/// loader has not recorded any packages yet".
fn native_get_system_package_names(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// `jdk/internal/vm/VMSupport.initAgentProperties(Ljava/util/Properties;)Ljava/util/Properties;` —
/// return the argument unchanged (no agent properties to add).
fn native_vm_support_init_agent_properties(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Echo the passed-in Properties back to the caller.
    let arg = args.first().cloned().unwrap_or(Value::Object(None));
    Ok(Some(arg))
}

/// `jdk/internal/vm/VMSupport.getVMTemporaryDirectory()Ljava/lang/String;` —
/// return the platform temp directory path.
fn native_vm_support_get_vm_temp_dir(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let tmp = std::env::temp_dir().to_string_lossy().to_string();
    let s = ctx.create_string(&tmp);
    Ok(Some(Value::Object(Some(s))))
}

/// Register all C4 boot-loader natives.
pub fn register_boot_loader_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    registry.register_with_kind(
        "jdk/internal/loader/BootLoader",
        "setBootLoaderUnnamedModule0",
        "(Ljava/lang/Module;)V",
        native_set_boot_loader_unnamed_module0,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "jdk/internal/loader/BootLoader",
        "getSystemPackageLocation",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_get_system_package_location,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "jdk/internal/loader/BootLoader",
        "getSystemPackageNames",
        "()[Ljava/lang/String;",
        native_get_system_package_names,
        NativeKind::Bridge,
    );

    // Bonus: jdk/internal/vm/VMSupport — stubs for the two natives that
    // can surface during VM bootstrap when an agent or diagnostic tool
    // touches the class.
    registry.register_with_kind(
        "jdk/internal/vm/VMSupport",
        "initAgentProperties",
        "(Ljava/util/Properties;)Ljava/util/Properties;",
        native_vm_support_init_agent_properties,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "jdk/internal/vm/VMSupport",
        "getVMTemporaryDirectory",
        "()Ljava/lang/String;",
        native_vm_support_get_vm_temp_dir,
        NativeKind::Bridge,
    );
    registry.set_category(__prev_cat);
}
