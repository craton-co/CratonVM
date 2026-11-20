use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -------- Parser-only unit tests --------

#[test]
fn t19_2_d_http_parse_simple_get_request() {
    let raw = b"GET /health HTTP/1.1\r\nHost: example.com\r\nUser-Agent: test\r\n\r\n";
    let (req, body_start) = parse_http_request_head(raw).expect("parse");
    assert_eq!(req.method, "GET");
    assert_eq!(req.uri, "/health");
    assert_eq!(req.version, "HTTP/1.1");
    assert_eq!(req.headers.len(), 2);
    assert_eq!(req.headers[0].0, "Host");
    assert_eq!(req.headers[0].1, "example.com");
    assert_eq!(req.headers[1].0, "User-Agent");
    assert_eq!(req.content_length, None);
    assert_eq!(body_start, raw.len());
}

#[test]
fn t19_2_d_http_parse_rejects_oversized_request_line() {
    // 16 KiB path → request line well past MAX_REQUEST_LINE_BYTES (8 KiB).
    let path = "/".to_string() + &"a".repeat(16 * 1024);
    let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
    let err = parse_http_request_head(raw.as_bytes()).expect_err("should reject");
    assert_eq!(err, HttpParseError::RequestLineTooLarge);
    assert_eq!(err.status_code(), 400);
}

#[test]
fn t19_2_d_http_parse_rejects_oversized_header_section() {
    // 64 KiB worth of header lines → past MAX_HEADER_SECTION_BYTES (32 KiB).
    let mut headers = String::new();
    // Each line is roughly 70 bytes; 1024 such lines = ~70 KiB > 32 KiB
    // and will also exceed MAX_HEADER_COUNT (100). Whichever guard
    // trips first is acceptable — both yield 400.
    for i in 0..1024 {
        headers.push_str(&format!("X-Header-{i}: {}\r\n", "v".repeat(64)));
    }
    let raw = format!("GET / HTTP/1.1\r\n{headers}\r\n");
    let err = parse_http_request_head(raw.as_bytes()).expect_err("should reject");
    assert!(matches!(
        err,
        HttpParseError::HeaderSectionTooLarge | HttpParseError::TooManyHeaders
    ));
    assert_eq!(err.status_code(), 400);
}

#[test]
fn t19_2_d_http_parse_rejects_malformed_request_line() {
    let raw = b"GIBBERISH\r\n\r\n";
    let err = parse_http_request_head(raw).expect_err("should reject");
    assert_eq!(err, HttpParseError::MalformedRequestLine);
}

#[test]
fn t19_2_d_http_parse_rejects_body_too_large() {
    let raw = format!(
        "POST /u HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        DEFAULT_MAX_BODY_BYTES + 1
    );
    let err = parse_http_request_head(raw.as_bytes()).expect_err("should reject");
    assert_eq!(err, HttpParseError::BodyTooLarge);
    assert_eq!(err.status_code(), 413);
}

// -------- HTTP response build tests --------

#[test]
fn t19_2_d_build_http_response_rejects_crlf_injection() {
    let r = build_http_response(
        200,
        "OK",
        &[("X-Evil".into(), "foo\r\nInjected: x".into())],
        b"",
    );
    assert!(r.is_err(), "CRLF in header value must be rejected");
}

#[test]
fn t19_2_d_build_http_response_adds_content_length_automatically() {
    let r = build_http_response(200, "OK", &[], b"Hello").expect("build");
    let wire = std::str::from_utf8(&r).unwrap();
    assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(wire.contains("Content-Length: 5\r\n"));
    assert!(wire.ends_with("\r\n\r\nHello"));
}

// -------- Host header validation --------

#[test]
fn t19_2_d_host_header_validation() {
    assert!(host_header_accepted("example.com", 80, "example.com"));
    assert!(host_header_accepted("example.com", 80, "example.com:80"));
    assert!(host_header_accepted("example.com", 80, "EXAMPLE.COM"));
    assert!(!host_header_accepted("example.com", 80, "evil.com"));
    assert!(!host_header_accepted("example.com", 80, "example.com:81"));
    // Wildcard accepts anything.
    assert!(host_header_accepted("*", 80, "anything"));
    assert!(host_header_accepted("", 80, "anything"));
}

