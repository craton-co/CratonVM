// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.6 — legacy `sun.net.www.protocol.http(s).HttpURLConnection` natives.
//!
//! The JDK's HTTP/1.1 stack predates `java.net.http.HttpClient` and exposes
//! a more stateful, blocking API where each connection object holds its own
//! socket, request buffer, and parsed response. Real apps still use it (e.g.
//! `URL.openConnection().getInputStream()` in WildFly's bootstrap), so we
//! wire it through to the same TCP+TLS plumbing as `http_client.rs` but with
//! the older semantics:
//!
//!   * a per-instance native registry keyed by `conn_id` (stored in field 0
//!     of the Java HttpURLConnection synthetic object);
//!   * `connect()` opens the socket, drives TLS for the `https` variant,
//!     sends the request line, and parses the response head;
//!   * `getInputStream()` returns a `ByteArrayInputStream` over the response
//!     body — this matches what the JDK actually does once the body is fully
//!     buffered (it's not a streaming abstraction in our world);
//!   * `getOutputStream()` returns a synthetic `ByteArrayOutputStream` that
//!     `connect()` will pick up the body from on the first read.
//!
//! This module owns ONLY the connection-level natives. URL stream-handler
//! dispatch (`URL.openConnection`) lives in `net_phase_e::register_re4_url_http`
//! and is unchanged.
//!
//! No stubs: every code path performs real I/O against the network or rejects
//! the call with a typed `IOException`. We never fabricate canned 200s.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg};

// ---------------------------------------------------------------------------
// Synthetic field layout for sun.net.www.protocol.http.HttpURLConnection
// ---------------------------------------------------------------------------

const HUC_CONN_ID: usize = 0; // i32 native registry key, -1 = unconnected
const HUC_URL_STR: usize = 1; // String form of the URL
const HUC_METHOD: usize = 2; // String, default "GET"
const HUC_REQ_HEADERS: usize = 3; // String[] of "key: value" lines
const HUC_REQ_BODY_STREAM: usize = 4; // ByteArrayOutputStream object or null
const HUC_DO_INPUT: usize = 5;
const HUC_DO_OUTPUT: usize = 6;
const HUC_CONNECTED: usize = 7;
const HUC_DISCONNECTED: usize = 8;
const HUC_INSTANCE_FOLLOW_REDIRECTS: usize = 9;
const HUC_CONNECT_TIMEOUT: usize = 10;
const HUC_READ_TIMEOUT: usize = 11;

const MAX_RESPONSE_BODY: usize = 16 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Native connection registry
// ---------------------------------------------------------------------------

struct ConnState {
    status: i32,
    response_body: Vec<u8>,
    response_headers: Vec<(String, String)>,
    /// Once `getInputStream` has been called and the bytes drained, we keep
    /// the body here so subsequent `read()` calls return the same data.
    body_consumed: bool,
}

struct ConnRegistry {
    next_id: i32,
    conns: HashMap<i32, ConnState>,
}

impl ConnRegistry {
    fn new() -> Self {
        Self { next_id: 1, conns: HashMap::new() }
    }
    fn allocate(&mut self, state: ConnState) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id <= 0 {
            self.next_id = 1; // wrap back into positive id space
        }
        self.conns.insert(id, state);
        id
    }
    fn get(&self, id: i32) -> Option<&ConnState> {
        self.conns.get(&id)
    }
    fn remove(&mut self, id: i32) {
        self.conns.remove(&id);
    }
}

fn registry() -> &'static Mutex<ConnRegistry> {
    static R: OnceLock<Mutex<ConnRegistry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(ConnRegistry::new()))
}

// ---------------------------------------------------------------------------
// rustls config — system trust store, no ALPN (legacy HTTP/1.1 only)
// ---------------------------------------------------------------------------

fn shared_legacy_config() -> Arc<ClientConfig> {
    static C: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    C.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        let result = rustls_native_certs::load_native_certs();
        for c in result.certs {
            let _ = roots.add(c);
        }
        let mut cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(cfg)
    })
    .clone()
}

// ---------------------------------------------------------------------------
// Errors / helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException { message: message.into() }.into()
}

fn iae<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException { message: message.into() }.into()
}

fn read_str_field(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize) -> Option<String> {
    match ctx.get_field(obj, idx) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push((b & 0xff) as u8);
        }
    }
    out
}

fn new_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i32));
    }
    arr
}

