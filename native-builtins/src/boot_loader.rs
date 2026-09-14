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
//!   the `jrt:/<module>` URL of a boot package, derived from the module
//!   registry. Returning null is legal ("unknown/not-recorded location") but
//!   is NOT harmless: it is what decides whether `BootLoader` defines a
//!   `Package` for a name at all.
//! - `getSystemPackageNames()[Ljava/lang/String;` — the packages the boot
//!   loader has defined a class in, slash form. Returning an empty
//!   `String[]` is also legal ("no packages recorded") and was this file's
//!   answer until 2026-09-11, at the cost of `Package.getPackages()`
//!   answering `[]` where HotSpot answers 91. See each function's own
//!   comment; the two are one answer and must move together.
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

/// The `jrt:/<module>` location of the boot package `package_slash`, or `None`
/// when no module in the run-time image owns it.
///
/// `jrt:/<module>` is the shape HotSpot reports for a package in the run-time
/// image, and it is the one the JDK's own consumer is written for:
/// `BootLoader.PackageHelper.findModule` strips the `jrt:/` prefix and resolves
/// the rest through `Modules.findLoadedModule` ->
/// `ModuleLayer.boot().findModule`, whose `orElseThrow` raises an
/// `InternalError`. A module name this VM's boot layer does not carry is
/// therefore a THROW inside `Package.getPackages()`, not a null -- which is why
/// this is derived from [`NativeContext::module_for_package`], the same registry
/// that names modules to `Class.getModule()`, rather than from a table here.
/// Measured on both arms before this was wired up: `ModuleLayer.boot()` carries
/// 69 modules and resolves `java.base`, `java.sql`, `java.logging`,
/// `java.management`, `java.desktop`, `jdk.unsupported`, `java.xml` and
/// `java.naming` by name.
fn boot_package_location(ctx: &dyn NativeContext, package_slash: &str) -> Option<String> {
    ctx.module_for_package(package_slash)
        .map(|module| format!("jrt:/{module}"))
}

