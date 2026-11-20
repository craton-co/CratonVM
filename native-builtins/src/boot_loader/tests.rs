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

/// An appended entry's location is its file-system path, as HotSpot's
/// `getSystemPackageLocation` answers it for `-Xbootclasspath/a`.
#[test]
fn an_appended_entry_location_is_its_path() {
    assert_eq!(
        entry_path_of_resource_url("file:/C:/x/out/p/A.class", "p/A.class").as_deref(),
        Some("C:/x/out")
    );
    assert_eq!(
        entry_path_of_resource_url("file:/home/u/out/p/A.class", "p/A.class").as_deref(),
        Some("/home/u/out")
    );
    assert_eq!(
        entry_path_of_resource_url("jar:file:/x/a.jar!/p/A.class", "p/A.class").as_deref(),
        Some("/x/a.jar")
    );
    assert_eq!(
        entry_path_of_resource_url("jrt:/java.base/p/A.class", "p/A.class"),
        None
    );
    assert_eq!(appended_package_location(&mock_ctx(), ""), None);
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
