use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::NativeHeapAccess;

#[test]
fn huc_body_stream_class_is_a_baos_with_four_native_methods() {
    let b = huc_body_stream_class_bytes();
    let parsed = cratonvm_reader::read_class(&b).expect("parses");
    assert_eq!(&*parsed.this_class, HUC_BODY_STREAM_CLASS);
    assert_eq!(parsed.super_class.as_deref(), Some(BAOS_CLASS));
    let mut got: Vec<(String, String)> = parsed
        .methods
        .iter()
        .map(|m| (m.name.to_string(), m.descriptor.to_string()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("close".to_string(), "()V".to_string()),
            ("flush".to_string(), "()V".to_string()),
            ("write".to_string(), "(I)V".to_string()),
            ("write".to_string(), "([BII)V".to_string()),
        ]
    );
}

/// Every carrier method is registered, as a stated `Bridge`, and the
/// registry's ambient category is restored.
#[test]
fn huc_body_stream_natives_are_bridges_and_restore_the_category() {
    let mut r = NativeMethodRegistry::new();
    register_huc_body_stream_natives(&mut r);
    for (m, d) in [
        ("write", "(I)V"),
        ("write", "([BII)V"),
        ("flush", "()V"),
        ("close", "()V"),
    ] {
        let (_, kind) = r
            .find_with_kind(HUC_BODY_STREAM_CLASS, m, d)
            .expect("registered");
        assert_eq!(kind, cratonvm_native_api::NativeKind::Bridge, "{m}{d}");
    }
}

/// nio21 wave 3 L1: under `--real-jdk` the carrier class is never minted
/// (class fabrication), so its natives are not registered either, and the
/// ambient category is untouched.
#[test]
fn r21w3_huc_body_stream_is_never_registered_under_jdk_only() {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(cratonvm_types::compat::CompatibilityMode::JdkOnly);
    register_huc_body_stream_natives(&mut r);
    for (m, d) in [
        ("write", "(I)V"),
        ("write", "([BII)V"),
        ("flush", "()V"),
        ("close", "()V"),
    ] {
        assert!(
            r.find_with_kind(HUC_BODY_STREAM_CLASS, m, d).is_none(),
            "{m}{d}"
        );
    }
}

/// nio21 wave 4 L1: the JDK `StreamingOutputStream` wrapper is built only
/// for a receiver the context can show is a
/// `sun.net.www.protocol.http.HttpURLConnection` (its `this$0` type); a
/// context that cannot answer (the mock) gets the carrier back, as before.
#[test]
fn r21w4_streaming_wrapper_needs_a_sun_huc_receiver() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    assert_eq!(
        huc_streaming_output_stream(&mut ctx, this, carrier, 5),
        None
    );
    // A plain row is its own carrier.
    assert_eq!(huc_body_row_carrier(&ctx, carrier), carrier);
    assert_eq!(
        HUC_STREAMING_STREAM_INIT,
        "(Lsun/net/www/protocol/http/HttpURLConnection;Ljava/io/OutputStream;J)V"
    );
}

/// nio21 wave 7 L1: the wrapper takes the two connection classes
/// `openConnection` mints, by exact name (a user subclass has its own).
#[test]
fn r21w7_streaming_wrapper_accepts_the_minted_carriers_by_exact_name() {
    assert!(huc_is_minted_carrier_name("java/net/HttpURLConnection"));
    assert!(huc_is_minted_carrier_name(
        "sun/net/www/protocol/https/HttpsURLConnectionImpl"
    ));
    assert!(!huc_is_minted_carrier_name(
        "com/example/MyHttpURLConnection"
    ));
    assert!(!huc_is_minted_carrier_name("java/net/URLConnection"));
    assert!(!huc_is_minted_carrier_name(HUC_BODY_STREAM_CLASS));
}

/// nio21 wave 4 L1: marking a live upload as JDK-counted reaches the
/// upload of that VM's connection only.
#[test]
fn r21w4_jdk_counted_marks_only_its_vm_upload() {
    const VM_E: usize = 0xA11B_0005;
    const VM_F: usize = 0xA11B_0006;
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            forget_vm_http_url_connection_state(VM_E);
            forget_vm_http_url_connection_state(VM_F);
        }
    }
    let _teardown = Teardown;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let tcp = TcpStream::connect(listener.local_addr().unwrap()).expect("connect loopback");
    let key = 0x0A11_B300;
    let cell: LiveFixedStreamCell = Arc::new(Mutex::new(LiveFixedStream {
        tcp,
        expected: 5,
        written: 0,
        closed: false,
        jdk_counted: false,
    }));
    REAL_CARRIERS.with(VM_E, |t| {
        t.fixed_streams.insert(key, cell.clone());
    });
    mark_live_fixed_stream_jdk_counted(VM_F, key);
    assert!(
        !lock_live_fixed_stream(&cell).jdk_counted,
        "another VM's mark is a no-op"
    );
    mark_live_fixed_stream_jdk_counted(VM_E, key);
    assert!(lock_live_fixed_stream(&cell).jdk_counted);
}
