use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// --- Registration tests ------------------------------------------------

#[test]
fn test_http_client_init_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find("java/net/http/HttpClient", "<init>", "()V")
        .is_some());
}

#[test]
fn test_http_client_new_builder_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "newBuilder",
            "()Ljava/net/http/HttpClient$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_client_new_http_client_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "newHttpClient",
            "()Ljava/net/http/HttpClient;"
        )
        .is_some());
}

#[test]
fn test_http_client_send_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r.find(
        "java/net/http/HttpClient",
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"
    ).is_some());
}

#[test]
fn test_http_client_send_async_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r.find(
        "java/net/http/HttpClient",
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"
    ).is_some());
}

#[test]
fn test_http_client_version_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "version",
            "()Ljava/net/http/HttpClient$Version;"
        )
        .is_some());
}

#[test]
fn test_http_client_follow_redirects_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "followRedirects",
            "()Ljava/net/http/HttpClient$Redirect;"
        )
        .is_some());
}

#[test]
fn test_http_client_connect_timeout_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "connectTimeout",
            "()Ljava/util/Optional;"
        )
        .is_some());
}

#[test]
fn test_http_client_ssl_context_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "sslContext",
            "()Ljavax/net/ssl/SSLContext;"
        )
        .is_some());
}

#[test]
fn test_http_client_new_websocket_builder_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "newWebSocketBuilder",
            "()Ljava/net/http/WebSocket$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_client_builder_build_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient$Builder",
            "build",
            "()Ljava/net/http/HttpClient;"
        )
        .is_some());
}

#[test]
fn test_http_client_builder_version_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient$Builder",
            "version",
            "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_client_builder_follow_redirects_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpClient$Builder",
            "followRedirects",
            "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_new_builder_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest",
            "newBuilder",
            "()Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_new_builder_uri_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest",
            "newBuilder",
            "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_method_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest",
            "method",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn test_http_request_builder_get_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$Builder",
            "GET",
            "()Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_builder_post_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$Builder",
            "POST",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_builder_put_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$Builder",
            "PUT",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_builder_delete_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$Builder",
            "DELETE",
            "()Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_builder_header_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$Builder",
            "header",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"
        )
        .is_some());
}

#[test]
fn test_http_request_builder_build_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$Builder",
            "build",
            "()Ljava/net/http/HttpRequest;"
        )
        .is_some());
}

#[test]
fn test_http_response_status_code_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find("java/net/http/HttpResponse", "statusCode", "()I")
        .is_some());
}

#[test]
fn test_http_response_body_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find("java/net/http/HttpResponse", "body", "()Ljava/lang/Object;")
        .is_some());
}

#[test]
fn test_http_response_headers_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpResponse",
            "headers",
            "()Ljava/net/http/HttpHeaders;"
        )
        .is_some());
}

#[test]
fn test_http_response_version_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpResponse",
            "version",
            "()Ljava/net/http/HttpClient$Version;"
        )
        .is_some());
}

#[test]
fn test_http_response_previous_response_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpResponse",
            "previousResponse",
            "()Ljava/util/Optional;"
        )
        .is_some());
}

#[test]
fn test_http_headers_all_values_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpHeaders",
            "allValues",
            "(Ljava/lang/String;)Ljava/util/List;"
        )
        .is_some());
}

#[test]
fn test_http_headers_first_value_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpHeaders",
            "firstValue",
            "(Ljava/lang/String;)Ljava/util/Optional;"
        )
        .is_some());
}

#[test]
fn test_http_headers_first_value_as_long_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpHeaders",
            "firstValueAsLong",
            "(Ljava/lang/String;)Ljava/util/OptionalLong;"
        )
        .is_some());
}

#[test]
fn test_http_headers_map_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find("java/net/http/HttpHeaders", "map", "()Ljava/util/Map;")
        .is_some());
}

#[test]
fn test_body_publishers_of_string_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofString",
            "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;"
        )
        .is_some());
}

#[test]
fn test_body_publishers_no_body_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpRequest$BodyPublishers",
            "noBody",
            "()Ljava/net/http/HttpRequest$BodyPublisher;"
        )
        .is_some());
}

#[test]
fn test_body_handlers_of_string_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpResponse$BodyHandlers",
            "ofString",
            "()Ljava/net/http/HttpResponse$BodyHandler;"
        )
        .is_some());
}

#[test]
fn test_body_handlers_discarding_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/HttpResponse$BodyHandlers",
            "discarding",
            "()Ljava/net/http/HttpResponse$BodyHandler;"
        )
        .is_some());
}

#[test]
fn test_websocket_send_text_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/WebSocket",
            "sendText",
            "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;"
        )
        .is_some());
}

#[test]
fn test_websocket_send_close_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find(
            "java/net/http/WebSocket",
            "sendClose",
            "(ILjava/lang/String;)Ljava/util/concurrent/CompletableFuture;"
        )
        .is_some());
}

#[test]
fn test_websocket_abort_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r.find("java/net/http/WebSocket", "abort", "()V").is_some());
}

#[test]
fn test_websocket_is_output_closed_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find("java/net/http/WebSocket", "isOutputClosed", "()Z")
        .is_some());
}

#[test]
fn test_websocket_is_input_closed_registered() {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    assert!(r
        .find("java/net/http/WebSocket", "isInputClosed", "()Z")
        .is_some());
}

// --- Logic / unit tests ------------------------------------------------

#[test]
fn test_method_idx_to_name_all_variants() {
    assert_eq!(method_idx_to_name(METHOD_GET), "GET");
    assert_eq!(method_idx_to_name(METHOD_POST), "POST");
    assert_eq!(method_idx_to_name(METHOD_PUT), "PUT");
    assert_eq!(method_idx_to_name(METHOD_DELETE), "DELETE");
    assert_eq!(method_idx_to_name(METHOD_HEAD), "HEAD");
    assert_eq!(method_idx_to_name(METHOD_PATCH), "PATCH");
    assert_eq!(method_idx_to_name(METHOD_OPTIONS), "OPTIONS");
    assert_eq!(method_idx_to_name(99), "GET"); // unknown defaults to GET
}

// [VULN fix nb-http2 (2)] CR/LF/NUL request-splitting guard.
#[test]
fn test_validate_no_crlf_accepts_clean_values() {
    assert!(validate_no_crlf("method", "GET").is_ok());
    assert!(validate_no_crlf("request-target", "/path?a=b&c=d").is_ok());
    assert!(validate_no_crlf("Host header", "example.com:8443").is_ok());
    assert!(validate_no_crlf("request-target", "").is_ok());
}

#[test]
fn test_validate_no_crlf_rejects_cr_lf_nul() {
    // A request-target carrying CRLF + an injected header must be rejected.
    let split = "/path\r\nX-Injected: evil";
    let err = validate_no_crlf("request-target", split).unwrap_err();
    assert!(matches!(err, RuntimeError::IllegalArgumentException { .. }));

    assert!(validate_no_crlf("Host header", "evil\rhost").is_err());
    assert!(validate_no_crlf("Host header", "evil\nhost").is_err());
    assert!(validate_no_crlf("method", "GET\0").is_err());
    // Bare LF (header smuggling against lenient parsers) is rejected too.
    assert!(validate_no_crlf("method", "GE\nT").is_err());
}

#[test]
fn test_http_version_constants() {
    assert_eq!(HTTP_VERSION_1_1, 0);
    assert_eq!(HTTP_VERSION_2, 1);
}

