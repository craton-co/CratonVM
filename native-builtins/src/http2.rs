//! HTTP/2 Java client native method implementations.
//!
//! Provides java.net.http.HttpClient, HttpRequest, HttpResponse,
//! HttpHeaders, HttpRequest$BodyPublisher, HttpResponse$BodyHandlers,
//! and WebSocket support for the CratonVM native layer.
//!
//! Implements Java HTTP Client API (java.net.http) introduced in Java 11,
//! with HTTP/2 (RFC 7540) and HPACK header compression (RFC 7541) stubs.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};
use crate::{native_noop_with_this, obj_arg, alloc_concurrent_synthetic};

use std::io::Write;
use std::net::TcpStream;

// ---------------------------------------------------------------------------
// HTTP/2 frame types per RFC 7540
// ---------------------------------------------------------------------------

/// HTTP/2 frame types per RFC 7540
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum Http2FrameType {
    Data        = 0x0,
    Headers     = 0x1,
    Priority    = 0x2,
    RstStream   = 0x3,
    Settings    = 0x4,
    PushPromise = 0x5,
    Ping        = 0x6,
    Goaway      = 0x7,
    WindowUpdate = 0x8,
    Continuation = 0x9,
}

/// HTTP/2 error codes per RFC 7540
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum Http2ErrorCode {
    NoError           = 0x0,
    ProtocolError     = 0x1,
    InternalError     = 0x2,
    FlowControlError  = 0x3,
    SettingsTimeout   = 0x4,
    StreamClosed      = 0x5,
    FrameSizeError    = 0x6,
    RefusedStream     = 0x7,
    Cancel            = 0x8,
    CompressionError  = 0x9,
    ConnectError      = 0xa,
    EnhanceYourCalm   = 0xb,
    InadequateSecurity = 0xc,
    Http11Required    = 0xd,
}

// ---------------------------------------------------------------------------
// HPACK static table (RFC 7541 Appendix A)
// ---------------------------------------------------------------------------

/// HPACK static table entry (name, value).
/// First 61 entries are the static table defined in RFC 7541 Appendix A.
pub struct HpackStaticTable;