#[derive(Debug, Clone)]
struct Url1 {
    scheme: String,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url1, String> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https".to_string(), r)
    } else if let Some(r) = url.strip_prefix("http://") {
        ("http".to_string(), r)
    } else {
        return Err(format!("unsupported scheme in {url}"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default_port: u16 = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match authority.rfind(':') {
        Some(idx) if !authority[idx..].contains(']') => {
            let h = &authority[..idx];
            let p_str = &authority[idx + 1..];
            let p = p_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port in {url}"))?;
            (h.to_string(), p)
        }
        _ => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(format!("empty host in {url}"));
    }
    Ok(Url1 { scheme, host, port, path: path.to_string() })
}

fn build_request(
    method: &str,
    parsed: &Url1,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(256 + body.len());
    let _ = write!(&mut out, "{method} {} HTTP/1.1\r\n", parsed.path);
    let default_port: u16 = if parsed.scheme == "https" { 443 } else { 80 };
    if parsed.port == default_port {
        let _ = write!(&mut out, "Host: {}\r\n", parsed.host);
    } else {
        let _ = write!(&mut out, "Host: {}:{}\r\n", parsed.host, parsed.port);
    }
    let mut has_user_agent = false;
    let mut has_content_length = false;
    let mut has_connection = false;
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == "user-agent" {
            has_user_agent = true;
        }
        if lk == "content-length" {
            has_content_length = true;
        }
        if lk == "connection" {
            has_connection = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: Java/CratonVM\r\n");
    }
    if !has_content_length && (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) {
        let _ = write!(&mut out, "Content-Length: {}\r\n", body.len());
    }
    if !has_connection {
        // HttpURLConnection in real-JDK defaults to closing the connection.
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_response<S: Read>(stream: &mut S) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let head_end;
    loop {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| format!("response read: {e}"))?;
        if n == 0 {
            return Err("connection closed before response head".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            head_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err("response head exceeded 64 KiB".into());
        }
    }

    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers_storage);
    let parse_status = resp
        .parse(&buf[..head_end])
        .map_err(|e| format!("httparse: {e}"))?;
    if parse_status.is_partial() {
        return Err("incomplete response head".into());
    }
    let status = resp.code.ok_or("no status code")? as i32;
    let mut headers: Vec<(String, String)> = Vec::with_capacity(resp.headers.len());
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for h in resp.headers.iter() {
        let name = h.name.to_string();
        let value = std::str::from_utf8(h.value)
            .map_err(|e| format!("non-utf8 header value for {name}: {e}"))?
            .to_string();
        let lname = name.to_ascii_lowercase();
        if lname == "content-length" {
            content_length = value.trim().parse::<usize>().ok();
        }
        if lname == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        headers.push((name, value));
    }

    let mut body_buf: Vec<u8> = Vec::new();
    if buf.len() > head_end {
        body_buf.extend_from_slice(&buf[head_end..]);
    }
    if chunked {
        let body = read_chunked(&mut body_buf, stream)?;
        return Ok((status, headers, body));
    }
    if let Some(target) = content_length {
        let target = target.min(MAX_RESPONSE_BODY);
        while body_buf.len() < target {
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
        }
        body_buf.truncate(target);
    } else {
        loop {
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
            if body_buf.len() >= MAX_RESPONSE_BODY {
                body_buf.truncate(MAX_RESPONSE_BODY);
                break;
            }
        }
    }

    Ok((status, headers, body_buf))
}

fn read_chunked<S: Read>(prefix: &mut Vec<u8>, stream: &mut S) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let line_end = loop {
            if let Some(pos) = find_subslice(prefix, b"\r\n") {
                break pos;
            }
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("chunked size: {e}"))?;
            if n == 0 {
                return Err("chunked: socket closed mid-header".into());
            }
            prefix.extend_from_slice(&tmp[..n]);
        };
        let size_line = std::str::from_utf8(&prefix[..line_end])
            .map_err(|e| format!("chunked size utf8: {e}"))?
            .to_string();
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("bad chunk size: {size_line:?}"))?;
        prefix.drain(..line_end + 2);
        if size == 0 {
            return Ok(out);
        }
        while prefix.len() < size + 2 {
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("chunked body: {e}"))?;
            if n == 0 {
                return Err("chunked: socket closed mid-body".into());
            }
            prefix.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&prefix[..size]);
        if out.len() > MAX_RESPONSE_BODY {
            return Err("body exceeded MAX_RESPONSE_BODY".into());
        }
        prefix.drain(..size + 2);
    }
}