#[test]
fn test_redirect_constants() {
    // These are `java.net.http.HttpClient.Redirect` ORDINALS, in the JDK's
    // declaration order — NEVER, ALWAYS, NORMAL. They used to be 0/2/1,
    // which disagreed with the `NEVER`/`ALWAYS`/`NORMAL` statics owned by
    // `phases_late::net_channels`, so `client.followRedirects() ==
    // HttpClient.Redirect.ALWAYS` compared two different numbers.
    assert_eq!(REDIRECT_NEVER, 0);
    assert_eq!(REDIRECT_ALWAYS, 1);
    assert_eq!(REDIRECT_NORMAL, 2);
}

#[test]
fn test_websocket_state_constants() {
    assert_eq!(WS_OPEN, 0);
    assert_eq!(WS_CLOSING, 1);
    assert_eq!(WS_CLOSED, 2);
}

#[test]
fn test_hpack_static_table_length() {
    // RFC 7541 defines exactly 61 static entries (index 1..=61)
    assert_eq!(HpackStaticTable::ENTRIES.len(), 61);
}

#[test]
fn test_hpack_find_method_get() {
    let idx = HpackStaticTable::find(":method", Some("GET"));
    assert_eq!(idx, Some(2)); // 1-based: entry 2 is ":method GET"
}

#[test]
fn test_hpack_find_method_post() {
    let idx = HpackStaticTable::find(":method", Some("POST"));
    assert_eq!(idx, Some(3));
}

#[test]
fn test_hpack_find_scheme_https() {
    let idx = HpackStaticTable::find(":scheme", Some("https"));
    assert_eq!(idx, Some(7));
}

#[test]
fn test_hpack_find_status_200() {
    let idx = HpackStaticTable::find(":status", Some("200"));
    assert_eq!(idx, Some(8));
}

#[test]
fn test_hpack_find_no_value_match() {
    // Searching by name only returns first occurrence
    let idx = HpackStaticTable::find(":method", None);
    assert!(idx.is_some());
    assert!(idx.unwrap() >= 1);
}

#[test]
fn test_hpack_find_nonexistent() {
    let idx = HpackStaticTable::find("x-custom-header", Some("value"));
    assert_eq!(idx, None);
}

#[test]
fn test_hpack_huffman_decode_known_vectors() {
    // RFC 7541 Appendix C.4.1: "www.example.com" Huffman-encoded.
    let encoded = [
        0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff,
    ];
    let decoded = hpack_huffman_decode(&encoded).expect("decode www.example.com");
    assert_eq!(&decoded, b"www.example.com");

    // RFC 7541 Appendix C.4.2: "no-cache".
    let encoded2 = [0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf];
    let decoded2 = hpack_huffman_decode(&encoded2).expect("decode no-cache");
    assert_eq!(&decoded2, b"no-cache");

    // RFC 7541 Appendix C.4.3: "custom-value".
    let encoded3 = [0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf];
    let decoded3 = hpack_huffman_decode(&encoded3).expect("decode custom-value");
    assert_eq!(&decoded3, b"custom-value");
}

#[test]
fn test_hpack_huffman_decode_empty() {
    assert_eq!(hpack_huffman_decode(&[]).unwrap(), Vec::<u8>::new());
}

#[test]
fn test_hpack_huffman_decode_rejects_bad_padding() {
    // Trailing bits that are not all-ones must be rejected (fail-closed).
    // 0x00 alone: first 5 bits 00000 decode to '0', leaving 3 zero pad bits
    // which are NOT all-ones -> error.
    assert!(hpack_huffman_decode(&[0x00]).is_err());
}

#[test]
fn test_http2_frame_type_values() {
    assert_eq!(Http2FrameType::Data as u8, 0x0);
    assert_eq!(Http2FrameType::Headers as u8, 0x1);
    assert_eq!(Http2FrameType::Settings as u8, 0x4);
    assert_eq!(Http2FrameType::Ping as u8, 0x6);
    assert_eq!(Http2FrameType::Goaway as u8, 0x7);
    assert_eq!(Http2FrameType::WindowUpdate as u8, 0x8);
    assert_eq!(Http2FrameType::Continuation as u8, 0x9);
}

#[test]
fn test_http2_error_code_values() {
    assert_eq!(Http2ErrorCode::NoError as u32, 0x0);
    assert_eq!(Http2ErrorCode::ProtocolError as u32, 0x1);
    assert_eq!(Http2ErrorCode::Cancel as u32, 0x8);
    assert_eq!(Http2ErrorCode::InadequateSecurity as u32, 0xc);
    assert_eq!(Http2ErrorCode::Http11Required as u32, 0xd);
}

#[test]
fn test_default_pool_size() {
    assert_eq!(DEFAULT_POOL_SIZE, 20);
}

#[test]
fn test_all_http2_natives_count() {
    // Smoke-test: ensure registration completes without panic
    // and that a representative cross-section of methods are present.
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    // HttpClient
    assert!(r
        .find(
            "java/net/http/HttpClient",
            "sslParameters",
            "()Ljavax/net/ssl/SSLParameters;"
        )
        .is_some());
    // HttpRequest
    assert!(r
        .find("java/net/http/HttpRequest", "expectContinue", "()Z")
        .is_some());
    // HttpResponse
    assert!(r
        .find(
            "java/net/http/HttpResponse",
            "sslSession",
            "()Ljava/util/Optional;"
        )
        .is_some());
    // WebSocket
    assert!(r
        .find(
            "java/net/http/WebSocket",
            "subprotocol",
            "()Ljava/lang/String;"
        )
        .is_some());
    // BodyHandlers
    assert!(r
        .find(
            "java/net/http/HttpResponse$BodyHandlers",
            "ofLines",
            "()Ljava/net/http/HttpResponse$BodyHandler;"
        )
        .is_some());
}

// --- M14: Real HTTP client/server tests ---------------------------------

#[test]
fn m14_http11_request_to_localhost_server() {
    // Spin up a real TCP server that responds with HTTP/1.1 200 OK.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("M14: accept failed");
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).expect("M14: read failed");
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(
            request.starts_with("GET /hello"),
            "M14: expected GET /hello, got: {}",
            request
        );

        let body = "Hello from CratonVM!";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream
            .write_all(response.as_bytes())
            .expect("M14: write failed");
        stream.flush().ok();
    });

    let (status, body) =
        http11_request("127.0.0.1", port, "GET", "/hello").expect("M14: http11_request failed");

    assert_eq!(status, 200, "M14: expected status 200, got {status}");
    assert_eq!(body, "Hello from CratonVM!", "M14: body mismatch");
    server.join().expect("M14: server thread panicked");
}

#[test]
fn m14_http11_post_request() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("M14: accept failed");
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).expect("M14: read failed");
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(
            request.starts_with("POST /api"),
            "M14: expected POST /api, got: {}",
            request
        );

        let body = "{\"status\":\"created\"}";
        let response = format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream
            .write_all(response.as_bytes())
            .expect("M14: write failed");
    });

    let (status, body) =
        http11_request("127.0.0.1", port, "POST", "/api").expect("M14: http11_request failed");

    assert_eq!(status, 201, "M14: expected status 201");
    assert_eq!(body, "{\"status\":\"created\"}");
    server.join().expect("M14: server thread panicked");
}

