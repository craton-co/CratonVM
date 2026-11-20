use super::*;
use cratonvm_native_api::NativeHeapAccess;
use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::VmError;

const NAME: &str = "dumpHeap0";
const DESC: &str = "(Ljava/lang/String;Z)V";

#[test]
fn gci_i47w_dh47_dumpheap0_is_a_bridge_in_both_modes() {
    for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        register_hotspot_diagnostic(&mut r);
        assert_eq!(
            r.kind_of(HOTSPOT_DIAGNOSTIC, NAME, DESC),
            Some(cratonvm_native_api::NativeKind::Bridge),
            "{mode:?}"
        );
        assert!(r.find(HOTSPOT_DIAGNOSTIC, NAME, DESC).is_some(), "{mode:?}");
        // JDK 8's class name is not registered any more.
        assert!(r.find("sun/management/HotSpotDiagnostic", NAME, DESC).is_none());
    }
    assert_eq!(HOTSPOT_DIAGNOSTIC, "com/sun/management/internal/HotSpotDiagnostic");
}

#[test]
fn gci_i47w_dh47_null_path_is_hotspots_npe() {
    let mut r = NativeMethodRegistry::new();
    register_hotspot_diagnostic(&mut r);
    let dump = r.find(HOTSPOT_DIAGNOSTIC, NAME, DESC).expect(NAME);
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    match dump(&mut ctx, &[Value::Object(None), Value::Object(None), Value::Int(1)]) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message },
        ))) => assert_eq!(message.as_deref(), Some("Output file name cannot be null.")),
        other => panic!("want NPE, got {other:?}"),
    }
}

#[test]
fn gci_i47w_dh47_a_refused_dump_is_an_ioexception_with_the_vms_message() {
    // The mock context has no heap dumper: its refusal text must reach
    // Java as `java.io.IOException(<text>)` (the VM's text for an
    // existing file is `File exists`, `hprof::open_error_reason`).
    let mut r = NativeMethodRegistry::new();
    register_hotspot_diagnostic(&mut r);
    let dump = r.find(HOTSPOT_DIAGNOSTIC, NAME, DESC).expect(NAME);
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let path = ctx.create_string_uninterned("dh47.hprof");
    for live in [0, 1] {
        match dump(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(path)), Value::Int(live)],
        ) {
            Err(MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
                message,
            }))) => assert_eq!(message, "heap dump not supported"),
            other => panic!("want IOException, got {other:?}"),
        }
    }
}