fn perform(
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let addr = format!("{}:{}", parsed.host, parsed.port);
    let mut last_err: Option<String> = None;
    let mut tcp: Option<TcpStream> = None;
    for sa in std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
        .map_err(|e| format!("resolve {addr}: {e}"))?
    {
        match TcpStream::connect_timeout(&sa, connect_timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(format!("connect {sa}: {e}")),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        last_err.unwrap_or_else(|| format!("could not resolve any address for {addr}"))
    })?;
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(read_timeout));
    let _ = tcp.set_nodelay(true);
    let req = build_request(method, parsed, headers, body);

    if parsed.scheme == "https" {
        let cfg = shared_legacy_config();
        let server_name = ServerName::try_from(parsed.host.clone())
            .map_err(|e| format!("bad server name {}: {e}", parsed.host))?;
        let conn = ClientConnection::new(cfg, server_name)
            .map_err(|e| format!("rustls ClientConnection::new: {e}"))?;
        let mut stream: StreamOwned<ClientConnection, TcpStream> =
            StreamOwned::new(conn, tcp);
        let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
        while stream.conn.is_handshaking() {
            if std::time::Instant::now() > deadline {
                return Err("TLS handshake timed out".into());
            }
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut stream.sock)
                    .map_err(|e| format!("handshake write: {e}"))?;
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut stream.sock)
                    .map_err(|e| format!("handshake read: {e}"))?;
                stream
                    .conn
                    .process_new_packets()
                    .map_err(|e| format!("handshake process: {e}"))?;
            }
        }
        stream.write_all(&req).map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        read_response(&mut stream)
    } else {
        let mut s = tcp;
        s.write_all(&req).map_err(|e| format!("write: {e}"))?;
        s.flush().map_err(|e| format!("flush: {e}"))?;
        read_response(&mut s)
    }
}

// ---------------------------------------------------------------------------
// Java-side helpers
// ---------------------------------------------------------------------------

fn extract_headers(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                if let Some(line) = ctx.read_string(s) {
                    if let Some(colon) = line.find(':') {
                        let k = line[..colon].trim().to_string();
                        let v = line[colon + 1..].trim().to_string();
                        if !k.is_empty() {
                            out.push((k, v));
                        }
                    }
                }
            }
        }
    }
    out
}

fn extract_body(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    // ByteArrayOutputStream synthetic field 0 = byte[] buf, field 1 = count.
    if let Value::Object(Some(baos)) = ctx.get_field(this, HUC_REQ_BODY_STREAM) {
        if let Value::Object(Some(arr)) = ctx.get_field(baos, 0) {
            let mut buf = read_byte_array(ctx, arr);
            if let Value::Int(count) = ctx.get_field(baos, 1) {
                if (count as usize) < buf.len() {
                    buf.truncate(count as usize);
                }
            }
            return buf;
        }
    }
    Vec::new()
}

fn ensure_connected(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    if matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Ok(None);
    }
    if matches!(ctx.get_field(this, HUC_DISCONNECTED), Value::Int(1)) {
        return Err(ioex("HttpURLConnection: connection already closed"));
    }
    let url_str = read_str_field(ctx, this, HUC_URL_STR)
        .ok_or_else(|| iae("HttpURLConnection: URL not set"))?;
    let parsed = parse_url(&url_str).map_err(ioex)?;
    let method = read_str_field(ctx, this, HUC_METHOD).unwrap_or_else(|| "GET".to_string());
    let headers = extract_headers(ctx, this);
    let body = extract_body(ctx, this);
    let connect_to = match ctx.get_field(this, HUC_CONNECT_TIMEOUT) {
        Value::Int(n) if n > 0 => Duration::from_millis(n as u64),
        _ => Duration::from_secs(30),
    };
    let read_to = match ctx.get_field(this, HUC_READ_TIMEOUT) {
        Value::Int(n) if n > 0 => Duration::from_millis(n as u64),
        _ => Duration::from_secs(60),
    };

    let (status, headers, body_bytes) =
        perform(&parsed, &method, &headers, &body, connect_to, read_to)
            .map_err(|e| ioex(format!("HttpURLConnection.connect failed: {e}")))?;

    let id = registry()
        .lock()
        .map_err(|_| ioex("connection registry poisoned"))?
        .allocate(ConnState {
            status,
            response_body: body_bytes,
            response_headers: headers,
            body_consumed: false,
        });
    ctx.set_field(this, HUC_CONN_ID, Value::Int(id));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
    Ok(None)
}

