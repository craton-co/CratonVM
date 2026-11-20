use super::lambda_proxy_class_name;
use crate::test_utils::mock_ctx;
use cratonvm_types::ClassId;

/// HotSpot's hidden-class tail is `0x` plus 16 zero-padded hex digits
/// (`Probe$$Lambda/0x0000000086040210`); the proxy name has that shape,
/// and an ordinary class id is not a lambda.
#[test]
fn a_lambda_proxy_name_has_hotspots_sixteen_digit_tail() {
    let ctx = mock_ctx();
    let proxy = ClassId::new(0x8000_0003);
    ctx.set_lambda_proxy_host(proxy, "p/q/Host");
    assert_eq!(
        lambda_proxy_class_name(&ctx, proxy).as_deref(),
        Some("p.q.Host$$Lambda/0x0000000080000003")
    );
    assert_eq!(lambda_proxy_class_name(&ctx, ClassId::new(7)), None);
}
