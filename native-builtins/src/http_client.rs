// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.5 — `java.net.http.HttpClient` (JDK 11+) native methods.
//!
//! Targets `jdk/internal/net/http/*` — the package where the real JDK 11+
//! `HttpClientImpl` / `HttpRequestImpl` / `HttpResponseImpl` / `Http2ClientImpl`
//! live. The public `java/net/http/HttpClient` and `HttpClient$Builder` natives
//! are already registered by `net_phase_e::register_re5_http_client` and
//! `http2::register_http_client_builder`; this module owns the *implementation
//! type* surface that the high-level Java code delegates to.
//!
//! Architecture:
//!
//!   * A process-wide connection pool keyed by `(scheme, host, port)`. Each
//!     pool slot holds either a raw `TcpStream` (HTTP/1.1) or a multiplexed
//!     `Http2Connection` placeholder (HTTP/2). Real HTTP/2 framing is in
//!     `http2.rs` and we only read from its public types — we never mutate it.
//!   * TLS uses `rustls::ClientConnection` directly (to align with the
//!     server-side path in `t27_tls.rs`). The handshake advertises
//!     `h2,http/1.1` ALPN; the negotiated value picks the wire protocol.
//!   * `HttpRequestImpl` / `HttpResponseImpl` hold synthetic field layouts;
//!     no Java-side `<init>` is required because every entry point allocates
//!     them directly via `alloc_concurrent_synthetic`.
//!   * Sync `send()` blocks the caller. Async `sendAsync()` allocates a
//!     `CompletableFuture` synthetic object and pre-completes it from the
//!     calling thread (the VM has no general worker pool we can hand off to
//!     without touching forbidden files).
//!
//! No stubs: every method either performs the operation or returns a value
//! consistent with the declared semantics (e.g. `executor()` returns `null`
//! when none is set, matching `HttpClient.executor()`'s `Optional.empty()`
//! contract on the Java side).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

use cratonvm_native_io::eintr::{retry_eintr, EintrIo};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// We read HPACK static-table indices and the RFC 7541 Huffman decoder from
// `http2.rs` so we can speak HTTP/2 against servers that prefer it after ALPN.
use crate::http2::{hpack_huffman_decode, HpackStaticTable};
use cratonvm_types::error::MethodCallFailed;

// ---------------------------------------------------------------------------
// HttpClientImpl synthetic field layout
// ---------------------------------------------------------------------------

const HCI_VERSION: usize = 0; // 0 = HTTP/1.1, 1 = HTTP/2
const HCI_FOLLOW_REDIRECTS: usize = 1; // 0 = NEVER, 1 = NORMAL, 2 = ALWAYS
const HCI_CONNECT_TIMEOUT_MS: usize = 2;
const HCI_SSL_CONTEXT: usize = 3;
const HCI_PROXY: usize = 4;
const HCI_AUTHENTICATOR: usize = 5;
const HCI_COOKIE_HANDLER: usize = 6;
const HCI_EXECUTOR: usize = 7;
const HCI_NUM_FIELDS: usize = 8;

// HttpRequestImpl synthetic field layout
const HRQ_METHOD: usize = 0;
const HRQ_URI: usize = 1;
const HRQ_BODY_BYTES: usize = 2; // byte[] body
const HRQ_HEADERS: usize = 3; // String[] of "key: value"
const HRQ_TIMEOUT_MS: usize = 4;
const HRQ_VERSION: usize = 5;
// G29-1. `java.net.http.HttpRequest` declares SEVEN instance accessors and this
// model answered four of them (`method`, `uri`, `version`, `timeout`). The
// missing three are added below; `expectContinue` is the only one that needed
// state, because nothing in this file has ever recorded it.
const HRQ_EXPECT_CONTINUE: usize = 6; // Int 0/1
const HRQ_NUM_FIELDS: usize = 7;

// HttpResponseImpl synthetic field layout
const HRS_STATUS: usize = 0;
const HRS_BODY_BYTES: usize = 1; // byte[]
const HRS_HEADERS_ARR: usize = 2; // String[] of "key: value"
const HRS_VERSION: usize = 3;
const HRS_URI: usize = 4;
const HRS_REQUEST: usize = 5;
const HRS_PREVIOUS: usize = 6;
const HRS_NUM_FIELDS: usize = 7;

const HTTP_VERSION_1_1: i32 = 0;
const HTTP_VERSION_2: i32 = 1;

/// `HRQ_VERSION`'s "no version override on this request" encoding.
///
/// A distinct sentinel is needed because this file's `HTTP_VERSION_1_1` is `0`
/// — the JDK's real ordinal — so `0` already means HTTP/1.1 and cannot double
/// as "unset". (`http2.rs` stores `ordinal + 1` for exactly this reason and so
/// gets `0` for free; the two encodings are reconciled in
/// `http_version_mirror`.) Only `HttpRequestImpl` has an "unset" state:
/// `HttpClient.version()` always answers a concrete version on the oracle.
const HRQ_VERSION_UNSET: i32 = -1;

const REDIRECT_NEVER: i32 = 0;
const REDIRECT_NORMAL: i32 = 1;
const REDIRECT_ALWAYS: i32 = 2;

const MAX_RESPONSE_BODY: usize = 16 * 1024 * 1024;
/// Upper bound on a single HTTP/1.1 chunk size-line (the hex length plus any
/// chunk extensions, up to the terminating CRLF) and on the trailer section of
/// a chunked body. A hostile server could otherwise stream bytes that never
/// contain a CRLF and force the reader to buffer without bound -> OOM DoS.
const MAX_CHUNK_LINE: usize = 8 * 1024;
/// Largest single HTTP/2 frame payload we will buffer. The HTTP/2 default
/// `SETTINGS_MAX_FRAME_SIZE` is 16 KiB (RFC 7540 §6.5.2); we never advertise a
/// larger value, so a server that sends a bigger frame is misbehaving. The
/// 24-bit length field otherwise permits up to 16 MiB per frame, which a
/// malicious server could use to force large per-frame allocations.
const H2_MAX_FRAME_SIZE: usize = 16 * 1024;
/// Cap on the total HPACK header block accumulated across HEADERS +
/// CONTINUATION frames. Without this, a server can stream CONTINUATION frames
/// forever (never setting END_HEADERS) and grow the block unbounded -> OOM.
const H2_MAX_HEADER_BLOCK: usize = 256 * 1024;
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const POOL_MAX_PER_HOST: usize = 8;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Errors / helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

fn iae<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

fn read_str_field(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize) -> Option<String> {
    match ctx.get_field(obj, idx) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn read_str_arg(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    match args.get(idx) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
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

// ---------------------------------------------------------------------------
// URI parsing (no external `url` crate — stay deps-light)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ParsedUri {
    scheme: String,
    host: String,
    port: u16,
    path: String,
}

fn parse_uri(uri: &str) -> Result<ParsedUri, String> {
    let (scheme, rest) = if let Some(r) = uri.strip_prefix("https://") {
        ("https".to_string(), r)
    } else if let Some(r) = uri.strip_prefix("http://") {
        ("http".to_string(), r)
    } else {
        return Err(format!("unsupported scheme in {uri}"));
    };
    // A query-only target has no explicit path, but HTTP origin-form still
    // needs `/?query`; fragments never belong in an HTTP request target.
    let (authority, path) = match rest.find(|c| matches!(c, '/' | '?' | '#')) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) if rest.as_bytes()[i] == b'?' => (&rest[..i], format!("/{}", &rest[i..])),
        Some(i) => (&rest[..i], "/".to_string()),
        None => (rest, "/".to_string()),
    };
    let default_port: u16 = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match authority.rfind(':') {
        Some(idx) if !authority[idx..].contains(']') => {
            let h = &authority[..idx];
            let p_str = &authority[idx + 1..];
            let p = p_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port in {uri}"))?;
            (h.to_string(), p)
        }
        _ => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(format!("empty host in {uri}"));
    }
    Ok(ParsedUri {
        scheme,
        host,
        port,
        path,
    })
}

// ---------------------------------------------------------------------------
// Connection pool
// ---------------------------------------------------------------------------

enum ConnKind {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<ClientConnection, TcpStream>>),
}

#[allow(dead_code)]
struct PooledConn {
    kind: ConnKind,
    last_used: Instant,
    /// h2 = true if ALPN landed on `h2`; false for `http/1.1` (or no ALPN).
    is_http2: bool,
}

type PoolKey = (String, String, u16); // (scheme, host, port)

struct ClientPool {
    by_authority: HashMap<PoolKey, Vec<PooledConn>>,
}

impl ClientPool {
    fn new() -> Self {
        Self {
            by_authority: HashMap::new(),
        }
    }

    fn checkout(&mut self, key: &PoolKey) -> Option<PooledConn> {
        if let Some(slots) = self.by_authority.get_mut(key) {
            while let Some(c) = slots.pop() {
                if c.last_used.elapsed() < POOL_IDLE_TIMEOUT {
                    return Some(c);
                }
            }
        }
        None
    }