fn with_state<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&ConnState) -> R,
) -> Option<R> {
    let id = match ctx.get_field(this, HUC_CONN_ID) {
        Value::Int(i) if i > 0 => i,
        _ => return None,
    };
    let reg = registry().lock().ok()?;
    reg.get(id).map(f)
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

fn huc_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
    let m = ctx.create_string("GET");
    ctx.set_field(this, HUC_METHOD, Value::Object(Some(m)));
    ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(None));
    ctx.set_field(this, HUC_REQ_BODY_STREAM, Value::Object(None));
    ctx.set_field(this, HUC_DO_INPUT, Value::Int(1));
    ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(0));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
    ctx.set_field(this, HUC_DISCONNECTED, Value::Int(0));
    ctx.set_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS, Value::Int(1));
    ctx.set_field(this, HUC_CONNECT_TIMEOUT, Value::Int(0));
    ctx.set_field(this, HUC_READ_TIMEOUT, Value::Int(0));
    // If args[1] is a URL object with field 0 = full URL string, capture it.
    if let Some(Value::Object(Some(url_obj))) = args.get(1) {
        let s_val = ctx.get_field(*url_obj, 0);
        if let Value::Object(Some(_)) = s_val {
            ctx.set_field(this, HUC_URL_STR, s_val);
        }
    }
    Ok(None)
}

fn huc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ensure_connected(ctx, this)
}

fn huc_get_response_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ensure_connected(ctx, this)?;
    let code = with_state(ctx, this, |s| s.status).unwrap_or(-1);
    Ok(Some(Value::Int(code)))
}

fn huc_get_response_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ensure_connected(ctx, this)?;
    let msg = with_state(ctx, this, |s| match s.status {
        200 => "OK".to_string(),
        201 => "Created".to_string(),
        204 => "No Content".to_string(),
        301 => "Moved Permanently".to_string(),
        302 => "Found".to_string(),
        304 => "Not Modified".to_string(),
        400 => "Bad Request".to_string(),
        401 => "Unauthorized".to_string(),
        403 => "Forbidden".to_string(),
        404 => "Not Found".to_string(),
        500 => "Internal Server Error".to_string(),
        503 => "Service Unavailable".to_string(),
        c => format!("Status {c}"),
    })
    .unwrap_or_else(|| "".to_string());
    let s = ctx.create_string(&msg);
    Ok(Some(Value::Object(Some(s))))
}

