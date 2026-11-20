use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const PUBLIC_IFACE: u16 = 0x0601;
const PACKAGE_IFACE: u16 = 0x0600;

fn iface(
    ctx: &mut crate::test_utils::MockNativeContext,
    name: &str,
    flags: u16,
    loader: i32,
) -> cratonvm_types::ClassId {
    let cid = ctx.ensure_class_initialized(name).unwrap();
    ctx.set_class_access_flags(cid, flags);
    ctx.set_loader_id_override(cid, loader);
    cid
}

/// `R13SpringProxyNestedIface`'s shape: `{ParameterizedType,
/// SerializableTypeProxy (package-private, app loader), Serializable}`
/// cached under an allocated built-in namespace. The proxy must be
/// defined by the application loader, the interface's own, or its
/// `checkcast TypeProvider` is refused as another runtime package.
#[test]
fn a_package_private_interface_puts_the_proxy_in_its_own_loader() {
    let mut ctx = crate::test_utils::mock_ctx();
    let pt = iface(
        &mut ctx,
        "java/lang/reflect/ParameterizedType",
        PUBLIC_IFACE,
        0,
    );
    let stp = iface(
        &mut ctx,
        "p/Wrapper$SerializableTypeProxy",
        PACKAGE_IFACE,
        2,
    );
    let ser = iface(&mut ctx, "java/io/Serializable", PUBLIC_IFACE, 0);
    let namespace = 41;
    assert_eq!(
        proxy_defining_loader_id(&mut ctx, namespace, &[pt, stp, ser]),
        cratonvm_types::ClassLoaderId::NATIVE_APPLICATION
    );
    // A user-defined loader's interface: that loader.
    let own = iface(&mut ctx, "q/Own", PACKAGE_IFACE, 57);
    assert_eq!(proxy_defining_loader_id(&mut ctx, namespace, &[own]), 57);
}

/// Controls: every interface public keeps the namespace (the proxy goes to
/// `jdk/proxyN`), and so does a package-private BOOTSTRAP interface (`0`
/// is the define boundary's "unspecified" sentinel, not bootstrap).
#[test]
fn public_interfaces_and_bootstrap_interfaces_keep_the_namespace() {
    let mut ctx = crate::test_utils::mock_ctx();
    let a = iface(&mut ctx, "p/PublicA", PUBLIC_IFACE, 2);
    let b = iface(&mut ctx, "java/util/PackageB", PACKAGE_IFACE, 0);
    assert_eq!(proxy_defining_loader_id(&mut ctx, 41, &[a]), 41);
    assert_eq!(proxy_defining_loader_id(&mut ctx, 41, &[a, b]), 41);
}