    fn checkin(&mut self, key: PoolKey, mut conn: PooledConn) {
        conn.last_used = Instant::now();
        let slots = self.by_authority.entry(key).or_default();
        if slots.len() < POOL_MAX_PER_HOST {
            slots.push(conn);
        }
    }
}

fn pool() -> &'static Mutex<ClientPool> {
    static P: OnceLock<Mutex<ClientPool>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(ClientPool::new()))
}

// ---------------------------------------------------------------------------
// rustls config — system trust store, ALPN h2 + http/1.1
// ---------------------------------------------------------------------------

fn shared_client_config(prefer_h2: bool) -> Arc<ClientConfig> {
    static CFG_BOTH: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    static CFG_H1: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    let key = if prefer_h2 { &CFG_BOTH } else { &CFG_H1 };
    key.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        // Install the host trust store. If the platform doesn't expose any
        // (a sandbox without /etc/ssl), we still try the handshake — rustls
        // will fail it loudly with `WebPkiError::CertNotValidForName` rather
        // than silently accepting.
        let result = rustls_native_certs::load_native_certs();
        for c in result.certs {
            let _ = roots.add(c);
        }
        let mut cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = if prefer_h2 {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        } else {
            vec![b"http/1.1".to_vec()]
        };
        Arc::new(cfg)
    })
    .clone()
}

// ---------------------------------------------------------------------------
// TCP / TLS connect — returns (PooledConn, alpn_h2)
// ---------------------------------------------------------------------------