fn huc_get_input_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    // Compatibility: `java.net.URL.openConnection()` (registered in
    // `net_phase_e::register_re4_url_http` and in `lib.rs::register_net_natives`)
    // allocates `java/net/HttpURLConnection` synthetics with a DIFFERENT field
    // layout — field 0 holds the originating URL object, not an i32 conn_id.
    // For non-http(s) URLs (file:, jar:, classpath:, jrt:, nested:), the right
    // behaviour is to delegate to `URL.openStream()` so Logback / Spring /
    // Cassandra can read XML configs through `URLConnection.getInputStream()`.
    // Without this branch, the HTTP-only `ensure_connected` path below treats
    // the URL object as a conn_id, fails `parse_url`, and returns a 0-byte
    // ByteArrayInputStream — which surfaces as `SAXParseException: Premature
    // end of file` in Logback's Joran parser (Cassandra NodeTool boot).
    if let Value::Object(Some(maybe_url)) = ctx.get_field(this, HUC_CONN_ID) {
        // Peek at the external form via the URL's full-URL string field
        // (field 5 in our URL synthetic), falling back to field 0.
        let url_str = match ctx.get_field(maybe_url, 5) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => match ctx.get_field(maybe_url, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            },
        };
        if !url_str.is_empty()
            && !url_str.starts_with("http://")
            && !url_str.starts_with("https://")
        {
            return ctx.invoke_virtual(
                maybe_url,
                "openStream",
                "()Ljava/io/InputStream;",
                &[],
            );
        }
    }

    ensure_connected(ctx, this)?;
    let body_bytes = with_state(ctx, this, |s| s.response_body.clone()).unwrap_or_default();
    let body_arr = new_byte_array(ctx, &body_bytes);
    let len = body_bytes.len() as i32;
    let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
    ctx.set_field(stream, 0, Value::Object(Some(body_arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(len));
    // Mark consumed so a follow-up read doesn't double-pull.
    if let Value::Int(id) = ctx.get_field(this, HUC_CONN_ID) {
        if let Ok(mut reg) = registry().lock() {
            if let Some(state) = reg.conns.get_mut(&id) {
                state.body_consumed = true;
            }
        }
    }
    Ok(Some(Value::Object(Some(stream))))
}

fn huc_get_error_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Ok(Some(Value::Object(None)));
    }
    let (status, body_bytes) =
        with_state(ctx, this, |s| (s.status, s.response_body.clone()))
            .unwrap_or((-1, Vec::new()));
    if status < 400 {
        return Ok(Some(Value::Object(None)));
    }
    let body_arr = new_byte_array(ctx, &body_bytes);
    let len = body_bytes.len() as i32;
    let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
    ctx.set_field(stream, 0, Value::Object(Some(body_arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(len));
    Ok(Some(Value::Object(Some(stream))))
}

fn huc_get_output_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !matches!(ctx.get_field(this, HUC_DO_OUTPUT), Value::Int(1)) {
        return Err(ioex("HttpURLConnection.getOutputStream: doOutput=false"));
    }
    if matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Err(ioex(
            "HttpURLConnection.getOutputStream: cannot write after connect",
        ));
    }
    // Lazily allocate a ByteArrayOutputStream-backed body buffer.
    if let Value::Object(Some(existing)) = ctx.get_field(this, HUC_REQ_BODY_STREAM) {
        return Ok(Some(Value::Object(Some(existing))));
    }
    let baos = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2);
    let backing = ctx.new_array(ArrayElementType::Byte, 0);
    ctx.set_field(baos, 0, Value::Object(Some(backing)));
    ctx.set_field(baos, 1, Value::Int(0));
    ctx.set_field(this, HUC_REQ_BODY_STREAM, Value::Object(Some(baos)));
    Ok(Some(Value::Object(Some(baos))))
}

fn huc_get_header_field_named(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let v = with_state(ctx, this, |s| {
        s.response_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
            .map(|(_, v)| v.clone())
    })
    .flatten();
    match v {
        Some(s) => {
            let js = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(js))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn huc_get_header_field_indexed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    ensure_connected(ctx, this)?;
    let v = with_state(ctx, this, |s| s.response_headers.get(idx as usize).cloned()).flatten();
    match v {
        Some((_k, val)) => {
            let js = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(js))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn huc_get_header_field_key_indexed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    ensure_connected(ctx, this)?;
    let v = with_state(ctx, this, |s| s.response_headers.get(idx as usize).cloned()).flatten();
    match v {
        Some((k, _)) => {
            let js = ctx.create_string(&k);
            Ok(Some(Value::Object(Some(js))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn huc_get_content_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let n = with_state(ctx, this, |s| s.response_body.len() as i32).unwrap_or(-1);
    Ok(Some(Value::Int(n)))
}

fn huc_get_content_length_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let n = with_state(ctx, this, |s| s.response_body.len() as i64).unwrap_or(-1);
    Ok(Some(Value::Long(n)))
}

fn huc_disconnect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Int(id) = ctx.get_field(this, HUC_CONN_ID) {
        if id > 0 {
            if let Ok(mut reg) = registry().lock() {
                reg.remove(id);
            }
        }
    }
    ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
    ctx.set_field(this, HUC_DISCONNECTED, Value::Int(1));
    Ok(None)
}

fn huc_set_request_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let m = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(iae("setRequestMethod: null method")),
    };
    let normalized = m.to_ascii_uppercase();
    if !matches!(
        normalized.as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "OPTIONS" | "PATCH" | "TRACE" | "CONNECT"
    ) {
        return Err(iae(format!("invalid HTTP method: {m}")));
    }
    let s = ctx.create_string(&normalized);
    ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
    Ok(None)
}

fn huc_get_request_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let m = match ctx.get_field(this, HUC_METHOD) {
        Value::Object(Some(s)) => s,
        _ => ctx.create_string("GET"),
    };
    Ok(Some(Value::Object(Some(m))))
}

fn huc_set_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(iae("setRequestProperty: null key")),
    };
    let value = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let line = ctx.create_string(&format!("{key}: {value}"));
    let arr = match ctx.get_field(this, HUC_REQ_HEADERS) {
        Value::Object(Some(a)) => a,
        _ => {
            let a = ctx.new_array(ArrayElementType::Reference, 32);
            ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(a)));
            a
        }
    };
    let len = ctx.array_length(arr);
    // Replace if the key already exists, else add to first empty slot.
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            let existing = ctx.read_string(s).unwrap_or_default();
            if let Some(colon) = existing.find(':') {
                if existing[..colon].trim().eq_ignore_ascii_case(&key) {
                    ctx.set_array_element(arr, i, Value::Object(Some(line)));
                    return Ok(None);
                }
            }
        }
    }
    for i in 0..len {
        if matches!(ctx.get_array_element(arr, i), Value::Object(None)) {
            ctx.set_array_element(arr, i, Value::Object(Some(line)));
            return Ok(None);
        }
    }
    Ok(None)
}

