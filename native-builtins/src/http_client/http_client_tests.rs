use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn test_register_http_client_impl_init() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r
        .find("jdk/internal/net/http/HttpClientImpl", "<init>", "()V")
        .is_some());
}

#[test]
fn test_register_http_client_impl_send() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r.find(
        "jdk/internal/net/http/HttpClientImpl",
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"
    ).is_some());
}

#[test]
fn test_register_http_client_impl_send_async() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r.find(
        "jdk/internal/net/http/HttpClientImpl",
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"
    ).is_some());
}

#[test]
fn test_register_http_request_impl() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r
        .find("jdk/internal/net/http/HttpRequestImpl", "<init>", "()V")
        .is_some());
    assert!(r
        .find(
            "jdk/internal/net/http/HttpRequestImpl",
            "method",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(
            "jdk/internal/net/http/HttpRequestImpl",
            "uri",
            "()Ljava/net/URI;"
        )
        .is_some());
}

#[test]
fn test_register_http_response_impl() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r
        .find(
            "jdk/internal/net/http/HttpResponseImpl",
            "statusCode",
            "()I"
        )
        .is_some());
    assert!(r
        .find(
            "jdk/internal/net/http/HttpResponseImpl",
            "body",
            "()Ljava/lang/Object;"
        )
        .is_some());
    assert!(r
        .find(
            "jdk/internal/net/http/HttpResponseImpl",
            "headers",
            "()Ljava/net/http/HttpHeaders;"
        )
        .is_some());
}

#[test]
fn test_register_http2_client_impl() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r
        .find(
            "jdk/internal/net/http/Http2ClientImpl",
            "<init>",
            "(Ljdk/internal/net/http/HttpClientImpl;)V"
        )
        .is_some());
}

#[test]
fn test_register_http1_parser_and_exchange() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    assert!(r
        .find("jdk/internal/net/http/Http1HeaderParser", "<init>", "()V")
        .is_some());
    assert!(r
        .find("jdk/internal/net/http/Http1HeaderParser", "parse", "([B)Z")
        .is_some());
    assert!(r
        .find("jdk/internal/net/http/Http1Exchange", "<init>", "()V")
        .is_some());
}

#[test]
fn test_parse_uri_http() {
    let p = parse_uri("http://example.com/foo/bar").unwrap();
    assert_eq!(p.scheme, "http");
    assert_eq!(p.host, "example.com");
    assert_eq!(p.port, 80);
    assert_eq!(p.path, "/foo/bar");
}

#[test]
fn test_parse_uri_https_with_port() {
    let p = parse_uri("https://example.com:8443/").unwrap();
    assert_eq!(p.scheme, "https");
    assert_eq!(p.host, "example.com");
    assert_eq!(p.port, 8443);
    assert_eq!(p.path, "/");
}

#[test]
fn test_parse_uri_no_path() {
    let p = parse_uri("https://example.com").unwrap();
    assert_eq!(p.path, "/");
    assert_eq!(p.port, 443);
}

#[test]
fn test_parse_uri_query_only_and_fragment() {
    let p = parse_uri("http://localhost:8080?trace=false&message=false").unwrap();
    assert_eq!(p.host, "localhost");
    assert_eq!(p.port, 8080);
    assert_eq!(p.path, "/?trace=false&message=false");

    let p = parse_uri("https://example.com:8443#client-only").unwrap();
    assert_eq!(p.host, "example.com");
    assert_eq!(p.port, 8443);
    assert_eq!(p.path, "/");
}

#[test]
fn test_parse_uri_rejects_unknown_scheme() {
    assert!(parse_uri("ftp://example.com").is_err());
}

#[test]
fn test_parse_uri_rejects_empty_host() {
    assert!(parse_uri("http:///path").is_err());
}