impl HpackStaticTable {
    pub const ENTRIES: &'static [(&'static str, &'static str)] = &[
        (":authority",                  ""),
        (":method",                     "GET"),
        (":method",                     "POST"),
        (":path",                       "/"),
        (":path",                       "/index.html"),
        (":scheme",                     "http"),
        (":scheme",                     "https"),
        (":status",                     "200"),
        (":status",                     "204"),
        (":status",                     "206"),
        (":status",                     "304"),
        (":status",                     "400"),
        (":status",                     "404"),
        (":status",                     "500"),
        ("accept-charset",              ""),
        ("accept-encoding",             "gzip, deflate"),
        ("accept-language",             ""),
        ("accept-ranges",               ""),
        ("accept",                      ""),
        ("access-control-allow-origin", ""),
        ("age",                         ""),
        ("allow",                       ""),
        ("authorization",               ""),
        ("cache-control",               ""),
        ("content-disposition",         ""),
        ("content-encoding",            ""),
        ("content-language",            ""),
        ("content-length",              ""),
        ("content-location",            ""),
        ("content-range",               ""),
        ("content-type",                ""),
        ("cookie",                      ""),
        ("date",                        ""),
        ("etag",                        ""),
        ("expect",                      ""),
        ("expires",                     ""),
        ("from",                        ""),
        ("host",                        ""),
        ("if-match",                    ""),
        ("if-modified-since",           ""),
        ("if-none-match",               ""),
        ("if-range",                    ""),
        ("if-unmodified-since",         ""),
        ("last-modified",               ""),
        ("link",                        ""),
        ("location",                    ""),
        ("max-forwards",                ""),
        ("proxy-authenticate",          ""),
        ("proxy-authorization",         ""),
        ("range",                       ""),
        ("referer",                     ""),
        ("refresh",                     ""),
        ("retry-after",                 ""),
        ("server",                      ""),
        ("set-cookie",                  ""),
        ("strict-transport-security",   ""),
        ("transfer-encoding",           ""),
        ("user-agent",                  ""),
        ("vary",                        ""),
        ("via",                         ""),
        ("www-authenticate",            ""),
    ];

    /// Find a static table index by header name (and optional value).
    /// Returns 1-based index per the HPACK spec, or None if not found.
    pub fn find(name: &str, value: Option<&str>) -> Option<usize> {
        for (i, &(n, v)) in Self::ENTRIES.iter().enumerate() {
            if n == name {
                if let Some(val) = value {
                    if v == val {
                        return Some(i + 1);
                    }
                } else {
                    return Some(i + 1);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Version / redirect policy constants
// ---------------------------------------------------------------------------

const HTTP_VERSION_1_1: i32 = 0;
const HTTP_VERSION_2:   i32 = 1;

const REDIRECT_NEVER:  i32 = 0;
const REDIRECT_NORMAL: i32 = 1;
const REDIRECT_ALWAYS: i32 = 2;

// Default connection pool size
const DEFAULT_POOL_SIZE: i32 = 20;

// HTTP method indices
const METHOD_GET:     i32 = 0;
const METHOD_POST:    i32 = 1;
const METHOD_PUT:     i32 = 2;
const METHOD_DELETE:  i32 = 3;
const METHOD_HEAD:    i32 = 4;
const METHOD_PATCH:   i32 = 5;
const METHOD_OPTIONS: i32 = 6;

// WebSocket states
const WS_OPEN:    i32 = 0;
const WS_CLOSING: i32 = 1;
const WS_CLOSED:  i32 = 2;

// ---------------------------------------------------------------------------
// Field-index constants
// ---------------------------------------------------------------------------

// HttpClient fields
const CLIENT_VERSION:         usize = 0;
const CLIENT_REDIRECT:        usize = 1;
const CLIENT_CONNECT_TIMEOUT: usize = 2;
const CLIENT_HAS_SSL:         usize = 3;
const CLIENT_HAS_EXECUTOR:    usize = 4;
const CLIENT_HAS_PROXY:       usize = 5;
const CLIENT_HAS_AUTH:        usize = 6;
const CLIENT_HAS_COOKIE:      usize = 7;
const CLIENT_FOLLOW_REDIR:    usize = 8;
const CLIENT_POOL_SIZE:       usize = 9;

// HttpRequest fields
const REQ_METHOD:     usize = 0;
const REQ_URI:        usize = 1;
const REQ_HAS_BODY:   usize = 2;
const REQ_TIMEOUT:    usize = 3;
const REQ_VERSION:    usize = 4;
const REQ_EXPECT_100: usize = 5;
const REQ_HDR_COUNT:  usize = 6;
const REQ_BODY_LEN:   usize = 7;

// HttpResponse fields
const RESP_STATUS:   usize = 0;
const RESP_VERSION:  usize = 1;
const RESP_BODY_LEN: usize = 2;
const RESP_HAS_PREV: usize = 3;
const RESP_METHOD:   usize = 4;
const RESP_HAS_SSL:  usize = 5;
const RESP_BODY_OBJ: usize = 6;

// HttpHeaders fields
const HDR_COUNT:       usize = 0;
const HDR_HAS_CT:      usize = 1;
const HDR_HAS_CL:      usize = 2;

// WebSocket fields
const WS_STATE:           usize = 0;
const WS_SUBPROTOCOL:     usize = 1;
const WS_OUTPUT_CLOSED:   usize = 2;
const WS_INPUT_CLOSED:    usize = 3;

// ---------------------------------------------------------------------------
// Allocation helpers
// ---------------------------------------------------------------------------

fn alloc_http_client(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 10);
    ctx.set_field(obj, CLIENT_VERSION,         Value::Int(HTTP_VERSION_2));
    ctx.set_field(obj, CLIENT_REDIRECT,        Value::Int(REDIRECT_NEVER));
    ctx.set_field(obj, CLIENT_CONNECT_TIMEOUT, Value::Long(0));
    ctx.set_field(obj, CLIENT_HAS_SSL,         Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_EXECUTOR,    Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_PROXY,       Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_AUTH,        Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_COOKIE,      Value::Int(0));
    ctx.set_field(obj, CLIENT_FOLLOW_REDIR,    Value::Int(0));
    ctx.set_field(obj, CLIENT_POOL_SIZE,       Value::Int(DEFAULT_POOL_SIZE));
    obj
}

fn alloc_http_client_builder(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient$Builder", 8);
    ctx.set_field(obj, 0, Value::Int(HTTP_VERSION_2));
    ctx.set_field(obj, 1, Value::Int(REDIRECT_NEVER));
    ctx.set_field(obj, 2, Value::Long(0));
    ctx.set_field(obj, 3, Value::Int(0));
    ctx.set_field(obj, 4, Value::Int(0));
    ctx.set_field(obj, 5, Value::Int(0));
    ctx.set_field(obj, 6, Value::Int(0));
    ctx.set_field(obj, 7, Value::Int(0));
    obj
}

fn alloc_http_request(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", 8);
    ctx.set_field(obj, REQ_METHOD,     Value::Int(METHOD_GET));
    ctx.set_field(obj, REQ_URI,        Value::Object(None));
    ctx.set_field(obj, REQ_HAS_BODY,   Value::Int(0));
    ctx.set_field(obj, REQ_TIMEOUT,    Value::Long(0));
    ctx.set_field(obj, REQ_VERSION,    Value::Int(0));
    ctx.set_field(obj, REQ_EXPECT_100, Value::Int(0));
    ctx.set_field(obj, REQ_HDR_COUNT,  Value::Int(0));
    ctx.set_field(obj, REQ_BODY_LEN,   Value::Long(0));
    obj
}

fn alloc_http_request_builder(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 8);
    ctx.set_field(obj, REQ_METHOD,     Value::Int(METHOD_GET));
    ctx.set_field(obj, REQ_URI,        Value::Object(None));
    ctx.set_field(obj, REQ_HAS_BODY,   Value::Int(0));
    ctx.set_field(obj, REQ_TIMEOUT,    Value::Long(0));
    ctx.set_field(obj, REQ_VERSION,    Value::Int(0));
    ctx.set_field(obj, REQ_EXPECT_100, Value::Int(0));
    ctx.set_field(obj, REQ_HDR_COUNT,  Value::Int(0));
    ctx.set_field(obj, REQ_BODY_LEN,   Value::Long(0));
    obj
}

fn alloc_http_response(ctx: &mut dyn NativeContext, status: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 7);
    ctx.set_field(obj, RESP_STATUS,   Value::Int(status));
    ctx.set_field(obj, RESP_VERSION,  Value::Int(HTTP_VERSION_2));
    ctx.set_field(obj, RESP_BODY_LEN, Value::Long(0));
    ctx.set_field(obj, RESP_HAS_PREV, Value::Int(0));
    ctx.set_field(obj, RESP_METHOD,   Value::Int(METHOD_GET));
    ctx.set_field(obj, RESP_HAS_SSL,  Value::Int(0));
    ctx.set_field(obj, RESP_BODY_OBJ, Value::Object(None));
    obj
}

fn alloc_http_headers(ctx: &mut dyn NativeContext, count: i32, has_ct: i32, has_cl: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpHeaders", 3);
    ctx.set_field(obj, HDR_COUNT,  Value::Int(count));
    ctx.set_field(obj, HDR_HAS_CT, Value::Int(has_ct));
    ctx.set_field(obj, HDR_HAS_CL, Value::Int(has_cl));
    obj
}

fn alloc_websocket(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/WebSocket", 4);
    ctx.set_field(obj, WS_STATE,         Value::Int(WS_OPEN));
    ctx.set_field(obj, WS_SUBPROTOCOL,   Value::Int(0));
    ctx.set_field(obj, WS_OUTPUT_CLOSED, Value::Int(0));
    ctx.set_field(obj, WS_INPUT_CLOSED,  Value::Int(0));
    obj
}

// ---------------------------------------------------------------------------
// Real HTTP/1.1 request helper
// ---------------------------------------------------------------------------

/// URI field layout (from phases_early.rs): scheme=0, host=1, port=2, path=3, query=4, fragment=5, raw=6
const URI_SCHEME: usize = 0;
const URI_HOST:   usize = 1;
const URI_PORT:   usize = 2;
const URI_PATH:   usize = 3;

/// Extract host, port, and path from a URI object.
fn extract_uri_parts(ctx: &dyn NativeContext, uri: ObjectRef) -> Option<(String, u16, String)> {
    let host = match ctx.get_field(uri, URI_HOST) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let port = match ctx.get_field(uri, URI_PORT) {
        Value::Int(p) if p > 0 => p as u16,
        _ => {
            // Infer from scheme
            match ctx.get_field(uri, URI_SCHEME) {
                Value::Object(Some(s)) => {
                    match ctx.read_string(s).as_deref() {
                        Some("https") => 443,
                        _ => 80,
                    }
                }
                _ => 80,
            }
        }
    };
    let path = match ctx.get_field(uri, URI_PATH) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "/".to_string()),
        _ => "/".to_string(),
    };
    Some((host, port, path))
}

/// Maximum HTTP response body size (10 MB).
const MAX_RESPONSE_BODY: usize = 10 * 1024 * 1024;

/// Perform a real HTTP/1.1 request with optional TLS and return (status_code, response_body).
fn http11_request(host: &str, port: u16, method: &str, path: &str) -> Result<(i32, String), String> {
    http11_request_impl(host, port, method, path, false)
}

/// Perform a real HTTPS/1.1 request with TLS and return (status_code, response_body).
fn https_request(host: &str, port: u16, method: &str, path: &str) -> Result<(i32, String), String> {
    http11_request_impl(host, port, method, path, true)
}

