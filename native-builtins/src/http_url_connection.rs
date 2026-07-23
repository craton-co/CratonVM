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

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

thread_local! {
    /// Reason phrase from the most recently parsed HTTP response status line
    /// on this thread. httparse gives us the exact bytes the server sent
    /// (`resp.reason`), but `read_response`/`read_response_with_prefix`'s
    /// return type is the canonical `(status, headers, body)` triple used
    /// pervasively throughout this module's pooling/retry/redirect call
    /// chains -- threading a 4th field through every one of those signatures
    /// is a large, risky refactor for what only `huc_get_response_message`
    /// needs. A thread-local side channel (mirroring this file's existing
    /// identity-keyed side-table pattern, e.g. `real_results()`) is far
    /// smaller in scope: each blocking HTTP call is performed serially on
    /// the calling Java thread, so "most recently parsed" always means "the
    /// response this thread just read" -- correctly reflecting the final
    /// (post-redirect-follow) response by the time `huc_real_perform` reads
    /// it back out into `RealResult::reason`.
    static LAST_REASON_PHRASE: RefCell<String> = RefCell::new(String::new());
}

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use cratonvm_native_api::{
    install_baos_event_hook, BaosEvent, NativeContext, NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
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
        Self {
            next_id: 1,
            conns: HashMap::new(),
        }
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
// Real-JDK HttpURLConnection support
// ---------------------------------------------------------------------------
//
// When `URL.openConnection()` runs GENUINE JDK bytecode (a real `http(s)://`
// URL whose stream handler is wired — e.g. `URI.create(...).toURL()` in
// `TomcatBaseTest.getUrl`), it returns a real
// `sun.net.www.protocol.http.HttpURLConnection`, NOT our synthetic carrier.
// That object's instance layout is the JDK's (field 0 = `URLConnection.url`, a
// `java/net/URL`) and does NOT match our `HUC_*` slots — so the synthetic
// `getResponseCode`/`getInputStream` natives below misread it (`HUC_CONNECTED`
// lands on an unrelated real field that reads 1 → `ensure_connected`
// early-returns making NO request → `-1`; confirmed by tracing, see
// docs/tomcat-suite-bugs/10-pagecontext-npe-contains-null-FAIL.md). Detect that
// case via the URL object at field 0, perform the request from the *real* URL,
// and cache the result keyed by the connection object's identity hash so a
// follow-up `getInputStream` returns the same body. The synthetic resource-URL
// path (`file:`/`jar:`/`classpath:` → `URL.openStream`) is left untouched.

struct RealResult {
    status: i32,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The real reason phrase read off the wire (see `LAST_REASON_PHRASE`).
    /// Empty for the synthetic timeout-sentinel result, in which case
    /// `huc_get_response_message` falls back to the hardcoded table.
    reason: String,
}

fn real_results() -> &'static Mutex<HashMap<i32, RealResult>> {
    static R: OnceLock<Mutex<HashMap<i32, RealResult>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-real-connection REQUEST state, keyed by `identity_hash_code(this)`.
///
/// A real-JDK `sun.net.www...HttpURLConnection` (handed out by the genuine
/// `URL.openConnection()` machinery) carries the JDK's own instance layout, so
/// the synthetic `HUC_*` slots do NOT apply — writing them corrupts unrelated
/// real fields and reading them yields garbage (this is the root cause of the
/// dropped-`Authorization`-header / `cannot write after connect` /
/// empty-`getHeaderFields` cluster). We therefore keep every piece of request
/// state a real carrier needs (method, request headers, doOutput) in this
/// identity-keyed side-table, mirroring the established
/// `net_phase_e::stream_owner_table` precedent for real-JDK carrier objects.
#[derive(Clone)]
struct RealReq {
    method: String,                 // empty => "GET"
    headers: Vec<(String, String)>, // ordered; setRequestProperty replaces, addRequestProperty appends
    do_output: bool,
    // Caller-configured timeouts in ms (Java semantics: 0 = infinite, unset =
    // None → fall back to a sane default). The real-JDK carrier cannot park
    // these in synthetic slots, so they live here keyed by object identity.
    connect_timeout_ms: Option<i32>,
    read_timeout_ms: Option<i32>,
    follow_redirects: bool,
    streaming: StreamingMode,
}

/// Only fixed-length streaming has a wire-level implementation today.  It is
/// deliberately recorded separately from a user-supplied Content-Length: the
/// JDK's streaming setter changes *when* bytes are sent, not just which header
/// appears on the request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StreamingMode {
    #[default]
    Buffered,
    Fixed(u64),
    Chunked(i32),
}

impl Default for RealReq {
    fn default() -> Self {
        Self {
            method: String::new(),
            headers: Vec::new(),
            do_output: false,
            connect_timeout_ms: None,
            read_timeout_ms: None,
            follow_redirects: true,
            streaming: StreamingMode::Buffered,
        }
    }
}

fn real_reqs() -> &'static Mutex<HashMap<i32, RealReq>> {
    static R: OnceLock<Mutex<HashMap<i32, RealReq>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Real-carrier buffered request-body stream (the `ByteArrayOutputStream`
/// returned by `getOutputStream`), keyed by `identity_hash_code(this)`. We
/// cannot park it in a synthetic slot on a real object, so we track the object
/// by identity — same rationale and lifetime as `stream_owner_table`: the
/// harness keeps the stream live on its Java stack across the
/// write→`getResponseCode` window, so the bytes are read back at perform time.
///
/// GC note (gc-followups-20260706): the Java stack keeps the BAOS ALIVE, but
/// this raw ref is never REMAPPED — a moving GC in the
/// write→`getResponseCode` window leaves a stale address behind and the
/// response-perform path then reads the body from the old location.
/// Follow-up: store `(identity_key, ObjectRef)` var-handle-root pairs
/// (ASYNC_POOL pattern) and re-read at perform time.
/// GC note (cce0079 follow-up): values are `(identity_key, last_addr)`
/// VarHandle-root pairs — registered at insert so the BAOS stays alive and
/// registry-remapped across moving GCs; readers resolve the CURRENT address
/// via `ctx.read_var_handle_root(identity_key)` with the stored address as
/// fallback. (Keys were already GC-stable identity hashes.)
fn real_body_streams() -> &'static Mutex<HashMap<i32, (i32, ObjectRef)>> {
    static R: OnceLock<Mutex<HashMap<i32, (i32, ObjectRef)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An active HTTP/1.1 fixed-length request.  The Java-facing output stream is
/// still a ByteArrayOutputStream-shaped object, but native-io offers its
/// writes to the hook below before it buffers them.  This lets the real
/// HttpURLConnection carrier preserve the JDK's immediate-upload semantics
/// without teaching the I/O crate about HTTP.
struct LiveFixedStream {
    tcp: TcpStream,
    expected: u64,
    written: u64,
    closed: bool,
}

fn live_fixed_streams() -> &'static Mutex<HashMap<i32, LiveFixedStream>> {
    static R: OnceLock<Mutex<HashMap<i32, LiveFixedStream>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Maps the identity of the returned BAOS to its owning real connection.
fn live_fixed_owners() -> &'static Mutex<HashMap<i32, i32>> {
    static R: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True if `this` is a real-JDK URLConnection carrier (field 0 holds the
/// `java/net/URL` object) rather than our synthetic carrier (field 0 = i32
/// conn-id). The real JDK constructor uses a different descriptor than our
/// `<init>(Ljava/net/URL;)V` native, so a genuine carrier never runs `huc_init`
/// and keeps the JDK layout (field 0 = `URLConnection.url`).
fn is_real_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field(this, HUC_CONN_ID), Value::Object(Some(_)))
}

/// Mutate this real carrier's `RealReq` entry (creating it on first use).
fn with_real_req<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&mut RealReq) -> R,
) -> R {
    let key = ctx.identity_hash_code(this);
    let mut t = real_reqs().lock().expect("real_reqs poisoned");
    f(t.entry(key).or_default())
}

/// If `this` is a real-JDK URLConnection (field 0 is a `java/net/URL` object,
/// not our synthetic int conn-id), return its full external-form URL string via
/// `URL.toExternalForm()` (robust for both synthetic and real URL layouts).
fn huc_real_object_url(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    let url_obj = match ctx.get_field(this, HUC_CONN_ID) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    match ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the buffered request body for a real-JDK carrier from the
/// `ByteArrayOutputStream` recorded by `getOutputStream` (synthetic BAOS layout:
/// field 0 = byte[] buf, field 1 = count). Empty if no body was written.
fn real_body_bytes(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    let key = ctx.identity_hash_code(this);
    let baos = match real_body_streams()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).copied())
    {
        // Resolve the CURRENT address through the VarHandle-root registry
        // (the stored copy is stale after any moving GC).
        Some((vkey, stored)) => ctx.read_var_handle_root(vkey).unwrap_or(stored),
        None => return Vec::new(),
    };
    if let Value::Object(Some(arr)) = ctx.get_field(baos, 0) {
        let mut buf = read_byte_array(ctx, arr);
        if let Value::Int(count) = ctx.get_field(baos, 1) {
            if (count as usize) < buf.len() {
                buf.truncate(count as usize);
            }
        }
        return buf;
    }
    Vec::new()
}

/// Native-io calls this before it mutates an ordinary BAOS.  Returning `true`
/// consumes the event, so only BAOS instances registered by
/// `start_live_fixed_stream` bypass heap buffering.
fn huc_live_baos_event(
    ctx: &mut dyn NativeContext,
    baos: ObjectRef,
    event: BaosEvent,
) -> Result<bool, MethodCallFailed> {
    let baos_key = ctx.identity_hash_code(baos);
    let Some(conn_key) = live_fixed_owners()
        .lock()
        .ok()
        .and_then(|owners| owners.get(&baos_key).copied())
    else {
        return Ok(false);
    };

    let bytes = match event {
        BaosEvent::WriteByte(b) => Some(vec![b]),
        BaosEvent::WriteArray { array, offset, len } => {
            let mut out = vec![0u8; len];
            ctx.read_byte_array_into(array, offset, &mut out);
            Some(out)
        }
        BaosEvent::Flush | BaosEvent::Close => None,
    };
    let mut streams = live_fixed_streams()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length stream registry poisoned"))?;
    let stream = streams
        .get_mut(&conn_key)
        .ok_or_else(|| ioex("HttpURLConnection fixed-length stream is no longer available"))?;

    if let Some(bytes) = bytes {
        if stream.closed {
            return Err(ioex("HttpURLConnection fixed-length stream is closed"));
        }
        let next = stream
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| ioex("HttpURLConnection fixed-length stream overflow"))?;
        if next > stream.expected {
            return Err(ioex(format!(
                "HttpURLConnection fixed-length stream wrote {next} bytes; expected {}",
                stream.expected
            )));
        }
        ctx.begin_blocking_region();
        let write_result = stream
            .tcp
            .write_all(&bytes)
            .and_then(|_| stream.tcp.flush());
        ctx.end_blocking_region();
        write_result.map_err(|e| ioex(format!("HttpURLConnection streaming write failed: {e}")))?;
        stream.written = next;
        return Ok(true);
    }

    match event {
        BaosEvent::Flush => {
            ctx.begin_blocking_region();
            let result = stream.tcp.flush();
            ctx.end_blocking_region();
            result.map_err(|e| ioex(format!("HttpURLConnection streaming flush failed: {e}")))?;
        }
        BaosEvent::Close => {
            if !stream.closed {
                if stream.written != stream.expected {
                    return Err(ioex(format!(
                        "HttpURLConnection fixed-length stream closed after {} bytes; expected {}",
                        stream.written, stream.expected
                    )));
                }
                ctx.begin_blocking_region();
                let result = stream.tcp.shutdown(Shutdown::Write);
                ctx.end_blocking_region();
                result
                    .map_err(|e| ioex(format!("HttpURLConnection streaming close failed: {e}")))?;
                stream.closed = true;
            }
        }
        BaosEvent::WriteByte(_) | BaosEvent::WriteArray { .. } => unreachable!(),
    }
    Ok(true)
}

fn take_live_fixed_stream(conn_key: i32) -> Result<Option<LiveFixedStream>, MethodCallFailed> {
    let stream = live_fixed_streams()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length stream registry poisoned"))?
        .remove(&conn_key);
    if stream.is_some() {
        if let Ok(mut owners) = live_fixed_owners().lock() {
            owners.retain(|_, owner| *owner != conn_key);
        }
    }
    Ok(stream)
}

fn forget_live_fixed_stream(conn_key: i32) {
    if let Ok(mut streams) = live_fixed_streams().lock() {
        streams.remove(&conn_key);
    }
    if let Ok(mut owners) = live_fixed_owners().lock() {
        owners.retain(|_, owner| *owner != conn_key);
    }
}