#[test]
fn test_build_http1_get_request() {
    let p = parse_uri("http://example.com/").unwrap();
    let req = build_http1_request("GET", &p, &[], &[]);
    let s = String::from_utf8(req).unwrap();
    assert!(s.starts_with("GET / HTTP/1.1\r\n"));
    assert!(s.contains("Host: example.com\r\n"));
    assert!(s.contains("User-Agent: cratonvm-httpclient/1.0"));
}

#[test]
fn test_build_http1_post_with_body() {
    let p = parse_uri("https://api.example.com:8443/v1").unwrap();
    let req = build_http1_request("POST", &p, &[], b"{\"x\":1}");
    let s = String::from_utf8(req).unwrap();
    assert!(s.starts_with("POST /v1 HTTP/1.1\r\n"));
    assert!(s.contains("Host: api.example.com:8443\r\n"));
    assert!(s.contains("Content-Length: 7\r\n"));
    assert!(s.ends_with("{\"x\":1}"));
}

#[test]
fn test_build_http1_preserves_user_supplied_headers() {
    let p = parse_uri("http://example.com/").unwrap();
    let h = vec![
        ("X-Token".to_string(), "secret".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let req = build_http1_request("GET", &p, &h, &[]);
    let s = String::from_utf8(req).unwrap();
    assert!(s.contains("X-Token: secret\r\n"));
    assert!(s.contains("Accept: application/json\r\n"));
    // Default Accept header must not be appended once user supplied one.
    assert_eq!(s.matches("Accept:").count(), 1);
}

#[test]
fn test_decode_chunked_simple() {
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(b"5\r\nhello\r\n");
    data.extend_from_slice(b"6\r\n world\r\n");
    data.extend_from_slice(b"0\r\n\r\n");
    let mut empty: &[u8] = &[];
    let body = read_chunked(&mut data, &mut empty).unwrap();
    assert_eq!(body, b"hello world");
}

#[test]
fn test_hpack_indexed_encode_then_decode() {
    let mut buf: Vec<u8> = Vec::new();
    // :method GET = static index 2.
    encode_hpack_indexed(&mut buf, 2);
    let (idx, rest) = decode_hpack_int(&buf, 7).unwrap();
    assert_eq!(idx, 2);
    assert!(rest.is_empty());
}

#[test]
fn test_hpack_string_roundtrip() {
    let mut buf: Vec<u8> = Vec::new();
    encode_hpack_string(&mut buf, b"x-custom-header");
    let (s, rest) = decode_hpack_string(&buf).unwrap();
    assert_eq!(s, "x-custom-header");
    assert!(rest.is_empty());
}

#[test]
fn test_pool_checkout_returns_none_when_empty() {
    let mut p = ClientPool::new();
    let key: PoolKey = ("https".into(), "example.com".into(), 443);
    assert!(p.checkout(&key).is_none());
}

#[test]
fn test_find_subslice() {
    assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
    assert_eq!(find_subslice(b"abc", b"d"), None);
    assert_eq!(find_subslice(b"abc", b""), None);
    assert_eq!(find_subslice(b"abc\r\n\r\nbody", b"\r\n\r\n"), Some(3));
}

#[test]
fn test_extract_request_headers_parses_lines() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let arr = ctx.new_ref_array(ClassId::new(0), 3);
    let h0 = ctx.create_string("Authorization: Bearer xyz");
    let h1 = ctx.create_string("Content-Type: application/json");
    let h2 = ctx.create_string("malformed-without-colon");
    ctx.set_array_element(arr, 0, Value::Object(Some(h0)));
    ctx.set_array_element(arr, 1, Value::Object(Some(h1)));
    ctx.set_array_element(arr, 2, Value::Object(Some(h2)));
    let parsed = extract_request_headers(&ctx, Value::Object(Some(arr)));
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].0, "Authorization");
    assert_eq!(parsed[0].1, "Bearer xyz");
    assert_eq!(parsed[1].0, "Content-Type");
    assert_eq!(parsed[1].1, "application/json");
}