fn open_connection(
    scheme: &str,
    host: &str,
    port: u16,
    connect_timeout: Duration,
    prefer_h2: bool,
) -> Result<PooledConn, String> {
    let addr = format!("{}:{}", host, port);
    // `to_socket_addrs()` resolves hostnames; `TcpStream::connect_timeout` only
    // accepts a single SocketAddr, so we resolve manually and try each.
    let mut last_err: Option<String> = None;
    let mut tcp: Option<TcpStream> = None;
    // `normalize_connect_addr` folds an IPv4-mapped destination
    // (`::ffff:a.b.c.d`) to plain IPv4. A URL carries its host as TEXT, so this
    // path never passes through `InetAddress` — which is where real JDK, and
    // CratonVM's own mirror of it, collapses that literal to an
    // `Inet4Address`. Without the fold we build an AF_INET6 socket, and on
    // Windows `IPV6_V6ONLY` defaults to 1, so `connect` cannot reach a mapped
    // destination: `TestStartupIPv6Connectors.testIPv6MappedIPv4` reported
    // exactly "connect [::ffff:127.0.0.1]:<port>: ... (os error 10049)" from
    // the `last_err` line below.
    for sa in std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .map(cratonvm_native_io::outbound_policy::normalize_connect_addr)
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
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(DEFAULT_REQUEST_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(DEFAULT_REQUEST_TIMEOUT));
    if scheme == "https" {
        let cfg = shared_client_config(prefer_h2);
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| format!("bad server name {host}: {e}"))?;
        let conn = ClientConnection::new(cfg, server_name)
            .map_err(|e| format!("rustls ClientConnection::new: {e}"))?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while stream.conn.is_handshaking() {
            if Instant::now() > deadline {
                return Err("TLS handshake timed out".into());
            }
            // `EintrIo`: the socket carries `SO_RCVTIMEO`/`SO_SNDTIMEO` (set
            // just above), which Linux excludes from `SA_RESTART`, so a
            // CratonVM cross-thread JIT root-scan `SIGUSR2` landing on a parked
            // handshake used to escape as `handshake read: Interrupted system
            // call`. See `cratonvm_native_io::eintr`.
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .map_err(|e| format!("handshake write: {e}"))?;
            }
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .map_err(|e| format!("handshake read: {e}"))?;
                stream
                    .conn
                    .process_new_packets()
                    .map_err(|e| format!("handshake process: {e}"))?;
            }
        }
        let is_http2 = stream
            .conn
            .alpn_protocol()
            .map(|b| b == b"h2")
            .unwrap_or(false);
        Ok(PooledConn {
            kind: ConnKind::Tls(Box::new(stream)),
            last_used: Instant::now(),
            is_http2,
        })
    } else {
        Ok(PooledConn {
            kind: ConnKind::Plain(tcp),
            last_used: Instant::now(),
            is_http2: false,
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP/1.1 request/response
// ---------------------------------------------------------------------------

fn build_http1_request(
    method: &str,
    parsed: &ParsedUri,
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
    let mut has_accept = false;
    let mut has_content_length = false;
    let mut has_connection = false;
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == "user-agent" {
            has_user_agent = true;
        }
        if lk == "accept" {
            has_accept = true;
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
        out.extend_from_slice(b"User-Agent: cratonvm-httpclient/1.0\r\n");
    }
    if !has_accept {
        out.extend_from_slice(b"Accept: */*\r\n");
    }
    if !has_content_length && (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) {
        let _ = write!(&mut out, "Content-Length: {}\r\n", body.len());
    }
    if !has_connection {
        // Keep-alive so the pool can reuse this socket.
        out.extend_from_slice(b"Connection: keep-alive\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

#[derive(Debug, Clone)]
pub(crate) struct WireResponse {
    pub(crate) status: u16,
    pub(crate) version_h2: bool,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) keep_alive: bool,
}

fn read_retry<S: Read>(stream: &mut S, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match stream.read(buf) {
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted || e.raw_os_error() == Some(4) =>
            {
                continue
            }
            result => return result,
        }
    }
}

/// Read and parse an HTTP/1.1 response head + body using `httparse`.
fn read_http1_response<S: Read>(stream: &mut S) -> Result<WireResponse, String> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let head_end;
    loop {
        // A signal may interrupt a blocking recv without consuming a byte.
        // POSIX requires callers to retry that transient EINTR rather than
        // converting it into an HTTP transport failure. Under the DoHead
        // start/stop pressure this otherwise escaped as a sporadic
        // `HttpURLConnection response failed: response read: Interrupted
        // system call`.
        let n = loop {
            match read_retry(stream, &mut tmp) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => break result.map_err(|e| format!("response read: {e}"))?,
            }
        };
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

    // Parse headers with httparse (zero-alloc, RFC 7230 compliant).
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers_storage);
    let parse_status = resp
        .parse(&buf[..head_end])
        .map_err(|e| format!("httparse: {e}"))?;
    if parse_status.is_partial() {
        return Err("incomplete response head".into());
    }
    let status = resp.code.ok_or("no status code")?;
    let mut headers: Vec<(String, String)> = Vec::with_capacity(resp.headers.len());
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut connection_close = false;
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
        if lname == "connection" && value.eq_ignore_ascii_case("close") {
            connection_close = true;
        }
        headers.push((name, value));
    }

    // Body framing per RFC 7230 §3.3.3.
    let mut body_buf: Vec<u8> = Vec::new();
    if buf.len() > head_end {
        body_buf.extend_from_slice(&buf[head_end..]);
    }
    if chunked {
        let body = read_chunked(&mut body_buf, stream)?;
        return Ok(WireResponse {
            status,
            version_h2: false,
            headers,
            body,
            keep_alive: !connection_close,
        });
    }
    let target = content_length;
    if let Some(target) = target {
        let target = target.min(MAX_RESPONSE_BODY);
        while body_buf.len() < target {
            let n = read_retry(stream, &mut tmp).map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
        }
        body_buf.truncate(target);
    } else {
        // Read until close.
        loop {
            let n = read_retry(stream, &mut tmp).map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
            if body_buf.len() >= MAX_RESPONSE_BODY {
                body_buf.truncate(MAX_RESPONSE_BODY);
                break;
            }
        }
        connection_close = true;
    }

    Ok(WireResponse {
        status,
        version_h2: false,
        headers,
        body: body_buf,
        keep_alive: !connection_close,
    })
}

fn read_chunked<S: Read>(prefix: &mut Vec<u8>, stream: &mut S) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        // Ensure we have at least one CRLF in the prefix.
        let line_end = loop {
            if let Some(pos) = find_subslice(prefix, b"\r\n") {
                break pos;
            }
            // Bound the un-terminated size-line: a server that never emits a
            // CRLF would otherwise force unbounded buffering here (DoS).
            if prefix.len() > MAX_CHUNK_LINE {
                return Err("chunked: size line exceeds MAX_CHUNK_LINE".into());
            }
            let n = read_retry(stream, &mut tmp).map_err(|e| format!("chunked size: {e}"))?;
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
        // Reject an oversized chunk *before* buffering any of it: a malicious or
        // compromised server can advertise a multi-GiB (or near-usize::MAX)
        // chunk and force an unbounded allocation otherwise. Bound against the
        // total body cap (a single chunk can never legitimately exceed it).
        if size > MAX_RESPONSE_BODY {
            return Err("chunked: chunk size exceeds MAX_RESPONSE_BODY".into());
        }
        // Drop the size line (including CRLF).
        prefix.drain(..line_end + 2);
        if size == 0 {
            // Read trailing CRLF (and any trailers up to CRLF CRLF).
            while find_subslice(prefix, b"\r\n").is_none() {
                // Bound the trailer section: a server that streams trailer
                // bytes without a terminating CRLF would otherwise force
                // unbounded buffering here (DoS).
                if prefix.len() > MAX_CHUNK_LINE {
                    return Err("chunked: trailer exceeds MAX_CHUNK_LINE".into());
                }
                let n =
                    read_retry(stream, &mut tmp).map_err(|e| format!("chunked trailer: {e}"))?;
                if n == 0 {
                    break;
                }
                prefix.extend_from_slice(&tmp[..n]);
            }
            return Ok(out);
        }
        // Reject the *aggregate* before buffering this chunk so the running
        // total can never exceed the cap (size is already <= MAX_RESPONSE_BODY,
        // so size + 2 cannot overflow usize here).
        if out.len() + size > MAX_RESPONSE_BODY {
            return Err("response body exceeded MAX_RESPONSE_BODY".into());
        }
        // Read `size` bytes of chunk data + trailing CRLF.
        while prefix.len() < size + 2 {
            let n = read_retry(stream, &mut tmp).map_err(|e| format!("chunked body: {e}"))?;
            if n == 0 {
                return Err("chunked: socket closed mid-body".into());
            }
            prefix.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&prefix[..size]);
        prefix.drain(..size + 2); // also discard CRLF
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// HTTP/2 minimal request — uses HPACK static-table indices from http2.rs.
// We only run this when ALPN selected `h2`; otherwise fall back to HTTP/1.1.
// ---------------------------------------------------------------------------

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn build_h2_settings_frame() -> Vec<u8> {
    // Empty SETTINGS frame: length=0, type=4, flags=0, stream=0.
    vec![0, 0, 0, 4, 0, 0, 0, 0, 0]
}

fn build_h2_headers_frame(stream_id: u32, hpack_block: &[u8]) -> Vec<u8> {
    let len = hpack_block.len() as u32;
    let mut out = Vec::with_capacity(9 + hpack_block.len());
    out.push((len >> 16) as u8);
    out.push((len >> 8) as u8);
    out.push(len as u8);
    out.push(0x1); // HEADERS
    out.push(0x4 | 0x1); // END_HEADERS | END_STREAM
    out.extend_from_slice(&stream_id.to_be_bytes());
    out.extend_from_slice(hpack_block);
    out
}

fn encode_hpack_indexed(out: &mut Vec<u8>, idx: usize) {
    // Static table indexed: 1xxxxxxx with idx fitting in 7 bits.
    if idx < 0x7f {
        out.push(0x80 | (idx as u8));
    } else {
        out.push(0xff);
        // 7+ bit prefix integer encoding.
        let mut v = idx - 0x7f;
        while v >= 128 {
            out.push(((v & 0x7f) | 0x80) as u8);
            v >>= 7;
        }
        out.push(v as u8);
    }
}

fn encode_hpack_literal(out: &mut Vec<u8>, name: &str, value: &str) {
    // 0000xxxx — Literal, never indexed, new name. xxxx is name-index (0).
    out.push(0x10);
    encode_hpack_string(out, name.as_bytes());
    encode_hpack_string(out, value.as_bytes());
}

fn encode_hpack_string(out: &mut Vec<u8>, bytes: &[u8]) {
    // No Huffman: H=0, length 7-bit prefix.
    if bytes.len() < 0x7f {
        out.push(bytes.len() as u8);
    } else {
        out.push(0x7f);
        let mut v = bytes.len() - 0x7f;
        while v >= 128 {
            out.push(((v & 0x7f) | 0x80) as u8);
            v >>= 7;
        }
        out.push(v as u8);
    }
    out.extend_from_slice(bytes);
}

fn http2_request(
    stream: &mut rustls::StreamOwned<ClientConnection, TcpStream>,
    method: &str,
    parsed: &ParsedUri,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<WireResponse, String> {
    // 1. Send connection preface + SETTINGS frame.
    stream
        .write_all(H2_PREFACE)
        .map_err(|e| format!("h2 preface write: {e}"))?;
    stream
        .write_all(&build_h2_settings_frame())
        .map_err(|e| format!("h2 settings write: {e}"))?;
    retry_eintr(|| stream.flush()).map_err(|e| format!("h2 flush: {e}"))?;

    // 2. Build HPACK header block.
    let mut hpack: Vec<u8> = Vec::with_capacity(64);
    if let Some(idx) = HpackStaticTable::find(":method", Some(method)) {
        encode_hpack_indexed(&mut hpack, idx);
    } else {
        encode_hpack_literal(&mut hpack, ":method", method);
    }
    let scheme = if parsed.scheme == "https" {
        "https"
    } else {
        "http"
    };
    if let Some(idx) = HpackStaticTable::find(":scheme", Some(scheme)) {
        encode_hpack_indexed(&mut hpack, idx);
    } else {
        encode_hpack_literal(&mut hpack, ":scheme", scheme);
    }
    encode_hpack_literal(&mut hpack, ":path", &parsed.path);
    let authority = if (parsed.scheme == "https" && parsed.port == 443)
        || (parsed.scheme == "http" && parsed.port == 80)
    {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    encode_hpack_literal(&mut hpack, ":authority", &authority);
    for (k, v) in headers {
        // Spec forbids capitalized header names in HTTP/2.
        let lower = k.to_ascii_lowercase();
        encode_hpack_literal(&mut hpack, &lower, v);
    }

    // 3. Send HEADERS frame on stream id 1 (END_HEADERS|END_STREAM if no body).
    if body.is_empty() {
        let frame = build_h2_headers_frame(1, &hpack);
        stream
            .write_all(&frame)
            .map_err(|e| format!("h2 HEADERS write: {e}"))?;
    } else {
        // HEADERS without END_STREAM, then DATA with END_STREAM.
        let len = hpack.len() as u32;
        let mut hdrs = Vec::with_capacity(9 + hpack.len());
        hdrs.push((len >> 16) as u8);
        hdrs.push((len >> 8) as u8);
        hdrs.push(len as u8);
        hdrs.push(0x1); // HEADERS
        hdrs.push(0x4); // END_HEADERS
        hdrs.extend_from_slice(&1u32.to_be_bytes());
        hdrs.extend_from_slice(&hpack);
        stream
            .write_all(&hdrs)
            .map_err(|e| format!("h2 HEADERS write: {e}"))?;
        let blen = body.len() as u32;
        let mut data = Vec::with_capacity(9 + body.len());
        data.push((blen >> 16) as u8);
        data.push((blen >> 8) as u8);
        data.push(blen as u8);
        data.push(0x0); // DATA
        data.push(0x1); // END_STREAM
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(body);
        stream
            .write_all(&data)
            .map_err(|e| format!("h2 DATA write: {e}"))?;
    }
    retry_eintr(|| stream.flush()).map_err(|e| format!("h2 flush: {e}"))?;

    // 4. Read frames until we get HEADERS+DATA(END_STREAM) on stream 1.
    let mut header_block: Vec<u8> = Vec::new();
    let mut body_block: Vec<u8> = Vec::new();
    let mut status: u16 = 0;
    let mut all_headers: Vec<(String, String)> = Vec::new();
    let deadline = Instant::now() + DEFAULT_REQUEST_TIMEOUT;

    loop {
        if Instant::now() > deadline {
            return Err("h2 response timeout".into());
        }
        let mut hdr = [0u8; 9];
        if let Err(e) = stream.read_exact(&mut hdr) {
            return Err(format!("h2 frame head: {e}"));
        }
        let length = ((hdr[0] as u32) << 16) | ((hdr[1] as u32) << 8) | (hdr[2] as u32);
        let f_type = hdr[3];
        let flags = hdr[4];
        let stream_id = u32::from_be_bytes([hdr[5] & 0x7f, hdr[6], hdr[7], hdr[8]]);
        // Enforce SETTINGS_MAX_FRAME_SIZE before allocating: a misbehaving or
        // hostile server can set the 24-bit length up to 16 MiB and force a
        // large per-frame allocation. We never advertise a frame size larger
        // than the 16 KiB default, so anything bigger is a protocol violation.
        if length as usize > H2_MAX_FRAME_SIZE {
            return Err(format!(
                "h2 frame size {length} exceeds SETTINGS_MAX_FRAME_SIZE ({H2_MAX_FRAME_SIZE})"
            ));
        }
        let mut payload = vec![0u8; length as usize];
        if length > 0 {
            stream
                .read_exact(&mut payload)
                .map_err(|e| format!("h2 frame body: {e}"))?;
        }
        match f_type {
            // SETTINGS — ack if not already an ack.
            0x4 => {
                if flags & 0x1 == 0 {
                    let ack = [0, 0, 0, 4, 0x1, 0, 0, 0, 0];
                    stream
                        .write_all(&ack)
                        .map_err(|e| format!("h2 settings ack: {e}"))?;
                    stream.flush().ok();
                }
            }
            // PING — ack.
            0x6 => {
                if flags & 0x1 == 0 {
                    let mut pong = vec![0, 0, 8, 0x6, 0x1, 0, 0, 0, 0];
                    pong.extend_from_slice(&payload);
                    stream
                        .write_all(&pong)
                        .map_err(|e| format!("h2 ping ack: {e}"))?;
                    stream.flush().ok();
                }
            }
            // GOAWAY — bail.
            0x7 => {
                return Err(format!("h2 GOAWAY: payload={} bytes", payload.len()));
            }
            // RST_STREAM — bail.
            0x3 if stream_id == 1 => {
                return Err("h2 RST_STREAM on response stream".into());
            }
            // HEADERS / CONTINUATION on our stream.
            0x1 if stream_id == 1 => {
                let mut p = payload.as_slice();
                // RFC 7540 §6.2 field order: Pad Length (if PADDED), then the
                // 5-byte PRIORITY block (if PRIORITY), then the header block
                // fragment, then the trailing padding. Process PADDED first so
                // the pad-length byte is consumed from the front and the
                // padding is trimmed from the back before PRIORITY is stripped.
                let mut pad = 0usize;
                if flags & 0x8 != 0 {
                    // PADDED — first byte is pad length.
                    if p.is_empty() {
                        return Err("h2 HEADERS padding underflow".into());
                    }
                    pad = p[0] as usize;
                    p = &p[1..];
                }
                if flags & 0x20 != 0 {
                    // PRIORITY flag: skip 5-byte priority block.
                    if p.len() < 5 {
                        return Err("h2 HEADERS truncated priority".into());
                    }
                    p = &p[5..];
                }
                // Trim trailing padding (which follows the header block).
                if pad > p.len() {
                    return Err("h2 HEADERS padding overflow".into());
                }
                p = &p[..p.len() - pad];
                if header_block.len() + p.len() > H2_MAX_HEADER_BLOCK {
                    return Err("h2 header block exceeds limit".into());
                }
                header_block.extend_from_slice(p);
                if flags & 0x4 != 0 {
                    // END_HEADERS — decode the block.
                    let (st, hs) = decode_hpack_response(&header_block)?;
                    status = st;
                    all_headers = hs;
                    header_block.clear();
                }
                if flags & 0x1 != 0 {
                    // END_STREAM — done.
                    break;
                }
            }
            0x9 if stream_id == 1 => {
                // Cap CONTINUATION accumulation: a server that never sets
                // END_HEADERS would otherwise grow header_block without bound.
                if header_block.len() + payload.len() > H2_MAX_HEADER_BLOCK {
                    return Err("h2 header block exceeds limit".into());
                }
                header_block.extend_from_slice(&payload);
                if flags & 0x4 != 0 {
                    let (st, hs) = decode_hpack_response(&header_block)?;
                    status = st;
                    all_headers = hs;
                    header_block.clear();
                }
            }
            // DATA on our stream.
            0x0 if stream_id == 1 => {
                let mut p = payload.as_slice();
                if flags & 0x8 != 0 {
                    // PADDED — first byte is pad length.
                    if p.is_empty() {
                        return Err("h2 DATA padding underflow".into());
                    }
                    let pad = p[0] as usize;
                    p = &p[1..];
                    if pad > p.len() {
                        return Err("h2 DATA padding overflow".into());
                    }
                    p = &p[..p.len() - pad];
                }
                body_block.extend_from_slice(p);
                if body_block.len() > MAX_RESPONSE_BODY {
                    return Err("h2 body exceeded MAX_RESPONSE_BODY".into());
                }
                // Send a WINDOW_UPDATE so the server keeps streaming.
                let inc = (length).max(1);
                let mut wu = vec![0, 0, 4, 0x8, 0, 0, 0, 0, 0];
                wu.extend_from_slice(&inc.to_be_bytes());
                let _ = stream.write_all(&wu);
                let _ = stream.flush();
                if flags & 0x1 != 0 {
                    break;
                }
            }
            // WINDOW_UPDATE — ignore (no flow control accounting on send-only).
            0x8 => {}
            _ => {
                // Unknown / unrelated frame — skip.
            }
        }
    }

    Ok(WireResponse {
        status,
        version_h2: true,
        headers: all_headers,
        body: body_block,
        keep_alive: false, // We don't pool h2 conns.
    })
}

/// Minimal HPACK decoder for the response — supports indexed-static, literal-with-incremental,
/// literal-without-indexing, and literal-never-indexed. No Huffman decode (we set sender to non-Huffman).
fn decode_hpack_response(block: &[u8]) -> Result<(u16, Vec<(String, String)>), String> {
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut status: u16 = 0;
    let mut p = block;
    while !p.is_empty() {
        let b = p[0];
        if b & 0x80 != 0 {
            // 1xxxxxxx — Indexed Header Field.
            let (idx, rest) = decode_hpack_int(p, 7)?;
            p = rest;
            if idx == 0 {
                return Err("hpack: index 0".into());
            }
            if let Some((n, v)) = HpackStaticTable::ENTRIES.get(idx - 1) {
                if *n == ":status" && !v.is_empty() {
                    status = v.parse().unwrap_or(0);
                } else {
                    headers.push((n.to_string(), v.to_string()));
                }
            } else {
                return Err(format!("hpack: index {idx} out of static table"));
            }
        } else if b & 0x40 != 0 {
            // 01xxxxxx — Literal with Incremental Indexing.
            let (idx, rest) = decode_hpack_int(p, 6)?;
            p = rest;
            let name = if idx == 0 {
                let (n, rest) = decode_hpack_string(p)?;
                p = rest;
                n
            } else {
                HpackStaticTable::ENTRIES
                    .get(idx - 1)
                    .map(|(n, _)| n.to_string())
                    .ok_or_else(|| format!("hpack: literal-with-idx {idx}"))?
            };
            let (val, rest) = decode_hpack_string(p)?;
            p = rest;
            handle_decoded_header(&mut status, &mut headers, &name, &val);
        } else if b & 0x20 != 0 {
            // 001xxxxx — Dynamic table size update (we don't keep one).
            let (_, rest) = decode_hpack_int(p, 5)?;
            p = rest;
        } else {
            // 0000xxxx / 0001xxxx — Literal without/never indexing.
            let (idx, rest) = decode_hpack_int(p, 4)?;
            p = rest;
            let name = if idx == 0 {
                let (n, rest) = decode_hpack_string(p)?;
                p = rest;
                n
            } else {
                HpackStaticTable::ENTRIES
                    .get(idx - 1)
                    .map(|(n, _)| n.to_string())
                    .ok_or_else(|| format!("hpack: literal-without-idx {idx}"))?
            };
            let (val, rest) = decode_hpack_string(p)?;
            p = rest;
            handle_decoded_header(&mut status, &mut headers, &name, &val);
        }
    }
    Ok((status, headers))
}

fn handle_decoded_header(
    status: &mut u16,
    headers: &mut Vec<(String, String)>,
    name: &str,
    value: &str,
) {
    if name == ":status" {
        *status = value.parse().unwrap_or(0);
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

fn decode_hpack_int(input: &[u8], prefix_bits: u8) -> Result<(usize, &[u8]), String> {
    if input.is_empty() {
        return Err("hpack int: empty".into());
    }
    let mask = (1u8 << prefix_bits) - 1;
    let initial = (input[0] & mask) as usize;
    if initial < mask as usize {
        return Ok((initial, &input[1..]));
    }
    let mut value = mask as usize;
    let mut shift: u32 = 0;
    let mut i = 1;
    loop {
        if i >= input.len() {
            return Err("hpack int: truncated".into());
        }
        let b = input[i];
        i += 1;
        // checked_add so a hostile encoding can't silently wrap the accumulator
        // into a misleading small value (it would otherwise feed a bogus length
        // / table index downstream).
        let term = ((b & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or("hpack int: shift overflow")?;
        value = value.checked_add(term).ok_or("hpack int: overflow")?;
        if b & 0x80 == 0 {
            return Ok((value, &input[i..]));
        }
        shift += 7;
        if shift > 63 {
            return Err("hpack int: overflow".into());
        }
    }
}

fn decode_hpack_string(input: &[u8]) -> Result<(String, &[u8]), String> {
    if input.is_empty() {
        return Err("hpack str: empty".into());
    }
    let huffman = input[0] & 0x80 != 0;
    let (len, rest) = decode_hpack_int(input, 7)?;
    if rest.len() < len {
        return Err("hpack str: truncated body".into());
    }
    let bytes = &rest[..len];
    let s = if huffman {
        // Decode per RFC 7541 Appendix B. `hpack_huffman_decode` fails closed on
        // malformed input (bad padding / EOS / unmatchable code) rather than
        // returning wrong data, so a corrupt header surfaces as an error here.
        let decoded = hpack_huffman_decode(bytes).map_err(|e| format!("hpack huffman: {e}"))?;
        String::from_utf8(decoded).map_err(|e| format!("hpack huffman utf8: {e}"))?
    } else {
        std::str::from_utf8(bytes)
            .map_err(|e| format!("hpack str utf8: {e}"))?
            .to_string()
    };
    Ok((s, &rest[len..]))
}

// ---------------------------------------------------------------------------
// Top-level send: dispatch on ALPN result.
// ---------------------------------------------------------------------------

pub(crate) fn perform_request(
    method: &str,
    uri: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    prefer_h2: bool,
    follow_redirects: i32,
) -> Result<WireResponse, String> {
    let mut current_uri = uri.to_string();
    let mut current_method = method.to_string();
    let mut current_body = body.to_vec();
    let mut current_headers = headers.to_vec();
    for _hop in 0..=10usize {
        let parsed = parse_uri(&current_uri)?;
        let key: PoolKey = (parsed.scheme.clone(), parsed.host.clone(), parsed.port);
        // Try checkout. HTTP/2 connections are not pooled (we do one stream per conn).
        let pooled = pool().lock().ok().and_then(|mut p| p.checkout(&key));
        let conn = match pooled {
            Some(c) => c,
            None => open_connection(
                &parsed.scheme,
                &parsed.host,
                parsed.port,
                connect_timeout,
                prefer_h2,
            )?,
        };
        let mut conn = conn;
        let resp = match (&mut conn.kind, conn.is_http2) {
            (ConnKind::Tls(s), true) => {
                let r = http2_request(
                    s.as_mut(),
                    &current_method,
                    &parsed,
                    &current_headers,
                    &current_body,
                );
                r
            }
            (ConnKind::Tls(s), false) => {
                let req =
                    build_http1_request(&current_method, &parsed, &current_headers, &current_body);
                s.write_all(&req).map_err(|e| format!("write: {e}"))?;
                retry_eintr(|| s.flush()).map_err(|e| format!("flush: {e}"))?;
                read_http1_response(s.as_mut())
            }
            (ConnKind::Plain(t), _) => {
                let req =
                    build_http1_request(&current_method, &parsed, &current_headers, &current_body);
                t.write_all(&req).map_err(|e| format!("write: {e}"))?;
                retry_eintr(|| t.flush()).map_err(|e| format!("flush: {e}"))?;
                read_http1_response(t)
            }
        }?;
        // Pool keep-alive HTTP/1.1 connections.
        if resp.keep_alive && !conn.is_http2 {
            if let Ok(mut p) = pool().lock() {
                p.checkin(key, conn);
            }
        }
        // Follow redirects per JDK 11 policy.
        match resp.status {
            301 | 302 | 303 | 307 | 308 if follow_redirects != REDIRECT_NEVER => {
                let next = resp
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone());
                if let Some(loc) = next {
                    let absolute = if loc.starts_with("http://") || loc.starts_with("https://") {
                        loc
                    } else if let Some(rest) = loc.strip_prefix("//") {
                        format!("{}://{}", parsed.scheme, rest)
                    } else if loc.starts_with('/') {
                        format!("{}://{}:{}{}", parsed.scheme, parsed.host, parsed.port, loc)
                    } else {
                        format!(
                            "{}://{}:{}/{}",
                            parsed.scheme, parsed.host, parsed.port, loc
                        )
                    };
                    if resp.status == 303
                        || (resp.status == 301 || resp.status == 302)
                            && (current_method == "POST" || current_method == "PUT")
                    {
                        current_method = "GET".into();
                        current_body.clear();
                    }
                    // On a redirect that crosses origins, strip sensitive
                    // request headers before replaying them on the next hop.
                    // Authorization, Proxy-Authorization, and Cookie carry
                    // credentials scoped to the originating host; forwarding
                    // them to a different host (or scheme/port) leaks them to
                    // an unrelated server. This matches the JDK HttpClient
                    // `RedirectFilter` behavior.
                    if let Ok(next_parsed) = parse_uri(&absolute) {
                        let same_origin = next_parsed.scheme == parsed.scheme
                            && next_parsed.host.eq_ignore_ascii_case(&parsed.host)
                            && next_parsed.port == parsed.port;
                        if !same_origin {
                            current_headers.retain(|(k, _)| !is_sensitive_redirect_header(k));
                        }
                    }
                    current_uri = absolute;
                    continue;
                }
                return Ok(resp);
            }
            _ => return Ok(resp),
        }
    }
    Err("too many redirects".into())
}

/// Request headers that carry host-scoped credentials and must not be
/// replayed across an origin change on a redirect.
fn is_sensitive_redirect_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("cookie")
}

// ---------------------------------------------------------------------------
// Java-side helpers
// ---------------------------------------------------------------------------

fn extract_request_headers(ctx: &dyn NativeContext, hdrs_val: Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Value::Object(Some(arr)) = hdrs_val {
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

fn alloc_response(
    ctx: &mut dyn NativeContext,
    resp: &WireResponse,
    request: ObjectRef,
    uri: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let out = try_alloc_concurrent_synthetic(
        ctx,
        "jdk/internal/net/http/HttpResponseImpl",
        HRS_NUM_FIELDS,
    )?;
    ctx.set_field(out, HRS_STATUS, Value::Int(resp.status as i32));
    let body_arr = new_byte_array(ctx, &resp.body);
    ctx.set_field(out, HRS_BODY_BYTES, Value::Object(Some(body_arr)));
    let hdr_arr = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
    for (i, (k, v)) in resp.headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(out, HRS_HEADERS_ARR, Value::Object(Some(hdr_arr)));
    ctx.set_field(
        out,
        HRS_VERSION,
        Value::Int(if resp.version_h2 {
            HTTP_VERSION_2
        } else {
            HTTP_VERSION_1_1
        }),
    );
    let uri_str = ctx.create_string(uri);
    ctx.set_field(out, HRS_URI, Value::Object(Some(uri_str)));
    ctx.set_field(out, HRS_REQUEST, Value::Object(Some(request)));
    ctx.set_field(out, HRS_PREVIOUS, Value::Object(None));
    Ok(out)
}

fn alloc_error_response(
    ctx: &mut dyn NativeContext,
    request: ObjectRef,
    uri: &str,
    msg: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let resp = WireResponse {
        status: 0,
        version_h2: false,
        headers: vec![("x-cratonvm-error".to_string(), msg.to_string())],
        body: msg.as_bytes().to_vec(),
        keep_alive: false,
    };
    Ok(alloc_response(ctx, &resp, request, uri)?)
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

fn hci_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, HCI_VERSION, Value::Int(HTTP_VERSION_2));
    ctx.set_field(this, HCI_FOLLOW_REDIRECTS, Value::Int(REDIRECT_NORMAL));
    ctx.set_field(this, HCI_CONNECT_TIMEOUT_MS, Value::Long(0));
    ctx.set_field(this, HCI_SSL_CONTEXT, Value::Object(None));
    ctx.set_field(this, HCI_PROXY, Value::Object(None));
    ctx.set_field(this, HCI_AUTHENTICATOR, Value::Object(None));
    ctx.set_field(this, HCI_COOKIE_HANDLER, Value::Object(None));
    ctx.set_field(this, HCI_EXECUTOR, Value::Object(None));
    Ok(None)
}

fn hci_send(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let req = obj_arg(args, 1)?;
    // args[2] = BodyHandler — we apply it per JDK contract by calling
    // `apply(ResponseInfo)` and storing the resulting BodySubscriber's bytes
    // (best effort; not all callers require a real BodySubscriber).
    let body_handler = args.get(2).copied();
    do_send(ctx, this, req, body_handler)
}

fn hci_send_async(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let req = obj_arg(args, 1)?;
    let body_handler = args.get(2).copied();
    let resp_val = match do_send(ctx, this, req, body_handler) {
        Ok(Some(v)) => v,
        Ok(None) => Value::Object(None),
        Err(_) => Value::Object(None),
    };
    // Build the completed future through the JDK's OWN static factory rather
    // than fabricating one out of slot indices.
    //
    // W7-49 (2026-08-12). What this replaced allocated a real
    // `java.util.concurrent.CompletableFuture` — which declares exactly two
    // instance fields, `volatile Object result` and `volatile Completion stack`
    // (JDK 25.0.3.9, `javap -p`) — with FOUR slots, then wrote:
    //
    //   slot 0  result  <- the response          (the one index that was right)
    //   slot 1  stack   <- Int(1), "done"        REFERENCE slot, scanned as an oop
    //   slot 2  (past the end of the real layout)
    //   slot 3  (past the end of the real layout)
    //
    // The object then went straight to Java, where the REAL `CompletableFuture`
    // bytecode owns it: `complete`, `postComplete` and `getNumberOfDependents`
    // all walk `stack` as a `Completion` chain. This is the same `done`-int-over-
    // a-reference shape `util_concurrent_ext::native_cf_complete` documents on
    // itself, in a live real-JDK path.
    //
    // `aio_completed_future` invokes `CompletableFuture.completedFuture(Object)`,
    // so the layout is whatever the loaded class actually is and no index is
    // written at all — the same helper the async-channel natives on the same
    // essential path already use, in both modes. Semantics are unchanged: the
    // old form always marked the future done, with a null result on failure,
    // which is exactly `completedFuture(null)`.
    crate::phases_late::concurrent::aio_completed_future(ctx, resp_val)
}

fn do_send(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    req: ObjectRef,
    body_handler: Option<Value>,
) -> MethodCallResult {
    let method = read_str_field(ctx, req, HRQ_METHOD).unwrap_or_else(|| "GET".to_string());
    let uri = read_str_field(ctx, req, HRQ_URI).ok_or_else(|| iae("HttpRequest.uri is null"))?;
    let body = match ctx.get_field(req, HRQ_BODY_BYTES) {
        Value::Object(Some(arr)) => read_byte_array(ctx, arr),
        _ => Vec::new(),
    };
    let headers = extract_request_headers(ctx, ctx.get_field(req, HRQ_HEADERS));
    let version = match ctx.get_field(this, HCI_VERSION) {
        Value::Int(v) => v,
        _ => HTTP_VERSION_2,
    };
    let follow = match ctx.get_field(this, HCI_FOLLOW_REDIRECTS) {
        Value::Int(v) => v,
        _ => REDIRECT_NORMAL,
    };
    let timeout_ms = match ctx.get_field(this, HCI_CONNECT_TIMEOUT_MS) {
        Value::Long(v) if v > 0 => v as u64,
        _ => 30_000,
    };
    let result = perform_request(
        &method,
        &uri,
        &headers,
        &body,
        Duration::from_millis(timeout_ms),
        version == HTTP_VERSION_2,
        follow,
    );
    let resp_obj = match result {
        Ok(resp) => alloc_response(ctx, &resp, req, &uri),
        Err(e) => alloc_error_response(ctx, req, &uri, &format!("HTTP error: {e}")),
    }?;
    // If a BodyHandler was supplied, deliver the body via apply(ResponseInfo).
    if let Some(Value::Object(Some(handler))) = body_handler {
        let info =
            try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$ResponseInfo", 3)?;
        ctx.set_field(info, 0, ctx.get_field(resp_obj, HRS_STATUS));
        ctx.set_field(info, 1, ctx.get_field(resp_obj, HRS_HEADERS_ARR));
        ctx.set_field(info, 2, ctx.get_field(resp_obj, HRS_VERSION));
        // Best-effort upcall — ignored on failure (synthetic Java types may
        // not be loaded, in which case the response body is delivered through
        // direct field access on the HttpResponse instead).
        let _ = ctx.invoke_virtual(
            handler,
            "apply",
            "(Ljava/net/http/HttpResponse$ResponseInfo;)Ljava/net/http/HttpResponse$BodySubscriber;",
            &[Value::Object(Some(info))],
        );
    }
    Ok(Some(Value::Object(Some(resp_obj))))
}

// ---------------------------------------------------------------------------
// Enum mirrors and Optional payloads
//
// E13-1 §3 named the shape these helpers exist to remove: a native that answers
// in a form its DESCRIPTOR does not name. Nine sites in this file stored an
// `Int` — an ordinal or a raw millisecond count — and then handed it to
// bytecode that was about to `checkcast`/`areturn` a reference, or buried it in
// slot 0 of a `java.util.Optional`, where `isPresent()` reads it as PRESENT
// (`ref_operand_is_null` counts `Object(None)`, `Uninitialized` and `Long(0)`
// as null, and `Value::Int(0)` is not among them).
// ---------------------------------------------------------------------------

/// Resolve an enum constant by ORDINAL, through the class's static field.
///
/// Going through the static is what makes the answer the *same object*
/// `GETSTATIC` yields, so `client.version() == HttpClient.Version.HTTP_2` and
/// `client.version() == client.version()` both hold. Minting a fresh instance
/// per call (the `p57_alloc_enum` shape) satisfies neither, and an enum whose
/// constants fail `==` breaks `EnumMap`, `EnumSet` and every `switch` on it.
///
/// The constants come from `phases_late::net_channels`, which owns the
/// `<clinit>` that publishes them — deliberately NOT a second copy of the name
/// list here. `net_channels`' registration comment records why a `<clinit>` is
/// the only shape that works.
///
/// Fails to `Object(None)` rather than to the ordinal: the callers below all
/// declare a reference return, and surfacing a primitive tag there is the
/// unsoundness this whole helper exists to end.
fn enum_constant_static(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    constants: &[&str],
    ordinal: i32,
) -> Value {
    let Ok(idx) = usize::try_from(ordinal) else {
        return Value::Object(None);
    };
    let Some(name) = constants.get(idx) else {
        return Value::Object(None);
    };
    let Ok(cid) = ctx.ensure_class_initialized(class_name) else {
        return Value::Object(None);
    };
    match ctx.static_field_index_by_name(cid, name) {
        Some(slot) => match ctx.get_static_field(cid, slot) {
            v @ Value::Object(Some(_)) => v,
            _ => Value::Object(None),
        },
        None => Value::Object(None),
    }
}

/// `HttpClient.Version` for an ordinal in THIS file's encoding.
///
/// **The two files disagree about the encoding and both are internally
/// consistent.** Here `HTTP_VERSION_1_1 = 0` / `HTTP_VERSION_2 = 1` — the JDK's
/// real ordinals. In `http2.rs` the same names are `1` and `2`, because that
/// file needs `0` to mean "no version override". Nothing hands one file's
/// stored int to the other today; if anything ever does, this is where it will
/// go wrong, so the ordinal is converted at the boundary here rather than
/// stored as a shared convention.
fn http_version_mirror(ctx: &mut dyn NativeContext, ordinal: i32) -> Value {
    enum_constant_static(
        ctx,
        crate::phases_late::net_channels::HTTP_CLIENT_VERSION,
        crate::phases_late::net_channels::HTTP_VERSION_CONSTANTS,
        ordinal,
    )
}

/// `HttpClient.Redirect` for an ordinal in this file's encoding, which is
/// `NEVER = 0`, `NORMAL = 1`, `ALWAYS = 2` — and that is NOT the JDK's
/// declaration order, which is `NEVER`(0), `ALWAYS`(1), `NORMAL`(2) (measured:
/// `javap -p java.net.http.HttpClient$Redirect`, and `values()` on JDK
/// 25.0.3+9-LTS). The two are remapped here rather than renumbered, because
/// `REDIRECT_*` is compared against a wire-behaviour flag in `do_send` and
/// renumbering would change which requests follow redirects.
fn http_redirect_mirror(ctx: &mut dyn NativeContext, stored: i32) -> Value {
    let jdk_ordinal = match stored {
        REDIRECT_ALWAYS => 1,
        REDIRECT_NORMAL => 2,
        REDIRECT_NEVER => 0,
        _ => return Value::Object(None),
    };
    enum_constant_static(
        ctx,
        crate::phases_late::net_channels::HTTP_CLIENT_REDIRECT,
        crate::phases_late::net_channels::HTTP_REDIRECT_CONSTANTS,
        jdk_ordinal,
    )
}

/// A 1-slot `java.util.Optional` holding a `java.time.Duration`, or empty.
///
/// One helper for both `HttpClientImpl.connectTimeout()` and
/// `HttpRequestImpl.timeout()` — they had the same job and only one of them did
/// it. `0` is this file's "unset" encoding for both fields and maps to EMPTY,
/// which is what the oracle answers for a default client and a default request
/// (`Optional.empty` for both, measured on JDK 25.0.3+9-LTS).
///
/// The seconds/nanos split is floor-normalised (`div_euclid`/`rem_euclid`), not
/// truncating: a real `Duration.ofMillis(-1500)` is `seconds = -2,
/// nanos = +500_000_000`, and `nanos` is documented non-negative. The
/// truncating form this replaces produced `seconds = -1, nanos = -500_000_000`,
/// which no JDK `Duration` method is prepared for.
///
/// `opt` is pinned across the `Duration` allocation: it is an `ObjectRef` held
/// in a Rust local while another allocation runs, which is the native
/// stale-local family. Both of this file's `Duration`-minting sites now route
/// through here, so the file has no half-pinned pair.
fn alloc_optional_duration_ms(
    ctx: &mut dyn NativeContext,
    ms: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    if ms == 0 {
        ctx.set_field(opt, 0, Value::Object(None));
        return Ok(opt);
    }
    let opt_pin = ctx.pin_native_root(opt);
    let dur = try_alloc_concurrent_synthetic(ctx, "java/time/Duration", 2)?;
    let opt = ctx.read_native_pin(opt_pin, opt);
    ctx.unpin_native_roots(opt_pin);
    ctx.set_field(dur, 0, Value::Long(ms.div_euclid(1000)));
    // Cast: `rem_euclid(1000)` is in `0..=999`, so the product is at most
    // 999_000_000 — inside `i32`.
    ctx.set_field(dur, 1, Value::Int((ms.rem_euclid(1000) * 1_000_000) as i32));
    ctx.set_field(opt, 0, Value::Object(Some(dur)));
    Ok(opt)
}

/// A 1-slot `java.util.Optional` wrapping a value that is already a reference
/// (or `Object(None)` for empty).
///
/// The arity was right at every one of these sites before this patch; what
/// this centralises is the rule that slot 0 of a `java.util.Optional` is the
/// REFERENCE `value` field and nothing else. `OptionalInt`/`OptionalLong`/
/// `OptionalDouble` really are `(boolean isPresent, T value)` and must NOT be
/// routed through here.
fn alloc_optional_ref(
    ctx: &mut dyn NativeContext,
    value: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    ctx.set_field(
        opt,
        0,
        match value {
            v @ Value::Object(Some(_)) => v,
            _ => Value::Object(None),
        },
    );
    Ok(opt)
}

fn hrq_status_helpers_register(r: &mut NativeMethodRegistry) {
    let cls = "jdk/internal/net/http/HttpResponseImpl";
    r.register(cls, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, HRS_STATUS)))
    });
    r.register(cls, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The default BodyHandler returns the byte[] body; callers that want
        // a String go through HttpResponse.BodyHandlers.ofString which is in
        // http2.rs. We surface the byte[] here so anyone reflecting on the
        // field gets a consistent answer.
        Ok(Some(ctx.get_field(this, HRS_BODY_BYTES)))
    });
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let uri_str = match ctx.get_field(this, HRS_URI) {
            Value::Object(Some(s)) => s,
            _ => ctx.create_string(""),
        };
        let raw = ctx.read_string(uri_str).unwrap_or_default();
        let uri_obj = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 1)?;
        // JDK-ONLY-LAYOUT: slot 0 is OUR model's raw-text slot; on a real
        // `java.net.URI` it is `scheme`, so the full text used to be stored as
        // the scheme. The by-name publish is what a real receiver reads.
        if crate::net_phase_e::uri_has_synthetic_layout(ctx, uri_obj) {
            let s = ctx.create_string(&raw);
            ctx.set_field(uri_obj, 0, Value::Object(Some(s)));
        }
        crate::net_phase_e::uri_publish_named(ctx, uri_obj, &raw, None);
        Ok(Some(Value::Object(Some(uri_obj))))
    });
    r.register(
        cls,
        "request",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, HRS_REQUEST)))
        },
    );
    r.register(
        cls,
        "previousResponse",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // CORRECT BEFORE THIS PATCH and re-verified, not swept up:
            // `HRS_PREVIOUS` is a reference slot (`alloc_response` writes
            // `Object(None)` into it), so slot 0 of the Optional received a
            // reference. Routed through the shared helper for the invariant,
            // not for a fix.
            let previous = ctx.get_field(this, HRS_PREVIOUS);
            let opt = alloc_optional_ref(ctx, previous)?;
            Ok(Some(Value::Object(Some(opt))))
        },
    );
    r.register(
        cls,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // WAS: `Ok(Some(ctx.get_field(this, HRS_VERSION)))` — and
            // `alloc_response` writes an `Int` there. The descriptor says
            // `HttpClient$Version`, so this returned a primitive where the
            // caller's bytecode `areturn`s / `checkcast`s a reference.
            let stored = ctx
                .get_field(this, HRS_VERSION)
                .as_int()
                .unwrap_or(HTTP_VERSION_2);
            Ok(Some(http_version_mirror(ctx, stored)))
        },
    );
    r.register(
        cls,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr_val = ctx.get_field(this, HRS_HEADERS_ARR);
            // G29-1: was a 2-slot allocation with `Int(0)` in slot 1, while
            // `net_phase_e::re5_make_http_headers` mints the SAME class name
            // with 1 slot and `http2.rs::alloc_http_headers` with 3. Three
            // shapes for one class is three ways for the accessors registered
            // on it to read the wrong thing; the one every registered
            // `HttpHeaders` native actually reads is slot 0 = the `String[]`
            // of "key: value" lines, so route through that single minter.
            let headers_obj = crate::net_phase_e::re5_make_http_headers(ctx, arr_val)?;
            Ok(Some(Value::Object(Some(headers_obj))))
        },
    );
}