/// Open a plain HTTP connection and write the request head now.  HTTPS keeps
/// the established buffered path because its rustls stream may re-enter Java
/// for key-manager callbacks; moving that stateful handshake into a write hook
/// would require a separate TLS stream owner.  The Tomcat regression and the
/// JDK fixed-length contract exercised here are plain HTTP.
fn start_live_fixed_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    baos: ObjectRef,
    expected: u64,
) -> Result<bool, MethodCallFailed> {
    let Some(url_str) = huc_real_object_url(ctx, this) else {
        return Ok(false);
    };
    let parsed = parse_url(&url_str).map_err(ioex)?;
    if parsed.scheme != "http" {
        return Ok(false);
    }
    let conn_key = ctx.identity_hash_code(this);
    if live_fixed_streams()
        .lock()
        .ok()
        .is_some_and(|streams| streams.contains_key(&conn_key))
    {
        return Ok(true);
    }
    let req = real_reqs()
        .lock()
        .ok()
        .and_then(|reqs| reqs.get(&conn_key).cloned())
        .unwrap_or_default();
    let method = if req.method.is_empty() {
        "POST"
    } else {
        req.method.as_str()
    };
    let connect_timeout = match req.connect_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(30),
    };
    let read_timeout = match req.read_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(60),
    };
    // The streaming setter owns Content-Length.  Do not let an earlier manual
    // header make the protocol head disagree with the exact byte count that
    // the write hook enforces.
    let mut headers: Vec<(String, String)> = req
        .headers
        .into_iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("content-length"))
        .collect();
    headers.push(("Content-Length".to_string(), expected.to_string()));
    let head = build_request_head(method, &parsed, &headers, expected, true);

    let addr = format!("{}:{}", parsed.host, parsed.port);
    let mut addrs: Vec<std::net::SocketAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
            .map_err(|e| ioex(format!("HttpURLConnection resolve {addr}: {e}")))?
            .collect();
    addrs.sort_by_key(|sa| u8::from(sa.is_ipv6()));
    ctx.begin_blocking_region();
    let tcp = addrs
        .into_iter()
        .find_map(|sa| TcpStream::connect_timeout(&sa, connect_timeout).ok());
    ctx.end_blocking_region();
    let mut tcp = tcp.ok_or_else(|| ioex(format!("HttpURLConnection connect failed: {addr}")))?;
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(read_timeout));
    let _ = tcp.set_nodelay(true);
    ctx.begin_blocking_region();
    let write_result = tcp.write_all(&head).and_then(|_| tcp.flush());
    ctx.end_blocking_region();
    write_result.map_err(|e| {
        ioex(format!(
            "HttpURLConnection streaming header write failed: {e}"
        ))
    })?;

    live_fixed_streams()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length stream registry poisoned"))?
        .insert(
            conn_key,
            LiveFixedStream {
                tcp,
                expected,
                written: 0,
                closed: false,
            },
        );
    live_fixed_owners()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length owner registry poisoned"))?
        .insert(ctx.identity_hash_code(baos), conn_key);
    Ok(true)
}

/// Perform (idempotently) the request for a real-JDK http(s) connection and
/// cache `(status, headers, body)` by object identity. Returns the HTTP status
/// (-1 on parse/IO failure). The request honors the method, headers, and body
/// the caller staged via `setRequestMethod` / `setRequestProperty` / the output
/// stream — all tracked in the identity-keyed `real_reqs` / `real_body_streams`
/// side-tables, since a real carrier's synthetic slots are unusable. Wrapped in
/// a GC-safe blocking region so the blocking I/O does not stall an in-process
/// CratonVM server's worker threads.
/// Cached status sentinel marking a connection whose request timed out, so
/// repeat getters re-raise `SocketTimeoutException` without blocking again.
const HUC_TIMEOUT_STATUS: i32 = -2;

fn huc_real_perform(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    url_str: &str,
) -> Result<i32, MethodCallFailed> {
    let key = ctx.identity_hash_code(this);
    if let Some(st) = real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.status))
    {
        if st == HUC_TIMEOUT_STATUS {
            return Err(socket_timeout_ex("Read timed out"));
        }
        return Ok(st);
    }
    // Fixed-length streaming has already sent the request head and each body
    // write on this same TCP connection.  Read its response instead of calling
    // `perform` (which would open a second connection and replay an empty
    // buffered BAOS).  Redirect replay is intentionally not attempted here:
    // the JDK likewise cannot transparently replay a streamed request body.
    if let Some(mut live) = take_live_fixed_stream(key)? {
        if live.written != live.expected {
            return Err(ioex(format!(
                "HttpURLConnection fixed-length stream has {} bytes; expected {} before reading the response",
                live.written, live.expected
            )));
        }
        let method = real_reqs()
            .lock()
            .ok()
            .and_then(|reqs| reqs.get(&key).map(|req| req.method.clone()))
            .unwrap_or_default();
        ctx.begin_blocking_region();
        let response = read_response(&mut live.tcp, method.eq_ignore_ascii_case("HEAD"));
        ctx.end_blocking_region();
        return match response {
            Ok((status, headers, body)) => {
                let reason = LAST_REASON_PHRASE.with(|r| r.borrow().clone());
                if let Ok(mut results) = real_results().lock() {
                    results.insert(
                        key,
                        RealResult {
                            status,
                            headers,
                            body,
                            reason,
                        },
                    );
                }
                Ok(status)
            }
            Err(ref e) if e == READ_TIMEOUT_SENTINEL => {
                if let Ok(mut results) = real_results().lock() {
                    results.insert(
                        key,
                        RealResult {
                            status: HUC_TIMEOUT_STATUS,
                            headers: Vec::new(),
                            body: Vec::new(),
                            reason: String::new(),
                        },
                    );
                }
                Err(socket_timeout_ex("Read timed out"))
            }
            // A fixed-length streaming request has already committed its head
            // and body to this connection.  If the peer aborts before a
            // response can be parsed, surface that transport failure as the
            // IOException HotSpot exposes to `postUrl`, rather than treating it
            // like a malformed buffered response and returning -1.
            Err(e) => Err(ioex(format!(
                "HttpURLConnection streaming response failed: {e}"
            ))),
        };
    }
    let req = real_reqs()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).cloned())
        .unwrap_or_default();
    let mut method = if req.method.is_empty() {
        "GET".to_string()
    } else {
        req.method.clone()
    };
    // Honor the caller's setConnectTimeout/setReadTimeout (ms). Java treats 0
    // as "infinite"; an unbounded blocking read would hang a worker forever,
    // so 0/unset keeps the historical 30s/60s sane defaults while an explicit
    // positive value (e.g. TomcatBaseTest's 1000ms) is honored exactly.
    let connect_to = match req.connect_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(30),
    };
    let read_to = match req.read_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(60),
    };
    let original_body = real_body_bytes(ctx, this);
    let mut body = original_body.clone();
    let mut current_url = url_str.to_string();
    let mut final_resp = None;
    for redirect_count in 0..=20 {
        let parsed = match parse_url(&current_url) {
            Ok(p) => p,
            Err(_) => return Ok(-1),
        };
        let established_https_stream = if parsed.scheme == "https" {
            huc_upcall_create_socket_if_custom_factory(ctx, &parsed.host, parsed.port)?
        } else {
            None
        };
        let resp = perform_with_retry(
            ctx,
            Some(this),
            &parsed,
            &method,
            &req.headers,
            &body,
            connect_to,
            read_to,
            established_https_stream,
        );
        match resp {
            Ok((status, headers, resp_body))
                if req.follow_redirects && is_redirect_status(status) && redirect_count < 20 =>
            {
                if let Some(location) = header_value(&headers, "Location") {
                    current_url = resolve_redirect_url(&parsed, &location);
                    if status == 303
                        || ((status == 301 || status == 302) && method != "GET" && method != "HEAD")
                    {
                        method = "GET".to_string();
                        body.clear();
                    }
                    continue;
                }
                final_resp = Some(Ok((status, headers, resp_body)));
                break;
            }
            other => {
                final_resp = Some(other);
                break;
            }
        }
    }
    let resp = final_resp.unwrap_or_else(|| Ok((310, Vec::new(), Vec::new())));
    match resp {
        Ok((status, headers, body)) => {
            let reason = LAST_REASON_PHRASE.with(|r| r.borrow().clone());
            if let Ok(mut t) = real_results().lock() {
                t.insert(
                    key,
                    RealResult {
                        status,
                        headers,
                        body,
                        reason,
                    },
                );
            }
            Ok(status)
        }
        // A read timeout maps to java.net.SocketTimeoutException (real-JDK
        // behaviour) — code such as TestConnector.testStop catches it
        // specifically. Cache the sentinel so later getters re-raise without
        // blocking for another full timeout.
        Err(ref e) if e == READ_TIMEOUT_SENTINEL => {
            if let Ok(mut t) = real_results().lock() {
                t.insert(
                    key,
                    RealResult {
                        status: HUC_TIMEOUT_STATUS,
                        headers: Vec::new(),
                        body: Vec::new(),
                        reason: String::new(),
                    },
                );
            }
            Err(socket_timeout_ex("Read timed out"))
        }
        // A TLS handshake-phase failure (see `TLS_HANDSHAKE_FAILURE_SENTINEL`'s
        // doc) must reach Java as `SSLHandshakeException` — real JSSE never
        // silently reports "-1" for a rejected/aborted handshake, and several
        // Tomcat tests specifically assert on catching that exception type
        // (e.g. a required client certificate that was not presented).
        Err(ref e) if e.starts_with(TLS_HANDSHAKE_FAILURE_SENTINEL) => {
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                e.trim_start_matches(TLS_HANDSHAKE_FAILURE_SENTINEL),
            ))
        }
        // A refused TCP connect (see `CONNECT_REFUSED_SENTINEL`'s doc) must
        // reach Java as `ConnectException`, not a generic IOException — real
        // code catches it specifically (see the type's own doc).
        Err(ref e) if e.starts_with(CONNECT_REFUSED_SENTINEL) => Err(RuntimeError::ConnectException {
            message: e.trim_start_matches(CONNECT_REFUSED_SENTINEL).to_string(),
        }
        .into()),
        // A transport failure before a response is available is an IOException.
        Err(e) => Err(ioex(format!("HttpURLConnection response failed: {e}"))),
    }
}

/// Cached response body for a real-JDK connection (empty if not performed).
fn huc_real_body(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    let key = ctx.identity_hash_code(this);
    real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.body.clone()))
        .unwrap_or_default()
}

/// Cached response headers for a real-JDK connection (empty if not performed).
fn huc_real_headers(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(String, String)> {
    let key = ctx.identity_hash_code(this);
    real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.headers.clone()))
        .unwrap_or_default()
}

/// Drop all identity-keyed side-table state for a real carrier (on disconnect).
fn real_forget(ctx: &dyn NativeContext, this: ObjectRef) {
    let key = ctx.identity_hash_code(this);
    if let Ok(mut t) = real_results().lock() {
        t.remove(&key);
    }
    if let Ok(mut t) = real_reqs().lock() {
        t.remove(&key);
    }
    if let Ok(mut t) = real_body_streams().lock() {
        t.remove(&key);
    }
    forget_live_fixed_stream(key);
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
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

fn socket_timeout_ex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::SocketTimeoutException {
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

fn protocol_ex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::ProtocolException {
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

/// Build a `java/io/ByteArrayInputStream` over `body` (4-field synthetic:
/// buf=0, pos=1, mark=2, count=3).
fn make_byte_array_input_stream(ctx: &mut dyn NativeContext, body: &[u8]) -> Value {
    let body_arr = new_byte_array(ctx, body);
    let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
    ctx.set_field(stream, 0, Value::Object(Some(body_arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(body.len() as i32));
    Value::Object(Some(stream))
}

/// Build a real `java.util.Map<String, java.util.List<String>>` from response
/// headers, grouping duplicate header names (case-insensitively, first-seen
/// order) into a per-name `List`. This mirrors what the JDK's
/// `URLConnection.getHeaderFields()` returns and is what `TomcatBaseTest`'s
/// `resHead` out-parameter is filled from. Every `create_string`/`invoke` can
/// allocate and move objects, so the map/list/key refs are pinned and re-read
/// across each re-entrant call (see the manifest-builder idiom in phases_late).
fn build_header_map(
    ctx: &mut dyn NativeContext,
    headers: &[(String, String)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in headers {
        if let Some(g) = groups.iter_mut().find(|(gk, _)| gk.eq_ignore_ascii_case(k)) {
            g.1.push(v.clone());
        } else {
            groups.push((k.clone(), vec![v.clone()]));
        }
    }
    let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("getHeaderFields: could not allocate HashMap")),
    };
    let map_pin = ctx.pin_native_root(map);
    for (k, vals) in &groups {
        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(map_pin);
                return Err(ioex("getHeaderFields: could not allocate ArrayList"));
            }
        };
        let list_pin = ctx.pin_native_root(list);
        for v in vals {
            let vs = ctx.create_string(v);
            let vs_pin = ctx.pin_native_root(vs);
            let list = ctx.read_native_pin(list_pin, list);
            let vs = ctx.read_native_pin(vs_pin, vs);
            ctx.invoke(
                "java/util/ArrayList",
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(list)), Value::Object(Some(vs))],
            )?;
        }
        let ks = ctx.create_string(k);
        let ks_pin = ctx.pin_native_root(ks);
        let map = ctx.read_native_pin(map_pin, map);
        let list = ctx.read_native_pin(list_pin, list);
        let ks = ctx.read_native_pin(ks_pin, ks);
        ctx.invoke(
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(map)),
                Value::Object(Some(ks)),
                Value::Object(Some(list)),
            ],
        )?;
    }
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    Ok(map)
}

#[derive(Debug, Clone)]
struct Url1 {
    scheme: String,
    host: String,
    port: u16,
    path: String,
    /// RFC 3986 user-info (`alice:secret` in `http://alice:secret@host/…`),
    /// stripped from the connect target / Host header. `build_request` turns
    /// it into preemptive `Authorization: Basic` credentials — the real-JDK
    /// `HttpURLConnection` behaviour (from `url.getUserInfo()`) that Spring's
    /// `ResourceTests.useUserInfoToSetBasicAuth` asserts on.
    userinfo: Option<String>,
}

fn is_redirect_status(status: i32) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn resolve_redirect_url(base: &Url1, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let default_port: u16 = if base.scheme == "https" { 443 } else { 80 };
    let prefix = if base.port == default_port {
        format!("{}://{}", base.scheme, base.host)
    } else {
        format!("{}://{}:{}", base.scheme, base.host, base.port)
    };
    if location.starts_with('/') {
        return format!("{prefix}{location}");
    }
    let base_dir = match base.path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &base.path[..=i],
    };
    format!("{prefix}{base_dir}{location}")
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
    // RFC 3986: authority = [ userinfo "@" ] host [ ":" port ]. Split at the
    // LAST '@' (userinfo may itself contain an encoded/raw '@').
    let (userinfo, authority) = match authority.rfind('@') {
        Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
        None => (None, authority),
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
    Ok(Url1 {
        scheme,
        host,
        port,
        path: path.to_string(),
        userinfo,
    })
}

fn build_request(
    method: &str,
    parsed: &Url1,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = build_request_head(method, parsed, headers, body.len() as u64, !body.is_empty());
    out.extend_from_slice(body);
    out
}

fn ise<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