#[test]
fn test_alloc_response_and_status() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let req = ctx.alloc_object(ClassId::new(0), HRQ_NUM_FIELDS);
    let resp = WireResponse {
        status: 200,
        version_h2: false,
        headers: vec![("X-Foo".to_string(), "bar".to_string())],
        body: b"hello".to_vec(),
        keep_alive: true,
    };
    let obj = alloc_response(&mut ctx, &resp, req, "http://example.com/").unwrap();
    assert_eq!(ctx.get_field(obj, HRS_STATUS), Value::Int(200));
    assert_eq!(
        ctx.get_field(obj, HRS_VERSION),
        Value::Int(HTTP_VERSION_1_1)
    );
}

#[test]
fn test_read_chunked_happy_path() {
    // "5\r\nhello\r\n0\r\n\r\n" -> "hello"
    let mut prefix = b"5\r\nhello\r\n0\r\n\r\n".to_vec();
    let mut empty = std::io::Cursor::new(Vec::<u8>::new());
    let out = read_chunked(&mut prefix, &mut empty).expect("decode");
    assert_eq!(out, b"hello");
}

#[test]
fn test_read_chunked_rejects_oversized_chunk_no_alloc() {
    // A near-usize::MAX hex chunk size must be rejected immediately, before
    // buffering anything (covers the overflow + pre-cap allocation DoS).
    let mut prefix = b"ffffffffffffffff\r\n".to_vec();
    let mut empty = std::io::Cursor::new(Vec::<u8>::new());
    let err = read_chunked(&mut prefix, &mut empty).unwrap_err();
    assert!(err.contains("MAX_RESPONSE_BODY"), "unexpected error: {err}");
}

#[test]
fn test_read_chunked_rejects_large_but_parseable_chunk() {
    // 0x7FFFFFFF (~2 GiB) is a valid hex size but exceeds the body cap; it
    // must be rejected without attempting to buffer 2 GiB.
    let mut prefix = b"7fffffff\r\n".to_vec();
    let mut empty = std::io::Cursor::new(Vec::<u8>::new());
    let err = read_chunked(&mut prefix, &mut empty).unwrap_err();
    assert!(err.contains("MAX_RESPONSE_BODY"), "unexpected error: {err}");
}

#[test]
fn test_decode_hpack_string_huffman_roundtrip() {
    // HPACK string: H=1, len=12, then Huffman("www.example.com" is 15 bytes;
    // use RFC 7541 C.4.1 "www.example.com" encoded = 12 bytes).
    // Length prefix 0x8c = 0x80 (Huffman) | 0x0c (len 12).
    let mut input = vec![0x8c];
    input.extend_from_slice(&[
        0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff,
    ]);
    let (s, rest) = decode_hpack_string(&input).expect("huffman decode");
    assert_eq!(s, "www.example.com");
    assert!(rest.is_empty());
}

#[test]
fn test_read_chunked_bounds_unterminated_size_line() {
    // A size line that never contains a CRLF must be rejected once it grows
    // past MAX_CHUNK_LINE instead of buffering without bound (DoS).
    let mut prefix = Vec::new();
    let stream_bytes = vec![b'0'; MAX_CHUNK_LINE + 4096];
    let mut stream = std::io::Cursor::new(stream_bytes);
    let err = read_chunked(&mut prefix, &mut stream).unwrap_err();
    assert!(err.contains("size line"), "unexpected error: {err}");
}

#[test]
fn test_read_chunked_bounds_unterminated_trailer() {
    // A zero chunk followed by trailer bytes that never terminate with a
    // CRLF must be rejected once they exceed MAX_CHUNK_LINE.
    let mut prefix = b"0\r\n".to_vec();
    // No CRLF anywhere in the trailer stream.
    let stream_bytes = vec![b'x'; MAX_CHUNK_LINE + 4096];
    let mut stream = std::io::Cursor::new(stream_bytes);
    let err = read_chunked(&mut prefix, &mut stream).unwrap_err();
    assert!(err.contains("trailer"), "unexpected error: {err}");
}