fn huc_add_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(iae("addRequestProperty: null key")),
    };
    let value = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let line = ctx.create_string(&format!("{key}: {value}"));
    let arr = match ctx.get_field(this, HUC_REQ_HEADERS) {
        Value::Object(Some(a)) => a,
        _ => {
            let a = ctx.new_array(ArrayElementType::Reference, 32);
            ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(a)));
            a
        }
    };
    let len = ctx.array_length(arr);
    for i in 0..len {
        if matches!(ctx.get_array_element(arr, i), Value::Object(None)) {
            ctx.set_array_element(arr, i, Value::Object(Some(line)));
            return Ok(None);
        }
    }
    Ok(None)
}

fn huc_set_do_input(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    ctx.set_field(this, HUC_DO_INPUT, Value::Int(v));
    Ok(None)
}

fn huc_set_do_output(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));
    Ok(None)
}

fn huc_set_connect_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if v < 0 {
        return Err(iae("setConnectTimeout: negative"));
    }
    ctx.set_field(this, HUC_CONNECT_TIMEOUT, Value::Int(v));
    Ok(None)
}

fn huc_set_read_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if v < 0 {
        return Err(iae("setReadTimeout: negative"));
    }
    ctx.set_field(this, HUC_READ_TIMEOUT, Value::Int(v));
    Ok(None)
}

fn huc_set_instance_follow_redirects(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    ctx.set_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS, Value::Int(v));
    Ok(None)
}

fn huc_get_instance_follow_redirects(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS)))
}

fn huc_using_proxy(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Always false — we don't honor proxies in the legacy path yet.
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

fn register_one(r: &mut NativeMethodRegistry, cls: &str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "<init>", "(Ljava/net/URL;)V", huc_init);
    r.register(cls, "<init>", "()V", huc_init);
    r.register(cls, "connect", "()V", huc_connect);
    r.register(cls, "getResponseCode", "()I", huc_get_response_code);
    r.register(
        cls,
        "getResponseMessage",
        "()Ljava/lang/String;",
        huc_get_response_message,
    );
    r.register(
        cls,
        "getInputStream",
        "()Ljava/io/InputStream;",
        huc_get_input_stream,
    );
    r.register(
        cls,
        "getErrorStream",
        "()Ljava/io/InputStream;",
        huc_get_error_stream,
    );
    r.register(
        cls,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        huc_get_output_stream,
    );
    r.register(
        cls,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        huc_get_header_field_named,
    );
    r.register(
        cls,
        "getHeaderField",
        "(I)Ljava/lang/String;",
        huc_get_header_field_indexed,
    );
    r.register(
        cls,
        "getHeaderFieldKey",
        "(I)Ljava/lang/String;",
        huc_get_header_field_key_indexed,
    );
    r.register(cls, "getContentLength", "()I", huc_get_content_length);
    r.register(
        cls,
        "getContentLengthLong",
        "()J",
        huc_get_content_length_long,
    );
    r.register(cls, "disconnect", "()V", huc_disconnect);
    r.register(
        cls,
        "setRequestMethod",
        "(Ljava/lang/String;)V",
        huc_set_request_method,
    );
    r.register(
        cls,
        "getRequestMethod",
        "()Ljava/lang/String;",
        huc_get_request_method,
    );
    r.register(
        cls,
        "setRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        huc_set_request_property,
    );
    r.register(
        cls,
        "addRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        huc_add_request_property,
    );
    r.register(cls, "setDoInput", "(Z)V", huc_set_do_input);
    r.register(cls, "setDoOutput", "(Z)V", huc_set_do_output);
    r.register(cls, "setConnectTimeout", "(I)V", huc_set_connect_timeout);
    r.register(cls, "setReadTimeout", "(I)V", huc_set_read_timeout);
    r.register(
        cls,
        "setInstanceFollowRedirects",
        "(Z)V",
        huc_set_instance_follow_redirects,
    );
    r.register(
        cls,
        "getInstanceFollowRedirects",
        "()Z",
        huc_get_instance_follow_redirects,
    );
    r.register(cls, "usingProxy", "()Z", huc_using_proxy);
    r.set_category(__prev_cat);
}