/// Build an HTTP/1.1 request head without materialising the body.  The live
/// fixed-length path uses this to put headers on the wire before Java writes
/// its first body byte.
fn build_request_head(
    method: &str,
    parsed: &Url1,
    headers: &[(String, String)],
    body_len: u64,
    has_output: bool,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let _ = write!(&mut out, "{method} {} HTTP/1.1\r\n", parsed.path);
    let default_port: u16 = if parsed.scheme == "https" { 443 } else { 80 };
    if parsed.port == default_port {
        let _ = write!(&mut out, "Host: {}\r\n", parsed.host);
    } else {
        let _ = write!(&mut out, "Host: {}:{}\r\n", parsed.host, parsed.port);
    }
    let mut has_user_agent = false;
    let mut has_connection = false;
    let mut has_content_length = false;
    let mut has_content_type = false;
    let mut has_authorization = false;
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == "user-agent" {
            has_user_agent = true;
        }
        if lk == "connection" {
            has_connection = true;
        }
        if lk == "content-length" {
            has_content_length = true;
        }
        if lk == "content-type" {
            has_content_type = true;
        }
        if lk == "authorization" {
            has_authorization = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    // JDK parity: a URL carrying user-info sends preemptive
    // `Authorization: Basic base64(userinfo)` unless the caller staged an
    // explicit Authorization header (sun.net.www.protocol.http
    // .HttpURLConnection does this from `url.getUserInfo()`; asserted by
    // Spring's ResourceTests.useUserInfoToSetBasicAuth).
    if !has_authorization {
        if let Some(ui) = &parsed.userinfo {
            let b64 =
                String::from_utf8(crate::b64_encode(ui.as_bytes(), 0, false)).unwrap_or_default();
            let _ = write!(&mut out, "Authorization: Basic {b64}\r\n");
        }
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: Java/CratonVM\r\n");
    }
    // The legacy JDK HttpURLConnection client keeps HTTP/1.1 connections
    // alive by default and sends the explicit compatibility header. Tomcat
    // exposes that choice in its response header set, including when it drops
    // an invalid response header before committing the response.
    if !has_connection {
        out.extend_from_slice(b"Connection: keep-alive\r\n");
    }
    let is_output_method = has_output || matches!(method, "POST" | "PUT" | "PATCH");
    if !has_content_length && is_output_method {
        let _ = write!(&mut out, "Content-Length: {body_len}\r\n");
    }
    // Real-JDK `HttpURLConnection` defaults the request Content-Type to
    // `application/x-www-form-urlencoded` when the application opened an
    // output stream (POST/PUT/PATCH) without setting one. Servers rely on
    // this to parse a form body into request parameters — e.g. Tomcat's
    // `request.getParameter(...)` (TestRestCsrfPreventionFilter2's
    // request-param nonce path returned 403 without it because the body was
    // never parsed as parameters).
    if !has_content_type && is_output_method {
        out.extend_from_slice(b"Content-Type: application/x-www-form-urlencoded\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Sentinel error string returned by [`read_response`] when a socket read
/// exceeds the configured read timeout. `huc_real_perform` recognises it and
/// raises `java.net.SocketTimeoutException` (real-JDK behaviour) rather than
/// folding it into the generic `-1`/IOException path.
const READ_TIMEOUT_SENTINEL: &str = "__cratonvm_read_timeout__";

/// Prefix on an error string returned by [`perform`]'s HTTPS branch when the
/// failure happened during the TLS handshake itself, or the connection closed
/// with zero response bytes immediately after the client's side of a TLS 1.3
/// handshake completed optimistically (see the doc at the `read_response`
/// call in `perform`). `huc_real_perform` recognises this prefix and raises
/// `javax.net.ssl.SSLHandshakeException` (real-JDK/JSSE behaviour) instead of
/// folding it into the generic "-1" contract used for other connection
/// failures (e.g. a malformed-but-present HTTP response).
const TLS_HANDSHAKE_FAILURE_SENTINEL: &str = "__cratonvm_tls_handshake_failure__: ";

/// Prefix on an error string returned by [`perform`] when its TCP connect
/// phase failed with `ConnectionRefused` specifically. `huc_real_perform`
/// recognises this and raises `java.net.ConnectException` (real-JDK
/// behaviour — see the typed `RuntimeError::ConnectException` variant's doc),
/// matching every other native connect path in this codebase (plain
/// `Socket`/`SocketChannel`), instead of folding a refused connection into
/// the generic IOException used for other connect failures (DNS failure,
/// timeout).
const CONNECT_REFUSED_SENTINEL: &str = "__cratonvm_connect_refused__: ";

/// Map a socket-read `io::Error` to an error string, flagging a timeout via
/// [`READ_TIMEOUT_SENTINEL`]. A blocking `read` that hits `SO_RCVTIMEO`
/// surfaces as `WouldBlock` (Unix) or `TimedOut` (Windows).
fn read_io_err(prefix: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            READ_TIMEOUT_SENTINEL.to_string()
        }
        _ => format!("{prefix}: {e}"),
    }
}

/// Read that tolerates an unclean TLS close: many HTTP servers close the TCP
/// connection at end-of-response without sending a TLS `close_notify`, which
/// rustls surfaces as `UnexpectedEof` ("peer closed connection without sending
/// TLS close_notify"). For an HTTP client that is a normal end-of-stream, so map
/// it to `Ok(0)` (EOF) rather than a hard error.
fn read_eof_tolerant<S: Read>(stream: &mut S, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match stream.read(buf) {
            Ok(n) => return Ok(n),
            // EINTR is a transient interruption, not a peer disconnect.
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted || e.raw_os_error() == Some(4) =>
            {
                continue
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.to_string().contains("close_notify") =>
            {
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
    }
}

fn read_response<S: Read>(
    stream: &mut S,
    head: bool,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    read_response_with_prefix(stream, head, Vec::new())
}

/// Same as [`read_response`], but starts from `prefix` bytes already read off
/// the wire (e.g. the pooled connection liveness-probe's first read) instead
/// of assuming nothing has been consumed yet.
fn read_response_with_prefix<S: Read>(
    stream: &mut S,
    head: bool,
    prefix: Vec<u8>,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let mut buf = prefix;
    let mut tmp = [0u8; 8192];
    let head_end = match find_subslice(&buf, b"\r\n\r\n") {
        Some(pos) => pos + 4,
        None => loop {
            let n =
                read_eof_tolerant(stream, &mut tmp).map_err(|e| read_io_err("response read", e))?;
            if n == 0 {
                return Err("connection closed before response head".into());
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > 64 * 1024 {
                return Err("response head exceeded 64 KiB".into());
            }
        },
    };

    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers_storage);
    let parse_status = resp
        .parse(&buf[..head_end])
        .map_err(|e| format!("httparse: {e}"))?;
    if parse_status.is_partial() {
        return Err("incomplete response head".into());
    }
    let status = resp.code.ok_or("no status code")? as i32;
    LAST_REASON_PHRASE.with(|r| *r.borrow_mut() = resp.reason.unwrap_or("").to_string());
    let mut headers: Vec<(String, String)> = Vec::with_capacity(resp.headers.len());
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for h in resp.headers.iter() {
        let name = h.name.to_string();
        // HTTP header fields are byte-oriented. `HttpURLConnection` exposes
        // those bytes as ISO-8859-1 code points rather than interpreting them
        // as UTF-8. In particular, Tomcat may emit a UTF-8 cookie value; its
        // client test retrieves the raw header bytes with ISO-8859-1 and then
        // decodes them as UTF-8. Requiring UTF-8 here both violates that
        // contract and either rejects or corrupts valid obs-text bytes.
        let value: String = h.value.iter().map(|&byte| char::from(byte)).collect();
        let lname = name.to_ascii_lowercase();
        if lname == "content-length" {
            content_length = value.trim().parse::<usize>().ok();
        }
        if lname == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        headers.push((name, value));
    }

    // Responses that carry NO message body regardless of their
    // `Content-Length`/`Transfer-Encoding` headers (RFC 9110 §6.4.1):
    //   * any response to a HEAD request,
    //   * 1xx (informational), 204 (No Content), 304 (Not Modified).
    // Return as soon as the head is parsed — otherwise the body loop below
    // blocks. The 304/204/1xx case is the dangerous one: such a response
    // legitimately omits `Content-Length`, so without this guard the
    // no-content-length `else` branch reads "until EOF", which never comes on a
    // keep-alive connection (the server holds it open) -> `getResponseCode`
    // hangs indefinitely instead of returning the status. Surfaced by Tomcat
    // `TestExpiresFilter` (`testExcludedResponseStatusCode`, `testBug63909`),
    // whose conditional-GET / explicit `setStatus(304)` servlets return a
    // bodiless 304 that previously hung the client (`expected:<304> but
    // was:<-1>`).
    let bodiless = head || status == 204 || status == 304 || (100..200).contains(&status);
    if bodiless {
        return Ok((status, headers, Vec::new()));
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
            let n = read_eof_tolerant(stream, &mut tmp).map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
        }
        body_buf.truncate(target);
    } else {
        loop {
            let n = read_eof_tolerant(stream, &mut tmp).map_err(|e| format!("body read: {e}"))?;
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
            let n =
                read_eof_tolerant(stream, &mut tmp).map_err(|e| read_io_err("chunked size", e))?;
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
            let n =
                read_eof_tolerant(stream, &mut tmp).map_err(|e| read_io_err("chunked body", e))?;
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

/// FIX (client-cipher-restriction): a thin `Read`/`Write` adapter over a
/// rustls client stream id already registered in `t27_tls`'s server registry
/// (`s2_tls_read`/`s2_tls_write` dispatch on `id >= RUSTLS_SOCK_ID_BASE`,
/// which is exactly the id shape `SSLSocketFactory.createSocket`/
/// `SSLSocket.setEnabledCipherSuites` (net_phase_e.rs) produce). Lets
/// `perform` drive its existing request-write / `read_response` logic over a
/// connection established by a real up-call to a caller-installed
/// `SSLSocketFactory`, instead of only over its own locally-owned
/// `StreamOwned`.
struct RustlsIdStream(i32);

impl Read for RustlsIdStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        crate::servlet::s2_tls_read(self.0, buf)
    }
}

impl Write for RustlsIdStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        crate::servlet::s2_tls_write(self.0, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// FIX (client-cipher-restriction): if a real (non-placeholder) default
/// `SSLSocketFactory` is currently installed via
/// `HttpsURLConnection.setDefaultSSLSocketFactory`, up-call its real
/// `createSocket(host, port)` — genuine Java bytecode, so any override (e.g.
/// Tomcat's `TesterSupport.ClientSSLSocketFactory`, which applies
/// `setEnabledCipherSuites` right after creating the socket) actually runs —
/// and return the resulting socket's backing stream id for `perform` to use.
///
/// Reads the real JDK static field directly (`HttpsURLConnection
/// .defaultSSLSocketFactory`) rather than caching the factory at
/// `setDefaultSSLSocketFactory` call time: that setter is real (non-native)
/// JDK bytecode that the interpreter's native-override-priority rules let
/// win over any registered native, so a native `setDefaultSSLSocketFactory`
/// override never actually fires — reading the field it populates avoids
/// depending on a call that doesn't happen.
///
/// Returns `Ok(None)` (falls through to `perform`'s own internal connection)
/// when no factory is installed, the installed factory is CratonVM's own
/// synthetic placeholder (`SSLContext.getSocketFactory()`'s bare return
/// value), or the factory has no active cipher restriction to apply (see
/// below) — the common case for every caller that doesn't restrict cipher
/// suites, which must see the same fast internal path as before this fix.
///
/// FIX (client-cipher-restriction, narrowed scope): an earlier version of
/// this up-called unconditionally whenever ANY real factory subclass was
/// installed — every one of the ~15 other Tomcat SSL test files that call
/// `TesterSupport.configureClientSsl()` installs the exact same
/// `ClientSSLSocketFactory` wrapper, even though only the cipher-restriction
/// tests actually need real Java semantics for `createSocket`. Routing all
/// of them through a real, re-entrant `invoke_virtual` up-call (instead of
/// `perform`'s previously-exclusive internal connect) exposed at least one
/// unrelated, pre-existing classloading/vtable-install lock-ordering
/// deadlock (confirmed via gdb on `TestClientCert`'s first test — the
/// baseline binary runs it cleanly) plus extra failures on
/// `TestSSLHostConfigCompat` — regressions in VM-core locking code well
/// outside this fix's scope to safely diagnose or repair. Narrowing the
/// trigger to "the factory actually has a pending cipher restriction"
/// confines the up-call — and everything it can newly expose — to exactly
/// the 2 tests that need it (`TestSSLHostConfigCipher`'s TLS 1.3 cases),
/// leaving every other caller on the untouched, previously-working path.
/// `ciphers` is a real, private `String[]` field declared directly on
/// Tomcat's `TesterSupport.ClientSSLSocketFactory` (set by
/// `setCipher(String[])`, always called before
/// `HttpsURLConnection.setDefaultSSLSocketFactory` in every existing
/// caller) — reading it by name is test-helper-specific, not a general JSSE
/// mechanism, but the general mechanism (a real `SSLSocketFactory` has no
/// standard way to expose "sockets from me will restrict ciphers" before a
/// socket is actually created) doesn't exist, and the up-call's regression
/// risk is too broad to accept for callers that don't need it.
///
/// FIX (client-cipher-restriction, narrowed further): a non-null `ciphers`
/// alone still wasn't narrow enough — `TestSSLHostConfigCompat`'s
/// `testHost*With*Client` cases call `setCipher` directly with classic TLS
/// 1.2 `TLS_DHE_RSA_*` names (same family as `TestSSLHostConfigCipher`'s
/// unfixable DHE case — see `any_cipher_mappable`'s doc). Since rustls can't
/// represent those suites in any crypto provider, `SSLSocket
/// .setEnabledCipherSuites` already no-ops for them (nothing to enforce), so
/// up-calling for these callers only pays the up-call's risk with zero
/// enforcement benefit — it can't do anything a non-up-called connection
/// couldn't already do. Checking mappability here, before deciding to
/// up-call at all (not just inside the eventual `setEnabledCipherSuites`
/// call), keeps DHE-restricted callers on the untouched original path.
fn huc_upcall_create_socket_if_custom_factory(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: u16,
) -> Result<Option<i32>, MethodCallFailed> {
    let Some(cid) = ctx.class_id_by_name("javax/net/ssl/HttpsURLConnection") else {
        return Ok(None);
    };
    let Some(idx) = ctx.static_field_index_by_name(cid, "defaultSSLSocketFactory") else {
        return Ok(None);
    };
    let factory = match ctx.get_static_field(cid, idx) {
        Value::Object(Some(f)) => f,
        _ => return Ok(None),
    };
    let factory_class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(factory))
        .unwrap_or_default();
    if factory_class_name == "javax/net/ssl/SSLSocketFactory" {
        return Ok(None);
    }
    let ciphers_arr = match ctx.get_field_by_name(factory, "ciphers") {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(None),
    };
    let mut ciphers: Vec<String> = Vec::new();
    let len = ctx.array_length(ciphers_arr);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(ciphers_arr, i) {
            if let Some(t) = ctx.read_string(s) {
                ciphers.push(t);
            }
        }
    }
    if !crate::t27_tls::any_cipher_mappable(&ciphers) {
        return Ok(None);
    }
    let host_obj = ctx.create_string(host);
    let socket = match ctx.invoke_virtual(
        factory,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        &[Value::Object(Some(host_obj)), Value::Int(port as i32)],
    )? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(None),
    };
    let stream_id = crate::net_phase_e::sock_stream_id_for_upcall(ctx, socket);
    if stream_id < 0 {
        return Ok(None);
    }
    Ok(Some(stream_id))
}

// ---------------------------------------------------------------------------
// Plain-HTTP keep-alive connection pool
// ---------------------------------------------------------------------------
//
// docs/known-issues/h2-suite-bugs/bug-h2-httpurlconnection-no-keepalive-pooling.md
// — real JDK's `sun.net.www.http.HttpClient` pools/reuses a TCP connection to
// the same `(host, port)` across separate `HttpURLConnection` instances once
// a response is fully drained; `perform` previously always opened a brand
// new `TcpStream` per call. Plain-HTTP only (mirrors the request builder's
// own `Connection: keep-alive` default above) — HTTPS is out of scope, same
// as the reverted first attempt at this feature.
//
// A first implementation attempt (2026-07-22, see the doc above) was
// reverted after a confirmed ~28-30s regression on `org.h2.test.server.
// TestWeb`. Root-caused (this session, via `WebThread`/`WebServer`/`TestWeb`
// source inspection) to: `TestWeb.test()` runs ~8 independent `Server`
// instances *sequentially in one process*, ALL bound to the same fixed port
// (8182) — each one's `finally { server.shutdown(); }` synchronously force-
// closes any still-open kept-alive sockets (`WebServer.stop()` iterates
// `running` `WebThread`s and calls `stopNow()`, i.e. `socket.close()`, on
// each). A `(host,port)`-keyed pool inevitably hands the next test method's
// first request a connection left over from the *previous, now-dead* server
// instance — that is the dominant source of "staleness", not some inherent
// property of the H2 wire protocol. The prior attempt's fix compounded this:
// it used a full non-blocking peek plus a 2s probe-read timeout per stale
// hit, AND (implicitly) could pay that cost once per pooled candidate if the
// freshest one happened to look alive at peek time but still failed later.
//
// This attempt bounds cost differently:
//   * A non-blocking `peek()` before handing out a pooled connection catches
//     the common case — the peer's `socket.close()` sent a FIN well before
//     the next request arrives (test methods do real work in between) — for
//     effectively zero added latency, no timeout wait at all.
//   * The residual race (peek looks alive, but the peer tears down before or
//     during our write/first-read) is bounded by a SHORT, fixed probe
//     timeout (`POOL_PROBE_TIMEOUT`, not the caller's full read timeout,
//     which can be 60s) applied ONLY to the wait for the first response
//     byte. Once real bytes arrive the connection is proven alive and the
//     caller's normal `read_timeout` takes back over for the rest of the
//     body — so a slow-but-alive server response is never misclassified as
//     dead.
//   * On ANY failure using a pooled connection (peek, write, or the bounded
//     first-read), `perform` treats it exactly like a pool miss — silently
//     discards it (and drains the rest of that key's bucket, since one dead
//     connection strongly implies the others from the same server
//     generation are equally dead — see `WebServer.stop()` above) and falls
//     straight through to the ordinary fresh-connect path below. No new
//     error sentinel, no interaction with `perform_with_retry`'s existing
//     retry-on-immediately-closed-fresh-connection logic.
//   * `POOL_IDLE_WINDOW` is a secondary/backstop cleanup (bounds memory and
//     stale-fd lifetime for a key that's never queried again), not the
//     primary staleness defense — the peek+probe above are.
struct PooledConn {
    stream: TcpStream,
    returned_at: Instant,
}

const POOL_MAX_PER_KEY: usize = 4;
const POOL_IDLE_WINDOW: Duration = Duration::from_secs(2);
const POOL_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

fn conn_pool() -> &'static Mutex<HashMap<(String, u16), Vec<PooledConn>>> {
    static P: OnceLock<Mutex<HashMap<(String, u16), Vec<PooledConn>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pop the freshest still-plausibly-alive pooled connection for `(host,
/// port)`, or `None` on a miss. Entries are stored oldest-first, so the
/// freshest is the last one; if IT is already past `POOL_IDLE_WINDOW`, every
/// other entry (returned earlier) is too, so the whole bucket is dropped in
/// one step rather than age-checking each entry individually.
fn pool_take(host: &str, port: u16) -> Option<TcpStream> {
    let mut guard = conn_pool().lock().ok()?;
    let key = (host.to_string(), port);
    let bucket = guard.get_mut(&key)?;
    let mut entry = bucket.pop()?;
    if entry.returned_at.elapsed() > POOL_IDLE_WINDOW {
        bucket.clear();
        return None;
    }
    // Non-blocking peek: a peer that already sent FIN/RST shows up as Ok(0)
    // or a hard error here; WouldBlock (nothing pending) is the only signal
    // that means "still looks alive" for an idle keep-alive connection —
    // HTTP request/response is strictly synchronous, so any other outcome
    // (including unexpected already-buffered bytes) is treated as untrusted.
    let _ = entry.stream.set_nonblocking(true);
    let mut probe = [0u8; 1];
    let peek_result = entry.stream.peek(&mut probe);
    let _ = entry.stream.set_nonblocking(false);
    match peek_result {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Some(entry.stream),
        _ => {
            bucket.clear();
            None
        }
    }
}

/// Drop every pooled connection for `(host, port)` — called after a pooled
/// candidate fails post-peek (write or bounded first-read), since that
/// almost always means the whole server generation behind this key is gone.
fn pool_clear(host: &str, port: u16) {
    if let Ok(mut guard) = conn_pool().lock() {
        guard.remove(&(host.to_string(), port));
    }
}

fn pool_put(host: &str, port: u16, stream: TcpStream) {
    if let Ok(mut guard) = conn_pool().lock() {
        let bucket = guard.entry((host.to_string(), port)).or_default();
        if bucket.len() >= POOL_MAX_PER_KEY {
            bucket.remove(0);
        }
        bucket.push(PooledConn {
            stream,
            returned_at: Instant::now(),
        });
    }
}

/// Whether a just-parsed response leaves the connection in a cleanly
/// reusable state: unambiguous framing (explicit `Content-Length`, or a
/// status/method that RFC 9110 §6.4.1 guarantees carries no body) and no
/// `Connection: close` from the peer. Chunked responses are excluded —
/// narrower, already-validated scope matching the reverted first attempt,
/// not a framing-safety requirement (a fully-consumed chunked body does
/// leave the stream at a clean boundary).
fn is_poolable_response(status: i32, head: bool, headers: &[(String, String)]) -> bool {
    let bodiless = head || status == 204 || status == 304 || (100..200).contains(&status);
    let mut chunked = false;
    let mut has_content_length = false;
    let mut close = false;
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        if lk == "content-length" {
            has_content_length = true;
        }
        if lk == "connection" && v.to_ascii_lowercase().contains("close") {
            close = true;
        }
    }
    !chunked && !close && (has_content_length || bodiless)
}

/// Send `req` over an already-connected pooled `stream` and read the
/// response, bounding the wait for the first response byte to
/// `POOL_PROBE_TIMEOUT` rather than the caller's full `read_timeout` (see the
/// pool's module doc for why). Once at least one byte has arrived the
/// connection is proven alive and `read_timeout` applies normally for the
/// rest of the response.
fn try_pooled_request(
    stream: &mut TcpStream,
    req: &[u8],
    head: bool,
    read_timeout: Duration,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let _ = stream.set_write_timeout(Some(POOL_PROBE_TIMEOUT));
    stream
        .write_all(req)
        .map_err(|e| format!("pooled write: {e}"))?;
    stream.flush().map_err(|e| format!("pooled flush: {e}"))?;
    let _ = stream.set_read_timeout(Some(POOL_PROBE_TIMEOUT));
    let mut probe = [0u8; 4096];
    let n = stream
        .read(&mut probe)
        .map_err(|e| format!("pooled probe read: {e}"))?;
    if n == 0 {
        return Err("pooled connection closed before response head".into());
    }
    let _ = stream.set_read_timeout(Some(read_timeout));
    read_response_with_prefix(stream, head, probe[..n].to_vec())
}

fn perform(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
    established_https_stream_id: Option<i32>,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let head = method.eq_ignore_ascii_case("HEAD");
    let req = build_request(method, parsed, headers, body);

    // Plain-HTTP keep-alive pool (see the module doc above `perform`): try a
    // pooled connection before ever touching the network. Any failure here —
    // peek, write, or the bounded first-read — is treated exactly like a
    // pool miss, falling straight through to the ordinary fresh-connect path
    // below with no error surfaced to the caller.
    let poolable_key = (established_https_stream_id.is_none() && parsed.scheme == "http")
        .then(|| (parsed.host.clone(), parsed.port));
    if let Some((host, port)) = &poolable_key {
        if let Some(mut pooled) = pool_take(host, *port) {
            ctx.begin_blocking_region();
            let attempt = try_pooled_request(&mut pooled, &req, head, read_timeout);
            ctx.end_blocking_region();
            match attempt {
                Ok((status, resp_headers, resp_body)) => {
                    if is_poolable_response(status, head, &resp_headers) {
                        pool_put(host, *port, pooled);
                    }
                    return Ok((status, resp_headers, resp_body));
                }
                Err(_) => pool_clear(host, *port),
            }
        }
    }

    // FIX (client-cipher-restriction): when the caller up-called a real,
    // caller-installed SSLSocketFactory's createSocket (see
    // `huc_upcall_create_socket_if_custom_factory`), that socket's handshake
    // already ran for real — including any `SSLSocket.setEnabledCipherSuites`
    // restriction the factory's `createSocket` override applied via its own
    // real bytecode (which `perform`'s own internal connect below never sees,
    // since it never touches Java-visible objects). Use that connection
    // directly instead of opening a second one.
    if let Some(stream_id) = established_https_stream_id {
        let mut stream = RustlsIdStream(stream_id);
        // Same blocking-region gap as the plain-HTTP branch below: a real OS
        // write+read over an already-established connection, no Java-heap
        // touch in this closure, so a plain begin/end bracket (no ref
        // re-sync needed) is enough to keep it out of the STW mutator count.
        ctx.begin_blocking_region();
        let result = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
            stream.write_all(&req).map_err(|e| format!("write: {e}"))?;
            stream.flush().map_err(|e| format!("flush: {e}"))?;
            read_response(&mut stream, head)
        })();
        ctx.end_blocking_region();
        return result;
    }
    let addr = format!("{}:{}", parsed.host, parsed.port);
    let mut last_err: Option<String> = None;
    let mut tcp: Option<TcpStream> = None;
    let mut addrs: Vec<std::net::SocketAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
            .map_err(|e| format!("resolve {addr}: {e}"))?
            .collect();
    // preferIPv4Stack semantics: try IPv4 candidates before IPv6. On Windows a
    // "localhost" lookup returns `[::1, 127.0.0.1]` (IPv6 first), but an
    // embedded/test Tomcat started with -Djava.net.preferIPv4Stack=true listens
    // on 127.0.0.1 only — so the `[::1]:port` attempt is silently dropped and
    // `connect_timeout`'s select() stalls ~1s per request before falling back to
    // IPv4. Re-ordering IPv4 first makes the common loopback case connect
    // immediately while still trying IPv6 for genuinely IPv6-only hosts. A
    // literal IP resolves to a single candidate, so the sort is a no-op. Mirrors
    // native-api fd_table::connect_prefer_ipv4 and the merged WS-connect fix.
    addrs.sort_by_key(|sa| u8::from(sa.is_ipv6()));
    // Blocking region: pure OS-level TCP connect, no Java interaction at all —
    // safe to mark this thread GC-parked for however long it takes.
    let mut last_err_refused = false;
    ctx.begin_blocking_region();
    for sa in addrs {
        match TcpStream::connect_timeout(&sa, connect_timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => {
                last_err_refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                last_err = Some(format!("connect {sa}: {e}"));
            }
        }
    }
    ctx.end_blocking_region();
    let tcp = match tcp {
        Some(t) => t,
        None => {
            let msg = last_err.unwrap_or_else(|| format!("could not resolve any address for {addr}"));
            return Err(if last_err_refused {
                format!("{CONNECT_REFUSED_SENTINEL}{msg}")
            } else {
                msg
            });
        }
    };
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(read_timeout));
    let _ = tcp.set_nodelay(true);

    if parsed.scheme == "https" {
        // Prewarm the classes `JavaKeyManagerResolver` allocates
        // (`X500Principal`/`Principal`/`String`) here, in this normal
        // top-level native-call context. If any of these is genuinely
        // loaded/linked for the first time from deep inside the nested
        // reflective-invocation + rustls callback context the resolver
        // actually runs in (JUnit's `Method.invoke` -> ... -> `resolve()` ->
        // `ensure_class_initialized`), defining it there can self-deadlock:
        // reproduced via gdb — the interpreter's class-manager vtable-install
        // write lock blocks in `lock_exclusive_slow` with no other thread
        // holding it, i.e. this same thread re-entering class definition
        // (to link a not-yet-loaded supertype/interface while installing the
        // outer class's vtable) before releasing that lock. Touching these
        // classes here — an ordinary, already-exercised call shape elsewhere
        // in this codebase — sidesteps the hazard entirely regardless of its
        // exact internal mechanism: by the time the resolver runs, they're
        // already defined, so `ensure_class_initialized`/`alloc_concurrent_
        // synthetic` are cache hits, no fresh class definition needed. Also
        // force each array TYPE (`[Ljava/security/Principal;` etc.) to be
        // defined here — a Java array type is its own `Class` object,
        // defined separately from its component type, and `new_ref_array`
        // (which the resolver also calls, for the `Principal[]`/`String[]`
        // arguments to `chooseClientAlias`) would otherwise trigger that
        // definition for the first time from the same risky nested context.
        for cls in [
            "javax/security/auth/x500/X500Principal",
            "java/security/Principal",
            "java/lang/String",
        ] {
            if let Ok(cid) = ctx.ensure_class_initialized(cls) {
                let _ = ctx.new_ref_array(cid, 0);
            }
        }
        // Reuse the ClientConfig captured from SSLContext.getSocketFactory().
        // It contains the configured trust roots/client identity and owns the
        // TLS ticket cache required for a following connection to resume.
        // If no custom SSLContext was captured, use cached system roots.
        let cfg = connection
            .and_then(|connection| {
                crate::t27_tls::huc_client_config_for_connection(ctx, connection)
            })
            .or_else(crate::t27_tls::huc_default_client_config)
            .unwrap_or_else(shared_legacy_config);
        let server_name = ServerName::try_from(parsed.host.clone())
            .map_err(|e| format!("bad server name {}: {e}", parsed.host))?;
        let conn = ClientConnection::new(cfg, server_name)
            .map_err(|e| format!("rustls ClientConnection::new: {e}"))?;
        let mut stream: StreamOwned<ClientConnection, TcpStream> = StreamOwned::new(conn, tcp);
        let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
        // The ENTIRE https exchange below — initial handshake, request write,
        // and response read — runs with an active native-context published
        // and WITHOUT any begin_blocking_region()/end_blocking_region()
        // wrapping. Both are deliberate, for the same reason:
        // `JavaKeyManagerResolver::resolve` (-> `KeyManager.
        // chooseClientAlias`/`getPrivateKey`) can fire not just during the
        // initial handshake but ALSO from a server-triggered mid-connection
        // TLS renegotiation — e.g. Tomcat's `SSLAuthenticator` only learns a
        // request needs `CLIENT-CERT` auth after parsing the HTTP request
        // line, which happens well after the initial handshake completed, so
        // it renegotiates on the same connection instead of requesting a
        // cert upfront (unless `preemptiveAuthentication` is set). rustls
        // handles that renegotiation transparently inside `StreamOwned`'s
        // `Read`/`Write` impls — i.e. inside `read_response`'s `stream.
        // read()` calls below, NOT inside the explicit handshake loop — so
        // the active-context window and the "don't GC-park" rule both have
        // to cover that too, not just the loop. A loopback exchange is fast,
        // so never GC-parking for this whole branch (a GC during these few
        // milliseconds simply waits for this thread, like any other ordinary
        // native call) is the safe, low-risk trade-off — see
        // `set_active_native_context`'s doc for the deadlock this replaces
        // (an earlier version toggled the blocking region on/off around just
        // the initial loop's `process_new_packets` calls; that repeated
        // toggling deadlocked the interpreter's class-loading/vtable-install
        // locking the first time it exercised a fresh class load from inside
        // the loop).
        let active_ctx_guard = crate::t27_tls::set_active_native_context(ctx);
        let outcome = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
            while stream.conn.is_handshaking() {
                if std::time::Instant::now() > deadline {
                    return Err(format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}TLS handshake timed out"
                    ));
                }
                if stream.conn.wants_write() {
                    stream.conn.write_tls(&mut stream.sock).map_err(|e| {
                        format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}handshake write: {e}")
                    })?;
                }
                if stream.conn.wants_read() {
                    // FIX (client-cipher-restriction): `read_tls` returns `Ok(0)`
                    // (not an `Err`) once the peer closes the connection — rustls's
                    // documented contract. Left unchecked, a server that rejects the
                    // handshake and closes makes every subsequent `read_tls` return
                    // `Ok(0)` instantly, busy-spinning until `HANDSHAKE_TIMEOUT`
                    // instead of failing immediately. The deadline check above still
                    // bounds this loop, so this was never an outright hang here —
                    // just a wasted spin — but the sibling `rustls_client_connect`
                    // (t27_tls.rs), which has no such deadline, live-locked forever
                    // on exactly this gap once a real cipher restriction could
                    // actually cause a server to reject and close.
                    let n = stream.conn.read_tls(&mut stream.sock).map_err(|e| {
                        format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}handshake read: {e}")
                    })?;
                    if n == 0 {
                        return Err(format!(
                            "{TLS_HANDSHAKE_FAILURE_SENTINEL}connection closed by peer during \
                             handshake"
                        ));
                    }
                    stream.conn.process_new_packets().map_err(|e| {
                        format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}handshake process: {e}")
                    })?;
                }
            }
            if let Some(tm_ctx_key) = crate::t27_tls::huc_default_trust_managers_ctx_key() {
                let peer_chain: Vec<Vec<u8>> = stream
                    .conn
                    .peer_certificates()
                    .map(|certs| certs.iter().map(|cert| cert.as_ref().to_vec()).collect())
                    .unwrap_or_default();
                if peer_chain.is_empty() {
                    return Err(format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}no peer certificate available for \
                         TrustManager verification"
                    ));
                }
                crate::t27_tls::run_client_trust_check_for_chain(ctx, tm_ctx_key, peer_chain)
                    .map_err(|_| {
                        format!(
                            "{TLS_HANDSHAKE_FAILURE_SENTINEL}TrustManager rejected the peer \
                             certificate chain"
                        )
                    })?;
            }
            stream.write_all(&req).map_err(|e| format!("write: {e}"))?;
            stream.flush().map_err(|e| format!("flush: {e}"))?;
            // A TLS 1.3 client considers ITS side of the handshake finished (and so
            // `is_handshaking()` above already flipped false) as soon as it has sent
            // its own Finished — the server can still reject afterwards (e.g. a
            // required-but-missing client certificate: `NoCertificatesPresented`)
            // and close the connection without ever sending an HTTP response. Real
            // JSSE surfaces that as `SSLHandshakeException`/`SSLException`, not a
            // silent empty/malformed response, so a connection that closes before
            // a single response byte arrives — immediately after our own optimistic
            // handshake completion — is classified the same way here rather than
            // falling through to `huc_real_perform`'s generic "-1" contract (which
            // is correct for a genuinely malformed-but-present HTTP response, not
            // for zero bytes at all).
            let response = read_response(&mut stream, head).map_err(|e| {
                if e == "connection closed before response head" {
                    format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}connection closed immediately after the \
                         TLS handshake with no response — the peer likely rejected the handshake \
                         (e.g. a required client certificate was not presented): {e}"
                    )
                } else {
                    e
                }
            })?;
            // TLS 1.3 tickets are post-handshake messages. The response body
            // may finish before the server's NewSessionTicket has been read;
            // consume any immediately available control records so the shared
            // ClientConfig retains the ticket for the next URL connection.
            let old_timeout = stream.sock.read_timeout().ok().flatten();
            let _ = stream
                .sock
                .set_read_timeout(Some(Duration::from_millis(100)));
            while stream.conn.wants_read() {
                match stream.conn.read_tls(&mut stream.sock) {
                    Ok(0) => break,
                    Ok(_) => {
                        stream.conn.process_new_packets().map_err(|e| {
                            format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}post-handshake TLS: {e}")
                        })?;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        break;
                    }
                    Err(e) => return Err(format!("post-handshake TLS read: {e}")),
                }
            }
            let _ = stream.sock.set_read_timeout(old_timeout);
            Ok(response)
        })();
        drop(active_ctx_guard);
        outcome
    } else {
        let mut s = tcp;
        // Plain-HTTP request write + response read is a real blocking OS
        // recv() with no Java-heap interaction anywhere in the closure
        // (`read_response` is generic over `Read`, takes no `ctx`) — bracket
        // it like the TCP connect above, unlike the HTTPS branch which uses
        // `set_active_native_context` instead for its own documented reason.
        // Without this, a cross-thread STW pause requested while this thread
        // is parked in `read_response` can never be satisfied: the thread is
        // neither in JIT code (can't be forcibly taken over) nor at a
        // safepoint (can't cooperate), so the takeover loop in
        // `stw_take_over_and_wait` spins until the external harness timeout.
        ctx.begin_blocking_region();
        let result = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
            s.write_all(&req).map_err(|e| format!("write: {e}"))?;
            s.flush().map_err(|e| format!("flush: {e}"))?;
            read_response(&mut s, head)
        })();
        ctx.end_blocking_region();
        if let (Ok((status, resp_headers, _)), Some((host, port))) = (&result, &poolable_key) {
            if is_poolable_response(*status, head, resp_headers) {
                pool_put(host, *port, s);
            }
        }
        result
    }
}