#[test]
fn test_is_sensitive_redirect_header() {
    // Credential-bearing headers are flagged (case-insensitively) so they
    // are stripped on a cross-origin redirect; ordinary headers are not.
    assert!(is_sensitive_redirect_header("Authorization"));
    assert!(is_sensitive_redirect_header("authorization"));
    assert!(is_sensitive_redirect_header("Proxy-Authorization"));
    assert!(is_sensitive_redirect_header("Cookie"));
    assert!(is_sensitive_redirect_header("COOKIE"));
    assert!(!is_sensitive_redirect_header("Accept"));
    assert!(!is_sensitive_redirect_header("User-Agent"));
    assert!(!is_sensitive_redirect_header("Content-Type"));
}

// -----------------------------------------------------------------------
// E21 — the accessors that answered in a form their descriptor does not
// name. Every test below INVOKES the native through the registry and
// asserts the slot it actually wrote; the 22 tests that were here before
// are registration-only (`find(...).is_some()`) and stayed green through
// the entire life of these defects.
//
// WHAT THE MOCK CAN AND CANNOT MEASURE, stated once: `MockNativeContext`
// has NO statics table — `set_static_field` is a no-op,
// `static_field_index_by_name` answers `None` and `get_static_field`
// answers `Value::Int(0)`. So `enum_constant_static` can only ever reach
// its fail-safe here, and these tests pin the LAW ("never a primitive
// where a reference is declared", "slot 0 of an Optional is a reference"),
// not the mirror lookup. The mirror needs a VM, and the record nominates
// the mock upgrade that would let a unit test see it.
// -----------------------------------------------------------------------

const CLS_CLIENT: &str = "jdk/internal/net/http/HttpClientImpl";
const CLS_REQ: &str = "jdk/internal/net/http/HttpRequestImpl";
const CLS_RESP: &str = "jdk/internal/net/http/HttpResponseImpl";

fn e21_cb(class: &str, name: &str, desc: &str) -> cratonvm_native_api::NativeCallback {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    r.find(class, name, desc)
        .unwrap_or_else(|| panic!("{class}.{name}{desc} must be registered"))
}

/// Invoke a `(this)` native. `<init>` and other `void` natives answer
/// `None` here — `e21_value` is the arm that demands a return value.
fn e21_call(
    cb: cratonvm_native_api::NativeCallback,
    ctx: &mut crate::test_utils::MockNativeContext,
    this: ObjectRef,
) -> Option<Value> {
    cb(ctx, &[Value::Object(Some(this))]).expect("native must not fail")
}

fn e21_value(
    cb: cratonvm_native_api::NativeCallback,
    ctx: &mut crate::test_utils::MockNativeContext,
    this: ObjectRef,
) -> Value {
    e21_call(cb, ctx, this).expect("native must return a value")
}

/// An `Optional` is ONE slot and that slot is a REFERENCE.
///
/// The mock zero-fills fields to `Value::Int(0)`, so a native that wrote
/// nothing fails here too — which is the same shape as the bug.
fn e21_assert_optional(ctx: &crate::test_utils::MockNativeContext, v: Value) -> Value {
    let Value::Object(Some(opt)) = v else {
        panic!("expected an Optional object, got {v:?}");
    };
    assert_eq!(
        ctx.object_num_fields(opt),
        1,
        "java.util.Optional declares ONE field, the reference `value`; a \
         2-slot (isPresent, value) layout is OptionalInt/Long/Double's"
    );
    let slot0 = ctx.get_field(opt, 0);
    assert!(
        matches!(slot0, Value::Object(_)),
        "slot 0 of a java.util.Optional is the reference `value`; {slot0:?} \
         there makes isPresent() answer TRUE for an empty Optional and \
         get() hand a primitive to a caller that will dereference it"
    );
    slot0
}