#[test]
fn m14_http11_connection_refused() {
    // Connecting to a port with no listener should fail gracefully.
    let result = http11_request("127.0.0.1", 1, "GET", "/");
    assert!(result.is_err(), "M14: expected connection error for port 1");
}

#[test]
fn m14_http11_404_response() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("M14: accept failed");
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf);
        let response = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\nNot Found";
        stream.write_all(response.as_bytes()).ok();
    });

    let (status, body) = http11_request("127.0.0.1", port, "GET", "/missing")
        .expect("M14: http11_request failed");

    assert_eq!(status, 404, "M14: expected 404");
    assert_eq!(body, "Not Found");
    server.join().expect("M14: server thread panicked");
}

// --- E2: `java.util.Optional` is (reference-or-null), never (flag, payload)
//
// `javap -p --module java.base java.util.Optional` (JDK 25) declares ONE
// instance field, `private final T value`, and it is a REFERENCE. The three
// PRIMITIVE Optionals genuinely do declare `boolean isPresent` at slot 0 and
// `value` at slot 1 -- which is why the (flag, payload) idiom looked right
// and why `firstValueAsLong` below must keep it. `ref_operand_is_null`
// (`vm/src/runtime/interpreter.rs`) then counted only `Object(None)`,
// `Uninitialized` and `Long(0)` as null, so an `Int(0)` parked in slot 0
// made an EMPTY Optional answer `isPresent() == true` and made `get()`
// return the flag. It counts `Int(0)` as null too since interpreter round
// i1 wave 5, but a PRESENT flag `Int(1)` in slot 0 still reads as a
// non-null reference, so the (flag, payload) shape stays wrong here. These tests INVOKE the natives rather than merely
// asserting they are registered -- the 64 tests above are registration-only
// and every one of them stayed green through the whole defect.
//
// See docs/feature-designs/native-dispatch-architecture.md

/// Every reference-`Optional` producer in this file, invoked for real.
/// Returns slot 0 of the `Optional` the native actually built.
fn opt_slot0(
    cb: cratonvm_native_api::NativeCallback,
    ctx: &mut crate::test_utils::MockNativeContext,
    args: &[Value],
) -> Value {
    match cb(ctx, args).expect("native must not fail") {
        Some(Value::Object(Some(opt))) => {
            assert_eq!(
                ctx.object_num_fields(opt),
                1,
                "java.util.Optional has exactly ONE instance field"
            );
            ctx.get_field(opt, 0)
        }
        other => panic!("expected an Optional object, got {other:?}"),
    }
}

fn find_cb(class: &str, name: &str, desc: &str) -> cratonvm_native_api::NativeCallback {
    let mut r = NativeMethodRegistry::new();
    register_http2_natives(&mut r);
    r.find(class, name, desc)
        .unwrap_or_else(|| panic!("{class}.{name}{desc} must be registered"))
}

fn alloc_of(
    ctx: &mut crate::test_utils::MockNativeContext,
    class: &str,
    n: usize,
) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    ctx.alloc_object(cid, n)
}

/// The assertion that names the defect. A bare `Value::Int` in slot 0 is
/// what `isPresent()` reads as the reference `value`.
fn assert_not_a_flag(site: &str, slot0: &Value) {
    if let Value::Int(n) = slot0 {
        panic!(
            "{site}: slot 0 of a java.util.Optional is the REFERENCE `value`, \
             but it holds Value::Int({n}). isPresent() reads this as PRESENT \
             (a non-zero Int is not null to ref_operand_is_null) and get() returns the flag."
        );
    }
}

#[test]
fn e2_http_client_presence_accessors_answer_empty_not_a_flag() {
    // executor/proxy/authenticator/cookieHandler store a presence FLAG and
    // never the object itself, so `empty()` is the only truthful answer.
    // These four have the CORRECT arity (1) and had the WRONG type, which
    // is the shape the layout-alias instrument (a slot COUNT comparison) is
    // structurally blind to.
    for (name, flag_slot) in [
        ("executor", CLIENT_HAS_EXECUTOR),
        ("proxy", CLIENT_HAS_PROXY),
        ("authenticator", CLIENT_HAS_AUTH),
        ("cookieHandler", CLIENT_HAS_COOKIE),
    ] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpClient", 10);
        // Set the flag to 1 -- the case that used to produce Int(1), which
        // `get()` would have handed back where an object is declared.
        ctx.set_field(this, flag_slot, Value::Int(1));
        let cb = find_cb("java/net/http/HttpClient", name, "()Ljava/util/Optional;");
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag(name, &slot0);
        assert_eq!(
            slot0,
            Value::Object(None),
            "{name}: no object is stored in this layout, so empty() is the only truthful answer"
        );
    }
}

#[test]
fn e2_connect_timeout_absent_is_null_and_present_is_a_duration() {
    let cb = || {
        find_cb(
            "java/net/http/HttpClient",
            "connectTimeout",
            "()Ljava/util/Optional;",
        )
    };

    // Absent: this is the row that used to report isPresent() == true.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpClient", 10);
    ctx.set_field(this, CLIENT_CONNECT_TIMEOUT, Value::Long(0));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("connectTimeout.absent", &slot0);
    assert_eq!(slot0, Value::Object(None));

    // Present: declared Optional<Duration>, so slot 0 owes a Duration
    // object -- not a Long and certainly not the flag. 1500ms == 1s + 500ms.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpClient", 10);
    ctx.set_field(this, CLIENT_CONNECT_TIMEOUT, Value::Long(1500));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("connectTimeout.present", &slot0);
    match slot0 {
        Value::Object(Some(dur)) => {
            assert_eq!(ctx.get_field(dur, 0), Value::Long(1), "Duration seconds");
            assert_eq!(
                ctx.get_field(dur, 1),
                Value::Int(500_000_000),
                "Duration nanos"
            );
        }
        other => panic!("connectTimeout present must hold a Duration, got {other:?}"),
    }
}

#[test]
fn e2_request_timeout_absent_is_null_and_present_is_a_duration() {
    let cb = || {
        find_cb(
            "java/net/http/HttpRequest",
            "timeout",
            "()Ljava/util/Optional;",
        )
    };

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
    ctx.set_field(this, REQ_TIMEOUT, Value::Long(0));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("timeout.absent", &slot0);
    assert_eq!(slot0, Value::Object(None));

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
    ctx.set_field(this, REQ_TIMEOUT, Value::Long(7000));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("timeout.present", &slot0);
    match slot0 {
        Value::Object(Some(dur)) => {
            assert_eq!(ctx.get_field(dur, 0), Value::Long(7), "Duration seconds");
            assert_eq!(ctx.get_field(dur, 1), Value::Int(0), "Duration nanos");
        }
        other => panic!("timeout present must hold a Duration, got {other:?}"),
    }
}

#[test]
fn e2_body_publisher_moves_the_publisher_into_slot_zero() {
    let cb = || {
        find_cb(
            "java/net/http/HttpRequest",
            "bodyPublisher",
            "()Ljava/util/Optional;",
        )
    };

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
    ctx.set_field(this, REQ_HAS_BODY, Value::Int(0));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("bodyPublisher.absent", &slot0);
    assert_eq!(slot0, Value::Object(None));

    // Present: the publisher used to be parked at slot 1, where no real
    // bytecode ever read it.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
    ctx.set_field(this, REQ_HAS_BODY, Value::Int(1));
    ctx.set_field(this, REQ_BODY_LEN, Value::Long(2));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("bodyPublisher.present", &slot0);
    assert!(
        matches!(slot0, Value::Object(Some(_))),
        "bodyPublisher present must hold the publisher itself, got {slot0:?}"
    );
}