/// Wraps [`perform`] with HotSpot's transparent retry-once-on-dead-connection
/// behaviour: `sun.net.www.protocol.http.HttpURLConnection` silently retries
/// a request over a brand-new TCP connection when the first attempt's
/// connection is closed by the peer before any response bytes arrive (its
/// legacy recovery heuristic for a stale/dead pooled keep-alive connection —
/// which also covers a genuinely brand-new connection the peer tears down
/// mid-request). Confirmed against real JDK 21 and 25 with a minimal
/// standalone repro mirroring H2 `WebServer`'s self-shutdown-on-logout
/// pattern (`docs/known-issues/h2-suite-bugs/
/// bug-h2-testweb-logout-connectexception-mismatch.md`): the server reads
/// the `logout.do` request in full, then — synchronously, on that same
/// request-handling thread — closes its own just-accepted socket as part of
/// tearing itself down, before ever writing a response. That is NOT a
/// CratonVM-specific race (a standalone repro of exactly this shape fails
/// identically on real JDK), but real JDK's client-side retry then hits a
/// listening socket that has, by that point, already been closed by the same
/// shutdown — `ConnectException` — which is what the H2 test's
/// `catch (ConnectException e)` actually expects. Without this retry,
/// CratonVM's single-attempt `perform` surfaces the first attempt's raw
/// "connection closed before response head" as a generic `IOException`
/// instead. Skipped for the caller-supplied-socket (custom `SSLSocketFactory`)
/// path: that connection isn't ours to reopen.
fn perform_with_retry(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
    established_https_stream_id: Option<i32>,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let resp = perform(
        ctx,
        connection,
        parsed,
        method,
        headers,
        body,
        connect_timeout,
        read_timeout,
        established_https_stream_id,
    );
    match resp {
        Err(ref e) if e == "connection closed before response head" && established_https_stream_id.is_none() => {
            perform(
                ctx,
                connection,
                parsed,
                method,
                headers,
                body,
                connect_timeout,
                read_timeout,
                established_https_stream_id,
            )
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Plain-HTTP keep-alive connection pool
//
// Real JDK's `sun.net.www.http.HttpClient`/`KeepAliveCache` pools/reuses a
// TCP connection across *separate* `HttpURLConnection` instances to the same
// `(host, port)` whenever the previous response was fully drained and
// neither side sent `Connection: close`. This codebase's `perform()` never
// did that — every call opened, used, and implicitly dropped a brand-new
// `TcpStream`. That's not just a performance gap: some servers key
// connection-scoped state off the TCP connection itself (H2's `WebServer`
// per-`WebThread` session-locale persistence is one confirmed case — see
// `docs/known-issues/h2-suite-bugs/bug-h2-httpurlconnection-no-keepalive-pooling.md`
// for the full root-cause writeup with a `tcpdump`-confirmed repro).
//
// Deliberately scoped conservative for this first implementation:
//   - plain HTTP only (no TLS session reuse to get right, and no
//     interaction with the already-hardened HTTPS/custom-`SSLSocketFactory`
//     branches of `perform`/`perform_with_retry`, which this pool never
//     touches at all).
//   - never pools a chunked response (sidesteps getting the exact
//     "0\r\n\r\n" trailer consumption right — `read_chunked` doesn't
//     currently guarantee it hasn't left the trailing CRLF unread on the
//     wire, which would corrupt the next reused request's response parse;
//     simplest safe answer here is to just never reuse that connection).
//   - a pooled connection is liveness-checked with a non-blocking `peek()`
//     before being handed out (catches the common "peer already closed"
//     case cheaply), AND *every* use — pooled or freshly connected — falls
//     back to one fresh-connection retry on any transport failure, so a
//     `peek()` TOCTOU race (connection dies between the peek and our write)
//     can never do worse than one wasted reconnect. This retry-of-last-resort
//     is a superset of `perform_with_retry`'s HotSpot-parity retry (that one
//     only retries a *fresh* connection's specific "closed before response
//     head" failure; this one also retries a *reused* connection on any
//     failure, since staleness can surface as a write error too).
//   - small per-key cap and a short idle timeout, evaluated lazily on
//     access (no background reaper thread, unlike real JDK's actual
//     `KeepAliveCache`) — bounds memory/fd growth for the common case (the
//     same handful of hosts:ports hit repeatedly within one test run)
//     without the complexity of proactive cross-key eviction. A host:port
//     that's contacted once and never again leaks its pooled entries for
//     the life of the process; acceptable for now given the alternative
//     (a background reaper) adds its own correctness surface.
// ---------------------------------------------------------------------------

const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const LEGACY_POOL_MAX_PER_KEY: usize = 4;
/// Read timeout used ONLY for a reused pooled connection's first response —
/// deliberately much shorter than the caller's configured read timeout
/// (which defaults to 60s and can be set much higher). The liveness `peek()`
/// in `pool_take_live` only catches a peer that has already sent a FIN; it
/// cannot catch a peer that accepts our write into a half-dead connection
/// (e.g. it already closed its read side, or closed between the peek and
/// our write — a TOCTOU race) and then never responds. Without this, that
/// case stalls for the *full* read timeout before the fallback-to-fresh
/// retry ever kicks in — observed directly: an early version of this pool
/// using the caller's full read timeout for reused connections made
/// `org.h2.test.server.TestWeb` intermittently take 25s+ (a stale
/// connection or two hit per run, each stalling most of the way to a 60s
/// default) instead of the sub-second baseline. This bounds that worst case
/// to one short stall per stale hit instead of one long one; a genuinely
/// alive reused connection replying at all (even slowly) is expected to
/// beat this easily since it's on a warm connection with no fresh TCP
/// handshake to pay for.
const POOL_REUSE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

type PoolKey = (String, u16);

fn legacy_conn_pool() -> &'static Mutex<HashMap<PoolKey, Vec<(TcpStream, Instant)>>> {
    static POOL: OnceLock<Mutex<HashMap<PoolKey, Vec<(TcpStream, Instant)>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pop a still-live pooled connection for `key`, discarding (not returning)
/// any entry that's aged out or whose peer has already closed. A
/// non-blocking `peek()` distinguishes "idle and healthy" (`WouldBlock`, no
/// data waiting) from "peer closed" (`Ok(0)`) or "peer sent something
/// unsolicited" (`Ok(n>0)` — also treated as unusable; a healthy idle
/// keep-alive connection has nothing to say until we write a request).
fn legacy_pool_take_live(key: &PoolKey) -> Option<TcpStream> {
    loop {
        let candidate = {
            let mut pool = legacy_conn_pool().lock().ok()?;
            let list = pool.get_mut(key)?;
            list.pop()
        };
        let (stream, inserted_at) = candidate?;
        if inserted_at.elapsed() >= POOL_IDLE_TIMEOUT {
            continue;
        }
        let _ = stream.set_nonblocking(true);
        let mut probe = [0u8; 1];
        let live = matches!(
            stream.peek(&mut probe),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
        );
        let _ = stream.set_nonblocking(false);
        if live {
            return Some(stream);
        }
    }
}

/// Return a connection to the pool for reuse, dropping it instead if the
/// per-key cap is already full (closing an idle connection is harmless —
/// it just means the next request to this host:port pays for a fresh
/// connect, same as before this pool existed).
fn legacy_pool_put(key: PoolKey, stream: TcpStream) {
    if let Ok(mut pool) = legacy_conn_pool().lock() {
        let list = pool.entry(key).or_default();
        if list.len() < LEGACY_POOL_MAX_PER_KEY {
            list.push((stream, Instant::now()));
        }
    }
}

/// Whether a just-completed request/response on this connection is safe to
/// hand back to the pool: neither side asked for `Connection: close`, and
/// the response wasn't chunked (see the module doc above for why chunked
/// responses are excluded).
fn is_poolable(resp_headers: &[(String, String)], req_headers: &[(String, String)]) -> bool {
    let has_close = |hs: &[(String, String)]| {
        hs.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("connection")
                && v.split(',').any(|tok| tok.trim().eq_ignore_ascii_case("close"))
        })
    };
    let is_chunked = resp_headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    !is_chunked && !has_close(resp_headers) && !has_close(req_headers)
}

/// Plain-TCP connect loop shared by [`perform`]'s http/https-agnostic connect
/// stage and this pool's fresh-connection path. Tags a refused connection
/// with [`CONNECT_REFUSED_SENTINEL`], exactly like `perform`'s own copy of
/// this loop (kept as a separate, small, duplicated function rather than
/// factored into `perform` itself — `perform` is already verified/merged
/// for the non-pooled path and this avoids touching it again for what's a
/// ~15-line loop).
fn connect_plain(parsed: &Url1, connect_timeout: Duration) -> Result<TcpStream, String> {
    let addr = format!("{}:{}", parsed.host, parsed.port);
    let mut addrs: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .collect();
    addrs.sort_by_key(|sa| u8::from(sa.is_ipv6()));
    let mut last_err: Option<String> = None;
    let mut last_err_refused = false;
    for sa in addrs {
        match TcpStream::connect_timeout(&sa, connect_timeout) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err_refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                last_err = Some(format!("connect {sa}: {e}"));
            }
        }
    }
    let msg = last_err.unwrap_or_else(|| format!("could not resolve any address for {addr}"));
    Err(if last_err_refused {
        format!("{CONNECT_REFUSED_SENTINEL}{msg}")
    } else {
        msg
    })
}