/// A default request has NO version override — measured on JDK 25.0.3+9-LTS:
/// `HttpRequest.newBuilder(uri).build().version()` is `Optional.empty`.
///
/// Before this patch `<init>` stored `Int(HTTP_VERSION_2)` and the accessor
/// dropped it straight into slot 0, so `isPresent()` was TRUE for every
/// request ever built and `get()` returned an `Int` typed as
/// `HttpClient$Version`.
#[test]
fn e21_request_version_is_empty_and_never_a_primitive() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(CLS_REQ).unwrap();
    let this = ctx.alloc_object(cid, HRQ_NUM_FIELDS);
    assert!(e21_call(e21_cb(CLS_REQ, "<init>", "()V"), &mut ctx, this).is_none());
    assert_eq!(
        ctx.get_field(this, HRQ_VERSION),
        Value::Int(HRQ_VERSION_UNSET),
        "<init> must record NO version override"
    );
    let v = e21_value(
        e21_cb(CLS_REQ, "version", "()Ljava/util/Optional;"),
        &mut ctx,
        this,
    );
    assert_eq!(e21_assert_optional(&ctx, v), Value::Object(None));
}

/// A request timeout is an `Optional<Duration>`, not an `Optional<long>`.
///
/// `Long(0)` reads as null to `ref_operand_is_null`, so the ABSENT arm was
/// accidentally right and only the PRESENT arm was wrong — which is why no
/// registration-only test and no absent-case fixture row could see it.
#[test]
fn e21_request_timeout_present_is_a_duration_not_a_long() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(CLS_REQ).unwrap();
    let this = ctx.alloc_object(cid, HRQ_NUM_FIELDS);
    ctx.set_field(this, HRQ_TIMEOUT_MS, Value::Long(7_000));
    let v = e21_value(
        e21_cb(CLS_REQ, "timeout", "()Ljava/util/Optional;"),
        &mut ctx,
        this,
    );
    let Value::Object(Some(dur)) = e21_assert_optional(&ctx, v) else {
        panic!("timeout(7000ms) must be PRESENT and hold a Duration");
    };
    assert_eq!(ctx.get_field(dur, 0), Value::Long(7));
    assert_eq!(ctx.get_field(dur, 1), Value::Int(0));

    let this2 = ctx.alloc_object(cid, HRQ_NUM_FIELDS);
    ctx.set_field(this2, HRQ_TIMEOUT_MS, Value::Long(0));
    let v2 = e21_value(
        e21_cb(CLS_REQ, "timeout", "()Ljava/util/Optional;"),
        &mut ctx,
        this2,
    );
    assert_eq!(e21_assert_optional(&ctx, v2), Value::Object(None));
}

/// A negative `Duration` is floor-normalised, not truncated:
/// `Duration.ofMillis(-1500)` is `seconds = -2, nanos = +500_000_000` on
/// the oracle, and `Duration`'s `nanos` is documented non-negative.
#[test]
fn e21_connect_timeout_duration_is_floor_normalised() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(CLS_CLIENT).unwrap();
    let this = ctx.alloc_object(cid, HCI_NUM_FIELDS);
    ctx.set_field(this, HCI_CONNECT_TIMEOUT_MS, Value::Long(-1_500));
    let v = e21_value(
        e21_cb(CLS_CLIENT, "connectTimeout", "()Ljava/util/Optional;"),
        &mut ctx,
        this,
    );
    let Value::Object(Some(dur)) = e21_assert_optional(&ctx, v) else {
        panic!("a non-zero connectTimeout must be PRESENT");
    };
    assert_eq!(ctx.get_field(dur, 0), Value::Long(-2));
    assert_eq!(ctx.get_field(dur, 1), Value::Int(500_000_000));
}