/// Declared `Optional<HttpClient$Version>`, so slot 0 owes an enum mirror
/// or null -- **never an ordinal**, which is the law this test states and
/// which has not changed.
///
/// AMENDED by E13. E2 wrote this asserting `Value::Object(None)` for BOTH
/// `REQ_VERSION == 0` and `REQ_VERSION == 2`, because `REQ_VERSION` could
/// not then be non-zero: its only writer,
/// `HttpRequest$Builder.version(HttpClient$Version)`, matched
/// `Some(Value::Int(n))` against a reference argument. E13 fixed that
/// builder, so `2` now means an explicit HTTP_2 override and the present
/// arm is live. The `Int(2)` row moved from "empty" to "an enum mirror";
/// `assert_not_a_flag` -- the actual C12-3 assertion -- still holds on
/// both, and `e13_request_version_optional_now_holds_the_enum_mirror`
/// drives the same site through the real builder.
#[test]
fn e2_request_version_is_never_an_ordinal() {
    for (stored, expect_present) in [(Value::Int(0), false), (Value::Int(2), true)] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
        ctx.set_field(this, REQ_VERSION, stored);
        let cb = find_cb(
            "java/net/http/HttpRequest",
            "version",
            "()Ljava/util/Optional;",
        );
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("version", &slot0);
        if expect_present {
            match slot0 {
                Value::Object(Some(mirror)) => assert_eq!(
                    ctx.get_field(mirror, ENUM_SLOT_ORDINAL),
                    Value::Int(HTTP_VERSION_2),
                    "an ENUM MIRROR, not the raw ordinal, belongs in slot 0"
                ),
                other => panic!("an explicit version override must be PRESENT, got {other:?}"),
            }
        } else {
            assert_eq!(slot0, Value::Object(None));
        }
    }
}

/// The row C12-3 calls "the one that settles what this is": the arity was
/// already correct (1 slot) and the TYPE was not. It is covered by NO Java
/// fixture -- `RJdkOptionalShape` cannot reach it without a loopback
/// `HttpServer` -- and it is invisible to the layout-alias instrument,
/// which compares slot COUNTS. This test is its only coverage.
#[test]
fn e2_previous_response_is_empty_not_a_flag() {
    for flag in [0, 1] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpResponse", 7);
        ctx.set_field(this, RESP_HAS_PREV, Value::Int(flag));
        let cb = find_cb(
            "java/net/http/HttpResponse",
            "previousResponse",
            "()Ljava/util/Optional;",
        );
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("previousResponse", &slot0);
        assert_eq!(
            slot0,
            Value::Object(None),
            "no previous response is stored anywhere in this layout"
        );
    }
}

#[test]
fn e2_ssl_session_moves_the_session_into_slot_zero() {
    let cb = || {
        find_cb(
            "java/net/http/HttpResponse",
            "sslSession",
            "()Ljava/util/Optional;",
        )
    };

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpResponse", 7);
    ctx.set_field(this, RESP_HAS_SSL, Value::Int(0));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("sslSession.absent", &slot0);
    assert_eq!(slot0, Value::Object(None));

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpResponse", 7);
    ctx.set_field(this, RESP_HAS_SSL, Value::Int(1));
    let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
    assert_not_a_flag("sslSession.present", &slot0);
    assert!(
        matches!(slot0, Value::Object(Some(_))),
        "sslSession present must hold the SSLSession itself, got {slot0:?}"
    );
}

#[test]
fn e2_first_value_moves_the_string_into_slot_zero() {
    let cb = || {
        find_cb(
            "java/net/http/HttpHeaders",
            "firstValue",
            "(Ljava/lang/String;)Ljava/util/Optional;",
        )
    };

    // Present: the header string used to sit at slot 1.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
    ctx.set_field(this, HDR_HAS_CT, Value::Int(1));
    let key = ctx.create_string("Content-Type");
    let slot0 = opt_slot0(
        cb(),
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(key))],
    );
    assert_not_a_flag("firstValue.present", &slot0);
    match slot0 {
        Value::Object(Some(s)) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some("application/json"));
        }
        other => panic!("firstValue present must hold the String, got {other:?}"),
    }

    // Absent: an unknown header name.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
    ctx.set_field(this, HDR_HAS_CT, Value::Int(1));
    let key = ctx.create_string("X-Absent");
    let slot0 = opt_slot0(
        cb(),
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(key))],
    );
    assert_not_a_flag("firstValue.absent", &slot0);
    assert_eq!(slot0, Value::Object(None));
}

/// NEGATIVE CONTROL -- the test that must FAIL if anyone "simplifies" the
/// fix above into a blanket one.
///
/// `java.util.OptionalLong` really does declare `boolean isPresent` at slot
/// 0 and `long value` at slot 1 (`javap -p --module java.base`, JDK 25), so
/// `firstValueAsLong`'s 2-slot (flag, payload) form is CORRECT and sits
/// twelve lines below the last site that was wrong. That adjacency is the
/// best available explanation of how the defect happened: the idiom is
/// right for the class one method away. Applying the reference-Optional
/// fix here would BREAK a working accessor.
#[test]
fn e2_optional_long_keeps_the_primitive_flag_payload_layout() {
    let cb = || {
        find_cb(
            "java/net/http/HttpHeaders",
            "firstValueAsLong",
            "(Ljava/lang/String;)Ljava/util/OptionalLong;",
        )
    };

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
    ctx.set_field(this, HDR_HAS_CL, Value::Int(1));
    let key = ctx.create_string("Content-Length");
    let opt = match cb()(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(key))],
    )
    .expect("native must not fail")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an OptionalLong, got {other:?}"),
    };
    assert_eq!(
        ctx.object_num_fields(opt),
        2,
        "OptionalLong declares TWO fields -- do not collapse it to one"
    );
    assert_eq!(
        ctx.get_field(opt, 0),
        Value::Int(1),
        "OptionalLong slot 0 IS `boolean isPresent` -- a flag here is CORRECT"
    );
    assert_eq!(
        ctx.get_field(opt, 1),
        Value::Long(20),
        "OptionalLong slot 1 IS `long value`"
    );
}

// =====================================================================
// E13 -- THE SIX BUILDERS THAT DECODED A REFERENCE AS AN `Int`.
//
// Behavioural, like E2's: each drives the native through the registry and
// asserts the value it actually STORED. `http2.rs` had 64 tests before E2
// and every one was registration-only, so all 64 stayed green through both
// of these defect families -- a native existing says nothing about what it
// writes.
//
// The setups build their arguments the way the VM mints them
// (`p57_alloc_enum`: name at slot 0, ordinal at slot 1;
// `util_time::alloc_duration`: seconds at slot 0, nanos at slot 1) rather
// than by name, so the class-side-witness fallback in the decoders is the
// path under test. `test_utils::MockNativeContext::get_field_by_name`
// answers `Value::Int(0)` for an unresolvable name where production answers
// `Value::Object(None)`; the decoders deliberately never take a value-side
// by-name read, so that divergence cannot steer these results.
//
// See docs/feature-designs/native-dispatch-architecture.md
// =====================================================================

const CLS_CLIENT_BUILDER: &str = "java/net/http/HttpClient$Builder";
const CLS_REQUEST_BUILDER: &str = "java/net/http/HttpRequest$Builder";
const DESC_CLIENT_VERSION: &str =
    "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;";
