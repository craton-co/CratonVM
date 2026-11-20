use super::*;
use cratonvm_classloading::proxy_gen::ProxyClassSpec;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn iface_with(
    ctx: &mut crate::test_utils::MockNativeContext,
    name: &str,
    flags: u16,
    methods: &[(&str, &str)],
) -> cratonvm_types::ClassId {
    let cid = ctx.ensure_class_initialized(name).unwrap();
    ctx.set_class_access_flags(cid, flags);
    ctx.set_declared_methods(
        cid,
        methods
            .iter()
            .map(|(n, d)| cratonvm_native_api::MethodMetadata {
                name: n.to_string(),
                descriptor: d.to_string(),
                access_flags: 0x0401,
                declaring_class_id: cid,
                exceptions: Vec::new(),
                signature: None,
            })
            .collect(),
    );
    cid
}

fn owner_of<'a>(spec: &'a ProxyClassSpec, name: &str) -> &'a str {
    spec.methods
        .iter()
        .find(|m| m.name == name)
        .map(|m| m.iface_owner.as_str())
        .unwrap_or("")
}

/// `{A, B}` both declaring `run()`: `A`'s (the JDK's first registration),
/// and `Object.toString` even though `B` redeclares it.
#[test]
fn the_first_interface_and_objects_methods_win() {
    let mut ctx = crate::test_utils::mock_ctx();
    let a = iface_with(&mut ctx, "p/A", 0x0601, &[("run", "()I")]);
    let b = iface_with(
        &mut ctx,
        "p/B",
        0x0601,
        &[("run", "()I"), ("toString", "()Ljava/lang/String;")],
    );
    let (_, spec) = build_proxy_spec_for(&mut ctx, 1, &[a, b]).expect("spec");
    assert_eq!(owner_of(&spec, "run"), "p/A");
    assert_eq!(owner_of(&spec, "toString"), "java/lang/Object");
    assert!(!spec.package_private, "public interfaces: a public proxy");
    // i4 wave 85, lane L3: `ProxyGenerator`'s numbering order (`m0` =
    // hashCode, `m1` = equals, `m2` = toString, then the interfaces').
    let names: Vec<&str> = spec.methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["hashCode", "equals", "toString", "run"]);
    let (_, spec) = build_proxy_spec_for(&mut ctx, 1, &[b, a]).expect("spec");
    assert_eq!(owner_of(&spec, "run"), "p/B");
}

/// Round 13 wave 5 hand-back: a complete chain ends in `java/lang/Object`
/// (`Throwable extends Object`); a checked exception is one that passes
/// through `Throwable` and not through `RuntimeException` / `Error`.
#[test]
fn a_thrown_chain_ending_in_object_is_classified() {
    let chain = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    assert!(proxy_thrown_chain_is_checked(&chain(&[
        "java/io/IOException",
        "java/lang/Exception",
        "java/lang/Throwable",
        "java/lang/Object",
    ])));
    assert!(!proxy_thrown_chain_is_checked(&chain(&[
        "java/lang/IllegalStateException",
        "java/lang/RuntimeException",
        "java/lang/Exception",
        "java/lang/Throwable",
        "java/lang/Object",
    ])));
    assert!(!proxy_thrown_chain_is_checked(&chain(&[
        "java/lang/AssertionError",
        "java/lang/Error",
        "java/lang/Throwable",
        "java/lang/Object",
    ])));
    assert!(!proxy_thrown_chain_is_checked(&chain(&[
        "p/NotAThrowable",
        "java/lang/Object"
    ])));
}

/// Round 13 wave 5 (lane proxy3): `ProxyGenerator.collectCompatibleTypes`
/// both ways. `A.run() throws IOException` + `B.run()` merge to nothing;
/// `IOException` + `FileNotFoundException` keep the narrower one; a type
/// that is not loaded is kept (the merge cannot disprove it).
#[test]
fn merged_throws_follow_collect_compatible_types() {
    let mut ctx = crate::test_utils::mock_ctx();
    let throwable = ctx.ensure_class_initialized("java/lang/Throwable").unwrap();
    let exception = ctx.ensure_class_initialized("java/lang/Exception").unwrap();
    let ioe = ctx.ensure_class_initialized("java/io/IOException").unwrap();
    let fnfe = ctx
        .ensure_class_initialized("java/io/FileNotFoundException")
        .unwrap();
    ctx.set_superclass(exception, throwable);
    ctx.set_superclass(ioe, exception);
    ctx.set_superclass(fnfe, ioe);
    let names = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    assert!(
        proxy_merged_exception_types(&ctx, &names(&["java/io/IOException"]), &[]).is_empty()
    );
    assert_eq!(
        proxy_merged_exception_types(
            &ctx,
            &names(&["java/io/IOException"]),
            &names(&["java/io/FileNotFoundException"])
        ),
        names(&["java/io/FileNotFoundException"])
    );
    assert_eq!(
        proxy_merged_exception_types(
            &ctx,
            &names(&["java/io/FileNotFoundException", "p/NotLoadedException"]),
            &names(&["java/lang/Exception"])
        ),
        names(&["java/io/FileNotFoundException", "p/NotLoadedException"])
    );
    assert_eq!(
        proxy_merged_exception_types(
            &ctx,
            &names(&["java/io/IOException"]),
            &names(&["java/io/IOException"])
        ),
        names(&["java/io/IOException"])
    );
}

/// Round 13 wave 5 (lane proxy3): a user class extending
/// `java.lang.reflect.Proxy` is not a proxy class (`isProxyClass`,
/// `getInvocationHandler`); a generated `$ProxyN` and the shim are.
#[test]
fn a_user_subclass_of_proxy_is_not_a_generated_proxy() {
    if !real_proxy_super() || !proxy_user_subclass_is_ordinary() {
        return;
    }
    let mut ctx = crate::test_utils::mock_ctx();
    let proxy = ctx
        .ensure_class_initialized("java/lang/reflect/Proxy")
        .unwrap();
    let generated = ctx.ensure_class_initialized("jdk/proxy1/$Proxy7").unwrap();
    let user = ctx.ensure_class_initialized("p/MyProxy").unwrap();
    let user_sub = ctx.ensure_class_initialized("p/$ProxyLike").unwrap();
    ctx.set_superclass(generated, proxy);
    ctx.set_superclass(user, proxy);
    ctx.set_superclass(user_sub, user);
    assert!(proxy_class_is_generated(&ctx, generated));
    assert!(!proxy_class_is_generated(&ctx, user));
    assert!(!proxy_class_is_generated(&ctx, user_sub));
    assert!(
        proxy_chain_reaches_instance(&ctx, user),
        "still on the chain"
    );
}


#[test]
fn a_non_public_interface_makes_the_class_package_private() {
    let mut ctx = crate::test_utils::mock_ctx();
    let h = iface_with(&mut ctx, "p/Hidden", 0x0600, &[("f", "()I")]);
    let (name, spec) = build_proxy_spec_for(&mut ctx, 1, &[h]).expect("spec");
    assert!(spec.package_private);
    assert!(name.starts_with("p/$Proxy"), "got {name}");
}
