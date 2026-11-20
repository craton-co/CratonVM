use super::*;
use crate::test_support::MockNativeContext;
use cratonvm_types::compat::CompatibilityMode;

const FORCE: &str = "()Ljava/nio/MappedByteBuffer;";

/// A JDK-built buffer (`DirectByteBuffer`, which inherits all three
/// methods) goes to `MappedByteBuffer`'s own bytecode, bound statically on
/// that class, and the JDK body's answer is the native's answer.
#[test]
fn r16w16_screen_hands_a_real_receiver_to_the_jdk_body() {
    let mut ctx = MockNativeContext::new();
    let real = ctx.alloc_object_with_class(16, "java/nio/DirectByteBuffer");
    let other = ctx.alloc_object_with_class(16, "java/nio/DirectByteBuffer");
    ctx.script_invoke("force", FORCE, Ok(Some(Value::Object(Some(other)))));
    ctx.script_invoke("load", FORCE, Ok(Some(Value::Object(Some(other)))));
    ctx.script_invoke("isLoaded", "()Z", Ok(Some(Value::Int(0))));
    let args = [Value::Object(Some(real))];
    assert_eq!(
        native_mbb_force_screened(&mut ctx, &args).expect("force"),
        Some(Value::Object(Some(other)))
    );
    assert_eq!(
        native_mbb_load_screened(&mut ctx, &args).expect("load"),
        Some(Value::Object(Some(other)))
    );
    assert_eq!(
        native_mbb_is_loaded_screened(&mut ctx, &args).expect("isLoaded"),
        Some(Value::Int(0))
    );
    let calls = ctx.recorded_calls();
    assert_eq!(calls.len(), 3, "one JDK body per call: {calls:?}");
    let expected = [("force", FORCE), ("load", FORCE), ("isLoaded", "()Z")];
    for (call, (m, d)) in calls.iter().zip(expected) {
        assert_eq!(call.declared_class.as_deref(), Some(MBB_CLASS));
        let got = (call.method_name.as_str(), call.descriptor.as_str());
        assert_eq!(got, (m, d));
        assert!(matches!(call.args.as_slice(), [Value::Object(Some(r))] if *r == real));
    }
}

/// A buffer this VM minted (`alloc_mapped_byte_buffer`, class
/// `MappedByteBuffer`) and a class-id-0 one keep the native, and the
/// screen calls nothing.
#[test]
fn r16w16_screen_keeps_the_native_for_a_vm_minted_buffer() {
    let mut ctx = MockNativeContext::new();
    let minted = alloc_mapped_byte_buffer(&mut ctx, 32);
    let bare = ctx.alloc_object(4);
    let before = ctx.recorded_calls().len();
    for this in [minted, bare] {
        assert!(mbb_receiver_is_vm_minted(&ctx, this));
        let args = [Value::Object(Some(this))];
        assert_eq!(
            native_mbb_force_screened(&mut ctx, &args).expect("force"),
            Some(Value::Object(Some(this)))
        );
        assert_eq!(
            native_mbb_load_screened(&mut ctx, &args).expect("load"),
            Some(Value::Object(Some(this)))
        );
        assert_eq!(
            native_mbb_is_loaded_screened(&mut ctx, &args).expect("isLoaded"),
            native_mbb_is_loaded(&mut ctx, &args).expect("isLoaded")
        );
    }
    assert_eq!(
        ctx.recorded_calls().len(),
        before,
        "the screen must not call out: {:?}",
        &ctx.recorded_calls()[before..]
    );
    let real = ctx.alloc_object_with_class(16, "java/nio/DirectByteBufferR");
    assert!(!mbb_receiver_is_vm_minted(&ctx, real));
}

/// The switch picks the callbacks: screened by default, the historical
/// natives under `CRATONVM_MAPPED_BUFFER_RECEIVER_SCREEN=0`.
#[test]
fn r16w16_switch_selects_the_callbacks() {
    let addr = |f: cratonvm_native_api::NativeCallback| f as usize;
    let (i, l, f) = mapped_buffer_callbacks(true);
    assert_eq!(addr(i), addr(native_mbb_is_loaded_screened));
    assert_eq!(addr(l), addr(native_mbb_load_screened));
    assert_eq!(addr(f), addr(native_mbb_force_screened));
    let (i, l, f) = mapped_buffer_callbacks(false);
    assert_eq!(addr(i), addr(native_mbb_is_loaded));
    assert_eq!(addr(l), addr(native_mbb_load));
    assert_eq!(addr(f), addr(native_mbb_force));
}

/// The three rows are retired: a `SyntheticStub` that `--compatible`
/// registers and dispatches, and that `--real-jdk` refuses, so the JDK
/// bytecode answers there. The ambient category is restored.
#[test]
fn r16w16_the_three_rows_are_retired_in_jdk_only_and_kept_in_compatible() {
    let rows = [("isLoaded", "()Z"), ("load", FORCE), ("force", FORCE)];
    let mut compatible = NativeMethodRegistry::new();
    register_nio_channel_extras(&mut compatible);
    let mut strict = NativeMethodRegistry::new();
    strict.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_nio_channel_extras(&mut strict);
    for (m, d) in rows {
        assert!(
            compatible.kind_of(MBB_CLASS, m, d) == Some(NativeKind::SyntheticStub),
            "{MBB_CLASS}.{m}{d} must be a retired SyntheticStub"
        );
        assert!(
            strict.find(MBB_CLASS, m, d).is_none(),
            "--real-jdk must refuse {MBB_CLASS}.{m}{d}"
        );
    }
    // The abstract `FileChannel.map` that mints the VM's own buffers stays.
    let map = "(Ljava/nio/channels/FileChannel$MapMode;JJ)Ljava/nio/MappedByteBuffer;";
    assert!(
        compatible.kind_of("java/nio/channels/FileChannel", "map", map)
            == Some(NativeKind::Bridge)
    );
}