const DESC_REQUEST_VERSION: &str =
    "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;";

/// An enum constant shaped the way `phases_late::p57_alloc_enum` mints one.
fn alloc_enum_const(
    ctx: &mut crate::test_utils::MockNativeContext,
    class: &str,
    name: &str,
    ordinal: i32,
) -> ObjectRef {
    let obj = alloc_of(ctx, class, 2);
    let n = ctx.create_string(name);
    ctx.set_field(obj, ENUM_SLOT_NAME, Value::Object(Some(n)));
    ctx.set_field(obj, ENUM_SLOT_ORDINAL, Value::Int(ordinal));
    obj
}

/// A `java.time.Duration` shaped the way `util_time::alloc_duration` mints
/// one: floor-normalised `seconds` (`Long`) and `nanos` (`Int`).
fn alloc_dur(
    ctx: &mut crate::test_utils::MockNativeContext,
    seconds: i64,
    nanos: i32,
) -> ObjectRef {
    let obj = alloc_of(ctx, "java/time/Duration", 2);
    ctx.set_field(obj, DUR_SLOT_SECONDS, Value::Long(seconds));
    ctx.set_field(obj, DUR_SLOT_NANOS, Value::Int(nanos));
    obj
}

/// Drive a fluent setter and check it hands the receiver back -- a builder
/// that returns anything else breaks the chain the caller wrote.
fn call_setter(
    cb: cratonvm_native_api::NativeCallback,
    ctx: &mut crate::test_utils::MockNativeContext,
    this: ObjectRef,
    arg: Value,
) {
    match cb(ctx, &[Value::Object(Some(this)), arg]).expect("native must not fail") {
        Some(Value::Object(Some(ret))) => assert_eq!(
            ret.as_ptr(),
            this.as_ptr(),
            "a fluent setter must return its own receiver"
        ),
        other => panic!("expected the builder back, got {other:?}"),
    }
}

/// THE DISCRIMINATING CASE. `HTTP_1_1` is the constant the pre-fix
/// `_ => HTTP_VERSION_2` fallback swallowed: a caller who asked for
/// HTTP/1.1 got an HTTP/2 client, and asking for HTTP_2 looked "right" only
/// because the default happened to agree. Both are asserted so a future
/// regression cannot hide behind the agreeing one.
#[test]
fn e13_client_builder_version_stores_the_constant_the_caller_passed() {
    for (name, ordinal, expected) in [
        ("HTTP_1_1", 0, HTTP_VERSION_1_1),
        ("HTTP_2", 1, HTTP_VERSION_2),
    ] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        let v = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Version", name, ordinal);
        let cb = find_cb(CLS_CLIENT_BUILDER, "version", DESC_CLIENT_VERSION);
        call_setter(cb, &mut ctx, this, Value::Object(Some(v)));
        assert_eq!(
            ctx.get_field(this, 0),
            Value::Int(expected),
            "version({name}) must store ordinal {expected}; the descriptor's parameter is a \
             REFERENCE, so a `Some(Value::Int(n))` match here never fires and the default wins"
        );
    }
}

/// The two encodings of one concept, separated. Each arm carries ONLY its
/// own encoding and the other slot is left at the mock's `Int(0)` default,
/// so each is a real single-path test:
///
/// * NAME-only `HTTP_2` -- slot 1 reads `Int(0)`. Answering `HTTP_2` proves
///   the name was consulted FIRST; a decoder that only read the ordinal
///   would answer `HTTP_1_1` here.
/// * ORDINAL-only `1` -- slot 0 holds no string, so the name lookup misses
///   and the ordinal path must carry it.
#[test]
fn e13_enum_decode_reads_the_name_first_and_the_ordinal_as_a_fallback() {
    let cb = || find_cb(CLS_CLIENT_BUILDER, "version", DESC_CLIENT_VERSION);

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    let by_name = alloc_of(&mut ctx, "java/net/http/HttpClient$Version", 2);
    let n = ctx.create_string("HTTP_2");
    ctx.set_field(by_name, ENUM_SLOT_NAME, Value::Object(Some(n)));
    call_setter(cb(), &mut ctx, this, Value::Object(Some(by_name)));
    assert_eq!(
        ctx.get_field(this, 0),
        Value::Int(HTTP_VERSION_2),
        "the NAME must be consulted before the ordinal slot"
    );

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    let by_ordinal = alloc_of(&mut ctx, "java/net/http/HttpClient$Version", 2);
    ctx.set_field(by_ordinal, ENUM_SLOT_ORDINAL, Value::Int(HTTP_VERSION_2));
    call_setter(cb(), &mut ctx, this, Value::Object(Some(by_ordinal)));
    assert_eq!(
        ctx.get_field(this, 0),
        Value::Int(HTTP_VERSION_2),
        "a constant with no readable name must still decode through its ordinal"
    );
}

#[test]
fn e13_client_builder_connect_timeout_decodes_a_duration() {
    let cb = || {
        find_cb(
            CLS_CLIENT_BUILDER,
            "connectTimeout",
            "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        )
    };

    // 1500 ms == Duration.ofMillis(1500) == seconds 1, nanos 500_000_000.
    // This is the value RJdkOptionalShape's `mint-connectTimeout-present-millis`
    // asks for, and it was unreachable: CLIENT_CONNECT_TIMEOUT could never
    // be non-zero, so `connectTimeout()` had no present arm to take.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    let d = alloc_dur(&mut ctx, 1, 500_000_000);
    call_setter(cb(), &mut ctx, this, Value::Object(Some(d)));
    assert_eq!(ctx.get_field(this, 2), Value::Long(1500));

    // Duration is FLOOR-normalised, so ofMillis(-1500) is seconds -2 with
    // nanos +500_000_000. A decoder that truncated toward zero would say
    // -1000 here.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    let d = alloc_dur(&mut ctx, -2, 500_000_000);
    call_setter(cb(), &mut ctx, this, Value::Object(Some(d)));
    assert_eq!(ctx.get_field(this, 2), Value::Long(-1500));

    // A null Duration keeps the pre-fix answer rather than inventing one.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    call_setter(cb(), &mut ctx, this, Value::Object(None));
    assert_eq!(ctx.get_field(this, 2), Value::Long(0));
}

#[test]
fn e13_request_builder_timeout_decodes_a_duration() {
    let cb = find_cb(
        CLS_REQUEST_BUILDER,
        "timeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
    );
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
    let d = alloc_dur(&mut ctx, 7, 0);
    call_setter(cb, &mut ctx, this, Value::Object(Some(d)));
    assert_eq!(
        ctx.get_field(this, REQ_TIMEOUT),
        Value::Long(7000),
        "the 7000 ms `mint-timeout-present-millis` asks for"
    );
}

/// Covers the enum decode AND the slot collision the decode uncovered:
/// `followRedirects` used to also write `Int(policy != NEVER)` into BUILDER
/// slot 7, which `build()` copies to `CLIENT_HAS_COOKIE`. That was inert
/// only while `policy` was stuck at `NEVER`.
#[test]
fn e13_follow_redirects_decodes_the_enum_and_leaves_the_cookie_flag_alone() {
    for (name, ordinal, follow) in [
        ("NEVER", REDIRECT_NEVER, 0),
        ("ALWAYS", REDIRECT_ALWAYS, 1),
        ("NORMAL", REDIRECT_NORMAL, 1),
    ] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bld = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        init_http_client_builder_fields(&mut ctx, bld);
        // A cookie handler IS configured -- the flag the stray write to
        // builder slot 7 would have overwritten with a redirect answer.
        let cookie_cb = find_cb(
            CLS_CLIENT_BUILDER,
            "cookieHandler",
            "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
        );
        let handler = alloc_of(&mut ctx, "java/net/CookieHandler", 1);
        call_setter(cookie_cb, &mut ctx, bld, Value::Object(Some(handler)));

        let e = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Redirect", name, ordinal);
        let cb = find_cb(
            CLS_CLIENT_BUILDER,
            "followRedirects",
            "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        );
        call_setter(cb, &mut ctx, bld, Value::Object(Some(e)));
        assert_eq!(
            ctx.get_field(bld, 1),
            Value::Int(ordinal),
            "followRedirects({name}) must store the policy it was given"
        );
        assert_eq!(
            ctx.get_field(bld, 7),
            Value::Int(1),
            "builder slot 7 is the COOKIE flag; followRedirects must not touch it"
        );

        let build = find_cb(CLS_CLIENT_BUILDER, "build", "()Ljava/net/http/HttpClient;");
        let client =
            match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
                Some(Value::Object(Some(c))) => c,
                other => panic!("build() must return an HttpClient, got {other:?}"),
            };
        assert_eq!(ctx.get_field(client, CLIENT_REDIRECT), Value::Int(ordinal));
        assert_eq!(
            ctx.get_field(client, CLIENT_HAS_COOKIE),
            Value::Int(1),
            "the cookie flag must survive the redirect policy"
        );
        assert_eq!(
            ctx.get_field(client, CLIENT_FOLLOW_REDIR),
            Value::Int(follow),
            "CLIENT_FOLLOW_REDIR is slot 8 and the copy loop stops at 7, so build() must \
             derive it"
        );
    }
}