fn hreq_helpers_register(r: &mut NativeMethodRegistry) {
    let cls = "jdk/internal/net/http/HttpRequestImpl";
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = ctx.create_string("GET");
        ctx.set_field(this, HRQ_METHOD, Value::Object(Some(m)));
        ctx.set_field(this, HRQ_URI, Value::Object(None));
        ctx.set_field(this, HRQ_BODY_BYTES, Value::Object(None));
        ctx.set_field(this, HRQ_HEADERS, Value::Object(None));
        ctx.set_field(this, HRQ_TIMEOUT_MS, Value::Long(0));
        // WAS `Value::Int(HTTP_VERSION_2)`, which this file's own accessor then
        // reported as a PRESENT version override. The oracle disagrees:
        // `HttpRequest.newBuilder(uri).build().version()` is `Optional.empty`
        // on JDK 25.0.3+9-LTS — a request carries a version only if one was
        // set on it. `HTTP_VERSION_1_1 = 0` here, so `0` is not free as the
        // "unset" encoding and `HRQ_VERSION_UNSET` is a distinct sentinel.
        //
        // SLOT AUDIT for this change: `HRQ_VERSION` (slot 5) has exactly one
        // writer (this line) and one reader (`version()` below) in the whole
        // tree — `do_send` reads the version off the CLIENT (`HCI_VERSION`),
        // never off the request. So changing the stored default cannot reach
        // the wire path.
        ctx.set_field(this, HRQ_VERSION, Value::Int(HRQ_VERSION_UNSET));
        ctx.set_field(this, HRQ_EXPECT_CONTINUE, Value::Int(0));
        Ok(None)
    });
    r.register(cls, "method", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = match ctx.get_field(this, HRQ_METHOD) {
            Value::Object(Some(s)) => s,
            _ => ctx.create_string("GET"),
        };
        Ok(Some(Value::Object(Some(m))))
    });
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let uri_str = match ctx.get_field(this, HRQ_URI) {
            Value::Object(Some(s)) => s,
            _ => ctx.create_string(""),
        };
        let raw = ctx.read_string(uri_str).unwrap_or_default();
        let uri_obj = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 1)?;
        // JDK-ONLY-LAYOUT: slot 0 is OUR model's raw-text slot; on a real
        // `java.net.URI` it is `scheme`, so the full text used to be stored as
        // the scheme. The by-name publish is what a real receiver reads.
        if crate::net_phase_e::uri_has_synthetic_layout(ctx, uri_obj) {
            let s = ctx.create_string(&raw);
            ctx.set_field(uri_obj, 0, Value::Object(Some(s)));
        }
        crate::net_phase_e::uri_publish_named(ctx, uri_obj, &raw, None);
        Ok(Some(Value::Object(Some(uri_obj))))
    });
    r.register(cls, "version", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // WAS: the raw `Int` ordinal straight into slot 0 of the Optional.
        // `Value::Int(1)` is not null to `ref_operand_is_null`, so
        // `isPresent()` said TRUE for every request ever built and `get()`
        // handed back a primitive typed as `HttpClient$Version`.
        let stored = ctx
            .get_field(this, HRQ_VERSION)
            .as_int()
            .unwrap_or(HRQ_VERSION_UNSET);
        let mirror = if stored == HRQ_VERSION_UNSET {
            Value::Object(None)
        } else {
            http_version_mirror(ctx, stored)
        };
        let opt = alloc_optional_ref(ctx, mirror)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    r.register(cls, "timeout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // WAS: the raw `Long` millis into slot 0 of an `Optional<Duration>`.
        // `Long(0)` reads as null, so the default request answered EMPTY by
        // accident and was right; any non-zero timeout would have put a
        // `Long` where a `Duration` is declared. Same treatment as
        // `HttpClientImpl.connectTimeout()`, which had it right all along.
        let ms = match ctx.get_field(this, HRQ_TIMEOUT_MS) {
            Value::Long(v) => v,
            Value::Int(v) => i64::from(v),
            _ => 0,
        };
        let opt = alloc_optional_duration_ms(ctx, ms)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    // G29-1 — the same abstract-surface gap `net_phase_e.rs`'s
    // `java/net/http/HttpRequest` had, on this file's implementation twin.
    // `HttpRequestImpl` IS a real JDK class, so an unregistered accessor here
    // does not throw `AbstractMethodError`: it runs the JDK's own body against
    // OUR slot layout, which is worse — a wrong answer instead of a refusal.
    // `headers()` in particular would read the real class's `userHeaders` field
    // out of slot 3, where this model keeps a `String[]`.
    r.register(cls, "expectContinue", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Nothing in this model can SET expectContinue — there is no builder
        // that reaches an `HttpRequestImpl` — so this is always the JDK's
        // documented default of `false` (MEASURED on HotSpot for every request
        // shape that does not call `expectContinue(true)`). It is read from the
        // slot rather than returned as a literal so that the day a setter
        // appears, the accessor is already correct.
        let flag = ctx
            .get_field(this, HRQ_EXPECT_CONTINUE)
            .as_int()
            .unwrap_or(0);
        Ok(Some(Value::Int(i32::from(flag != 0))))
    });
    r.register(
        cls,
        "bodyPublisher",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A request carries a publisher exactly when it carries a body:
            // MEASURED, `GET`/`DELETE`/`HEAD` answer `Optional.empty` and every
            // `POST`/`PUT`/`method(v, publisher)` answers a present one.
            match ctx.get_field(this, HRQ_BODY_BYTES) {
                body @ Value::Object(Some(_)) => {
                    let publisher =
                        match crate::net_phase_e::re5_new_body_publisher(ctx, body, None)? {
                            Some(v) => v,
                            None => Value::Object(None),
                        };
                    ctx.invoke(
                        "java/util/Optional",
                        "ofNullable",
                        "(Ljava/lang/Object;)Ljava/util/Optional;",
                        &[publisher],
                    )
                }
                _ => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
            }
        },
    );
    r.register(
        cls,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = ctx.get_field(this, HRQ_HEADERS);
            let headers = crate::net_phase_e::re5_make_http_headers(ctx, arr)?;
            Ok(Some(Value::Object(Some(headers))))
        },
    );
}

