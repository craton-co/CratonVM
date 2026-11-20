use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess as _;

const VM: usize = 0x18F0_0601;

/// The `InetAddress` rows the build files belong to this test's VM.
struct Teardown;
impl Drop for Teardown {
    fn drop(&mut self) {
        crate::net_phase_e::forget_vm_socket_side_tables(VM);
    }
}

fn string_field(ctx: &dyn NativeContext, obj: ObjectRef, slot: usize) -> Option<String> {
    match ctx.get_field(obj, slot) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn only_address(ctx: &dyn NativeContext, iface: ObjectRef) -> ObjectRef {
    let Value::Object(Some(addrs)) = ctx.get_field(iface, 2) else {
        panic!("no address array");
    };
    assert_eq!(ctx.array_length(addrs), 1);
    match ctx.get_array_element(addrs, 0) {
        Value::Object(Some(ia)) => ia,
        other => panic!("expected an InetAddress, got {other:?}"),
    }
}

#[test]
fn the_built_interfaces_are_complete_and_no_pin_is_left_behind() {
    let _teardown = Teardown;
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let pins_before = ctx.native_pin_count_for_test();
    let ifaces = p61_build_network_interfaces(&mut ctx).expect("the interfaces");
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "both handle scopes closed"
    );
    assert!(
        matches!(ifaces.len(), 1 | 2),
        "loopback, plus the primary one"
    );

    let c: &dyn NativeContext = &ctx;
    let lo = ifaces[0];
    assert_eq!(string_field(c, lo, 0).as_deref(), Some("lo"));
    assert_eq!(
        string_field(c, lo, 1).as_deref(),
        Some("Loopback Interface")
    );
    assert_eq!(c.get_field(lo, 3), Value::Int(1));
    assert_eq!(c.get_field(lo, 4), Value::Int(0b111));
    let lo_addr = only_address(c, lo);
    assert_eq!(
        crate::net_phase_e::inet_addr_resolve(c, lo_addr),
        Some(("localhost".to_string(), "127.0.0.1".to_string()))
    );

    if let Some(&eth) = ifaces.get(1) {
        assert_eq!(string_field(c, eth, 0).as_deref(), Some("eth0"));
        assert_eq!(
            string_field(c, eth, 1).as_deref(),
            Some("Primary Network Interface")
        );
        assert_eq!(c.get_field(eth, 3), Value::Int(2));
        assert_eq!(c.get_field(eth, 4), Value::Int(0b101));
        let eth_addr = only_address(c, eth);
        assert!(crate::net_phase_e::inet_addr_resolve(c, eth_addr).is_some());
    }
}