#[test]
fn e13_request_builder_version_stores_the_ordinal_plus_one() {
    let cb = || find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
    for (name, ordinal, stored) in [("HTTP_1_1", 0, 1), ("HTTP_2", 1, 2)] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        let v = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Version", name, ordinal);
        call_setter(cb(), &mut ctx, this, Value::Object(Some(v)));
        assert_eq!(
            ctx.get_field(this, REQ_VERSION),
            Value::Int(stored),
            "REQ_VERSION encodes 0 = no override, so {name} is stored as {stored}"
        );
    }
}

/// An argument the decoder cannot read must leave each site's OWN default
/// standing -- never a silent decay to ordinal 0. That decay is what turned
/// `TimeUnit.SECONDS.toNanos(1)` into `1` (`phases_early::tu_ordinal`), and
/// here it would make `version(<unreadable>)` mean HTTP_1_1 on the client
/// and an explicit HTTP_1_1 override on the request.
#[test]
fn e13_an_undecodable_enum_argument_keeps_each_sites_own_default() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    let cb = find_cb(CLS_CLIENT_BUILDER, "version", DESC_CLIENT_VERSION);
    call_setter(cb, &mut ctx, this, Value::Object(None));
    assert_eq!(
        ctx.get_field(this, 0),
        Value::Int(HTTP_VERSION_2),
        "HttpClient$Builder.version's documented default is HTTP_2, not ordinal 0"
    );

    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
    let cb = find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
    call_setter(cb, &mut ctx, this, Value::Object(None));
    assert_eq!(
        ctx.get_field(this, REQ_VERSION),
        Value::Int(0),
        "HttpRequest$Builder.version's default is 0 == NO override"
    );

    // An ordinal outside the modelled constants is not a usable answer
    // either -- `version_enum` would have to invent a name for it.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
    let bogus = alloc_of(&mut ctx, "java/net/http/HttpClient$Version", 2);
    ctx.set_field(bogus, ENUM_SLOT_ORDINAL, Value::Int(97));
    let cb = find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
    call_setter(cb, &mut ctx, this, Value::Object(Some(bogus)));
    assert_eq!(ctx.get_field(this, REQ_VERSION), Value::Int(0));
}

/// `BodyPublishers.ofByteArray` is STATIC, so `args[0]` is the `[B` itself.
/// Every publisher it minted reported `contentLength() == 0`, while the
/// `ofString` sibling directly above it always read its reference argument.
#[test]
fn e13_of_byte_array_reads_the_array_length() {
    let cb = find_cb(
        "java/net/http/HttpRequest$BodyPublishers",
        "ofByteArray",
        "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
    );
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 5);
    let bp = match cb(&mut ctx, &[Value::Object(Some(arr))]).expect("native must not fail") {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected a BodyPublisher, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(bp, 0),
        Value::Long(5),
        "contentLength() reads slot 0 as a Long"
    );
}

/// Found by grepping the SHAPE ("a builder that does not read the reference
/// it was handed") rather than the `Value::Int` idiom: `POST`, `PUT` and
/// `method(String, BodyPublisher)` recorded only THAT a body existed, so
/// `bodyPublisher().get().contentLength()` answered 0 for every body --
/// which is `mint-bodyPublisher-present-len`.
#[test]
fn e13_post_put_and_method_carry_the_publishers_content_length() {
    let bp_cb = find_cb(
        "java/net/http/HttpRequest$BodyPublishers",
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
    );
    for (method, desc, extra) in [
        (
            "POST",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
            0usize,
        ),
        (
            "PUT",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
            0,
        ),
        (
            "method",
            "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
            1,
        ),
    ] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        let body = ctx.create_string("hi");
        let bp = match bp_cb(&mut ctx, &[Value::Object(Some(body))])
            .expect("ofString must not fail")
        {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected a BodyPublisher, got {other:?}"),
        };
        let cb = find_cb(CLS_REQUEST_BUILDER, method, desc);
        let mut args = vec![Value::Object(Some(this))];
        if extra == 1 {
            let name = ctx.create_string("POST");
            args.push(Value::Object(Some(name)));
        }
        args.push(Value::Object(Some(bp)));
        cb(&mut ctx, &args).expect("native must not fail");
        assert_eq!(ctx.get_field(this, REQ_HAS_BODY), Value::Int(1));
        assert_eq!(
            ctx.get_field(this, REQ_BODY_LEN),
            Value::Long(2),
            "{method} must carry the publisher's declared length, not just a has-body flag"
        );
    }
}

/// NEGATIVE CONTROL -- the test that must FAIL if anyone "simplifies" the
/// six fixes above into "no builder matches `Value::Int`".
///
/// `expectContinue`'s descriptor is `(Z)`: a primitive `boolean`, which
/// really does arrive as a `Value::Int`. The idiom is CORRECT here and
/// wrong twelve lines away, exactly as `firstValueAsLong`'s
/// `(isPresent, value)` layout is correct where the reference `Optional`'s
/// was wrong. Read the DESCRIPTOR, not the neighbouring line.
#[test]
fn e13_expect_continue_still_reads_a_primitive_boolean() {
    let cb = || {
        find_cb(
            CLS_REQUEST_BUILDER,
            "expectContinue",
            "(Z)Ljava/net/http/HttpRequest$Builder;",
        )
    };
    for flag in [0, 1] {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        call_setter(cb(), &mut ctx, this, Value::Int(flag));
        assert_eq!(
            ctx.get_field(this, REQ_EXPECT_100),
            Value::Int(flag),
            "a primitive boolean parameter DOES arrive as Value::Int -- this arm is correct"
        );
    }
}

