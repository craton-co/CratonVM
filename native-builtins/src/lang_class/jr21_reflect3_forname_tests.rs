//! JIT round 21 lane reflect3 (`CRATONVM_FORNAME_BUILTIN_LOADED_FIRST`):
//! which `Class.forName` calls may take the built-in loader's
//! `findLoadedClass0` answer instead of running `loadClass`.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::NativeHeapAccess;

fn instance_of(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        other => panic!("no instance of {class}: {other:?}"),
    }
}

#[test]
fn only_the_jdk_application_and_platform_loader_classes_qualify() {
    let mut ctx = mock_ctx();
    let app = instance_of(&mut ctx, "jdk/internal/loader/ClassLoaders$AppClassLoader");
    let platform = instance_of(
        &mut ctx,
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
    );
    let custom_system = instance_of(&mut ctx, "com/example/MySystemLoader");
    let url = instance_of(&mut ctx, "java/net/URLClassLoader");
    assert!(for_name_loader_is_builtin_jdk_class(&ctx, app));
    assert!(for_name_loader_is_builtin_jdk_class(&ctx, platform));
    // A `-Djava.system.class.loader` loader runs its own `loadClass`.
    assert!(!for_name_loader_is_builtin_jdk_class(&ctx, custom_system));
    assert!(!for_name_loader_is_builtin_jdk_class(&ctx, url));
}

#[test]
fn the_shortcut_declines_outside_jdk_only_and_for_names_check_name_refuses() {
    let mut ctx = mock_ctx();
    let app = instance_of(&mut ctx, "jdk/internal/loader/ClassLoaders$AppClassLoader");
    let name = ctx.create_string("java.lang.String");
    // `--compatible` (the mock's default): `loadClass` runs as before, and the
    // loader and name come back untouched.
    let (answer, loader, name_back) = for_name_builtin_loaded_first(
        &mut ctx,
        app,
        name,
        "java.lang.String",
        "java/lang/String",
    );
    assert!(answer.is_none());
    assert_eq!(loader.as_ptr(), app.as_ptr());
    assert_eq!(name_back.as_ptr(), name.as_ptr());
    // `--real-jdk`: a name the JDK's `checkName` refuses never reaches the
    // native (its `findLoadedClass` answers null for it before the native).
    ctx.set_jdk_only(true);
    for (dotted, internal) in [
        ("java/lang/String", "java/lang/String"),
        ("[I", "[I"),
        ("", ""),
    ] {
        let (answer, _, _) =
            for_name_builtin_loaded_first(&mut ctx, app, name, dotted, internal);
        assert!(answer.is_none(), "{dotted:?} must run loadClass");
    }
    // Not a built-in loader: declines before the native too.
    let custom = instance_of(&mut ctx, "com/example/MySystemLoader");
    let (answer, _, _) = for_name_builtin_loaded_first(
        &mut ctx,
        custom,
        name,
        "java.lang.String",
        "java/lang/String",
    );
    assert!(answer.is_none());
}