/// The four presence accessors were already the right SHAPE — right arity
/// AND a reference in slot 0. This is the negative control: it must keep
/// passing, and anyone who "simplifies" the helper into a flag breaks it
/// by name.
#[test]
fn e21_presence_accessors_are_empty_optionals_not_flags() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(CLS_CLIENT).unwrap();
    let this = ctx.alloc_object(cid, HCI_NUM_FIELDS);
    assert!(e21_call(e21_cb(CLS_CLIENT, "<init>", "()V"), &mut ctx, this).is_none());
    for name in ["executor", "proxy", "cookieHandler", "authenticator"] {
        let v = e21_value(
            e21_cb(CLS_CLIENT, name, "()Ljava/util/Optional;"),
            &mut ctx,
            this,
        );
        assert_eq!(
            e21_assert_optional(&ctx, v),
            Value::Object(None),
            "{name}() stores no object anywhere, so empty() is the honest answer"
        );
    }
}

/// The three enum-returning accessors must never surface a primitive.
///
/// Under the mock the mirror lookup cannot succeed (no statics table), so
/// the assertion is the FAIL-SAFE: `Object(None)`, never `Int(ordinal)`.
/// That is precisely the discriminator — every one of these three returned
/// `Value::Int` before this patch, against a descriptor naming
/// `HttpClient$Version` / `HttpClient$Redirect`.
#[test]
fn e21_enum_accessors_never_return_an_ordinal() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let ccid = ctx.ensure_class_initialized(CLS_CLIENT).unwrap();
    let client = ctx.alloc_object(ccid, HCI_NUM_FIELDS);
    assert!(e21_call(e21_cb(CLS_CLIENT, "<init>", "()V"), &mut ctx, client).is_none());
    let rcid = ctx.ensure_class_initialized(CLS_RESP).unwrap();
    let resp = ctx.alloc_object(rcid, HRS_NUM_FIELDS);
    ctx.set_field(resp, HRS_VERSION, Value::Int(HTTP_VERSION_2));

    for (this, cls, name, desc) in [
        (
            client,
            CLS_CLIENT,
            "version",
            "()Ljava/net/http/HttpClient$Version;",
        ),
        (
            client,
            CLS_CLIENT,
            "followRedirects",
            "()Ljava/net/http/HttpClient$Redirect;",
        ),
        (
            resp,
            CLS_RESP,
            "version",
            "()Ljava/net/http/HttpClient$Version;",
        ),
    ] {
        let v = e21_value(e21_cb(cls, name, desc), &mut ctx, this);
        assert!(
            matches!(v, Value::Object(_)),
            "{cls}.{name}{desc} declares a REFERENCE return; {v:?} is an \
             ordinal handed to bytecode about to areturn/checkcast it"
        );
    }
}

/// `HttpResponse.previousResponse()` — covered by nothing else in the
/// repository, and the row C12-3 calls "the one that settles what this is".
#[test]
fn e21_previous_response_is_an_empty_optional() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let cid = ctx.ensure_class_initialized(CLS_RESP).unwrap();
    let this = ctx.alloc_object(cid, HRS_NUM_FIELDS);
    ctx.set_field(this, HRS_PREVIOUS, Value::Object(None));
    let v = e21_value(
        e21_cb(CLS_RESP, "previousResponse", "()Ljava/util/Optional;"),
        &mut ctx,
        this,
    );
    assert_eq!(e21_assert_optional(&ctx, v), Value::Object(None));
}