/// END TO END through the pair E2 could not join up: builder -> build() ->
/// accessor. E2 pinned `HttpRequest.version()` at unconditionally EMPTY and
/// said so in a comment, because `REQ_VERSION` could not be non-zero. This
/// patch makes it non-zero, so slot 0 of the `Optional` must now hold an
/// ENUM MIRROR -- still never an ordinal and never a flag.
#[test]
fn e13_request_version_optional_now_holds_the_enum_mirror() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bld = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
    init_http_request_fields(&mut ctx, bld);
    let v = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Version", "HTTP_2", 1);
    let set = find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
    call_setter(set, &mut ctx, bld, Value::Object(Some(v)));
    let build = find_cb(
        CLS_REQUEST_BUILDER,
        "build",
        "()Ljava/net/http/HttpRequest;",
    );
    let req = match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
        Some(Value::Object(Some(r))) => r,
        other => panic!("build() must return an HttpRequest, got {other:?}"),
    };

    let cb = find_cb(
        "java/net/http/HttpRequest",
        "version",
        "()Ljava/util/Optional;",
    );
    let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(req))]);
    assert_not_a_flag("version.present", &slot0);
    match slot0 {
        Value::Object(Some(mirror)) => {
            let name = match ctx.get_field(mirror, ENUM_SLOT_NAME) {
                Value::Object(Some(s)) => ctx.read_string(s),
                other => panic!("an enum mirror's slot 0 is its name, got {other:?}"),
            };
            assert_eq!(name.as_deref(), Some("HTTP_2"));
            assert_eq!(
                ctx.get_field(mirror, ENUM_SLOT_ORDINAL),
                Value::Int(HTTP_VERSION_2)
            );
        }
        other => panic!("version() present must hold an HttpClient$Version, got {other:?}"),
    }

    // The absent arm still answers empty -- no override was requested.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let req = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
    init_http_request_fields(&mut ctx, req);
    let cb = find_cb(
        "java/net/http/HttpRequest",
        "version",
        "()Ljava/util/Optional;",
    );
    let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(req))]);
    assert_eq!(slot0, Value::Object(None));
}

/// The other two `Optional` present arms E2 could only reach by writing the
/// backing field by hand, now driven through the BUILDER the way Java does.
/// This is what `mint-connectTimeout-present-millis` and
/// `mint-timeout-present-millis` execute.
#[test]
fn e13_the_duration_optionals_are_reachable_through_the_builder_now() {
    // HttpClient: connectTimeout(Duration.ofMillis(1500)).build().connectTimeout()
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bld = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
    init_http_client_builder_fields(&mut ctx, bld);
    let d = alloc_dur(&mut ctx, 1, 500_000_000);
    let set = find_cb(
        CLS_CLIENT_BUILDER,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
    );
    call_setter(set, &mut ctx, bld, Value::Object(Some(d)));
    let build = find_cb(CLS_CLIENT_BUILDER, "build", "()Ljava/net/http/HttpClient;");
    let client =
        match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
            Some(Value::Object(Some(c))) => c,
            other => panic!("build() must return an HttpClient, got {other:?}"),
        };
    let cb = find_cb(
        "java/net/http/HttpClient",
        "connectTimeout",
        "()Ljava/util/Optional;",
    );
    let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(client))]);
    assert_not_a_flag("connectTimeout.present", &slot0);
    match slot0 {
        Value::Object(Some(dur)) => {
            assert_eq!(ctx.get_field(dur, DUR_SLOT_SECONDS), Value::Long(1));
            assert_eq!(
                ctx.get_field(dur, DUR_SLOT_NANOS),
                Value::Int(500_000_000),
                "1500 ms must round-trip through the builder as 1 s + 500 ms"
            );
        }
        other => panic!("connectTimeout present must hold a Duration, got {other:?}"),
    }

    // HttpRequest: POST(ofString("hi")).build().bodyPublisher().contentLength()
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let bld = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
    init_http_request_fields(&mut ctx, bld);
    let body = ctx.create_string("hi");
    let bp_cb = find_cb(
        "java/net/http/HttpRequest$BodyPublishers",
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
    );
    let bp =
        match bp_cb(&mut ctx, &[Value::Object(Some(body))]).expect("ofString must not fail") {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected a BodyPublisher, got {other:?}"),
        };
    let post = find_cb(
        CLS_REQUEST_BUILDER,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
    );
    call_setter(post, &mut ctx, bld, Value::Object(Some(bp)));
    let build = find_cb(
        CLS_REQUEST_BUILDER,
        "build",
        "()Ljava/net/http/HttpRequest;",
    );
    let req = match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
        Some(Value::Object(Some(r))) => r,
        other => panic!("build() must return an HttpRequest, got {other:?}"),
    };
    let cb = find_cb(
        "java/net/http/HttpRequest",
        "bodyPublisher",
        "()Ljava/util/Optional;",
    );
    let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(req))]);
    assert_not_a_flag("bodyPublisher.present", &slot0);
    match slot0 {
        Value::Object(Some(pub_obj)) => assert_eq!(
            ctx.get_field(pub_obj, 0),
            Value::Long(2),
            "the body \"hi\" is 2 bytes; this is `mint-bodyPublisher-present-len`"
        ),
        other => panic!("bodyPublisher present must hold the publisher, got {other:?}"),
    }
}

// --- G34-1: the three shapes of java.net.http.HttpHeaders ---------------
//
// `NativeMethodRegistry::register` is last-write-wins with no unregister
// API, and TWO registrars claim
// `HttpHeaders.{map,firstValue,allValues,firstValueAsLong}`: this file's
// `register_http_headers` and `net_phase_e`'s. Whichever runs last owns
// the slot for every receiver in the VM, including the ones the OTHER
// minter produced. These tests pin the discriminator that makes that
// survivable, because the boot order is not something either file can see.

/// The three minters, and the one property that separates them.
///
/// MEASURED (`--dump-native-registry`, `target-rel2`, 2026-08-17): under
/// `--real-jdk` only `net_phase_e`'s rows exist, all `overwrote=null`. In a
/// `synthetic-jdk` build both registrars run and this file's bodies own the
/// four slots. The guard has to hold in both.
#[test]
fn http_headers_counter_shape_separates_the_three_minters() {
    let mut ctx = crate::test_utils::MockNativeContext::new();

    // (a) This file's minter: 3 slots, `Value::Int` counters.
    let mine = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
    init_http_headers_fields(&mut ctx, mine, 2, 1, 0);
    assert!(
        http_headers_is_counter_shape(&ctx, mine),
        "alloc_http_headers puts Value::Int in HDR_COUNT; that IS the counter shape"
    );

    // (b) `net_phase_e::re5_make_http_headers`: 1 slot holding a String[].
    //     A reference in slot 0 is the whole difference.
    let theirs = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
    let arr_stand_in = ctx.create_string("Accept: text/plain");
    ctx.set_field(theirs, 0, Value::Object(Some(arr_stand_in)));
    assert!(
        !http_headers_is_counter_shape(&ctx, theirs),
        "net_phase_e keeps a String[] in slot 0; decoding it as HDR_COUNT is the hazard"
    );

    // (c) The real JDK's `HttpHeaders.of(Map, BiPredicate)`: slot 0 is the
    //     real `Map` field. MEASURED on HotSpot 25.0.3+9-LTS,
    //     `map().getClass()` is `java.util.Collections$UnmodifiableMap`.
    let real = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
    ctx.set_field(real, 0, Value::Object(None));
    assert!(
        !http_headers_is_counter_shape(&ctx, real),
        "a real JDK HttpHeaders holds a Map reference in slot 0, never a counter"
    );
}