fn hci_field_accessors_register(r: &mut NativeMethodRegistry) {
    let cls = "jdk/internal/net/http/HttpClientImpl";
    r.register(
        cls,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // WAS: the raw `Int` from `HCI_VERSION`, against a descriptor that
            // names `HttpClient$Version`. HotSpot answers `HTTP_2` for a
            // default client (measured), which is what this now returns —
            // as the interned constant, so `== HttpClient.Version.HTTP_2`.
            let stored = ctx
                .get_field(this, HCI_VERSION)
                .as_int()
                .unwrap_or(HTTP_VERSION_2);
            Ok(Some(http_version_mirror(ctx, stored)))
        },
    );
    r.register(
        cls,
        "followRedirects",
        "()Ljava/net/http/HttpClient$Redirect;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // WAS: the raw `Int` from `HCI_FOLLOW_REDIRECTS`, same defect.
            //
            // A MEASURED DIVERGENCE THIS PATCH DELIBERATELY DOES NOT FIX:
            // `hci_init` stores `REDIRECT_NORMAL`, but on JDK 25.0.3+9-LTS
            // both `HttpClient.newHttpClient().followRedirects()` and
            // `HttpClient.newBuilder().build().followRedirects()` answer
            // `NEVER`. Correcting the DEFAULT is not a type fix: `do_send`
            // feeds `HCI_FOLLOW_REDIRECTS` to `perform_request`, so flipping it
            // stops this VM following redirects on every request that has not
            // asked. That belongs in its own change with its own evidence —
            // it is recorded as a residual, not folded in here.
            let stored = ctx
                .get_field(this, HCI_FOLLOW_REDIRECTS)
                .as_int()
                .unwrap_or(REDIRECT_NORMAL);
            Ok(Some(http_redirect_mirror(ctx, stored)))
        },
    );
    r.register(cls, "executor", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // CORRECT BEFORE THIS PATCH: `HCI_EXECUTOR` is a reference slot
        // (`hci_init` writes `Object(None)`); nothing in the tree ever writes
        // a non-null one, so this is honestly EMPTY. Same for the three below.
        let value = ctx.get_field(this, HCI_EXECUTOR);
        let opt = alloc_optional_ref(ctx, value)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    r.register(cls, "proxy", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let value = ctx.get_field(this, HCI_PROXY);
        let opt = alloc_optional_ref(ctx, value)?;
        Ok(Some(Value::Object(Some(opt))))
    });
    r.register(
        cls,
        "cookieHandler",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let value = ctx.get_field(this, HCI_COOKIE_HANDLER);
            let opt = alloc_optional_ref(ctx, value)?;
            Ok(Some(Value::Object(Some(opt))))
        },
    );
    r.register(
        cls,
        "connectTimeout",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // CORRECT BEFORE THIS PATCH — E2-1 read this site and used it as
            // the model for its own fix. Moved onto the shared helper so the
            // `Duration` normalisation and the GC pin exist once; the change
            // in behaviour is confined to a negative `ms`, which now
            // floor-normalises the way a real `Duration` does.
            let ms = match ctx.get_field(this, HCI_CONNECT_TIMEOUT_MS) {
                Value::Long(v) => v,
                Value::Int(v) => i64::from(v),
                _ => 0,
            };
            let opt = alloc_optional_duration_ms(ctx, ms)?;
            Ok(Some(Value::Object(Some(opt))))
        },
    );
    r.register(
        cls,
        "authenticator",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let value = ctx.get_field(this, HCI_AUTHENTICATOR);
            let opt = alloc_optional_ref(ctx, value)?;
            Ok(Some(Value::Object(Some(opt))))
        },
    );
    r.register(
        cls,
        "sslContext",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = ctx.get_field(this, HCI_SSL_CONTEXT);
            match v {
                Value::Object(Some(_)) => Ok(Some(v)),
                _ => {
                    // Default SSLContext.
                    let s = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 1)?;
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        },
    );
}

