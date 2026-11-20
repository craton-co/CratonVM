use super::{declared_methods_with_synthetic, lambda_proxy_interface_ids};
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{LambdaSerialMetadata, NativeClassAccess};
use cratonvm_types::ClassId;

fn meta(iface: &str, markers: &[&str], bridges: &[&str]) -> LambdaSerialMetadata {
    LambdaSerialMetadata {
        functional_interface: iface.to_string(),
        sam_method_name: "m".to_string(),
        sam_descriptor: "()Ljava/lang/String;".to_string(),
        impl_class: "p/Host".to_string(),
        impl_member: "lambda$main$0".to_string(),
        impl_descriptor: "()Ljava/lang/String;".to_string(),
        impl_ref_kind: 6,
        instantiated_descriptor: "()Ljava/lang/String;".to_string(),
        capture_types: String::new(),
        marker_interfaces: markers.iter().map(|s| s.to_string()).collect(),
        bridges: bridges.iter().map(|s| s.to_string()).collect(),
    }
}

/// `(ObjM & StrM) () -> "s"`: HotSpot's spun class declares `String m()`
/// and the `FLAG_BRIDGES` bridge `Object m()`, both public, and implements
/// `ObjM, StrM` (each once). The bridge is `ACC_PUBLIC | ACC_BRIDGE`
/// (0x0041), as `InnerClassLambdaMetafactory` generates it and as HotSpot
/// answers `Method.isBridge()` (interpreter round i4 wave 86, lane L4); the
/// SAM implementation is `ACC_PUBLIC` only. A lambda without bridges or
/// markers declares the SAM only and implements its functional interface
/// only.
#[test]
fn a_lambda_class_declares_its_bridges_and_implements_its_markers() {
    let mut ctx = mock_ctx();
    let obj_m = ctx
        .ensure_class_initialized("p/ObjM")
        .unwrap_or(ClassId::new(0));
    let str_m = ctx
        .ensure_class_initialized("p/StrM")
        .unwrap_or(ClassId::new(0));
    assert_ne!(obj_m, str_m);

    let proxy = ClassId::new(0x8000_0c01);
    ctx.set_lambda_proxy_host(proxy, "p/Host");
    ctx.set_lambda_serial_metadata(
        proxy,
        meta("p/ObjM", &["p/StrM", "p/ObjM"], &["()Ljava/lang/Object;"]),
    );
    let mut declared: Vec<(String, u16)> = declared_methods_with_synthetic(&ctx, proxy)
        .into_iter()
        .filter(|m| m.name == "m")
        .map(|m| (m.descriptor, m.access_flags))
        .collect();
    declared.sort();
    assert_eq!(
        declared,
        vec![
            ("()Ljava/lang/Object;".to_string(), 0x0041),
            ("()Ljava/lang/String;".to_string(), 0x0001),
        ]
    );
    assert_eq!(
        lambda_proxy_interface_ids(&mut ctx, proxy, obj_m),
        vec![obj_m, str_m]
    );

    let plain = ClassId::new(0x8000_0c02);
    ctx.set_lambda_proxy_host(plain, "p/Host");
    ctx.set_lambda_serial_metadata(plain, meta("p/ObjM", &[], &[]));
    let declared = declared_methods_with_synthetic(&ctx, plain)
        .into_iter()
        .filter(|m| m.name == "m")
        .count();
    assert_eq!(declared, 1);
    assert_eq!(
        lambda_proxy_interface_ids(&mut ctx, plain, obj_m),
        vec![obj_m]
    );
}