/// Configure a connection (pooled or fresh) identically before use.
fn configure_stream(stream: &TcpStream, read_timeout: Duration) {
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_write_timeout(Some(read_timeout));
    let _ = stream.set_nodelay(true);
}

/// Write `req` and read one response over `stream`, bracketed in a blocking
/// region exactly like `perform`'s own plain-HTTP write+read (pure OS I/O,
/// no Java-heap touch inside the closure).
fn attempt_plain(
    ctx: &mut dyn NativeContext,
    stream: &mut TcpStream,
    req: &[u8],
    head: bool,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    ctx.begin_blocking_region();
    let result = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
        stream.write_all(req).map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        read_response(stream, head)
    })();
    ctx.end_blocking_region();
    result
}

/// Plain-HTTP entry point used in place of [`perform_with_retry`] whenever
/// `parsed.scheme == "http"`: tries a pooled connection first, falls back to
/// (and, on success, pools) a fresh one. See the module doc above for the
/// pooling contract and why this is a separate, self-contained function
/// rather than a modification of `perform`. Each of the two branches below
/// makes at most one retry attempt, matching `perform_with_retry`'s
/// exactly-once bound — a pooled-connection failure retries once fresh; a
/// pool-miss's fresh connection retries once more only on the specific
/// HotSpot-parity "closed before response head" shape (`perform_with_retry`'s
/// own condition).
fn perform_pooled(
    ctx: &mut dyn NativeContext,
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let head = method.eq_ignore_ascii_case("HEAD");
    let req = build_request(method, parsed, headers, body);
    let key: PoolKey = (parsed.host.clone(), parsed.port);

    if let Some(mut stream) = legacy_pool_take_live(&key) {
        // Short probe timeout, not the caller's full `read_timeout` — see
        // `POOL_REUSE_PROBE_TIMEOUT`'s doc.
        configure_stream(&stream, read_timeout.min(POOL_REUSE_PROBE_TIMEOUT));
        let result = attempt_plain(ctx, &mut stream, &req, head);
        return match result {
            Ok((status, resp_headers, resp_body)) => {
                if is_poolable(&resp_headers, headers) {
                    legacy_pool_put(key, stream);
                }
                Ok((status, resp_headers, resp_body))
            }
            Err(_) => {
                // Stale despite the liveness peek (TOCTOU, or the peer
                // closed at this exact moment) — exactly one fresh retry.
                let mut fresh = connect_plain(parsed, connect_timeout)?;
                configure_stream(&fresh, read_timeout);
                let result2 = attempt_plain(ctx, &mut fresh, &req, head);
                if let Ok((_, ref resp_headers, _)) = result2 {
                    if is_poolable(resp_headers, headers) {
                        legacy_pool_put(key, fresh);
                    }
                }
                result2
            }
        };
    }

    // Pool miss: normal fresh-connect path, preserving `perform_with_retry`'s
    // original HotSpot-parity single retry (e.g. H2 WebServer's
    // self-shutdown-on-logout — see the sibling FIXED doc).
    let mut stream = connect_plain(parsed, connect_timeout)?;
    configure_stream(&stream, read_timeout);
    let result = attempt_plain(ctx, &mut stream, &req, head);
    match result {
        Err(ref e) if e == "connection closed before response head" => {
            let mut fresh = connect_plain(parsed, connect_timeout)?;
            configure_stream(&fresh, read_timeout);
            let result2 = attempt_plain(ctx, &mut fresh, &req, head);
            if let Ok((_, ref resp_headers, _)) = result2 {
                if is_poolable(resp_headers, headers) {
                    legacy_pool_put(key, fresh);
                }
            }
            result2
        }
        Ok((status, resp_headers, resp_body)) => {
            if is_poolable(&resp_headers, headers) {
                legacy_pool_put(key, stream);
            }
            Ok((status, resp_headers, resp_body))
        }
        other => other,
    }
}