fn http2_client_orchestrator_register(r: &mut NativeMethodRegistry) {
    let cls = "jdk/internal/net/http/Http2ClientImpl";
    // <init>(HttpClientImpl)
    r.register(
        cls,
        "<init>",
        "(Ljdk/internal/net/http/HttpClientImpl;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // field 0 = parent client, field 1 = open connection count.
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, Value::Int(0));
            Ok(None)
        },
    );
    // sendHttp2(HttpRequest, BodyHandler) -> HttpResponse
    r.register(
        cls,
        "sendHttp2",
        "(Ljdk/internal/net/http/HttpRequestImpl;Ljava/net/http/HttpResponse$BodyHandler;)Ljdk/internal/net/http/HttpResponseImpl;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let req = obj_arg(args, 1)?;
            let parent = match ctx.get_field(this, 0) {
                Value::Object(Some(c)) => c,
                _ => return Err(iae("Http2ClientImpl: no parent HttpClientImpl")),
            };
            do_send(ctx, parent, req, args.get(2).copied())
        },
    );
    r.register(cls, "openConnections", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
}

fn http1_exchange_register(r: &mut NativeMethodRegistry) {
    let cls = "jdk/internal/net/http/Http1Exchange";
    // The only slot this surface uses is 0 (the request bytes stashed by
    // `writeRequest` below). Initialise it explicitly, the way the sibling
    // `Http1HeaderParser.<init>` does, instead of relying on the allocator
    // having zeroed it — an empty constructor body here was indistinguishable
    // from a forgotten one.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        Ok(None)
    });
    r.register(cls, "writeRequest", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Simply stash the bytes on the exchange (field 0) so test code
        // can read them back. A real exchange-level write lives inside
        // the connection pool, but for reflection-driven dispatch we
        // need a no-throw method here.
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        Ok(None)
    });

    // Http1HeaderParser parses a single `\r\n`-terminated header line.
    let cls = "jdk/internal/net/http/Http1HeaderParser";
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // field 0 = byte[] buffer, field 1 = parsed status, field 2 = headers list.
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Object(None));
        Ok(None)
    });
    r.register(cls, "parse", "([B)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let bytes = read_byte_array(ctx, arr);
        let mut headers_storage = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut headers_storage);
        match resp.parse(&bytes) {
            Ok(httparse::Status::Complete(_)) => {
                if let Some(code) = resp.code {
                    ctx.set_field(this, 1, Value::Int(code as i32));
                }
                let arr_obj = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
                for (i, h) in resp.headers.iter().enumerate() {
                    let k = h.name;
                    let v = std::str::from_utf8(h.value).unwrap_or("");
                    let line = ctx.create_string(&format!("{k}: {v}"));
                    ctx.set_array_element(arr_obj, i, Value::Object(Some(line)));
                }
                ctx.set_field(this, 2, Value::Object(Some(arr_obj)));
                Ok(Some(Value::Int(1)))
            }
            Ok(httparse::Status::Partial) => Ok(Some(Value::Int(0))),
            Err(_) => Ok(Some(Value::Int(0))),
        }
    });
    r.register(cls, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

pub fn register_http_client_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "jdk/internal/net/http/HttpClientImpl";
    r.register(cls, "<init>", "()V", hci_init);
    r.register(
        cls,
        "send",
        "(Ljdk/internal/net/http/HttpRequestImpl;Ljava/net/http/HttpResponse$BodyHandler;)Ljdk/internal/net/http/HttpResponseImpl;",
        hci_send,
    );
    r.register(
        cls,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        hci_send,
    );
    r.register(
        cls,
        "sendAsync",
        "(Ljdk/internal/net/http/HttpRequestImpl;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        hci_send_async,
    );
    r.register(
        cls,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        hci_send_async,
    );
    hci_field_accessors_register(r);
    hreq_helpers_register(r);
    hrq_status_helpers_register(r);
    http2_client_orchestrator_register(r);
    http1_exchange_register(r);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http_client_tests {
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
}