/// The two files' version encodings differ and that is deliberate; this
/// test exists so a "tidy-up" that unifies them has to read the reason.
/// `http2.rs` stores `ordinal + 1` (so `0` can mean "no override"); this
/// file stores the JDK ordinal itself and uses a separate sentinel.
#[test]
fn e21_this_files_version_constants_are_the_jdk_ordinals() {
    assert_eq!(HTTP_VERSION_1_1, 0);
    assert_eq!(HTTP_VERSION_2, 1);
    assert_eq!(
        crate::phases_late::net_channels::HTTP_VERSION_CONSTANTS,
        &["HTTP_1_1", "HTTP_2"],
        "declaration order IS the ordinal; measured with javap on JDK 25"
    );
    assert_eq!(
        crate::phases_late::net_channels::HTTP_REDIRECT_CONSTANTS,
        &["NEVER", "ALWAYS", "NORMAL"],
        "NOT this file's REDIRECT_* order (NEVER, NORMAL, ALWAYS) — \
         http_redirect_mirror remaps between them"
    );
    assert_ne!(
        REDIRECT_NORMAL, 2,
        "if REDIRECT_* is ever renumbered to the JDK's ordinals, \
         http_redirect_mirror's remap must go with it"
    );
}

/// G29-1. `HttpRequestImpl` is this file's implementation twin of
/// `java.net.http.HttpRequest`, which declares SEVEN instance accessors.
/// Four were registered here (`method`, `uri`, `version`, `timeout`) and
/// three were not. Unlike the abstract class in `net_phase_e.rs`, an
/// unregistered accessor on THIS class does not throw: `HttpRequestImpl` is
/// a real JDK class, so the JDK's own body runs against a layout that is
/// not the JDK's — a wrong answer instead of a refusal, which is worse.
#[test]
fn http_request_impl_answers_all_seven_request_accessors() {
    let mut r = NativeMethodRegistry::new();
    register_http_client_real(&mut r);
    for (name, descriptor) in [
        ("method", "()Ljava/lang/String;"),
        ("uri", "()Ljava/net/URI;"),
        ("timeout", "()Ljava/util/Optional;"),
        ("version", "()Ljava/util/Optional;"),
        ("bodyPublisher", "()Ljava/util/Optional;"),
        ("expectContinue", "()Z"),
        ("headers", "()Ljava/net/http/HttpHeaders;"),
    ] {
        assert!(
            r.find("jdk/internal/net/http/HttpRequestImpl", name, descriptor)
                .is_some(),
            "jdk/internal/net/http/HttpRequestImpl.{name}{descriptor} has no native — \
             the real JDK body would run over this file's slot layout"
        );
    }
}

/// The slot `expectContinue` reads must be inside the object `<init>`
/// allocates, and must not collide with a slot that already has an owner.
#[test]
fn hrq_expect_continue_slot_is_inside_the_allocation_and_unique() {
    let slots = [
        HRQ_METHOD,
        HRQ_URI,
        HRQ_BODY_BYTES,
        HRQ_HEADERS,
        HRQ_TIMEOUT_MS,
        HRQ_VERSION,
        HRQ_EXPECT_CONTINUE,
    ];
    let mut sorted = slots.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), slots.len(), "two HRQ slots share an index");
    assert_eq!(sorted, (0..HRQ_NUM_FIELDS).collect::<Vec<_>>());
}

/// gc-common w19-d: `HttpRequestImpl.uri()` / `HttpResponseImpl.uri()`
/// build the URI rooted across its raw-text String and the by-name
/// publish. The mock never moves an object at an allocation, so this
/// checks the text, the unset-slot answer and that no root is left behind.
#[test]
fn w19d_impl_uri_carries_the_slot_text_and_releases_its_roots() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let holder = ctx.fresh_object_ref();
    let text = ctx.create_string("http://example.test/p");
    ctx.set_field(holder, 1, Value::Object(Some(text)));
    assert_eq!(uri_text_of_slot(&ctx, holder, 1), "http://example.test/p");
    assert_eq!(
        uri_text_of_slot(&ctx, holder, 2),
        "",
        "an unset slot reads as empty"
    );

    let uri = http_impl_uri_object(&mut ctx, "http://example.test/p").unwrap();
    match ctx.get_field(uri, 0) {
        Value::Object(Some(s)) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some("http://example.test/p"))
        }
        other => panic!("the synthetic layout keeps the text in slot 0, got {other:?}"),
    }
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