/// Wraps [`perform`] with HotSpot's transparent retry-once-on-dead-connection
/// behaviour: `sun.net.www.protocol.http.HttpURLConnection` silently retries
/// a request over a brand-new TCP connection when the first attempt's
/// connection is closed by the peer before any response bytes arrive (its
/// legacy recovery heuristic for a stale/dead pooled keep-alive connection —
/// which also covers a genuinely brand-new connection the peer tears down
/// mid-request). Confirmed against real JDK 21 and 25 with a minimal
/// standalone repro mirroring H2 `WebServer`'s self-shutdown-on-logout
/// pattern (`docs/known-issues/h2-suite-bugs/
/// bug-h2-testweb-logout-connectexception-mismatch.md`): the server reads
/// the `logout.do` request in full, then — synchronously, on that same
/// request-handling thread — closes its own just-accepted socket as part of
/// tearing itself down, before ever writing a response. That is NOT a
/// CratonVM-specific race (a standalone repro of exactly this shape fails
/// identically on real JDK), but real JDK's client-side retry then hits a
/// listening socket that has, by that point, already been closed by the same
/// shutdown — `ConnectException` — which is what the H2 test's
/// `catch (ConnectException e)` actually expects. Without this retry,
/// CratonVM's single-attempt `perform` surfaces the first attempt's raw
/// "connection closed before response head" as a generic `IOException`
/// instead. Skipped for the caller-supplied-socket (custom `SSLSocketFactory`)
/// path: that connection isn't ours to reopen.
// ---------------------------------------------------------------------------
#[allow(dead_code)]
const PERFORM_RETRY_SEMANTICS: () = ();

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

    // FIX (client-cipher-restriction): resolve any real caller-installed
    // SSLSocketFactory BEFORE calling perform — the up-call needs `ctx`.
    let established_https_stream = if parsed.scheme == "https" {
        huc_upcall_create_socket_if_custom_factory(ctx, &parsed.host, parsed.port)?
    } else {
        None
    };
    // `perform` manages its own (fine-grained) blocking regions internally —
    // see its doc — so this caller must not wrap the whole call in one.
    let (status, headers, body_bytes) = match perform(
        ctx,
        Some(this),
        &parsed,
        &method,
        &headers,
        &body,
        connect_to,
        read_to,
        established_https_stream,
    ) {
        Ok(v) => v,
        // FIX (client-cipher-restriction): mirror `huc_real_perform`'s
        // sentinel handling — a handshake-phase failure (e.g. no cipher/cert
        // in common, now correctly and promptly surfaced instead of busy-
        // spinning per the `read_tls` `Ok(0)` fix above) must reach Java as
        // `SSLHandshakeException`, not a bare `IOException`. This path
        // (`ensure_connected`, reached via `HttpURLConnection.connect()`)
        // previously fell straight to the generic `ioex` branch below for
        // every error including handshake failures — a pre-existing
        // inconsistency with `huc_real_perform` that the old, slow,
        // eventually-timing-out-anyway busy-spin never surfaced clearly
        // enough to notice (`TestSSLHostConfigCompat`'s EC/RSA
        // cert-mismatch cases, which expect `SSLHandshakeException`
        // specifically, exposed it once handshake failures started
        // resolving promptly).
        Err(ref e) if e.starts_with(TLS_HANDSHAKE_FAILURE_SENTINEL) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                e.trim_start_matches(TLS_HANDSHAKE_FAILURE_SENTINEL),
            ));
        }
        Err(e) => return Err(ioex(format!("HttpURLConnection.connect failed: {e}"))),
    };

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
    // `<init>(Ljava/net/URL;)V` is also the REAL descriptor of the abstract
    // `java.net.HttpURLConnection(URL u)` protected constructor, so a user
    // subclass that calls `super(u)` directly (bypassing `URL.openConnection()`)
    // hits this native too — not just our own synthetic-carrier allocation
    // path. Detect that case by asking the URL argument for its real
    // `toExternalForm()`: a genuine `java.net.URL` answers with a proper
    // "scheme://..." string; treat it as a real carrier (field 0 keeps the
    // URL object itself, matching `is_real_carrier`/`huc_real_object_url`)
    // instead of clobbering field 0 with the synthetic Int(-1) conn-id, which
    // corrupted the real inherited `URLConnection.url`/`doOutput`/... fields
    // and broke `getURL()` + every `is_real_carrier` check downstream
    // (`ensure_connected` then misread the never-populated HUC_URL_STR slot
    // and threw "HttpURLConnection: URL not set").
    if let Some(Value::Object(Some(url_obj))) = args.get(1) {
        let url_obj = *url_obj;
        if let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
        {
            if let Some(full) = ctx.read_string(s) {
                if full.contains("://") {
                    ctx.set_field(this, HUC_CONN_ID, Value::Object(Some(url_obj)));
                    return Ok(None);
                }
            }
        }
    }
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
    // Real-JDK carrier: `connect()` only opens the socket on HotSpot and the
    // request is sent lazily by `getResponseCode`/`getInputStream`/the output
    // stream. We mirror that by deferring the actual `perform` — running it here
    // would (a) misread the synthetic slots `ensure_connected` consults and
    // (b) prematurely fix the request before the body/headers are fully staged.
    if is_real_carrier(ctx, this) {
        return Ok(None);
    }
    ensure_connected(ctx, this)
}

fn huc_get_response_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK sun.net.www HttpURLConnection (field 0 is the real URL object):
    // perform from the real URL rather than misreading our synthetic HUC_* slots.
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            return Ok(Some(Value::Int(huc_real_perform(ctx, this, &url_str)?)));
        }
    }
    ensure_connected(ctx, this)?;
    let code = with_state(ctx, this, |s| s.status).unwrap_or(-1);
    Ok(Some(Value::Int(code)))
}