/// `firstValue` on a receiver this file did not mint must answer ABSENT,
/// not invent a `content-type` out of another minter's slots.
///
/// MEASURED on HotSpot: `firstValue` of a name the headers do not carry is
/// `Optional.empty`, so declining is also the oracle-correct answer.
#[test]
fn http_headers_first_value_declines_a_foreign_receiver() {
    let cb = find_cb(
        "java/net/http/HttpHeaders",
        "firstValue",
        "(Ljava/lang/String;)Ljava/util/Optional;",
    );
    let mut ctx = crate::test_utils::MockNativeContext::new();

    // This file's own receiver, content-type present: still answered.
    let mine = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
    init_http_headers_fields(&mut ctx, mine, 1, 1, 0);
    let name = ctx.create_string("Content-Type");
    let slot0 = opt_slot0(
        cb,
        &mut ctx,
        &[Value::Object(Some(mine)), Value::Object(Some(name))],
    );
    assert_not_a_flag("firstValue.own", &slot0);
    match slot0 {
        Value::Object(Some(sv)) => assert_eq!(
            ctx.read_string(sv).as_deref(),
            Some("application/json"),
            "this file's own receiver must keep answering exactly as before the guard"
        ),
        other => panic!("expected the content-type string, got {other:?}"),
    }

    // A `net_phase_e`-shaped receiver: ONE slot, a reference in it. The
    // guard must fire BEFORE HDR_HAS_CT/HDR_HAS_CL are read, because slots
    // 1 and 2 do not exist on this object at all.
    let theirs = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
    let arr_stand_in = ctx.create_string("Content-Type: text/plain");
    ctx.set_field(theirs, 0, Value::Object(Some(arr_stand_in)));
    let name2 = ctx.create_string("Content-Type");
    let slot0 = opt_slot0(
        cb,
        &mut ctx,
        &[Value::Object(Some(theirs)), Value::Object(Some(name2))],
    );
    assert_eq!(
        slot0,
        Value::Object(None),
        "a receiver this file did not mint must get Optional.empty, not a \
         content-type fabricated from a slot that belongs to another layout"
    );
}

/// The same for `firstValueAsLong`, whose `OptionalLong` really does carry
/// a `(boolean isPresent, long value)` pair — so the absent answer has to
/// be `Int(0)` in slot 0, not merely a null reference.
#[test]
fn http_headers_first_value_as_long_declines_a_foreign_receiver() {
    let cb = find_cb(
        "java/net/http/HttpHeaders",
        "firstValueAsLong",
        "(Ljava/lang/String;)Ljava/util/OptionalLong;",
    );
    let mut ctx = crate::test_utils::MockNativeContext::new();

    let mine = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
    init_http_headers_fields(&mut ctx, mine, 1, 0, 1);
    let name = ctx.create_string("Content-Length");
    let got = match cb(
        &mut ctx,
        &[Value::Object(Some(mine)), Value::Object(Some(name))],
    )
    .expect("native must not fail")
    {
        Some(Value::Object(Some(o))) => (ctx.get_field(o, 0), ctx.get_field(o, 1)),
        other => panic!("expected an OptionalLong, got {other:?}"),
    };
    assert_eq!(
        got,
        (Value::Int(1), Value::Long(20)),
        "this file's own receiver must keep answering exactly as before the guard"
    );

    let theirs = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
    let arr_stand_in = ctx.create_string("Content-Length: 20");
    ctx.set_field(theirs, 0, Value::Object(Some(arr_stand_in)));
    let name2 = ctx.create_string("Content-Length");
    let got = match cb(
        &mut ctx,
        &[Value::Object(Some(theirs)), Value::Object(Some(name2))],
    )
    .expect("native must not fail")
    {
        Some(Value::Object(Some(o))) => (ctx.get_field(o, 0), ctx.get_field(o, 1)),
        other => panic!("expected an OptionalLong, got {other:?}"),
    };
    assert_eq!(
        got,
        (Value::Int(0), Value::Long(0)),
        "a foreign receiver must get OptionalLong.empty, not a fabricated 20"
    );
}

/// The ratchet. Every accessor `register_http_headers` registers reads a
/// slot map that only ONE of the three minters produces, so every one of
/// them must consult the discriminator — or, for `map`, read no counter at
/// all. A guard dropped in a later edit is silent at runtime (it answers a
/// plausible wrong header) and this is what makes it loud instead.
///
/// Source-level on purpose: the failure being ratcheted is "someone edits
/// this function and forgets", which no amount of behaviour on today's
/// receivers can catch.
#[test]
fn every_http_headers_accessor_is_shape_guarded() {
    let src = include_str!("../http2.rs");
    let start = src
        .find("fn register_http_headers(")
        .expect("register_http_headers must exist");
    let end = src[start..]
        .find("\nfn register_body_publisher(")
        .map(|o| start + o)
        .expect("register_body_publisher must follow register_http_headers");
    let body = &src[start..end];

    let guards = body
        .matches("http_headers_is_counter_shape(ctx, this)")
        .count();
    assert_eq!(
        guards, 3,
        "allValues / firstValue / firstValueAsLong must each test \
         http_headers_is_counter_shape before reading HDR_HAS_CT/HDR_HAS_CL. \
         `map` is the fourth accessor and is guarded more strongly — it \
         reads no counter slot at all."
    );

    // `map` must not have regained a counter read.
    let map_start = body
        .find("r.register(cls, \"map\", \"()Ljava/util/Map;\"")
        .expect("map must still be registered");
    assert!(
        !body[map_start..].contains("get_field(this, HDR_COUNT)"),
        "map() must not read HDR_COUNT: the slot is a counter only on this \
         file's own receiver, and the returned map carries no header names \
         to put in it either way"
    );
}

/// The `java/util/HashMap` and `java/util/ArrayList` this file hands back
/// must be shaped the way the rest of the crate shapes them, because it is
/// `native-collections`' registered natives — not this file — that answer
/// `size()`/`get()` on them afterwards.
///
/// MEASURED by sweep (2026-08-17): before this change `http2.rs` was the
/// ONLY `java/util/HashMap` allocation in `native-builtins` that asked for
/// 2 slots; seven other files ask for 3 and pair it with `native_map_init`.
#[test]
fn collection_returns_follow_the_crate_wide_slot_convention() {
    let src = include_str!("../http2.rs");
    assert!(
        !src.contains("try_alloc_concurrent_synthetic(ctx, \"java/util/HashMap\", 2)"),
        "a 2-slot java/util/HashMap cannot be read by any registered \
         native_map_* body; the crate convention is 3 slots + native_map_init"
    );
    assert!(
        src.contains("cratonvm_native_collections::native_map_init"),
        "the HashMap this file returns must be initialised by the same \
         helper every other allocator in the crate uses"
    );
    assert!(
        src.contains("cratonvm_native_collections::native_al_init"),
        "the ArrayList this file returns must be initialised by \
         native_al_init; hand-writing Int into slot 0 type-puns the slot \
         native_al_init keeps the backing Object[] in"
    );
}