pub fn register_http_url_connection_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // The legacy `sun.net.www.protocol.http.HttpURLConnection` is the bulk of
    // the surface; the `https` variant subclasses it and overrides only TLS-
    // specific accessors. Both classes get the same native registrations so
    // an Https instance dispatches into the same connect() path with TLS.
    register_one(r, "sun/net/www/protocol/http/HttpURLConnection");
    register_one(r, "sun/net/www/protocol/https/HttpsURLConnectionImpl");
    // Some apps use the abstract base class directly via reflection.
    register_one(r, "java/net/HttpURLConnection");
    register_one(r, "javax/net/ssl/HttpsURLConnection");
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http_url_connection_tests {
    use super::*;

    #[test]
    fn test_register_http_url_connection_init() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(r
            .find(
                "sun/net/www/protocol/http/HttpURLConnection",
                "<init>",
                "(Ljava/net/URL;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_https_url_connection_impl_init() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(r
            .find(
                "sun/net/www/protocol/https/HttpsURLConnectionImpl",
                "<init>",
                "(Ljava/net/URL;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_huc_connect_methods() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        let cls = "sun/net/www/protocol/http/HttpURLConnection";
        assert!(r.find(cls, "connect", "()V").is_some());
        assert!(r.find(cls, "getResponseCode", "()I").is_some());
        assert!(r
            .find(cls, "getInputStream", "()Ljava/io/InputStream;")
            .is_some());
        assert!(r
            .find(cls, "getOutputStream", "()Ljava/io/OutputStream;")
            .is_some());
        assert!(r.find(cls, "disconnect", "()V").is_some());
    }

    #[test]
    fn test_register_huc_header_methods() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        let cls = "sun/net/www/protocol/http/HttpURLConnection";
        assert!(r
            .find(cls, "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "getHeaderField", "(I)Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "getHeaderFieldKey", "(I)Ljava/lang/String;")
            .is_some());
        assert!(r.find(cls, "getContentLength", "()I").is_some());
        assert!(r.find(cls, "getContentLengthLong", "()J").is_some());
    }

    #[test]
    fn test_register_huc_request_property_methods() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        let cls = "sun/net/www/protocol/http/HttpURLConnection";
        assert!(r
            .find(
                cls,
                "setRequestProperty",
                "(Ljava/lang/String;Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "addRequestProperty",
                "(Ljava/lang/String;Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(cls, "setRequestMethod", "(Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getRequestMethod", "()Ljava/lang/String;")
            .is_some());
    }

    #[test]
    fn test_parse_url_http() {
        let p = parse_url("http://example.com/foo").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 80);
        assert_eq!(p.path, "/foo");
    }

    #[test]
    fn test_parse_url_https_with_port() {
        let p = parse_url("https://example.com:8443").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 8443);
        assert_eq!(p.path, "/");
    }

    #[test]
    fn test_parse_url_rejects_unknown_scheme() {
        assert!(parse_url("ftp://example.com").is_err());
    }

    #[test]
    fn test_parse_url_rejects_empty_host() {
        assert!(parse_url("http:///foo").is_err());
    }

    #[test]
    fn test_build_request_get() {
        let p = parse_url("http://example.com/foo").unwrap();
        let req = build_request("GET", &p, &[], &[]);
        let s = String::from_utf8(req).unwrap();
        assert!(s.starts_with("GET /foo HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com\r\n"));
        assert!(s.contains("User-Agent: Java/CratonVM\r\n"));
        assert!(s.contains("Connection: close\r\n"));
    }

    #[test]
    fn test_build_request_post_body() {
        let p = parse_url("https://example.com:8443/api").unwrap();
        let req = build_request("POST", &p, &[("Content-Type".to_string(), "application/json".to_string())], b"{}");
        let s = String::from_utf8(req).unwrap();
        assert!(s.contains("POST /api HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com:8443\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.contains("Content-Type: application/json\r\n"));
        assert!(s.ends_with("{}"));
    }

    #[test]
    fn test_find_subslice() {
        assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
        assert_eq!(find_subslice(b"abc", b""), None);
    }

    #[test]
    fn test_read_chunked_simple() {
        let mut data: Vec<u8> = Vec::new();
        // Chunk sizes are hex; "in \r\nchunks." is 12 bytes = 0xc.
        // Embedded CRLF inside the chunk data is preserved per RFC 7230 §4.1.
        data.extend_from_slice(b"4\r\nWiki\r\n");
        data.extend_from_slice(b"6\r\npedia \r\n");
        data.extend_from_slice(b"c\r\nin \r\nchunks.\r\n");
        data.extend_from_slice(b"0\r\n\r\n");
        let mut empty: &[u8] = &[];
        let body = read_chunked(&mut data, &mut empty).unwrap();
        assert_eq!(body, b"Wikipedia in \r\nchunks.");
    }

    #[test]
    fn test_registry_allocates_unique_ids() {
        let mut reg = ConnRegistry::new();
        let id1 = reg.allocate(ConnState {
            status: 200,
            response_body: b"a".to_vec(),
            response_headers: vec![],
            body_consumed: false,
        });
        let id2 = reg.allocate(ConnState {
            status: 404,
            response_body: b"b".to_vec(),
            response_headers: vec![],
            body_consumed: false,
        });
        assert_ne!(id1, id2);
        assert_eq!(reg.get(id1).unwrap().status, 200);
        assert_eq!(reg.get(id2).unwrap().status, 404);
    }

    #[test]
    fn test_registry_remove_clears_state() {
        let mut reg = ConnRegistry::new();
        let id = reg.allocate(ConnState {
            status: 200,
            response_body: vec![],
            response_headers: vec![],
            body_consumed: false,
        });
        assert!(reg.get(id).is_some());
        reg.remove(id);
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn test_huc_init_sets_defaults() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let url_obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let url_str = ctx.create_string("http://example.com/");
        ctx.set_field(url_obj, 0, Value::Object(Some(url_str)));
        let _ = huc_init(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(url_obj))],
        );
        assert_eq!(ctx.get_field(this, HUC_CONN_ID), Value::Int(-1));
        assert_eq!(ctx.get_field(this, HUC_DO_INPUT), Value::Int(1));
        assert_eq!(ctx.get_field(this, HUC_DO_OUTPUT), Value::Int(0));
    }

    #[test]
    fn test_huc_set_request_method_normalizes() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let post = ctx.create_string("post");
        let _ = huc_set_request_method(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(post))],
        );
        let m = match ctx.get_field(this, HUC_METHOD) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
            _ => panic!("expected method string"),
        };
        assert_eq!(m, "POST");
    }

    #[test]
    fn test_huc_set_request_method_rejects_bogus() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let bad = ctx.create_string("FROBNICATE");
        let result = huc_set_request_method(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(bad))],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_huc_disconnect_clears_state() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let _ = huc_disconnect(&mut ctx, &[Value::Object(Some(this))]);
        assert_eq!(ctx.get_field(this, HUC_CONN_ID), Value::Int(-1));
        assert_eq!(ctx.get_field(this, HUC_DISCONNECTED), Value::Int(1));
    }

    #[test]
    fn test_huc_get_response_message_known_codes() {
        // Build a fake state and run get_response_message via the pipeline.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        // Cannot run ensure_connected without a real network — just exercise
        // the code-name table directly.
        let id = registry().lock().unwrap().allocate(ConnState {
            status: 404,
            response_body: vec![],
            response_headers: vec![],
            body_consumed: false,
        });
        ctx.set_field(this, HUC_CONN_ID, Value::Int(id));
        ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
        let res = huc_get_response_message(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        let s = match res {
            Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
            _ => panic!("expected string"),
        };
        assert_eq!(s, "Not Found");
    }

    #[test]
    fn test_huc_set_connect_timeout_negative_rejected() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let res = huc_set_connect_timeout(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Int(-1)],
        );
        assert!(res.is_err());
    }

    #[test]
    fn test_anchor_pub_function_exists() {
        // Anchor-grep guard: enforce the exported API name keeps existing.
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(!r.is_empty());
    }
}