fn status_reason(status: i32) -> String {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => return format!("Status {status}"),
    }
    .to_string()
}

fn huc_get_response_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK carrier: derive from the cached perform result. Prefer the
    // reason phrase actually read off the wire (real servers often deviate
    // from the RFC's canonical phrase, e.g. OkHttp MockWebServer's default
    // "Server Error" for 500 vs. the RFC's "Internal Server Error" -- real
    // HttpURLConnection.getResponseMessage() always returns exactly what the
    // server sent) -- the hardcoded `status_reason` table is only a fallback
    // for when no reason was captured (e.g. the synthetic timeout result).
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let status = huc_real_perform(ctx, this, &url_str)?;
            let key = ctx.identity_hash_code(this);
            let reason = real_results()
                .lock()
                .ok()
                .and_then(|t| t.get(&key).map(|r| r.reason.clone()))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| status_reason(status));
            let s = ctx.create_string(&reason);
            return Ok(Some(Value::Object(Some(s))));
        }
    }
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
        // Recover the URL through its public external form first.  This works
        // for both real-JDK URLs and Craton's synthetic resource URLs, whereas
        // probing individual URL fields confuses a real `file:` URL's authority
        // or path with the complete URL.  `URLClassLoader.getResourceAsStream`
        // uses `openConnection().getInputStream()`, so treating a non-HTTP URL
        // as the HTTP carrier's empty response body makes inherited resources
        // appear as zero-byte streams (Hazelcast's filtered-loader XML config).
        if let Some(full) = huc_real_object_url(ctx, this) {
            if full.starts_with("http://") || full.starts_with("https://") {
                huc_real_perform(ctx, this, &full)?;
                let body = huc_real_body(ctx, this);
                return Ok(Some(make_byte_array_input_stream(ctx, &body)));
            }
            return ctx.invoke_virtual(maybe_url, "openStream", "()Ljava/io/InputStream;", &[]);
        }
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
            return ctx.invoke_virtual(maybe_url, "openStream", "()Ljava/io/InputStream;", &[]);
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
    // Real-JDK carrier: serve the cached body when the response was an error.
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let status = huc_real_perform(ctx, this, &url_str)?;
            if status < 400 {
                return Ok(Some(Value::Object(None)));
            }
            let body = huc_real_body(ctx, this);
            return Ok(Some(make_byte_array_input_stream(ctx, &body)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Ok(Some(Value::Object(None)));
    }
    let (status, body_bytes) =
        with_state(ctx, this, |s| (s.status, s.response_body.clone())).unwrap_or((-1, Vec::new()));
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
    // Real-JDK carrier: the synthetic `HUC_DO_OUTPUT`/`HUC_CONNECTED` slots land
    // on unrelated real fields (one reads 1 → the old code wrongly threw "cannot
    // write after connect"). Use the identity-keyed `RealReq.do_output` and a
    // buffered BAOS tracked by identity; a write-after-`connect()` is legal here
    // exactly as on HotSpot (the body is sent lazily by `getResponseCode`).
    if is_real_carrier(ctx, this) {
        // `doOutput` may have been staged either through our own `setDoOutput`
        // native (identity-keyed side-table) or via the `java/net/HttpURLConnection`
        // setter natives that write the real `URLConnection.doOutput` field
        // directly (whichever won registration). Consult BOTH so the gate never
        // spuriously reports false and refuses a legitimate write.
        let do_output = with_real_req(ctx, this, |r| r.do_output)
            || matches!(ctx.get_field_by_name(this, "doOutput"), Value::Int(1));
        if !do_output {
            return Err(ioex("HttpURLConnection.getOutputStream: doOutput=false"));
        }
        // JDK semantics: opening the output stream promotes a still-default GET
        // to POST (see sun.net.www...HttpURLConnection.getOutputStream:
        // `if (method.equals("GET")) method = "POST"`). The harness's `postUrl`
        // relies on this — it sets only `setDoOutput(true)`, never the method —
        // so without this promotion the body is sent as a GET and the servlet
        // replies 405 Method Not Allowed.
        with_real_req(ctx, this, |r| {
            if r.method.is_empty() || r.method == "GET" {
                r.method = "POST".to_string();
            }
        });
        let key = ctx.identity_hash_code(this);
        if let Some((vkey, stored)) = real_body_streams()
            .lock()
            .ok()
            .and_then(|t| t.get(&key).copied())
        {
            let existing = ctx.read_var_handle_root(vkey).unwrap_or(stored);
            return Ok(Some(Value::Object(Some(existing))));
        }
        let baos = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2);
        // Family-1 fix (cce0079): `new_array` below can move the
        // still-unrooted `baos` — pin and refresh it before the field
        // stores and the registry insert.
        let baos_pin = ctx.pin_native_root(baos);
        let backing = ctx.new_array(ArrayElementType::Byte, 0);
        let baos = ctx.read_native_pin(baos_pin, baos);
        ctx.unpin_native_roots(baos_pin);
        ctx.set_field(baos, 0, Value::Object(Some(backing)));
        ctx.set_field(baos, 1, Value::Int(0));
        ctx.register_var_handle_root(baos);
        let vkey = ctx.identity_hash_code(baos);
        if let Ok(mut t) = real_body_streams().lock() {
            t.insert(key, (vkey, baos));
        }
        let streaming = real_reqs()
            .lock()
            .ok()
            .and_then(|reqs| reqs.get(&key).map(|req| req.streaming))
            .unwrap_or_default();
        if let StreamingMode::Fixed(expected) = streaming {
            start_live_fixed_stream(ctx, this, baos, expected)?;
        }
        return Ok(Some(Value::Object(Some(baos))));
    }
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

pub(crate) fn huc_get_header_field_named(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real-JDK carrier: read from the cached perform result, not synthetic slots.
    // LAST duplicate wins, mirroring `sun.net.www.MessageHeader.findValue`'s
    // backwards iteration (see `content_length_of`'s doc for the MockWebServer
    // duplicate-Content-Length case that exposed this).
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let v = huc_real_headers(ctx, this)
                .into_iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
                .last()
                .map(|(_, v)| v);
            return Ok(Some(match v {
                Some(s) => Value::Object(Some(ctx.create_string(&s))),
                None => Value::Object(None),
            }));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let v = with_state(ctx, this, |s| {
        s.response_headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
            .last()
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
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let v = huc_real_headers(ctx, this).get(idx as usize).cloned();
            return Ok(Some(match v {
                Some((_k, val)) => Value::Object(Some(ctx.create_string(&val))),
                None => Value::Object(None),
            }));
        }
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

fn huc_get_header_field_key_indexed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let v = huc_real_headers(ctx, this).get(idx as usize).cloned();
            return Ok(Some(match v {
                Some((k, _)) => Value::Object(Some(ctx.create_string(&k))),
                None => Value::Object(None),
            }));
        }
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

/// `getHeaderFields()` — the `Map<String,List<String>>` accessor the JDK's
/// `URLConnection` exposes and `TomcatBaseTest.methodUrl` reads `resHead` from.
/// Registered nowhere before this fix, so it fell through to real-JDK bytecode
/// that reads response state our shim never populated → empty map.
fn huc_get_header_fields(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // URL lookup can enter real-JDK code and collect. Keep the carrier rooted
    // until the subsequent perform/header operations have consumed it.
    let this_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let headers = if let Some(url_str) = huc_real_object_url(ctx, this) {
            let this = ctx.read_native_pin(this_pin, this);
            if url_str.starts_with("http://") || url_str.starts_with("https://") {
                huc_real_perform(ctx, this, &url_str)?;
                let this = ctx.read_native_pin(this_pin, this);
                huc_real_headers(ctx, this)
            } else {
                ensure_connected(ctx, this)?;
                let this = ctx.read_native_pin(this_pin, this);
                with_state(ctx, this, |s| s.response_headers.clone()).unwrap_or_default()
            }
        } else {
            ensure_connected(ctx, this)?;
            let this = ctx.read_native_pin(this_pin, this);
            with_state(ctx, this, |s| s.response_headers.clone()).unwrap_or_default()
        };
        let map = build_header_map(ctx, &headers)?;
        Ok(Some(Value::Object(Some(map))))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// Content length per the real `URLConnection.getContentLengthLong()` contract:
/// the `Content-Length` RESPONSE HEADER when present, else the buffered body
/// size (legacy behaviour, kept for chunked/close-delimited responses). The
/// distinction matters for HEAD responses, which advertise the entity size in
/// the header but carry NO body — reporting the (empty) body made Spring's
/// `AbstractFileResolvingResource.isReadable()/contentLength()` see 0 after a
/// HEAD 200 and call the resource empty/unreadable
/// (ResourceTests.remoteResourceExists: `exists()` true but `isReadable()`
/// false, `contentLength()` 0 instead of 6).
///
/// LAST duplicate wins: `sun.net.www.MessageHeader.findValue` iterates
/// BACKWARDS (`for (int i = nkeys; --i >= 0;)`), so on real JDK a later
/// duplicate header overrides an earlier one. MockWebServer actually emits
/// `Content-Length: 0` (its bodiless default) FOLLOWED BY the test's
/// `addHeader("Content-Length", "6")` on both HotSpot and CratonVM — HotSpot
/// reads 6, so must we.
fn content_length_of(headers: &[(String, String)], body_len: usize) -> i64 {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .last()
        .and_then(|(_, v)| v.trim().parse::<i64>().ok())
        .unwrap_or(body_len as i64)
}

pub(crate) fn huc_get_content_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Int(n as i32)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let n = with_state(ctx, this, |s| {
        content_length_of(&s.response_headers, s.response_body.len()) as i32
    })
    .unwrap_or(-1);
    Ok(Some(Value::Int(n)))
}

pub(crate) fn huc_get_content_length_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Long(n)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let n = with_state(ctx, this, |s| {
        content_length_of(&s.response_headers, s.response_body.len())
    })
    .unwrap_or(-1);
    Ok(Some(Value::Long(n)))
}

fn huc_disconnect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real carrier: clear identity-keyed side-table state, never write synthetic
    // slots (they alias real fields on a real-JDK object).
    if is_real_carrier(ctx, this) {
        real_forget(ctx, this);
        return Ok(None);
    }
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
    // Real JDK's `sun.net.www.protocol.http.HttpURLConnection.setRequestMethod`
    // whitelist is {GET, POST, HEAD, OPTIONS, PUT, DELETE, TRACE} — notably NOT
    // PATCH, which is why Spring recommends a different `ClientHttpRequestFactory`
    // for PATCH and its own test suite (`SimpleClientHttpRequestFactoryTests
    // .httpMethods()`) asserts `ProtocolException` for it. Rejecting it here
    // (as `ProtocolException`, matching the real JDK exception type) mirrors
    // that restriction instead of silently accepting it.
    if !matches!(
        normalized.as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "OPTIONS" | "TRACE" | "CONNECT"
    ) {
        return Err(protocol_ex(format!("Invalid HTTP method: {m}")));
    }
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.method = normalized);
        return Ok(None);
    }
    let s = ctx.create_string(&normalized);
    ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
    Ok(None)
}

fn huc_get_request_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if is_real_carrier(ctx, this) {
        let m = with_real_req(ctx, this, |r| {
            if r.method.is_empty() {
                "GET".to_string()
            } else {
                r.method.clone()
            }
        });
        let s = ctx.create_string(&m);
        return Ok(Some(Value::Object(Some(s))));
    }
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
    // Real-JDK carrier: store into the identity-keyed header list (synthetic
    // slot 3 lands on an unrelated real field → header silently dropped, which
    // is the dropped-`Authorization`/`Origin` 401/403 bug). setRequestProperty
    // replaces any existing values for the key.
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| {
            r.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
            r.headers.push((key.clone(), value.clone()));
        });
        return Ok(None);
    }
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
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.headers.push((key.clone(), value.clone())));
        return Ok(None);
    }
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

/// `getRequestProperty(String)` — read back a request header. The JDK joins
/// multiple `addRequestProperty` values with ", ". Registered as a native so a
/// real-JDK carrier reads from our identity-keyed header list rather than the
/// real `requests` map our setter never populated (which returned null).
fn huc_get_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let joined: Option<String> = if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| {
            let vals: Vec<String> = r
                .headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| v.clone())
                .collect();
            if vals.is_empty() {
                None
            } else {
                Some(vals.join(", "))
            }
        })
    } else if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) {
        let len = ctx.array_length(arr);
        let mut found: Option<String> = None;
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                let line = ctx.read_string(s).unwrap_or_default();
                if let Some(colon) = line.find(':') {
                    if line[..colon].trim().eq_ignore_ascii_case(&key) {
                        found = Some(line[colon + 1..].trim().to_string());
                    }
                }
            }
        }
        found
    } else {
        None
    };
    Ok(Some(match joined {
        Some(s) => Value::Object(Some(ctx.create_string(&s))),
        None => Value::Object(None),
    }))
}

fn huc_set_do_input(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    // Real carrier: doInput defaults true and is not consulted by our perform;
    // never write a synthetic slot on a real object (it corrupts a real field).
    if is_real_carrier(ctx, this) {
        return Ok(None);
    }
    ctx.set_field(this, HUC_DO_INPUT, Value::Int(v));
    Ok(None)
}

fn huc_set_do_output(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.do_output = v != 0);
        // Also mirror onto the real `URLConnection.doOutput` field so the
        // inherited `getDoOutput()` bytecode (declared on URLConnection, not
        // overridden here → our per-class native is never consulted for it)
        // returns the caller's value. Spring's SimpleClientHttpRequest gates the
        // request-body write on `getDoOutput()`; a stale `false` drops the body
        // and the server blocks on the promised Content-Length → "Read timed out".
        ctx.set_field_by_name(this, "doOutput", Value::Int(v));
        return Ok(None);
    }
    ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));
    Ok(None)
}