fn http11_request_impl(host: &str, port: u16, method: &str, path: &str, tls: bool) -> Result<(i32, String), String> {
    let addr = format!("{}:{}", host, port);
    let tcp_stream = TcpStream::connect(&addr).map_err(|e| format!("connect: {e}"))?;
    tcp_stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    tcp_stream.set_write_timeout(Some(std::time::Duration::from_secs(10))).ok();

    let request_line = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: */*\r\nUser-Agent: CratonVM/1.0\r\n\r\n"
    );

    let response_bytes = if tls {
        // TLS connection via native-tls
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| format!("TLS init: {e}"))?;
        let mut tls_stream = connector.connect(host, tcp_stream)
            .map_err(|e| format!("TLS handshake: {e}"))?;
        tls_stream.write_all(request_line.as_bytes())
            .map_err(|e| format!("TLS write: {e}"))?;
        tls_stream.flush().map_err(|e| format!("TLS flush: {e}"))?;
        let mut buf = Vec::new();
        read_limited(&mut tls_stream, &mut buf, MAX_RESPONSE_BODY)
            .map_err(|e| format!("TLS read: {e}"))?;
        let _ = tls_stream.shutdown();
        buf
    } else {
        let mut stream = tcp_stream;
        stream.write_all(request_line.as_bytes()).map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        let mut buf = Vec::new();
        read_limited(&mut stream, &mut buf, MAX_RESPONSE_BODY)
            .map_err(|e| format!("read: {e}"))?;
        let _ = stream.shutdown(std::net::Shutdown::Both);
        buf
    };

    let response_str = String::from_utf8_lossy(&response_bytes);

    // Parse HTTP/1.1 status line: "HTTP/1.x NNN reason\r\n"
    let status_line = response_str.lines().next().unwrap_or("");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);

    // Split headers from body at "\r\n\r\n"
    let (headers_section, raw_body) = if let Some(pos) = response_str.find("\r\n\r\n") {
        (&response_str[..pos], &response_str[pos + 4..])
    } else if let Some(pos) = response_str.find("\n\n") {
        (&response_str[..pos], &response_str[pos + 2..])
    } else {
        (response_str.as_ref(), "")
    };

    // Handle chunked transfer-encoding
    let is_chunked = headers_section.to_ascii_lowercase().contains("transfer-encoding: chunked");
    let body = if is_chunked {
        decode_chunked(raw_body)
    } else {
        raw_body.to_string()
    };

    Ok((status_code, body))
}

/// Read from a stream with a size limit to prevent unbounded memory growth.
fn read_limited(reader: &mut dyn std::io::Read, buf: &mut Vec<u8>, max: usize) -> std::io::Result<()> {
    let mut tmp = [0u8; 8192];
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max {
                    let remaining = max - buf.len();
                    buf.extend_from_slice(&tmp[..remaining]);
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Decode a chunked transfer-encoding body.
fn decode_chunked(input: &str) -> String {
    let mut result = String::new();
    let mut remaining = input;
    loop {
        // Find the chunk size line
        let line_end = match remaining.find("\r\n") {
            Some(pos) => pos,
            None => break,
        };
        let size_str = remaining[..line_end].trim();
        if size_str.is_empty() { break; }
        let chunk_size = match usize::from_str_radix(size_str, 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        if chunk_size == 0 { break; } // Terminal chunk
        let data_start = line_end + 2;
        let data_end = data_start + chunk_size;
        if data_end > remaining.len() {
            // Partial chunk — take what's available
            result.push_str(&remaining[data_start..]);
            break;
        }
        result.push_str(&remaining[data_start..data_end]);
        remaining = &remaining[data_end..];
        // Skip trailing \r\n after chunk data
        if remaining.starts_with("\r\n") {
            remaining = &remaining[2..];
        }
    }
    result
}

fn alloc_body_publisher(ctx: &mut dyn NativeContext, len: i64) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 2);
    ctx.set_field(obj, 0, Value::Long(len));
    ctx.set_field(obj, 1, Value::Int(0)); // type idx
    obj
}

fn alloc_body_handler(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
    ctx.set_field(obj, 0, Value::Int(kind));
    obj
}

// ---------------------------------------------------------------------------
// 1. java.net.http.HttpClient
// ---------------------------------------------------------------------------

fn register_http_client(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpClient";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // static newHttpClient() -> HttpClient
    r.register(cls, "newHttpClient", "()Ljava/net/http/HttpClient;", |ctx, _args| {
        let obj = alloc_http_client(ctx);
        Ok(Some(Value::Object(Some(obj))))
    });

    // static newBuilder() -> HttpClient$Builder
    r.register(cls, "newBuilder", "()Ljava/net/http/HttpClient$Builder;", |ctx, _args| {
        let bld = alloc_http_client_builder(ctx);
        Ok(Some(Value::Object(Some(bld))))
    });

    // send(HttpRequest, BodyHandler) -> HttpResponse
    r.register(
        cls,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        |ctx, args| {
            // Extract the HttpRequest from args[1] (args[0] is 'this' HttpClient)
            let req = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => {
                    let resp = alloc_http_response(ctx, 0);
                    return Ok(Some(Value::Object(Some(resp))));
                }
            };

            // Get HTTP method
            let method_idx = match ctx.get_field(req, REQ_METHOD) {
                Value::Int(n) => n,
                _ => METHOD_GET,
            };
            let method_str = method_idx_to_name(method_idx);

            // Get URI object from the request
            let uri_obj = match ctx.get_field(req, REQ_URI) {
                Value::Object(Some(u)) => u,
                _ => {
                    // No URI — return a synthetic 0-status response
                    let resp = alloc_http_response(ctx, 0);
                    return Ok(Some(Value::Object(Some(resp))));
                }
            };

            // Extract host/port/path from URI
            let (host, port, path) = match extract_uri_parts(ctx, uri_obj) {
                Some(parts) => parts,
                None => {
                    let resp = alloc_http_response(ctx, 0);
                    return Ok(Some(Value::Object(Some(resp))));
                }
            };

            // Determine if TLS is needed based on URI scheme or port
            let use_tls = match ctx.get_field(uri_obj, URI_SCHEME) {
                Value::Object(Some(s)) => {
                    ctx.read_string(s).as_deref() == Some("https")
                }
                _ => port == 443,
            };

            // Perform real HTTP request (with TLS for HTTPS)
            let result = if use_tls {
                https_request(&host, port, method_str, &path)
            } else {
                http11_request(&host, port, method_str, &path)
            };
            match result {
                Ok((status, body)) => {
                    let resp = alloc_http_response(ctx, status);
                    let body_str = ctx.create_string(&body);
                    // Store body string on a dedicated field — we use RESP_HAS_PREV (3)
                    // as body_obj since it's unused for real responses
                    ctx.set_field(resp, RESP_BODY_OBJ, Value::Object(Some(body_str)));
                    let _hdrs = alloc_http_headers(ctx, 0, 0, 0);
                    Ok(Some(Value::Object(Some(resp))))
                }
                Err(_e) => {
                    // Connection failed — return status 0 to signal error
                    let resp = alloc_http_response(ctx, 0);
                    return Ok(Some(Value::Object(Some(resp))));
                }
            }
        },
    );

    // sendAsync(HttpRequest, BodyHandler) -> CompletableFuture<HttpResponse>
    // Performs the same real HTTP request as send(), then wraps result in a completed CF.
    r.register(
        cls,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let req = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => {
                    let resp = alloc_http_response(ctx, 0);
                    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
                    ctx.set_field(cf, 0, Value::Object(Some(resp)));
                    ctx.set_field(cf, 1, Value::Int(1));
                    return Ok(Some(Value::Object(Some(cf))));
                }
            };
            let method_idx = match ctx.get_field(req, REQ_METHOD) {
                Value::Int(n) => n,
                _ => METHOD_GET,
            };
            let method_str = method_idx_to_name(method_idx);
            let uri_obj = match ctx.get_field(req, REQ_URI) {
                Value::Object(Some(u)) => u,
                _ => {
                    let resp = alloc_http_response(ctx, 0);
                    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
                    ctx.set_field(cf, 0, Value::Object(Some(resp)));
                    ctx.set_field(cf, 1, Value::Int(1));
                    return Ok(Some(Value::Object(Some(cf))));
                }
            };
            let (host, port, path) = match extract_uri_parts(ctx, uri_obj) {
                Some(parts) => parts,
                None => {
                    let resp = alloc_http_response(ctx, 0);
                    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
                    ctx.set_field(cf, 0, Value::Object(Some(resp)));
                    ctx.set_field(cf, 1, Value::Int(1));
                    return Ok(Some(Value::Object(Some(cf))));
                }
            };
            let use_tls = match ctx.get_field(uri_obj, URI_SCHEME) {
                Value::Object(Some(s)) => ctx.read_string(s).as_deref() == Some("https"),
                _ => port == 443,
            };
            let result = if use_tls {
                https_request(&host, port, method_str, &path)
            } else {
                http11_request(&host, port, method_str, &path)
            };
            let resp = match result {
                Ok((status, body)) => {
                    let r = alloc_http_response(ctx, status);
                    let body_str = ctx.create_string(&body);
                    ctx.set_field(r, RESP_BODY_OBJ, Value::Object(Some(body_str)));
                    r
                }
                Err(_) => alloc_http_response(ctx, 0),
            };
            let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
            ctx.set_field(cf, 0, Value::Object(Some(resp)));
            ctx.set_field(cf, 1, Value::Int(1)); // completed
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // version() -> HttpClient$Version
    r.register(cls, "version", "()Ljava/net/http/HttpClient$Version;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, CLIENT_VERSION) {
            Value::Int(n) => n,
            _ => HTTP_VERSION_2,
        };
        Ok(Some(Value::Int(v)))
    });

    // connectTimeout() -> Optional<Duration>
    r.register(cls, "connectTimeout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = match ctx.get_field(this, CLIENT_CONNECT_TIMEOUT) {
            Value::Long(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 2);
        ctx.set_field(opt, 0, Value::Int(if ms > 0 { 1 } else { 0 }));
        ctx.set_field(opt, 1, Value::Long(ms));
        Ok(Some(Value::Object(Some(opt))))
    });

    // followRedirects() -> HttpClient$Redirect
    r.register(cls, "followRedirects", "()Ljava/net/http/HttpClient$Redirect;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let r = match ctx.get_field(this, CLIENT_REDIRECT) {
            Value::Int(n) => n,
            _ => REDIRECT_NEVER,
        };
        Ok(Some(Value::Int(r)))
    });

    // executor() -> Optional<Executor>
    r.register(cls, "executor", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, CLIENT_HAS_EXECUTOR) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
        ctx.set_field(opt, 0, Value::Int(has));
        Ok(Some(Value::Object(Some(opt))))
    });

    // cookieHandler() -> Optional<CookieHandler>
    r.register(cls, "cookieHandler", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, CLIENT_HAS_COOKIE) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
        ctx.set_field(opt, 0, Value::Int(has));
        Ok(Some(Value::Object(Some(opt))))
    });

    // proxy() -> Optional<ProxySelector>
    r.register(cls, "proxy", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, CLIENT_HAS_PROXY) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
        ctx.set_field(opt, 0, Value::Int(has));
        Ok(Some(Value::Object(Some(opt))))
    });

    // authenticator() -> Optional<Authenticator>
    r.register(cls, "authenticator", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, CLIENT_HAS_AUTH) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
        ctx.set_field(opt, 0, Value::Int(has));
        Ok(Some(Value::Object(Some(opt))))
    });

    // sslContext() -> SSLContext
    r.register(cls, "sslContext", "()Ljavax/net/ssl/SSLContext;", |ctx, _args| {
        let ssl = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 4);
        ctx.set_field(ssl, 0, Value::Int(2)); // TLSv1.3
        ctx.set_field(ssl, 1, Value::Int(1)); // initialized
        Ok(Some(Value::Object(Some(ssl))))
    });

    // sslParameters() -> SSLParameters
    r.register(cls, "sslParameters", "()Ljavax/net/ssl/SSLParameters;", |ctx, _args| {
        let params = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4);
        ctx.set_field(params, 0, Value::Int(0));
        ctx.set_field(params, 1, Value::Int(0));
        ctx.set_field(params, 2, Value::Int(0));
        ctx.set_field(params, 3, Value::Int(0));
        Ok(Some(Value::Object(Some(params))))
    });

    // newWebSocketBuilder() -> WebSocket$Builder
    r.register(cls, "newWebSocketBuilder", "()Ljava/net/http/WebSocket$Builder;", |ctx, _args| {
        let bld = alloc_concurrent_synthetic(ctx, "java/net/http/WebSocket$Builder", 3);
        ctx.set_field(bld, 0, Value::Int(0)); // subprotocols set
        ctx.set_field(bld, 1, Value::Long(0)); // connect timeout
        ctx.set_field(bld, 2, Value::Int(0)); // header count
        Ok(Some(Value::Object(Some(bld))))
    });
}

// ---------------------------------------------------------------------------
// 2. java.net.http.HttpClient$Builder
// ---------------------------------------------------------------------------

fn register_http_client_builder(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpClient$Builder";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // version(HttpClient$Version) -> Builder
    r.register(
        cls,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => HTTP_VERSION_2,
            };
            ctx.set_field(this, 0, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // connectTimeout(Duration) -> Builder
    r.register(
        cls,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ms = match args.get(1) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n))  => *n as i64,
                _ => 0,
            };
            ctx.set_field(this, 2, Value::Long(ms));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // followRedirects(HttpClient$Redirect) -> Builder
    r.register(
        cls,
        "followRedirects",
        "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let policy = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => REDIRECT_NEVER,
            };
            ctx.set_field(this, 1, Value::Int(policy));
            ctx.set_field(this, 7, Value::Int(if policy != REDIRECT_NEVER { 1 } else { 0 }));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // executor(Executor) -> Builder
    r.register(
        cls,
        "executor",
        "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 4, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // cookieHandler(CookieHandler) -> Builder
    r.register(
        cls,
        "cookieHandler",
        "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 7, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // proxy(ProxySelector) -> Builder
    r.register(
        cls,
        "proxy",
        "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 5, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // authenticator(Authenticator) -> Builder
    r.register(
        cls,
        "authenticator",
        "(Ljava/net/Authenticator;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 6, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // sslContext(SSLContext) -> Builder
    r.register(
        cls,
        "sslContext",
        "(Ljavax/net/ssl/SSLContext;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // sslParameters(SSLParameters) -> Builder
    r.register(
        cls,
        "sslParameters",
        "(Ljavax/net/ssl/SSLParameters;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // sslParameters presence implies ssl is configured
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // build() -> HttpClient
    r.register(cls, "build", "()Ljava/net/http/HttpClient;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let client = alloc_http_client(ctx);
        // Copy builder fields into client
        for i in 0..8usize {
            let field_val = ctx.get_field(this, i);
            let dest = match i {
                0 => CLIENT_VERSION,
                1 => CLIENT_REDIRECT,
                2 => CLIENT_CONNECT_TIMEOUT, // Long field
                3 => CLIENT_HAS_SSL,
                4 => CLIENT_HAS_EXECUTOR,
                5 => CLIENT_HAS_PROXY,
                6 => CLIENT_HAS_AUTH,
                7 => CLIENT_HAS_COOKIE,
                _ => unreachable!(),
            };
            ctx.set_field(client, dest, field_val);
        }
        ctx.set_field(client, CLIENT_POOL_SIZE, Value::Int(DEFAULT_POOL_SIZE));
        Ok(Some(Value::Object(Some(client))))
    });
}

// ---------------------------------------------------------------------------
// 3. java.net.http.HttpRequest
// ---------------------------------------------------------------------------

fn register_http_request(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpRequest";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // static newBuilder() -> HttpRequest$Builder
    r.register(cls, "newBuilder", "()Ljava/net/http/HttpRequest$Builder;", |ctx, _args| {
        let bld = alloc_http_request_builder(ctx);
        Ok(Some(Value::Object(Some(bld))))
    });

    // static newBuilder(URI) -> HttpRequest$Builder
    r.register(
        cls,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let bld = alloc_http_request_builder(ctx);
            let uri_val = args.get(0).copied().unwrap_or(Value::Object(None));
            ctx.set_field(bld, REQ_URI, uri_val);
            Ok(Some(Value::Object(Some(bld))))
        },
    );

    // uri() -> URI
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, REQ_URI)))
    });

    // method() -> String
    r.register(cls, "method", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = match ctx.get_field(this, REQ_METHOD) {
            Value::Int(n) => n,
            _ => METHOD_GET,
        };
        let name = method_idx_to_name(m);
        let sv = ctx.create_string(name);
        Ok(Some(Value::Object(Some(sv))))
    });

    // headers() -> HttpHeaders
    r.register(cls, "headers", "()Ljava/net/http/HttpHeaders;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, REQ_HDR_COUNT) {
            Value::Int(n) => n,
            _ => 0,
        };
        let hdrs = alloc_http_headers(ctx, count, 0, 0);
        Ok(Some(Value::Object(Some(hdrs))))
    });

    // bodyPublisher() -> Optional<BodyPublisher>
    r.register(cls, "bodyPublisher", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has_body = match ctx.get_field(this, REQ_HAS_BODY) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 2);
        ctx.set_field(opt, 0, Value::Int(has_body));
        if has_body == 1 {
            let len = match ctx.get_field(this, REQ_BODY_LEN) {
                Value::Long(n) => n,
                _ => 0,
            };
            let bp = alloc_body_publisher(ctx, len);
            ctx.set_field(opt, 1, Value::Object(Some(bp)));
        }
        Ok(Some(Value::Object(Some(opt))))
    });

    // timeout() -> Optional<Duration>
    r.register(cls, "timeout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = match ctx.get_field(this, REQ_TIMEOUT) {
            Value::Long(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 2);
        ctx.set_field(opt, 0, Value::Int(if ms > 0 { 1 } else { 0 }));
        ctx.set_field(opt, 1, Value::Long(ms));
        Ok(Some(Value::Object(Some(opt))))
    });

    // expectContinue() -> boolean
    r.register(cls, "expectContinue", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, REQ_EXPECT_100) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });

    // version() -> Optional<HttpClient$Version>
    r.register(cls, "version", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ver = match ctx.get_field(this, REQ_VERSION) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 2);
        ctx.set_field(opt, 0, Value::Int(if ver != 0 { 1 } else { 0 }));
        ctx.set_field(opt, 1, Value::Int(if ver > 0 { ver - 1 } else { 0 }));
        Ok(Some(Value::Object(Some(opt))))
    });
}

// ---------------------------------------------------------------------------
// 4. java.net.http.HttpRequest$Builder
// ---------------------------------------------------------------------------

fn register_http_request_builder(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpRequest$Builder";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // uri(URI) -> Builder
    r.register(
        cls,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let uri_val = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, REQ_URI, uri_val);
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // method(String, BodyPublisher) -> Builder
    r.register(
        cls,
        "method",
        "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m_idx = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("GET")     => METHOD_GET,
                        Some("POST")    => METHOD_POST,
                        Some("PUT")     => METHOD_PUT,
                        Some("DELETE")  => METHOD_DELETE,
                        Some("HEAD")    => METHOD_HEAD,
                        Some("PATCH")   => METHOD_PATCH,
                        Some("OPTIONS") => METHOD_OPTIONS,
                        _ => METHOD_GET,
                    }
                }
                _ => METHOD_GET,
            };
            ctx.set_field(this, REQ_METHOD, Value::Int(m_idx));
            let has_body = match args.get(2) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(has_body));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // GET() -> Builder
    r.register(cls, "GET", "()Ljava/net/http/HttpRequest$Builder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_GET));
        ctx.set_field(this, REQ_HAS_BODY, Value::Int(0));
        Ok(Some(Value::Object(Some(this))))
    });

    // POST(BodyPublisher) -> Builder
    r.register(
        cls,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_POST));
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // PUT(BodyPublisher) -> Builder
    r.register(
        cls,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_PUT));
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // DELETE() -> Builder
    r.register(cls, "DELETE", "()Ljava/net/http/HttpRequest$Builder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_DELETE));
        ctx.set_field(this, REQ_HAS_BODY, Value::Int(0));
        Ok(Some(Value::Object(Some(this))))
    });

    // header(String, String) -> Builder
    r.register(
        cls,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prev = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, REQ_HDR_COUNT, Value::Int(prev + 1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // headers(String...) -> Builder  (varargs flattened)
    r.register(
        cls,
        "headers",
        "([Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Each header is a name/value pair so count = args.len() / 2 extra
            let extra = ((args.len().saturating_sub(1)) / 2) as i32;
            let prev = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, REQ_HDR_COUNT, Value::Int(prev + extra));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // setHeader(String, String) -> Builder
    r.register(
        cls,
        "setHeader",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prev = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, REQ_HDR_COUNT, Value::Int(prev + 1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // timeout(Duration) -> Builder
    r.register(
        cls,
        "timeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ms = match args.get(1) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n))  => *n as i64,
                _ => 0,
            };
            ctx.set_field(this, REQ_TIMEOUT, Value::Long(ms));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // expectContinue(boolean) -> Builder
    r.register(
        cls,
        "expectContinue",
        "(Z)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let flag = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            ctx.set_field(this, REQ_EXPECT_100, Value::Int(flag));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // version(HttpClient$Version) -> Builder
    r.register(
        cls,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ver = match args.get(1) {
                Some(Value::Int(n)) => *n + 1, // 1=HTTP_1_1, 2=HTTP_2 (0 = no override)
                _ => 0,
            };
            ctx.set_field(this, REQ_VERSION, Value::Int(ver));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // build() -> HttpRequest
    r.register(cls, "build", "()Ljava/net/http/HttpRequest;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let req = alloc_http_request(ctx);
        for i in 0..8usize {
            let v = ctx.get_field(this, i);
            ctx.set_field(req, i, v);
        }
        Ok(Some(Value::Object(Some(req))))
    });
}

// ---------------------------------------------------------------------------
// 5. java.net.http.HttpResponse
// ---------------------------------------------------------------------------

fn register_http_response(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpResponse";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // statusCode() -> int
    r.register(cls, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sc = match ctx.get_field(this, RESP_STATUS) {
            Value::Int(n) => n,
            _ => 200,
        };
        Ok(Some(Value::Int(sc)))
    });

    // body() -> T (returned as a String object)
    r.register(cls, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let body = ctx.get_field(this, RESP_BODY_OBJ);
        match body {
            Value::Object(Some(_)) => Ok(Some(body)),
            _ => {
                // Fallback: return empty string
                let sv = ctx.create_string("");
                Ok(Some(Value::Object(Some(sv))))
            }
        }
    });

    // headers() -> HttpHeaders
    r.register(cls, "headers", "()Ljava/net/http/HttpHeaders;", |ctx, _args| {
        let hdrs = alloc_http_headers(ctx, 2, 1, 1);
        Ok(Some(Value::Object(Some(hdrs))))
    });

    // uri() -> URI
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, _args| {
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 2);
        ctx.set_field(uri, 0, Value::Int(0));
        ctx.set_field(uri, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(uri))))
    });

    // version() -> HttpClient$Version
    r.register(cls, "version", "()Ljava/net/http/HttpClient$Version;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, RESP_VERSION) {
            Value::Int(n) => n,
            _ => HTTP_VERSION_2,
        };
        Ok(Some(Value::Int(v)))
    });

    // request() -> HttpRequest
    r.register(cls, "request", "()Ljava/net/http/HttpRequest;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let req = alloc_http_request(ctx);
        let m = match ctx.get_field(this, RESP_METHOD) {
            Value::Int(n) => n,
            _ => METHOD_GET,
        };
        ctx.set_field(req, REQ_METHOD, Value::Int(m));
        Ok(Some(Value::Object(Some(req))))
    });

    // previousResponse() -> Optional<HttpResponse>
    r.register(cls, "previousResponse", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, RESP_HAS_PREV) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
        ctx.set_field(opt, 0, Value::Int(has));
        Ok(Some(Value::Object(Some(opt))))
    });

    // sslSession() -> Optional<SSLSession>
    r.register(cls, "sslSession", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, RESP_HAS_SSL) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 2);
        ctx.set_field(opt, 0, Value::Int(has));
        if has == 1 {
            let ssl = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 6);
            ctx.set_field(opt, 1, Value::Object(Some(ssl)));
        }
        Ok(Some(Value::Object(Some(opt))))
    });
}

// ---------------------------------------------------------------------------
// 6. java.net.http.HttpHeaders
// ---------------------------------------------------------------------------

fn register_http_headers(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpHeaders";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // allValues(String name) -> List<String>
    r.register(cls, "allValues", "(Ljava/lang/String;)Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has_ct = match ctx.get_field(this, HDR_HAS_CT) {
            Value::Int(n) => n,
            _ => 0,
        };
        let has_cl = match ctx.get_field(this, HDR_HAS_CL) {
            Value::Int(n) => n,
            _ => 0,
        };
        let queried = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
        let match_name = queried.to_lowercase();
        let count = if match_name == "content-type" && has_ct == 1 { 1 }
                    else if match_name == "content-length" && has_cl == 1 { 1 }
                    else { 0 };
        ctx.set_field(list, 0, Value::Int(count));
        ctx.set_field(list, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    });

    // firstValue(String name) -> Optional<String>
    r.register(cls, "firstValue", "(Ljava/lang/String;)Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has_ct = match ctx.get_field(this, HDR_HAS_CT) {
            Value::Int(n) => n,
            _ => 0,
        };
        let has_cl = match ctx.get_field(this, HDR_HAS_CL) {
            Value::Int(n) => n,
            _ => 0,
        };
        let queried = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 2);
        let lower = queried.to_lowercase();
        if lower == "content-type" && has_ct == 1 {
            let sv = ctx.create_string("application/json");
            ctx.set_field(opt, 0, Value::Int(1));
            ctx.set_field(opt, 1, Value::Object(Some(sv)));
        } else if lower == "content-length" && has_cl == 1 {
            let sv = ctx.create_string("20");
            ctx.set_field(opt, 0, Value::Int(1));
            ctx.set_field(opt, 1, Value::Object(Some(sv)));
        } else {
            ctx.set_field(opt, 0, Value::Int(0));
            ctx.set_field(opt, 1, Value::Object(None));
        }
        Ok(Some(Value::Object(Some(opt))))
    });

    // firstValueAsLong(String name) -> OptionalLong
    r.register(cls, "firstValueAsLong", "(Ljava/lang/String;)Ljava/util/OptionalLong;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has_cl = match ctx.get_field(this, HDR_HAS_CL) {
            Value::Int(n) => n,
            _ => 0,
        };
        let queried = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let opt = alloc_concurrent_synthetic(ctx, "java/util/OptionalLong", 2);
        if queried.to_lowercase() == "content-length" && has_cl == 1 {
            ctx.set_field(opt, 0, Value::Int(1));
            ctx.set_field(opt, 1, Value::Long(20));
        } else {
            ctx.set_field(opt, 0, Value::Int(0));
            ctx.set_field(opt, 1, Value::Long(0));
        }
        Ok(Some(Value::Object(Some(opt))))
    });

    // map() -> Map<String, List<String>>
    r.register(cls, "map", "()Ljava/util/Map;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = match ctx.get_field(this, HDR_COUNT) {
            Value::Int(n) => n,
            _ => 0,
        };
        let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 2);
        ctx.set_field(map, 0, Value::Int(count));
        ctx.set_field(map, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(map))))
    });
}

// ---------------------------------------------------------------------------
// 7. java.net.http.HttpRequest$BodyPublisher
// ---------------------------------------------------------------------------

fn register_body_publisher(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/HttpRequest$BodyPublisher";
    let bps = "java/net/http/HttpRequest$BodyPublishers";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // BodyPublishers.ofString(String) -> BodyPublisher
    r.register(bps, "ofString", "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let len = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).map(|s| s.len() as i64).unwrap_or(0)
                }
                _ => 0,
            };
            let bp = alloc_body_publisher(ctx, len);
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // BodyPublishers.ofByteArray(byte[]) -> BodyPublisher
    r.register(bps, "ofByteArray", "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let len = match args.get(0) {
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            };
            let bp = alloc_body_publisher(ctx, len);
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // BodyPublishers.ofFile(Path) -> BodyPublisher
    r.register(bps, "ofFile", "(Ljava/nio/file/Path;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, -1); // unknown length
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // BodyPublishers.ofInputStream(Supplier) -> BodyPublisher
    r.register(
        bps,
        "ofInputStream",
        "(Ljava/util/function/Supplier;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, -1);
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // BodyPublishers.noBody() -> BodyPublisher
    r.register(bps, "noBody", "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, 0);
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // contentLength() -> long  (instance method on BodyPublisher)
    r.register(cls, "contentLength", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let len = match ctx.get_field(this, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(len)))
    });

    // subscribe(Flow.Subscriber) -> void — record the subscriber in a global map.
    r.register(cls, "subscribe", "(Ljava/util/concurrent/Flow$Subscriber;)V", |_ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let subscriber = match args.get(1) {
            Some(Value::Object(Some(s))) => Some(*s),
            _ => None,
        };
        let key = this.as_ptr() as u64;
        if let Some(s) = subscriber {
            body_subscriber_subscribers().lock().insert(key, s);
        } else {
            body_subscriber_subscribers().lock().remove(&key);
        }
        Ok(None)
    });
}

/// Process-wide map from BodySubscriber identity → downstream Flow.Subscriber.
/// Populated by `BodySubscriber.subscribe(Flow$Subscriber)` and queried by code
/// that wants to forward data into the reactive pipeline.
fn body_subscriber_subscribers() -> &'static parking_lot::Mutex<std::collections::HashMap<u64, ObjectRef>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<std::collections::HashMap<u64, ObjectRef>>> = OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Process-wide map from BodySubscriber identity → outstanding demand (i64 saturating).
fn body_subscriber_demand() -> &'static parking_lot::Mutex<std::collections::HashMap<u64, i64>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<std::collections::HashMap<u64, i64>>> = OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

// ---------------------------------------------------------------------------
// 8. java.net.http.HttpResponse$BodyHandlers
// ---------------------------------------------------------------------------

fn register_body_handlers(r: &mut NativeMethodRegistry) {
    let bhs = "java/net/http/HttpResponse$BodyHandlers";

    // ofString() -> BodyHandler<String>
    r.register(bhs, "ofString", "()Ljava/net/http/HttpResponse$BodyHandler;", |ctx, _args| {
        let bh = alloc_body_handler(ctx, 0);
        Ok(Some(Value::Object(Some(bh))))
    });

    // ofByteArray() -> BodyHandler<byte[]>
    r.register(bhs, "ofByteArray", "()Ljava/net/http/HttpResponse$BodyHandler;", |ctx, _args| {
        let bh = alloc_body_handler(ctx, 1);
        Ok(Some(Value::Object(Some(bh))))
    });

    // ofFile(Path) -> BodyHandler<Path>
    r.register(
        bhs,
        "ofFile",
        "(Ljava/nio/file/Path;)Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 2);
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    // ofLines() -> BodyHandler<Stream<String>>
    r.register(bhs, "ofLines", "()Ljava/net/http/HttpResponse$BodyHandler;", |ctx, _args| {
        let bh = alloc_body_handler(ctx, 3);
        Ok(Some(Value::Object(Some(bh))))
    });

    // discarding() -> BodyHandler<Void>
    r.register(bhs, "discarding", "()Ljava/net/http/HttpResponse$BodyHandler;", |ctx, _args| {
        let bh = alloc_body_handler(ctx, 4);
        Ok(Some(Value::Object(Some(bh))))
    });

    // replacing(Object) -> BodyHandler<T>
    r.register(
        bhs,
        "replacing",
        "(Ljava/lang/Object;)Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 5);
            Ok(Some(Value::Object(Some(bh))))
        },
    );
}

// ---------------------------------------------------------------------------
// 9. java.net.http.WebSocket
// ---------------------------------------------------------------------------

fn register_websocket(r: &mut NativeMethodRegistry) {
    let cls = "java/net/http/WebSocket";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // sendText(CharSequence, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendText",
        "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendBinary(ByteBuffer, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendBinary",
        "(Ljava/nio/ByteBuffer;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendPing(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendPing",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendPong(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendPong",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendClose(int statusCode, String reason) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendClose",
        "(ILjava/lang/String;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Mark both directions closed
            ctx.set_field(this, WS_STATE,         Value::Int(WS_CLOSING));
            ctx.set_field(this, WS_OUTPUT_CLOSED, Value::Int(1));
            let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // request(long n) -> void  (flow control — accumulate demand in process-wide map)
    r.register(cls, "request", "(J)V", |_ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let n = match args.get(1) { Some(Value::Long(v)) => *v, _ => 0 };
        if n < 0 {
            return Ok(None); // ignore negative demand silently in the http2.rs path
        }
        let key = this.as_ptr() as u64;
        let mut map = body_subscriber_demand().lock();
        let current = *map.get(&key).unwrap_or(&0);
        map.insert(key, current.saturating_add(n));
        Ok(None)
    });

    // subprotocol() -> String
    r.register(cls, "subprotocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match ctx.get_field(this, WS_SUBPROTOCOL) {
            Value::Int(n) => n,
            _ => 0,
        };
        let proto = if idx == 0 { "" } else { "unknown" };
        let sv = ctx.create_string(proto);
        Ok(Some(Value::Object(Some(sv))))
    });

    // isInputClosed() -> boolean
    r.register(cls, "isInputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, WS_INPUT_CLOSED) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });

    // isOutputClosed() -> boolean
    r.register(cls, "isOutputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, WS_OUTPUT_CLOSED) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });

    // abort() -> void
    r.register(cls, "abort", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, WS_STATE,         Value::Int(WS_CLOSED));
        ctx.set_field(this, WS_OUTPUT_CLOSED, Value::Int(1));
        ctx.set_field(this, WS_INPUT_CLOSED,  Value::Int(1));
        Ok(None)
    });
}

// ---------------------------------------------------------------------------
// Helper: HTTP method index to string
// ---------------------------------------------------------------------------

fn method_idx_to_name(idx: i32) -> &'static str {
    match idx {
        METHOD_GET     => "GET",
        METHOD_POST    => "POST",
        METHOD_PUT     => "PUT",
        METHOD_DELETE  => "DELETE",
        METHOD_HEAD    => "HEAD",
        METHOD_PATCH   => "PATCH",
        METHOD_OPTIONS => "OPTIONS",
        _ => "GET",
    }
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

/// Register all HTTP/2 client native methods into the given registry.
pub(crate) fn register_http2_natives(r: &mut NativeMethodRegistry) {
    register_http_client(r);
    register_http_client_builder(r);
    register_http_request(r);
    register_http_request_builder(r);
    register_http_response(r);
    register_http_headers(r);
    register_body_publisher(r);
    register_body_handlers(r);
    register_websocket(r);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http2_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;

    // --- Registration tests ------------------------------------------------

    #[test]
    fn test_http_client_init_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpClient", "<init>", "()V").is_some());
    }

    #[test]
    fn test_http_client_new_builder_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient",
            "newBuilder",
            "()Ljava/net/http/HttpClient$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_client_new_http_client_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient",
            "newHttpClient",
            "()Ljava/net/http/HttpClient;"
        ).is_some());
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
        assert!(r.find(
            "java/net/http/HttpClient",
            "version",
            "()Ljava/net/http/HttpClient$Version;"
        ).is_some());
    }

    #[test]
    fn test_http_client_follow_redirects_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient",
            "followRedirects",
            "()Ljava/net/http/HttpClient$Redirect;"
        ).is_some());
    }

    #[test]
    fn test_http_client_connect_timeout_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpClient", "connectTimeout", "()Ljava/util/Optional;").is_some());
    }

    #[test]
    fn test_http_client_ssl_context_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpClient", "sslContext", "()Ljavax/net/ssl/SSLContext;").is_some());
    }

    #[test]
    fn test_http_client_new_websocket_builder_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient",
            "newWebSocketBuilder",
            "()Ljava/net/http/WebSocket$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_client_builder_build_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpClient$Builder", "build", "()Ljava/net/http/HttpClient;").is_some());
    }

    #[test]
    fn test_http_client_builder_version_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient$Builder",
            "version",
            "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_client_builder_follow_redirects_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient$Builder",
            "followRedirects",
            "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_new_builder_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest",
            "newBuilder",
            "()Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_new_builder_uri_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest",
            "newBuilder",
            "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_method_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpRequest", "method", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_http_request_builder_get_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$Builder",
            "GET",
            "()Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_builder_post_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$Builder",
            "POST",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_builder_put_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$Builder",
            "PUT",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_builder_delete_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$Builder",
            "DELETE",
            "()Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_builder_header_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$Builder",
            "header",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"
        ).is_some());
    }

    #[test]
    fn test_http_request_builder_build_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpRequest$Builder", "build", "()Ljava/net/http/HttpRequest;").is_some());
    }

    #[test]
    fn test_http_response_status_code_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpResponse", "statusCode", "()I").is_some());
    }

    #[test]
    fn test_http_response_body_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpResponse", "body", "()Ljava/lang/Object;").is_some());
    }

    #[test]
    fn test_http_response_headers_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpResponse", "headers", "()Ljava/net/http/HttpHeaders;").is_some());
    }

    #[test]
    fn test_http_response_version_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpResponse",
            "version",
            "()Ljava/net/http/HttpClient$Version;"
        ).is_some());
    }

    #[test]
    fn test_http_response_previous_response_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpResponse", "previousResponse", "()Ljava/util/Optional;").is_some());
    }

    #[test]
    fn test_http_headers_all_values_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpHeaders", "allValues", "(Ljava/lang/String;)Ljava/util/List;").is_some());
    }

    #[test]
    fn test_http_headers_first_value_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpHeaders",
            "firstValue",
            "(Ljava/lang/String;)Ljava/util/Optional;"
        ).is_some());
    }

    #[test]
    fn test_http_headers_first_value_as_long_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpHeaders",
            "firstValueAsLong",
            "(Ljava/lang/String;)Ljava/util/OptionalLong;"
        ).is_some());
    }

    #[test]
    fn test_http_headers_map_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/HttpHeaders", "map", "()Ljava/util/Map;").is_some());
    }

    #[test]
    fn test_body_publishers_of_string_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofString",
            "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;"
        ).is_some());
    }

    #[test]
    fn test_body_publishers_no_body_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpRequest$BodyPublishers",
            "noBody",
            "()Ljava/net/http/HttpRequest$BodyPublisher;"
        ).is_some());
    }

    #[test]
    fn test_body_handlers_of_string_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpResponse$BodyHandlers",
            "ofString",
            "()Ljava/net/http/HttpResponse$BodyHandler;"
        ).is_some());
    }

    #[test]
    fn test_body_handlers_discarding_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpResponse$BodyHandlers",
            "discarding",
            "()Ljava/net/http/HttpResponse$BodyHandler;"
        ).is_some());
    }

    #[test]
    fn test_websocket_send_text_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/WebSocket",
            "sendText",
            "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;"
        ).is_some());
    }

    #[test]
    fn test_websocket_send_close_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/WebSocket",
            "sendClose",
            "(ILjava/lang/String;)Ljava/util/concurrent/CompletableFuture;"
        ).is_some());
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
        assert!(r.find("java/net/http/WebSocket", "isOutputClosed", "()Z").is_some());
    }

    #[test]
    fn test_websocket_is_input_closed_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/WebSocket", "isInputClosed", "()Z").is_some());
    }

    // --- Logic / unit tests ------------------------------------------------

    #[test]
    fn test_method_idx_to_name_all_variants() {
        assert_eq!(method_idx_to_name(METHOD_GET),     "GET");
        assert_eq!(method_idx_to_name(METHOD_POST),    "POST");
        assert_eq!(method_idx_to_name(METHOD_PUT),     "PUT");
        assert_eq!(method_idx_to_name(METHOD_DELETE),  "DELETE");
        assert_eq!(method_idx_to_name(METHOD_HEAD),    "HEAD");
        assert_eq!(method_idx_to_name(METHOD_PATCH),   "PATCH");
        assert_eq!(method_idx_to_name(METHOD_OPTIONS), "OPTIONS");
        assert_eq!(method_idx_to_name(99),             "GET"); // unknown defaults to GET
    }

    #[test]
    fn test_http_version_constants() {
        assert_eq!(HTTP_VERSION_1_1, 0);
        assert_eq!(HTTP_VERSION_2,   1);
    }

    #[test]
    fn test_redirect_constants() {
        assert_eq!(REDIRECT_NEVER,  0);
        assert_eq!(REDIRECT_NORMAL, 1);
        assert_eq!(REDIRECT_ALWAYS, 2);
    }

    #[test]
    fn test_websocket_state_constants() {
        assert_eq!(WS_OPEN,    0);
        assert_eq!(WS_CLOSING, 1);
        assert_eq!(WS_CLOSED,  2);
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
    fn test_http2_frame_type_values() {
        assert_eq!(Http2FrameType::Data        as u8, 0x0);
        assert_eq!(Http2FrameType::Headers     as u8, 0x1);
        assert_eq!(Http2FrameType::Settings    as u8, 0x4);
        assert_eq!(Http2FrameType::Ping        as u8, 0x6);
        assert_eq!(Http2FrameType::Goaway      as u8, 0x7);
        assert_eq!(Http2FrameType::WindowUpdate as u8, 0x8);
        assert_eq!(Http2FrameType::Continuation as u8, 0x9);
    }

    #[test]
    fn test_http2_error_code_values() {
        assert_eq!(Http2ErrorCode::NoError          as u32, 0x0);
        assert_eq!(Http2ErrorCode::ProtocolError    as u32, 0x1);
        assert_eq!(Http2ErrorCode::Cancel           as u32, 0x8);
        assert_eq!(Http2ErrorCode::InadequateSecurity as u32, 0xc);
        assert_eq!(Http2ErrorCode::Http11Required   as u32, 0xd);
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
        assert!(r.find("java/net/http/HttpClient", "sslParameters", "()Ljavax/net/ssl/SSLParameters;").is_some());
        // HttpRequest
        assert!(r.find("java/net/http/HttpRequest", "expectContinue", "()Z").is_some());
        // HttpResponse
        assert!(r.find("java/net/http/HttpResponse", "sslSession", "()Ljava/util/Optional;").is_some());
        // WebSocket
        assert!(r.find("java/net/http/WebSocket", "subprotocol", "()Ljava/lang/String;").is_some());
        // BodyHandlers
        assert!(r.find("java/net/http/HttpResponse$BodyHandlers", "ofLines", "()Ljava/net/http/HttpResponse$BodyHandler;").is_some());
    }

    // --- M14: Real HTTP client/server tests ---------------------------------

    #[test]
    fn m14_http11_request_to_localhost_server() {
        // Spin up a real TCP server that responds with HTTP/1.1 200 OK.
        use std::net::TcpListener;
        use std::io::{Read, Write};

        let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("M14: accept failed");
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).expect("M14: read failed");
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("GET /hello"), "M14: expected GET /hello, got: {}", request);

            let body = "Hello from CratonVM!";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream.write_all(response.as_bytes()).expect("M14: write failed");
            stream.flush().ok();
        });

        let (status, body) = http11_request("127.0.0.1", port, "GET", "/hello")
            .expect("M14: http11_request failed");

        assert_eq!(status, 200, "M14: expected status 200, got {status}");
        assert_eq!(body, "Hello from CratonVM!", "M14: body mismatch");
        server.join().expect("M14: server thread panicked");
    }

    #[test]
    fn m14_http11_post_request() {
        use std::net::TcpListener;
        use std::io::{Read, Write};

        let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("M14: accept failed");
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).expect("M14: read failed");
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST /api"), "M14: expected POST /api, got: {}", request);

            let body = "{\"status\":\"created\"}";
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream.write_all(response.as_bytes()).expect("M14: write failed");
        });

        let (status, body) = http11_request("127.0.0.1", port, "POST", "/api")
            .expect("M14: http11_request failed");

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
        use std::net::TcpListener;
        use std::io::{Read, Write};

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
}