/// `getSystemPackageLocation(Ljava/lang/String;)Ljava/lang/String;` — the
/// `jrt:/<module>` URL of a boot package, from the module registry that already
/// knows the answer.
///
/// Returning null is legal ("location not recorded") and was this VM's answer
/// for every package until 2026-09-11. Null is not a harmless null here:
/// `BootLoader.getDefinedPackage(pn)` defines a `Package` ONLY when this native
/// answers non-null, and `BootLoader.packages()` maps every name from
/// [`native_get_system_package_names`] through that method. A null location
/// therefore turns a populated name list into a stream of NULLS, and
/// `Package.getPackages()` into an array of nulls -- strictly worse than the
/// empty array both natives used to return. The two are one answer and move
/// together; [`native_get_system_package_names`] reports only names this
/// function can place.
///
/// The argument is slash form: `BootLoader.getDefinedPackage` calls
/// `getSystemPackageLocation(pn.replace('.', '/'))`, which is also the form
/// [`NativeContext::module_for_package`] takes.
fn native_get_system_package_location(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(name_obj))) = args.first() else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(package_slash) = ctx.read_string(*name_obj) else {
        return Ok(Some(Value::Object(None)));
    };
    match boot_package_location(&*ctx, &package_slash) {
        Some(location) => {
            let s = ctx.create_string(&location);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `getSystemPackageNames()[Ljava/lang/String;` — the packages the boot loader
/// has actually defined a class in, in the **VM-internal slash form** the JDK's
/// own declaration specifies ("the binary name of the packages defined by the
/// boot loader, in VM internal form (forward slashes instead of dot)").
///
/// This returned an empty array until 2026-09-11. That is *legal* -- a caller
/// iterates zero times -- and it was wrong about this VM all the same, because
/// two callers read it:
///
/// * `BootLoader.packages()` is `Arrays.stream(getSystemPackageNames())
///   .map(name -> getDefinedPackage(name.replace('/', '.')))`, and that is the
///   FIRST term of real `ClassLoader.getPackages()`,
///   `Stream.concat(BootLoader.packages(), pkgs).toArray(Package[]::new)`. An
///   empty name list made `Package.getPackages()` answer `[]` where HotSpot 25
///   answers 91 packages on the same probe, measured on three arms.
/// * `BootLoader.getDefinedPackage(pn)`, which defines nothing for a package
///   with no recorded location. That one was NOT observable: every documented
///   route to it -- `Package.getPackage`, `ClassLoader.getPackage`,
///   `Class.getPackage` -- is short-circuited by a native one call earlier, as
///   the 2026-08-22 `getDefinedPackage` record says in the same breath as it
///   defers this row. So the empty answer was only ever visible through the
///   plural, which is why it survived that fix.
///
/// The answer is derived, not tabulated: every loaded class whose name the
/// bootstrap loader would own ([`crate::classloader::is_bootstrap_class_name`],
/// the predicate the rest of this crate already uses for that question) donates
/// its package. That is also HotSpot's semantics -- its package table holds the
/// packages a LOADED class put there, not the image's whole package list -- so
/// the answer legitimately grows during a run.
///
/// One deliberate over-report: that predicate claims the PLATFORM loader's
/// packages too (`javax/`, `com/sun/`), which HotSpot would report from
/// `ClassLoaders.platformClassLoader()` instead. It is the consistent answer for
/// this VM, which assigns every image class to the boot loader -- the
/// 2026-08-22 `getDefinedPackage` record states that as the standing divergence
/// -- and `ClassLoader.getPackages()`, the caller this exists for, unions the
/// boot loader with every ancestor anyway, so the union is unaffected.
fn native_get_system_package_names(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // BTreeSet: distinct, and sorted, so two runs of the same program hand the
    // JDK the same order. HotSpot promises no order; a stable one costs nothing
    // and keeps a diff of two probe runs readable.
    let mut packages: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for class_id in ctx.list_loaded_class_ids() {
        let Some(internal) = ctx.class_name_of_id(class_id) else {
            continue;
        };
        // `is_bootstrap_class_name` answers true for array names (`[Ljava/...`),
        // correctly -- an array of a boot class is boot-loaded -- but a package
        // table has no entry for an array, so they go before the prefix test
        // rather than contributing `[Ljava/lang`.
        if internal.starts_with('[') {
            continue;
        }
        if !crate::classloader::is_bootstrap_class_name(&internal) {
            continue;
        }
        let Some(last_slash) = internal.rfind('/') else {
            // The default package. HotSpot records no entry for it, and
            // `PackageHelper.definePackage` throws `InternalError` on an empty
            // name, so passing one on would be worse than dropping it.
            continue;
        };
        packages.insert(internal[..last_slash].to_string());
    }

    // Only packages this VM can also give a LOCATION for: see
    // [`native_get_system_package_location`]. A name without one becomes a NULL
    // element in `BootLoader.packages()`, so reporting it would trade an empty
    // array for an array with holes in it. Filtered after the loop, on the
    // ~100 distinct packages rather than the ~3000 classes, because each check
    // takes the class manager's read lock.
    packages.retain(|package_slash| boot_package_location(&*ctx, package_slash).is_some());

    // TYPED, not `new_array(Reference, n)`: the JDK declares this native as
    // returning `String[]` and hands the result straight to `Arrays.stream`,
    // whose `T[]` is a `String[]` at that call site, so an array whose runtime
    // component type is `java.lang.Object` fails the caller's own `checkcast`.
    // The same trap is documented, and was measured, one file over --
    // `lang_class::i2_classloader_get_defined_packages` for `Package[]`.
    let arr = match ctx.class_id_by_name("java/lang/String") {
        Some(string_id) => ctx.new_ref_array(string_id, packages.len()),
        None => ctx.new_array(ArrayElementType::Reference, packages.len()),
    };
    for (i, name) in packages.iter().enumerate() {
        let s = ctx.create_string(name);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

    /// Read a `String[]` the mock holds back into Rust.
    fn names(
        ctx: &mut crate::test_utils::MockNativeContext,
        arr: cratonvm_types::ObjectRef,
    ) -> Vec<String> {
        (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                other => panic!("element {i} is not a String: {other:?}"),
            })
            .collect()
    }

    fn call_names(ctx: &mut crate::test_utils::MockNativeContext) -> Vec<String> {
        match native_get_system_package_names(ctx, &[]) {
            Ok(Some(Value::Object(Some(arr)))) => names(ctx, arr),
            other => panic!("getSystemPackageNames returned {other:?}"),
        }
    }

    fn call_location(
        ctx: &mut crate::test_utils::MockNativeContext,
        package_slash: &str,
    ) -> Option<String> {
        let key = ctx.create_string(package_slash);
        match native_get_system_package_location(ctx, &[Value::Object(Some(key))]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            Ok(Some(Value::Object(None))) => None,
            other => panic!("getSystemPackageLocation returned {other:?}"),
        }
    }

    /// The names are the packages of the LOADED boot classes, in slash form and
    /// distinct -- the JDK's own declaration asks for "VM internal form".
    #[test]
    fn the_names_are_the_loaded_boot_packages_in_slash_form() {
        let mut ctx = mock_ctx();
        ctx.declare_module_package("java/lang", "java.base");
        ctx.declare_module_package("java/util", "java.base");
        ctx.declare_loaded_class("java/lang/String");
        ctx.declare_loaded_class("java/lang/Integer");
        ctx.declare_loaded_class("java/util/ArrayList");
        assert_eq!(call_names(&mut ctx), vec!["java/lang", "java/util"]);
    }

    /// An application class is not the boot loader's, whatever else is loaded.
    #[test]
    fn an_application_class_donates_no_package() {
        let mut ctx = mock_ctx();
        ctx.declare_module_package("com/example/app", "com.example");
        ctx.declare_loaded_class("com/example/app/Main");
        assert!(call_names(&mut ctx).is_empty());
    }

    /// A package the module registry cannot place is DROPPED rather than
    /// reported: `BootLoader.packages()` maps every name through
    /// `getDefinedPackage`, which yields null for a package with no location, so
    /// reporting it would trade an empty array for an array with a hole in it.
    #[test]
    fn a_package_with_no_module_is_not_reported() {
        let mut ctx = mock_ctx();
        ctx.declare_loaded_class("java/lang/String");
        assert!(
            call_names(&mut ctx).is_empty(),
            "a name with no location becomes a null element in BootLoader.packages()"
        );
        ctx.declare_module_package("java/lang", "java.base");
        assert_eq!(call_names(&mut ctx), vec!["java/lang"]);
    }

    /// Arrays and the default package have no entry in HotSpot's package table,
    /// and an empty name makes `PackageHelper.definePackage` throw
    /// `InternalError`, so neither may be reported.
    #[test]
    fn arrays_and_the_default_package_donate_nothing() {
        let mut ctx = mock_ctx();
        ctx.declare_module_package("java/lang", "java.base");
        ctx.declare_loaded_class("[Ljava/lang/String;");
        ctx.declare_loaded_class("DefaultPackageClass");
        assert!(call_names(&mut ctx).is_empty());
    }

    /// The location is the `jrt:/<module>` URL the JDK's own
    /// `PackageHelper.findModule` parses.
    #[test]
    fn the_location_is_the_jrt_url_of_the_packages_module() {
        let mut ctx = mock_ctx();
        ctx.declare_module_package("java/sql", "java.sql");
        assert_eq!(
            call_location(&mut ctx, "java/sql").as_deref(),
            Some("jrt:/java.sql")
        );
        assert_eq!(call_location(&mut ctx, "no/such/pkg"), None);
    }

    /// Every name reported has a location, which is the invariant that keeps
    /// `BootLoader.packages()` free of nulls.
    #[test]
    fn every_reported_name_has_a_location() {
        let mut ctx = mock_ctx();
        ctx.declare_module_package("java/lang", "java.base");
        ctx.declare_module_package("java/util/zip", "java.base");
        ctx.declare_loaded_class("java/lang/String");
        ctx.declare_loaded_class("java/util/zip/ZipFile");
        ctx.declare_loaded_class("javax/sql/DataSource");
        for name in call_names(&mut ctx) {
            assert!(
                call_location(&mut ctx, &name).is_some(),
                "{name} was reported without a location"
            );
        }
    }
}