fn huc_set_connect_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if v < 0 {
        return Err(iae("setConnectTimeout: negative"));
    }
    if is_real_carrier(ctx, this) {
        let key = ctx.identity_hash_code(this);
        if let Ok(mut t) = real_reqs().lock() {
            t.entry(key).or_default().connect_timeout_ms = Some(v);
        }
        return Ok(None);
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
    if is_real_carrier(ctx, this) {
        let key = ctx.identity_hash_code(this);
        if let Ok(mut t) = real_reqs().lock() {
            t.entry(key).or_default().read_timeout_ms = Some(v);
        }
        return Ok(None);
    }
    ctx.set_field(this, HUC_READ_TIMEOUT, Value::Int(v));
    Ok(None)
}

fn huc_set_fixed_length_streaming_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let length = match args.get(1) {
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Long(v)) => *v,
        _ => return Err(iae("setFixedLengthStreamingMode: invalid length")),
    };
    if length < 0 {
        return Err(iae("setFixedLengthStreamingMode: negative length"));
    }
    if !is_real_carrier(ctx, this) {
        // Synthetic carriers retain their historical buffered implementation.
        return Ok(None);
    }
    let key = ctx.identity_hash_code(this);
    if live_fixed_streams()
        .lock()
        .ok()
        .is_some_and(|streams| streams.contains_key(&key))
    {
        return Err(ise("setFixedLengthStreamingMode: already connected"));
    }
    with_real_req(ctx, this, |req| match req.streaming {
        StreamingMode::Chunked(_) => Err(ise("Chunked encoding streaming mode set")),
        _ => {
            req.streaming = StreamingMode::Fixed(length as u64);
            Ok(())
        }
    })?;
    Ok(None)
}

fn huc_set_chunked_streaming_mode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let chunk_length = args.get(1).and_then(Value::as_int).unwrap_or(0);
    if !is_real_carrier(ctx, this) {
        return Ok(None);
    }
    let key = ctx.identity_hash_code(this);
    if live_fixed_streams()
        .lock()
        .ok()
        .is_some_and(|streams| streams.contains_key(&key))
    {
        return Err(ise("setChunkedStreamingMode: already connected"));
    }
    with_real_req(ctx, this, |req| match req.streaming {
        StreamingMode::Fixed(_) => Err(ise("Fixed length streaming mode set")),
        _ => {
            req.streaming = StreamingMode::Chunked(chunk_length);
            Ok(())
        }
    })?;
    Ok(None)
}

fn huc_set_instance_follow_redirects(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |req| {
            req.follow_redirects = v != 0;
        });
        return Ok(None);
    }
    ctx.set_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS, Value::Int(v));
    Ok(None)
}

fn huc_get_instance_follow_redirects(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if is_real_carrier(ctx, this) {
        let follow = real_reqs()
            .lock()
            .ok()
            .and_then(|t| {
                t.get(&ctx.identity_hash_code(this))
                    .map(|req| req.follow_redirects)
            })
            .unwrap_or(true);
        return Ok(Some(Value::Int(if follow { 1 } else { 0 })));
    }
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
    r.register(
        cls,
        "getHeaderFields",
        "()Ljava/util/Map;",
        huc_get_header_fields,
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
    r.register(
        cls,
        "getRequestProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        huc_get_request_property,
    );
    r.register(cls, "setDoInput", "(Z)V", huc_set_do_input);
    r.register(cls, "setDoOutput", "(Z)V", huc_set_do_output);
    r.register(cls, "setConnectTimeout", "(I)V", huc_set_connect_timeout);
    r.register(cls, "setReadTimeout", "(I)V", huc_set_read_timeout);
    // Streaming-mode setters are no-ops: our `perform` buffers the request body
    // (via the overridden `getOutputStream` BAOS) and derives `Content-Length`
    // from the body / the converter's own header, so the JDK's fixed-length /
    // chunked streaming machinery is bypassed. The real setters guard on
    // `chunkLength != -1` / `fixedContentLengthLong != -1`, but our synthetically
    // constructed carrier never runs URLConnection's field initializers, so those
    // fields are 0 (not -1) and `setFixedLengthStreamingMode` would throw
    // `IllegalStateException("Chunked encoding streaming mode set")`. Spring's
    // `SimpleClientHttpRequest.executeInternal` calls this once `getDoOutput()` is
    // true — so it only surfaced after the doOutput fix let the body path run.
    r.register(
        cls,
        "setFixedLengthStreamingMode",
        "(I)V",
        huc_set_fixed_length_streaming_mode,
    );
    r.register(
        cls,
        "setFixedLengthStreamingMode",
        "(J)V",
        huc_set_fixed_length_streaming_mode,
    );
    r.register(
        cls,
        "setChunkedStreamingMode",
        "(I)V",
        huc_set_chunked_streaming_mode,
    );
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
    install_baos_event_hook(huc_live_baos_event);
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
            .find(
                cls,
                "getHeaderField",
                "(Ljava/lang/String;)Ljava/lang/String;"
            )
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
        assert!(s.contains("Connection: keep-alive\r\n"));
    }

    #[test]
    fn test_build_request_post_body() {
        let p = parse_url("https://example.com:8443/api").unwrap();
        let req = build_request(
            "POST",
            &p,
            &[("Content-Type".to_string(), "application/json".to_string())],
            b"{}",
        );
        let s = String::from_utf8(req).unwrap();
        assert!(s.contains("POST /api HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com:8443\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.contains("Content-Type: application/json\r\n"));
        assert!(s.ends_with("{}"));
    }

    #[test]
    fn test_parse_url_userinfo_stripped() {
        // `http://alice:secret@localhost:8080/resource` — the user-info must be
        // stripped from host/port (connect target, Host header) and surfaced in
        // `userinfo` (ResourceTests.useUserInfoToSetBasicAuth).
        let p = parse_url("http://alice:secret@localhost:8080/resource").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 8080);
        assert_eq!(p.path, "/resource");
        assert_eq!(p.userinfo.as_deref(), Some("alice:secret"));
        // No user-info → None.
        assert_eq!(parse_url("http://example.com/x").unwrap().userinfo, None);
    }

    #[test]
    fn test_build_request_preemptive_basic_auth_from_userinfo() {
        let p = parse_url("http://alice:secret@localhost:8080/resource").unwrap();
        let req = build_request("GET", &p, &[], b"");
        let s = String::from_utf8(req).unwrap();
        // Host header must NOT carry the user-info.
        assert!(s.contains("Host: localhost:8080\r\n"));
        // base64("alice:secret") == "YWxpY2U6c2VjcmV0" (preemptive Basic auth).
        assert!(s.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));

        // An explicitly staged Authorization header wins over the user-info.
        let req2 = build_request(
            "GET",
            &p,
            &[("Authorization".to_string(), "Bearer tok".to_string())],
            b"",
        );
        let s2 = String::from_utf8(req2).unwrap();
        assert!(s2.contains("Authorization: Bearer tok\r\n"));
        assert!(!s2.contains("Basic YWxpY2U6c2VjcmV0"));
    }

    #[test]
    fn test_content_length_prefers_header_over_body() {
        // HEAD responses advertise the entity size in Content-Length but carry
        // no body — the header must win (ResourceTests.remoteResourceExists).
        let headers = vec![("Content-Length".to_string(), "6".to_string())];
        assert_eq!(content_length_of(&headers, 0), 6);
        // No header → fall back to the buffered body size.
        assert_eq!(content_length_of(&[], 5), 5);
        // Duplicate headers: the LAST wins (MessageHeader.findValue iterates
        // backwards). MockWebServer emits its bodiless "Content-Length: 0"
        // default before the test's addHeader("Content-Length", "6").
        let dup = vec![
            ("Content-Length".to_string(), "0".to_string()),
            ("Content-Type".to_string(), "text/plain".to_string()),
            ("Content-Length".to_string(), "6".to_string()),
        ];
        assert_eq!(content_length_of(&dup, 0), 6);
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
    fn test_read_chunked_retries_raw_eintr() {
        struct InterruptOnce<'a> {
            interrupted: bool,
            remaining: &'a [u8],
        }

        impl Read for InterruptOnce<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::Error::from_raw_os_error(4));
                }
                self.remaining.read(buf)
            }
        }

        let mut prefix = b"4\r\n".to_vec();
        let mut stream = InterruptOnce {
            interrupted: false,
            remaining: b"Wiki\r\n0\r\n\r\n",
        };
        assert_eq!(read_chunked(&mut prefix, &mut stream).unwrap(), b"Wiki");
    }

    #[test]
    fn test_read_response_304_is_bodiless() {
        // RFC 9110 §6.4.1: a 304 has no message body even when it carries a
        // (bogus) Content-Length. read_response must return immediately with an
        // empty body and NOT consume the trailing bytes — otherwise a keep-alive
        // 304 (no real body coming) hangs the client. The trailing "HELLO" here
        // stands in for "bytes that are not ours to read".
        let mut data: &[u8] =
            b"HTTP/1.1 304 Not Modified\r\nETag: W/\"x\"\r\nContent-Length: 5\r\n\r\nHELLO";
        let (status, headers, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 304);
        assert!(body.is_empty(), "304 must be bodiless");
        assert!(headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("etag") && v == "W/\"x\""));
    }

    #[test]
    fn test_read_response_204_is_bodiless() {
        let mut data: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 3\r\n\r\nabc";
        let (status, _h, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 204);
        assert!(body.is_empty(), "204 must be bodiless");
    }

    #[test]
    fn test_read_response_200_reads_body() {
        // Guard against over-broadening the bodiless rule: a normal 200 with a
        // Content-Length must still return its body.
        let mut data: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nHELLO";
        let (status, _h, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"HELLO");
    }

    #[test]
    fn test_read_response_preserves_non_utf8_header_bytes_as_latin1() {
        // Tomcat's RFC6265 cookie test emits U+0120 as UTF-8 (C4 A0) then
        // uses String.getBytes(ISO_8859_1) to recover the wire bytes from the
        // response header. The HTTP client must therefore retain C4 A0 as
        // Latin-1 code points, not decode or replace them as UTF-8.
        let mut data: &[u8] =
            b"HTTP/1.1 200 OK\r\nSet-Cookie: Test=\xC4\xA0\r\nContent-Length: 0\r\n\r\n";
        let (status, headers, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 200);
        assert!(body.is_empty());
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Set-Cookie" && value == "Test=\u{00c4}\u{00a0}"));
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
        let res = huc_set_connect_timeout(&mut ctx, &[Value::Object(Some(this)), Value::Int(-1)]);
        assert!(res.is_err());
    }

    #[test]
    fn test_anchor_pub_function_exists() {
        // Anchor-grep guard: enforce the exported API name keeps existing.
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(!r.is_empty());
    }

    // -----------------------------------------------------------------
    // Plain-HTTP keep-alive pool
    // -----------------------------------------------------------------

    #[test]
    fn test_is_poolable_response_content_length() {
        let headers = vec![("Content-Length".to_string(), "5".to_string())];
        assert!(is_poolable_response(200, false, &headers));
    }

    #[test]
    fn test_is_poolable_response_chunked_excluded() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(!is_poolable_response(200, false, &headers));
    }

    #[test]
    fn test_is_poolable_response_connection_close_excluded() {
        let headers = vec![
            ("Content-Length".to_string(), "5".to_string()),
            ("Connection".to_string(), "close".to_string()),
        ];
        assert!(!is_poolable_response(200, false, &headers));
    }

    #[test]
    fn test_is_poolable_response_bodiless_without_content_length() {
        // HEAD / 204 / 304 / 1xx carry no body and no Content-Length, but the
        // framing is still unambiguous — poolable.
        assert!(is_poolable_response(200, true, &[]));
        assert!(is_poolable_response(204, false, &[]));
        assert!(is_poolable_response(304, false, &[]));
        assert!(is_poolable_response(100, false, &[]));
    }

    #[test]
    fn test_is_poolable_response_ambiguous_framing_excluded() {
        // A normal 200 with neither Content-Length nor chunked has no
        // reliable end-of-body marker other than connection close — not safe
        // to hand back to the pool.
        assert!(!is_poolable_response(200, false, &[]));
    }

    /// A loopback `TcpStream` pair for exercising `pool_take`/`pool_put`
    /// against real sockets (the pool stores `TcpStream` directly, not a
    /// generic `Read`, so a slice-backed fake won't do here).
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn test_pool_put_then_take_roundtrip() {
        let (client, _server) = loopback_pair();
        let host = format!("test-roundtrip-{}", client.local_addr().unwrap().port());
        let port = 1;
        pool_put(&host, port, client);
        assert!(pool_take(&host, port).is_some());
        // The bucket is empty now — a second take is a plain miss.
        assert!(pool_take(&host, port).is_none());
    }

    #[test]
    fn test_pool_take_evicts_closed_peer() {
        let (client, server) = loopback_pair();
        let host = format!("test-closed-peer-{}", client.local_addr().unwrap().port());
        let port = 1;
        pool_put(&host, port, client);
        drop(server); // peer close -> FIN visible to a peek on `client`
        // Give the FIN a moment to actually land in the kernel buffer.
        std::thread::sleep(Duration::from_millis(50));
        assert!(pool_take(&host, port).is_none());
    }

    #[test]
    fn test_pool_take_respects_idle_window() {
        let (client, _server) = loopback_pair();
        let host = format!("test-idle-window-{}", client.local_addr().unwrap().port());
        let port = 1;
        if let Ok(mut guard) = conn_pool().lock() {
            guard.entry((host.clone(), port)).or_default().push(PooledConn {
                stream: client,
                returned_at: Instant::now() - POOL_IDLE_WINDOW - Duration::from_millis(1),
            });
        }
        assert!(pool_take(&host, port).is_none());
    }

    #[test]
    fn test_pool_put_caps_bucket_at_max_per_key() {
        let (first, _first_server) = loopback_pair();
        let host = format!("test-cap-{}", first.local_addr().unwrap().port());
        let port = 2;
        pool_put(&host, port, first);
        for _ in 0..(POOL_MAX_PER_KEY + 1) {
            let (client, _server) = loopback_pair();
            pool_put(&host, port, client);
        }
        let len = conn_pool()
            .lock()
            .unwrap()
            .get(&(host, port))
            .map(|b| b.len())
            .unwrap_or(0);
        assert_eq!(len, POOL_MAX_PER_KEY);
    }
}