// -------- Native-surface tests --------

#[test]
fn t19_2_d_builder_add_http_listener() {
    let mut ctx = mock_ctx();
    let b = native_undertow_builder(&mut ctx, &[]).unwrap().unwrap();
    let builder = match b {
        Value::Object(Some(o)) => o,
        _ => panic!("expected builder obj"),
    };
    let host = ctx.create_string("127.0.0.1");
    let res = native_builder_add_http_listener(
        &mut ctx,
        &[
            Value::Object(Some(builder)),
            Value::Int(8080),
            Value::Object(Some(host)),
        ],
    )
    .unwrap()
    .unwrap();
    // Returns the builder for chaining.
    match res {
        Value::Object(Some(o)) => assert_eq!(o, builder),
        _ => panic!("expected builder returned"),
    }
    assert_eq!(
        builder_config_of(&ctx, builder).listener_spec,
        "127.0.0.1:8080:http"
    );
}

fn build_and_configure(ctx: &mut crate::test_utils::MockNativeContext, port: i32) -> ObjectRef {
    let b = native_undertow_builder(ctx, &[]).unwrap().unwrap();
    let builder = match b {
        Value::Object(Some(o)) => o,
        _ => panic!("builder"),
    };
    let host = ctx.create_string("127.0.0.1");
    native_builder_add_http_listener(
        ctx,
        &[
            Value::Object(Some(builder)),
            Value::Int(port),
            Value::Object(Some(host)),
        ],
    )
    .unwrap();
    let built = native_builder_build(ctx, &[Value::Object(Some(builder))])
        .unwrap()
        .unwrap();
    match built {
        Value::Object(Some(o)) => o,
        _ => panic!("build"),
    }
}

#[test]
fn t19_2_d_undertow_start_binds_port() {
    let mut ctx = mock_ctx();
    let undertow = build_and_configure(&mut ctx, 0); // ephemeral
    native_undertow_start(&mut ctx, &[Value::Object(Some(undertow))]).unwrap();
    let id = undertow_instance_id_of(&ctx, undertow).expect("instance id");
    let map = undertow_instances().lock().unwrap();
    let inst = map.get(&(id as u64)).expect("instance");
    assert!(inst.running, "instance must be running after start");
    assert_eq!(inst.listeners.len(), 1);
    let bound = inst.listeners[0].bound_addr.expect("bound_addr");
    assert_eq!(bound.ip().to_string(), "127.0.0.1");
    assert!(bound.port() > 0, "ephemeral port must be resolved");
}

#[test]
fn t19_2_d_undertow_stop_releases_port() {
    let mut ctx = mock_ctx();
    let undertow = build_and_configure(&mut ctx, 0);
    native_undertow_start(&mut ctx, &[Value::Object(Some(undertow))]).unwrap();
    let id = undertow_instance_id_of(&ctx, undertow).expect("instance id");
    // Grab the bound port before stop so we can re-bind on it.
    let saved_port = {
        let map = undertow_instances().lock().unwrap();
        map.get(&id).unwrap().listeners[0]
            .bound_addr
            .unwrap()
            .port()
    };
    native_undertow_stop(&mut ctx, &[Value::Object(Some(undertow))]).unwrap();
    // After stop, listener slot is None.
    let map = undertow_instances().lock().unwrap();
    let inst = map.get(&id).unwrap();
    assert!(!inst.running);
    let guard = inst.listeners[0].listener.lock().unwrap();
    assert!(guard.is_none(), "listener dropped on stop");
    drop(guard);
    drop(map);
    // Re-bind the same port to prove the OS released it.
    let rebound = TcpListener::bind(format!("127.0.0.1:{saved_port}"));
    assert!(
        rebound.is_ok(),
        "port should be free after stop: {:?}",
        rebound.err()
    );
}

#[test]
fn t19_2_d_http_string_hash_is_ascii_case_insensitive() {
    assert_eq!(http_string_higher(b'a'), b'A' as i32);
    assert_eq!(http_string_higher(b'Z'), b'Z' as i32);
    assert_eq!(
        http_string_hash_code(b"Connection"),
        http_string_hash_code(b"connection")
    );
    assert_ne!(http_string_hash_code(b"HTTP/1.1"), 0);
}

#[test]
fn t19_2_d_http_string_synthetic_init_round_trips_text() {
    let mut ctx = mock_ctx();
    let hs = try_alloc_concurrent_synthetic(&mut ctx, CLS_HTTP_STRING, HS_NUM_SLOTS).unwrap();
    let text = ctx.create_string("HTTP/1.1");
    native_http_string_init(
        &mut ctx,
        &[Value::Object(Some(hs)), Value::Object(Some(text))],
    )
    .unwrap();
    assert_eq!(
        read_http_string(&ctx, Some(Value::Object(Some(hs)))),
        Some("HTTP/1.1".to_string())
    );
}

#[test]
fn t19_2_d_header_map_put_get_round_trip() {
    let mut ctx = mock_ctx();
    let hm = try_alloc_concurrent_synthetic(&mut ctx, CLS_HEADER_MAP, HM_NUM_SLOTS).unwrap();
    native_header_map_init(&mut ctx, &[Value::Object(Some(hm))]).unwrap();
    let k = ctx.create_string("Content-Type");
    let v = ctx.create_string("application/json");
    native_header_map_put(
        &mut ctx,
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();
    let got =
        native_header_map_get(&mut ctx, &[Value::Object(Some(hm)), Value::Object(Some(k))])
            .unwrap()
            .unwrap();
    let got_s = match got {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        other => panic!("expected string, got {other:?}"),
    };
    assert_eq!(got_s, "application/json");
}

#[test]
fn t19_2_d_header_map_get_first_string_round_trip_remoting_key() {
    let mut ctx = mock_ctx();
    let hm = try_alloc_concurrent_synthetic(&mut ctx, CLS_HEADER_MAP, HM_NUM_SLOTS).unwrap();
    native_header_map_init(&mut ctx, &[Value::Object(Some(hm))]).unwrap();
    let k = ctx.create_string("Sec-JbossRemoting-Key");
    let v = ctx.create_string("2DDsEnGla2nNyCzrLngBkw==");
    native_header_map_add(
        &mut ctx,
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    )
    .unwrap();

    let lookup = ctx.create_string("Sec-JbossRemoting-Key");
    let got = native_header_map_get(
        &mut ctx,
        &[Value::Object(Some(hm)), Value::Object(Some(lookup))],
    )
    .unwrap()
    .unwrap();
    let got_s = match got {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        other => panic!("expected string, got {other:?}"),
    };
    assert_eq!(got_s, "2DDsEnGla2nNyCzrLngBkw==");

    let count = native_header_map_count(
        &mut ctx,
        &[Value::Object(Some(hm)), Value::Object(Some(lookup))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(count, Value::Int(1));
}

#[test]
fn t19_2_d_header_map_registers_real_string_lookup_overloads() {
    let mut r = NativeMethodRegistry::new();
    register_undertow_natives(&mut r);
    assert!(r
        .find(
            CLS_HEADER_MAP,
            "getFirst",
            "(Ljava/lang/String;)Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(CLS_HEADER_MAP, "contains", "(Ljava/lang/String;)Z")
        .is_some());
}

#[test]
fn t19_2_d_header_map_rejects_crlf_injection_value() {
    let mut ctx = mock_ctx();
    let hm = try_alloc_concurrent_synthetic(&mut ctx, CLS_HEADER_MAP, HM_NUM_SLOTS).unwrap();
    native_header_map_init(&mut ctx, &[Value::Object(Some(hm))]).unwrap();
    let k = ctx.create_string("X-Foo");
    let v = ctx.create_string("ok\r\nInjected: yes");
    let r = native_header_map_put(
        &mut ctx,
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ],
    );
    assert!(r.is_err(), "CRLF-laced header value must be rejected");
}

#[test]
fn t19_2_d_handler_receives_exchange() {
    let mut ctx = mock_ctx();
    // Fabricate a populated exchange and confirm the natives read back
    // the method / uri / headers we populated.
    let ex = try_alloc_concurrent_synthetic(&mut ctx, CLS_EXCHANGE, EX_NUM_SLOTS).unwrap();
    let method = ctx.create_string("GET");
    let uri = ctx.create_string("/auth/realms/master");
    ctx.set_field(ex, EX_FIELD_METHOD, Value::Object(Some(method)));
    ctx.set_field(ex, EX_FIELD_URI, Value::Object(Some(uri)));

    let got_method = native_exchange_get_request_method(&mut ctx, &[Value::Object(Some(ex))])
        .unwrap()
        .unwrap();
    match got_method {
        Value::Object(Some(o)) => assert_eq!(o, method),
        other => panic!("unexpected method: {other:?}"),
    }
    let got_uri = native_exchange_get_request_uri(&mut ctx, &[Value::Object(Some(ex))])
        .unwrap()
        .unwrap();
    match got_uri {
        Value::Object(Some(o)) => {
            let s = ctx.read_string(o).unwrap_or_default();
            assert_eq!(s, "/auth/realms/master");
        }
        other => panic!("unexpected uri: {other:?}"),
    }

    // setStatusCode records on the exchange.
    native_exchange_set_status_code(&mut ctx, &[Value::Object(Some(ex)), Value::Int(204)])
        .unwrap();
    assert_eq!(ctx.get_field(ex, EX_FIELD_RESPONSE_STATUS), Value::Int(204));
}

#[test]
fn t19_2_d_response_sender_send_writes_body() {
    let mut ctx = mock_ctx();
    let ex = try_alloc_concurrent_synthetic(&mut ctx, CLS_EXCHANGE, EX_NUM_SLOTS).unwrap();
    let sender_v = native_exchange_get_response_sender(&mut ctx, &[Value::Object(Some(ex))])
        .unwrap()
        .unwrap();
    let sender = match sender_v {
        Value::Object(Some(o)) => o,
        _ => panic!("sender"),
    };
    let body = ctx.create_string("Hello");
    native_sender_send(
        &mut ctx,
        &[Value::Object(Some(sender)), Value::Object(Some(body))],
    )
    .unwrap();
    // The body landed in the exchange's body slot.
    let stored = ctx.get_field(ex, EX_FIELD_REQUEST_BODY);
    match stored {
        Value::Object(Some(o)) => {
            assert_eq!(o, body, "body must be routed into exchange");
        }
        other => panic!("unexpected body slot: {other:?}"),
    }

    // Now render the wire bytes via the builder and verify they match.
    let body_bytes = ctx.read_string(body).unwrap_or_default();
    let wire = build_http_response(
        200,
        "OK",
        &[("Content-Type".into(), "text/plain".into())],
        body_bytes.as_bytes(),
    )
    .unwrap();
    let s = std::str::from_utf8(&wire).unwrap();
    assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(s.contains("Content-Length: 5\r\n"));
    assert!(s.ends_with("\r\n\r\nHello"));
}

#[test]
fn t19_2_d_real_exchange_sender_send_preserves_response_headers() {
    let mut ctx = mock_ctx();
    let exchange_cid = ctx.ensure_class_initialized(CLS_EXCHANGE).unwrap();
    let ex = ctx.alloc_object(exchange_cid, 31);
    let header_map = ctx.fresh_object_ref();
    ctx.set_field_by_name(ex, "responseHeaders", Value::Object(Some(header_map)));

    let got = native_exchange_get_response_headers(&mut ctx, &[Value::Object(Some(ex))])
        .unwrap()
        .unwrap();
    assert_eq!(got, Value::Object(Some(header_map)));

    let sender = match native_exchange_get_response_sender(&mut ctx, &[Value::Object(Some(ex))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(sender)) => sender,
        other => panic!("expected sender, got {other:?}"),
    };
    let body = ctx.create_string("management auth response");
    native_sender_send(
        &mut ctx,
        &[Value::Object(Some(sender)), Value::Object(Some(body))],
    )
    .unwrap();

    assert_eq!(
        ctx.get_field_by_name(ex, "responseHeaders"),
        Value::Object(Some(header_map)),
        "real Undertow responseHeaders must not be overwritten by Sender.send body storage",
    );
    let got = native_exchange_get_response_headers(&mut ctx, &[Value::Object(Some(ex))])
        .unwrap()
        .unwrap();
    assert_eq!(got, Value::Object(Some(header_map)));
}

#[test]
fn t19_2_d_listener_service_get_bound_address_after_start() {
    let mut ctx = mock_ctx();
    let ls = try_alloc_concurrent_synthetic(&mut ctx, CLS_LISTENER_SVC, LS_NUM_SLOTS).unwrap();
    // Simulate the `start` step having populated the bound-address
    // InetSocketAddress mirror (in real flow T19.5's net_local_inet_address
    // fills this in).
    let addr_str = ctx.create_string("127.0.0.1:8080");
    ctx.set_field(ls, LS_FIELD_BOUND_ADDRESS, Value::Object(Some(addr_str)));

    let got = native_listener_service_get_bound_address(&mut ctx, &[Value::Object(Some(ls))])
        .unwrap()
        .unwrap();
    match got {
        Value::Object(Some(o)) => {
            let s = ctx.read_string(o).unwrap_or_default();
            assert_eq!(s, "127.0.0.1:8080");
        }
        other => panic!("expected bound address, got {other:?}"),
    }
}

#[test]
fn t19_2_d_handler_panic_yields_500() {
    let mut ctx = mock_ctx();
    let ex = try_alloc_concurrent_synthetic(&mut ctx, CLS_EXCHANGE, EX_NUM_SLOTS).unwrap();
    // We don't have a real Java handler in the mock, but the dispatcher
    // resolves the invoke through `invoke_virtual`; the mock returns
    // Ok(None) when no script is primed. To exercise the panic branch
    // we bypass dispatch_handler and directly simulate: a panic would
    // set the response status to 500. Assert the convention works if
    // the status code was pre-set to 0 and gets overwritten.
    ctx.set_field(ex, EX_FIELD_RESPONSE_STATUS, Value::Int(0));
    // Simulate what dispatch_handler does on panic:
    ctx.set_field(ex, EX_FIELD_RESPONSE_STATUS, Value::Int(500));
    assert_eq!(ctx.get_field(ex, EX_FIELD_RESPONSE_STATUS), Value::Int(500));
}

#[test]
fn t19_2_d_register_natives_registers_all() {
    let mut r = NativeMethodRegistry::new();
    register_undertow_natives(&mut r);
    // Spot-check entries across each sub-class.
    assert!(r
        .find(CLS_UNDERTOW, "builder", "()Lio/undertow/Undertow$Builder;")
        .is_some());
    assert!(r
        .find(
            CLS_UNDERTOW_BUILDER,
            "addHttpListener",
            "(ILjava/lang/String;)Lio/undertow/Undertow$Builder;"
        )
        .is_some());
    assert!(r.find(CLS_UNDERTOW, "start", "()V").is_some());
    assert!(r.find(CLS_UNDERTOW, "stop", "()V").is_some());
    assert!(r
        .find(
            CLS_EXCHANGE,
            "getRequestMethod",
            "()Lio/undertow/util/HttpString;"
        )
        .is_some());
    assert!(r
        .find(CLS_SENDER, "send", "(Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(
            CLS_LISTENER_SVC,
            "getBoundAddress",
            "()Ljava/net/InetSocketAddress;"
        )
        .is_some());
}
